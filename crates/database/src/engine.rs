use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use anyhow::{Context as _, Result, anyhow, bail};
use collections::HashMap;
use http_client::HttpClient;
use serde::{Deserialize, Serialize};
use util::ResultExt as _;

use crate::{
    BrowseRequest, Capability, ChangeSet, Collection, CollectionRef, CollectionSchema,
    ConnectionSpec, DatabaseKind, HistoryBatch, Page, QueryLanguage, Rehearsal, ResultSet, Source,
    clickhouse::ClickHouseSource, convex::ConvexSource, sql::SqlSource,
};

/// A serializable request, so the same engine can run in-process or on a
/// remote host behind the project RPC.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Request {
    Connect {
        connection: String,
        spec: ConnectionSpec,
        secret: Option<String>,
    },
    Disconnect {
        connection: String,
    },
    Collections {
        connection: String,
    },
    Describe {
        connection: String,
        collection: CollectionRef,
    },
    Query {
        connection: String,
        text: String,
        limit: usize,
        read_only: bool,
    },
    Browse {
        connection: String,
        collection: CollectionRef,
        request: BrowseRequest,
    },
    RenderChanges {
        connection: String,
        changes: ChangeSet,
    },
    Apply {
        connection: String,
        changes: ChangeSet,
    },
    History {
        connection: String,
        cursor: Option<String>,
    },
    Assess {
        connection: String,
        text: String,
    },
    /// Runs `changes` in a transaction and describes what they did. With
    /// `hold`, the transaction stays open until `Settle` commits or rolls it
    /// back, so what the user approves is exactly what commits.
    Rehearse {
        connection: String,
        changes: ChangeSet,
        hold: bool,
    },
    Settle {
        connection: String,
        token: u64,
        commit: bool,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Response {
    Connected {
        capability: Capability,
        language: QueryLanguage,
    },
    Disconnected,
    Collections(Vec<Collection>),
    Schema(CollectionSchema),
    Rows(ResultSet),
    Page(Page),
    Text(String),
    Applied(u64),
    History(HistoryBatch),
    Notes(Vec<String>),
    /// `token` is set when the rehearsal's transaction is being held.
    Rehearsed {
        notes: Vec<String>,
        token: Option<u64>,
    },
}

/// How long a held rehearsal may wait for approval before it rolls back by
/// itself. Its transaction holds row locks and its connection meanwhile.
const HELD_REHEARSAL_TIMEOUT: Duration = Duration::from_secs(120);

struct Connected {
    source: Arc<dyn Source>,
    capability: Capability,
    // ponytail: one request at a time per connection. The SQL drivers each hold
    // a single session, and read-only queries are bracketed by
    // START TRANSACTION / ROLLBACK on it, so concurrent requests would land
    // inside each other's transactions. A pool would lift this.
    lock: Arc<tokio::sync::Mutex<()>>,
    /// A rehearsal whose transaction is open, waiting for approval. It owns
    /// the lock until it is settled.
    held: parking_lot::Mutex<Option<Held>>,
}

struct Held {
    token: u64,
    affected: u64,
    _guard: tokio::sync::OwnedMutexGuard<()>,
}

impl Connected {
    /// Waits for the connection, or fails fast while a rehearsal awaits
    /// approval rather than blocking until it is settled.
    async fn acquire(&self, connection: &str) -> Result<tokio::sync::MutexGuard<'_, ()>> {
        // ponytail: a request that checks just before a rehearsal is stored
        // still queues behind it. Holding `held` across the lock would close
        // that gap.
        if self.held.lock().is_some() {
            bail!(
                "a rehearsed change on {connection} is waiting for approval; approve or reject it first"
            );
        }
        Ok(self.lock.lock().await)
    }

    async fn settle(&self, token: u64, commit: bool) -> Result<u64> {
        let held = {
            let mut held = self.held.lock();
            match held.take() {
                Some(found) if found.token == token => found,
                other => {
                    *held = other;
                    bail!("the rehearsal expired and was rolled back; nothing was written");
                }
            }
        };
        self.source.settle(commit).await?;
        Ok(if commit { held.affected } else { 0 })
    }
}

/// Runs database requests. Its futures must be polled on a Tokio runtime,
/// since the MySQL and Postgres drivers are Tokio-based.
pub struct Engine {
    http_client: Arc<dyn HttpClient>,
    connections: parking_lot::Mutex<HashMap<String, Arc<Connected>>>,
    next_token: AtomicU64,
    held_rehearsal_timeout: Duration,
}

impl Engine {
    pub fn new(http_client: Arc<dyn HttpClient>) -> Self {
        Self {
            http_client,
            connections: parking_lot::Mutex::new(HashMap::default()),
            next_token: AtomicU64::new(1),
            held_rehearsal_timeout: HELD_REHEARSAL_TIMEOUT,
        }
    }

    /// Shortens how long held rehearsals wait, so tests can watch one expire.
    pub fn with_held_rehearsal_timeout(mut self, timeout: Duration) -> Self {
        self.held_rehearsal_timeout = timeout;
        self
    }

    pub async fn handle(&self, request: Request) -> Result<Response> {
        match request {
            Request::Connect {
                connection,
                spec,
                secret,
            } => self.connect(connection, spec, secret).await,
            Request::Disconnect { connection } => {
                self.connections.lock().remove(&connection);
                Ok(Response::Disconnected)
            }
            Request::Collections { connection } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                Ok(Response::Collections(connected.source.collections().await?))
            }
            Request::Describe {
                connection,
                collection,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                let mut schema = connected.source.describe(&collection).await?;
                if !connected.capability.is_writable() {
                    schema.writable = Some(false);
                }
                Ok(Response::Schema(schema))
            }
            Request::Query {
                connection,
                text,
                limit,
                read_only,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                let read_only = read_only || !connected.capability.is_writable();
                Ok(Response::Rows(
                    connected.source.query(&text, limit, read_only).await?,
                ))
            }
            Request::Browse {
                connection,
                collection,
                request,
            } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                Ok(Response::Page(
                    connected.source.browse(&collection, &request).await?,
                ))
            }
            Request::RenderChanges {
                connection,
                changes,
            } => {
                let connected = self.connected(&connection)?;
                Ok(Response::Text(connected.source.render_changes(&changes)?))
            }
            Request::Apply {
                connection,
                changes,
            } => {
                let connected = self.connected(&connection)?;
                if let Capability::ReadOnly { reason } = &connected.capability {
                    bail!("{connection} is read-only: {reason}");
                }
                let _guard = connected.acquire(&connection).await?;
                Ok(Response::Applied(connected.source.apply(&changes).await?))
            }
            Request::History { connection, cursor } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                Ok(Response::History(connected.source.history(cursor).await?))
            }
            Request::Assess { connection, text } => {
                let connected = self.connected(&connection)?;
                let _guard = connected.acquire(&connection).await?;
                Ok(Response::Notes(connected.source.assess(&text).await?))
            }
            Request::Rehearse {
                connection,
                changes,
                hold,
            } => {
                let connected = self.connected(&connection)?;
                if let Capability::ReadOnly { reason } = &connected.capability {
                    bail!("{connection} is read-only: {reason}");
                }
                if connected.held.lock().is_some() {
                    bail!("another rehearsed change on {connection} is waiting for approval");
                }
                let guard = connected.lock.clone().lock_owned().await;
                let Rehearsal {
                    notes,
                    affected,
                    held,
                } = connected.source.rehearse(&changes, hold).await?;
                if !held {
                    return Ok(Response::Rehearsed { notes, token: None });
                }
                let token = self.next_token.fetch_add(1, Ordering::Relaxed);
                *connected.held.lock() = Some(Held {
                    token,
                    affected,
                    _guard: guard,
                });
                let weak = Arc::downgrade(&connected);
                let timeout = self.held_rehearsal_timeout;
                tokio::spawn(async move {
                    tokio::time::sleep(timeout).await;
                    let Some(connected) = weak.upgrade() else {
                        return;
                    };
                    let still_waiting = connected
                        .held
                        .lock()
                        .as_ref()
                        .is_some_and(|held| held.token == token);
                    if still_waiting {
                        connected
                            .settle(token, false)
                            .await
                            .context("rolling back an expired rehearsal")
                            .log_err();
                    }
                });
                Ok(Response::Rehearsed {
                    notes,
                    token: Some(token),
                })
            }
            Request::Settle {
                connection,
                token,
                commit,
            } => {
                let connected = self.connected(&connection)?;
                Ok(Response::Applied(connected.settle(token, commit).await?))
            }
        }
    }

    fn connected(&self, connection: &str) -> Result<Arc<Connected>> {
        self.connections
            .lock()
            .get(connection)
            .cloned()
            .ok_or_else(|| anyhow!("{connection} is not connected"))
    }

    async fn connect(
        &self,
        connection: String,
        spec: ConnectionSpec,
        secret: Option<String>,
    ) -> Result<Response> {
        let source: Arc<dyn Source> = match spec.kind {
            DatabaseKind::Mysql | DatabaseKind::Postgres => {
                Arc::new(SqlSource::connect(&connection, &spec, secret).await?)
            }
            DatabaseKind::Clickhouse => Arc::new(ClickHouseSource::direct(
                self.http_client.clone(),
                url(&spec)?,
                spec.user.clone().unwrap_or_else(|| "default".to_string()),
                secret,
            )),
            DatabaseKind::ClickhouseRelay => Arc::new(ClickHouseSource::relay(
                self.http_client.clone(),
                url(&spec)?,
                secret.context("a ClickHouse relay needs its relay secret")?,
            )),
            DatabaseKind::Convex => Arc::new(ConvexSource::new(
                self.http_client.clone(),
                url(&spec)?,
                secret.context("a Convex connection needs an admin or deploy key")?,
            )),
        };
        let capability = if spec.read_only {
            // Still probe, so bad credentials fail on connect rather than on
            // first use.
            source.probe().await?;
            Capability::ReadOnly {
                reason: "pinned read-only in settings".to_string(),
            }
        } else {
            source.probe().await?
        };
        let language = source.query_language();
        self.connections.lock().insert(
            connection,
            Arc::new(Connected {
                source,
                capability: capability.clone(),
                lock: Arc::default(),
                held: parking_lot::Mutex::new(None),
            }),
        );
        Ok(Response::Connected {
            capability,
            language,
        })
    }
}

fn url(spec: &ConnectionSpec) -> Result<String> {
    let url = spec
        .url
        .as_deref()
        .context("this connection needs a `url`")?
        .trim_end_matches('/');
    Ok(url.to_string())
}
