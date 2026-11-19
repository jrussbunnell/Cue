use std::{ops::Range, sync::Arc};

use anyhow::{Context as _, Result, bail};
use async_trait::async_trait;
use zqlz_core::{Connection, ConnectionConfig, DatabaseDriver, SchemaIntrospection};

use crate::{
    BrowseRequest, Capability, Change, ChangeSet, Collection, CollectionKind, CollectionRef,
    CollectionSchema, Column, ConnectionSpec, DatabaseEnvironment, DatabaseKind, Field, Filter,
    FilterOp, Page, QueryLanguage, Reference, Rehearsal, ResultSet, Source, Value, checkable,
    default_port, kind_name,
};

pub(crate) struct SqlSource {
    kind: DatabaseKind,
    connection: Arc<dyn Connection>,
    database: Option<String>,
}

impl SqlSource {
    pub async fn connect(
        name: &str,
        spec: &ConnectionSpec,
        secret: Option<String>,
    ) -> Result<Self> {
        let mut config = ConnectionConfig::new(kind_name(spec.kind), name);
        config.host = spec.host.clone().unwrap_or_else(|| "localhost".to_string());
        config.port = spec.port.unwrap_or_else(|| default_port(spec.kind));
        config.database = spec.database.clone();
        config.username = spec.user.clone();
        config.password = secret;
        if let Some(ssl_mode) = &spec.ssl_mode {
            config
                .params
                .insert("ssl_mode".to_string(), driver_ssl_mode(spec.kind, ssl_mode));
        }
        let connection = match spec.kind {
            DatabaseKind::Mysql => zqlz_driver_mysql::MySqlDriver::new().connect(&config).await,
            DatabaseKind::Postgres => {
                zqlz_driver_postgres::PostgresDriver::new()
                    .connect(&config)
                    .await
            }
            _ => bail!("{} is not a SQL driver", kind_name(spec.kind)),
        }
        .with_context(|| format!("connecting to {name}"))?;
        let source = Self {
            kind: spec.kind,
            connection,
            database: spec.database.clone(),
        };
        source
            .configure_session(spec.environment)
            .await
            .with_context(|| format!("configuring the session on {name}"))?;
        Ok(source)
    }

    /// Names the session so it is recognizable in `pg_stat_activity`, and on
    /// prod bounds how long a statement, a lock wait or an idle transaction
    /// may hold anything up.
    async fn configure_session(&self, environment: DatabaseEnvironment) -> Result<()> {
        let prod = environment == DatabaseEnvironment::Prod;
        match self.kind {
            DatabaseKind::Postgres => {
                let mut settings = vec![("application_name", "Cue")];
                if prod {
                    settings.extend([
                        ("lock_timeout", "10s"),
                        ("idle_in_transaction_session_timeout", "10min"),
                        ("statement_timeout", "15min"),
                    ]);
                }
                let calls = (1..=settings.len())
                    .map(|index| {
                        format!(
                            "set_config({}, {}, false)",
                            self.placeholder(index * 2 - 1),
                            self.placeholder(index * 2)
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                let params = settings
                    .iter()
                    .flat_map(|(name, value)| {
                        [
                            Value::Text(name.to_string()),
                            Value::Text(value.to_string()),
                        ]
                    })
                    .collect::<Vec<_>>();
                self.rows(&format!("SELECT {calls}"), &params).await?;
            }
            _ if prod => {
                self.statement("SET SESSION innodb_lock_wait_timeout = 10")
                    .await?
            }
            _ => {}
        }
        Ok(())
    }

    fn introspection(&self) -> Result<&dyn SchemaIntrospection> {
        self.connection
            .as_schema_introspection()
            .context("this driver does not support schema introspection")
    }

    async fn rows(&self, sql: &str, params: &[Value]) -> Result<ResultSet> {
        let params = params.iter().map(to_driver_value).collect::<Vec<_>>();
        let result = self.connection.execute(sql, &params).await?;
        Ok(match result.result {
            Some(result) => ResultSet {
                columns: result
                    .columns
                    .iter()
                    .map(|column| Column {
                        name: column.name.clone(),
                        type_name: column.data_type.clone(),
                    })
                    .collect(),
                rows: result
                    .rows
                    .into_iter()
                    .map(|row| row.values.into_iter().map(from_driver_value).collect())
                    .collect(),
                truncated: false,
                affected_rows: None,
            },
            None => ResultSet {
                affected_rows: Some(result.affected_rows),
                ..ResultSet::default()
            },
        })
    }

    async fn statement(&self, sql: &str) -> Result<()> {
        self.connection.execute(sql, &[]).await?;
        Ok(())
    }

    fn quote(&self, identifier: &str) -> String {
        quote_identifier(self.kind, identifier)
    }

    fn table(&self, collection: &CollectionRef) -> String {
        match &collection.namespace {
            Some(namespace) => {
                format!("{}.{}", self.quote(namespace), self.quote(&collection.name))
            }
            None => self.quote(&collection.name),
        }
    }

    fn placeholder(&self, index: usize) -> String {
        match self.kind {
            DatabaseKind::Postgres => format!("${index}"),
            _ => "?".to_string(),
        }
    }

    fn begin_read_only(&self) -> &'static str {
        match self.kind {
            DatabaseKind::Postgres => "BEGIN READ ONLY",
            _ => "START TRANSACTION READ ONLY",
        }
    }

    fn begin(&self) -> &'static str {
        match self.kind {
            DatabaseKind::Postgres => "BEGIN",
            _ => "START TRANSACTION",
        }
    }

    /// Builds the statements for a change set. With `inline`, values are
    /// rendered as literals in the same pass (for previews) instead of being
    /// bound, so a value can never be mistaken for a placeholder.
    fn statements(&self, changes: &ChangeSet, inline: bool) -> Result<Vec<(String, Vec<Value>)>> {
        let table = self.table(&changes.collection);
        changes
            .changes
            .iter()
            .map(|change| {
                let mut params = Vec::new();
                let sql = match change {
                    Change::Update { key, set, expected } => {
                        anyhow::ensure!(!key.is_empty(), "an update needs a key");
                        anyhow::ensure!(!set.is_empty(), "an update needs at least one field");
                        let assignments = set
                            .iter()
                            .map(|(field, value)| {
                                format!(
                                    "{} = {}",
                                    self.quote(field),
                                    self.bind(value, &mut params, inline)
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(", ");
                        let mut conditions = key.clone();
                        conditions.extend(checkable(expected));
                        let condition = self.key_condition(&conditions, &mut params, inline);
                        format!("UPDATE {table} SET {assignments} WHERE {condition}")
                    }
                    Change::Insert { values } => {
                        anyhow::ensure!(!values.is_empty(), "an insert needs at least one field");
                        let fields = values
                            .iter()
                            .map(|(field, _)| self.quote(field))
                            .collect::<Vec<_>>()
                            .join(", ");
                        let placeholders = values
                            .iter()
                            .map(|(_, value)| self.bind(value, &mut params, inline))
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("INSERT INTO {table} ({fields}) VALUES ({placeholders})")
                    }
                    Change::Delete { key } => {
                        anyhow::ensure!(!key.is_empty(), "a delete needs a key");
                        let condition = self.key_condition(key, &mut params, inline);
                        format!("DELETE FROM {table} WHERE {condition}")
                    }
                };
                Ok((sql, params))
            })
            .collect()
    }

    fn bind(&self, value: &Value, params: &mut Vec<Value>, inline: bool) -> String {
        if inline {
            sql_literal(self.kind, value)
        } else {
            params.push(value.clone());
            self.placeholder(params.len())
        }
    }

    fn key_condition(
        &self,
        key: &[(String, Value)],
        params: &mut Vec<Value>,
        inline: bool,
    ) -> String {
        key.iter()
            .map(|(field, value)| {
                if value.is_nullish() {
                    format!("{} IS NULL", self.quote(field))
                } else {
                    format!(
                        "{} = {}",
                        self.quote(field),
                        self.bind(value, params, inline)
                    )
                }
            })
            .collect::<Vec<_>>()
            .join(" AND ")
    }

    fn filter_condition(&self, filter: &Filter, params: &mut Vec<Value>) -> String {
        let field = self.quote(&filter.field);
        let mut bind = |value: Value| {
            params.push(value);
            self.placeholder(params.len())
        };
        match filter.op {
            FilterOp::IsNull => format!("{field} IS NULL"),
            FilterOp::IsNotNull => format!("{field} IS NOT NULL"),
            FilterOp::Contains => {
                let pattern = Value::Text(format!("%{}%", filter.value.display()));
                match self.kind {
                    DatabaseKind::Postgres => {
                        format!("CAST({field} AS TEXT) ILIKE {}", bind(pattern))
                    }
                    _ => format!("{field} LIKE {}", bind(pattern)),
                }
            }
            op => {
                let operator = match op {
                    FilterOp::Eq => "=",
                    FilterOp::NotEq => "<>",
                    FilterOp::Lt => "<",
                    FilterOp::Lte => "<=",
                    FilterOp::Gt => ">",
                    _ => ">=",
                };
                format!("{field} {operator} {}", bind(filter.value.clone()))
            }
        }
    }

    async fn primary_key(&self, collection: &CollectionRef) -> Result<Vec<String>> {
        Ok(self
            .introspection()?
            .get_primary_key(collection.namespace.as_deref(), &collection.name)
            .await?
            .map(|key| key.columns)
            .unwrap_or_default())
    }

    async fn probe_mysql(&self) -> Result<Capability> {
        let flags = self
            .rows("SELECT @@global.read_only, @@global.super_read_only", &[])
            .await?;
        if let Some(row) = flags.rows.first()
            && row.iter().any(|value| value_is_true(value))
        {
            return Ok(Capability::ReadOnly {
                reason: "the server is read-only".to_string(),
            });
        }
        let mut grants = text_column(&self.rows("SHOW GRANTS", &[]).await?);
        let roles = grants
            .iter()
            .filter_map(|grant| granted_role(grant))
            .collect::<Vec<_>>();
        if !roles.is_empty() {
            let sql = format!("SHOW GRANTS FOR CURRENT_USER() USING {}", roles.join(", "));
            grants.extend(text_column(&self.rows(&sql, &[]).await?));
        }
        Ok(
            if mysql_grants_allow_writes(&grants, self.database.as_deref()) {
                Capability::Writable
            } else {
                Capability::ReadOnly {
                    reason: "this user has no INSERT, UPDATE or DELETE grant".to_string(),
                }
            },
        )
    }

    async fn probe_postgres(&self) -> Result<Capability> {
        let read_only = self
            .rows(
                "SELECT pg_is_in_recovery() \
                 OR current_setting('transaction_read_only') = 'on' \
                 OR current_setting('default_transaction_read_only') = 'on'",
                &[],
            )
            .await?;
        if first_value(&read_only).is_some_and(value_is_true) {
            return Ok(Capability::ReadOnly {
                reason: "the server or session is read-only".to_string(),
            });
        }
        let writable = self
            .rows(
                "SELECT EXISTS (SELECT 1 FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 AND table_type = 'BASE TABLE' \
                 AND has_table_privilege(quote_ident(table_schema) || '.' || quote_ident(table_name), \
                 'INSERT, UPDATE, DELETE'))",
                &[],
            )
            .await?;
        Ok(if first_value(&writable).is_some_and(value_is_true) {
            Capability::Writable
        } else {
            Capability::ReadOnly {
                reason: "this role has no INSERT, UPDATE or DELETE privilege".to_string(),
            }
        })
    }
}

#[async_trait]
impl Source for SqlSource {
    fn query_language(&self) -> QueryLanguage {
        match self.kind {
            DatabaseKind::Postgres => QueryLanguage::Postgres,
            _ => QueryLanguage::MySql,
        }
    }

    async fn probe(&self) -> Result<Capability> {
        match self.kind {
            DatabaseKind::Postgres => self.probe_postgres().await,
            _ => self.probe_mysql().await,
        }
    }

    async fn collections(&self) -> Result<Vec<Collection>> {
        let (sql, params) = match self.kind {
            DatabaseKind::Postgres => (
                "SELECT table_schema, table_name, table_type FROM information_schema.tables \
                 WHERE table_schema NOT IN ('pg_catalog', 'information_schema') \
                 AND table_schema NOT LIKE 'pg_toast%' ORDER BY 1, 2",
                Vec::new(),
            ),
            _ => match &self.database {
                Some(database) => (
                    "SELECT table_schema AS table_schema, table_name AS table_name, \
                     table_type AS table_type FROM information_schema.tables \
                     WHERE table_schema = ? ORDER BY 1, 2",
                    vec![Value::Text(database.clone())],
                ),
                None => (
                    "SELECT table_schema AS table_schema, table_name AS table_name, \
                     table_type AS table_type FROM information_schema.tables \
                     WHERE table_schema NOT IN ('mysql', 'information_schema', \
                     'performance_schema', 'sys') ORDER BY 1, 2",
                    Vec::new(),
                ),
            },
        };
        let result = self.rows(sql, &params).await?;
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [namespace, name, table_type] = row.as_slice() else {
                    return None;
                };
                Some(Collection {
                    reference: CollectionRef {
                        namespace: Some(namespace.display()),
                        name: name.display(),
                    },
                    kind: if table_type.display().contains("VIEW") {
                        CollectionKind::View
                    } else {
                        CollectionKind::Table
                    },
                })
            })
            .collect())
    }

    async fn describe(&self, collection: &CollectionRef) -> Result<CollectionSchema> {
        let introspection = self.introspection()?;
        let namespace = collection.namespace.as_deref();
        let columns = introspection
            .get_columns(namespace, &collection.name)
            .await?;
        let mut primary_key = self.primary_key(collection).await?;
        if primary_key.is_empty() {
            primary_key = columns
                .iter()
                .filter(|column| column.is_primary_key)
                .map(|column| column.name.clone())
                .collect();
        }
        let references = introspection
            .get_foreign_keys(namespace, &collection.name)
            .await?
            .into_iter()
            .map(|foreign_key| Reference {
                source: collection.clone(),
                fields: foreign_key.columns,
                target: CollectionRef {
                    namespace: foreign_key
                        .referenced_schema
                        .or_else(|| collection.namespace.clone()),
                    name: foreign_key.referenced_table,
                },
                target_fields: foreign_key.referenced_columns,
            })
            .collect();
        let mut referenced_by = Vec::new();
        if let Some(all) = introspection.list_all_foreign_keys(namespace).await? {
            for (table, foreign_keys) in all {
                for foreign_key in foreign_keys {
                    let target_namespace = foreign_key
                        .referenced_schema
                        .clone()
                        .or_else(|| collection.namespace.clone());
                    if foreign_key.referenced_table == collection.name
                        && target_namespace == collection.namespace
                    {
                        referenced_by.push(Reference {
                            source: CollectionRef {
                                namespace: collection.namespace.clone(),
                                name: table.clone(),
                            },
                            fields: foreign_key.columns,
                            target: collection.clone(),
                            target_fields: foreign_key.referenced_columns,
                        });
                    }
                }
            }
        }
        let writable = match self.kind {
            DatabaseKind::Postgres => {
                let result = self
                    .rows(
                        "SELECT has_table_privilege($1, 'INSERT, UPDATE, DELETE')",
                        &[Value::Text(self.table(collection))],
                    )
                    .await?;
                first_value(&result).map(value_is_true)
            }
            _ => None,
        };
        Ok(CollectionSchema {
            fields: columns
                .into_iter()
                .map(|column| Field {
                    name: column.name,
                    type_name: column.data_type,
                    nullable: column.nullable,
                })
                .collect(),
            primary_key,
            references,
            referenced_by,
            writable,
        })
    }

    async fn query(&self, text: &str, limit: usize, read_only: bool) -> Result<ResultSet> {
        let text = text.trim().trim_end_matches(';').trim();
        anyhow::ensure!(!text.is_empty(), "the query is empty");
        let result = if read_only {
            // Three layers, because each alone has a hole: a second statement
            // could COMMIT its way out of the transaction, and MySQL commits
            // implicitly before DDL, running it outside a read-only
            // transaction unless the session itself is read-only. (The
            // Postgres driver's extended protocol also refuses a second
            // statement, but that is the driver's choice, not ours.)
            check_read_only_statement(self.kind, text)?;
            let (session_read_only, session_read_write) = match self.kind {
                DatabaseKind::Postgres => (
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
                    "SET SESSION CHARACTERISTICS AS TRANSACTION READ WRITE",
                ),
                _ => (
                    "SET SESSION TRANSACTION READ ONLY",
                    "SET SESSION TRANSACTION READ WRITE",
                ),
            };
            self.statement(session_read_only).await?;
            let result = match self.statement(self.begin_read_only()).await {
                Ok(()) => {
                    let result = self.rows(text, &[]).await;
                    // Always end the transaction, even after a failed query,
                    // so the session is not left inside it.
                    self.statement("ROLLBACK").await.map(|()| result)
                }
                Err(error) => Err(error),
            };
            self.statement(session_read_write).await?;
            result??
        } else {
            self.rows(text, &[]).await?
        };
        Ok(truncate(result, limit))
    }

    async fn browse(&self, collection: &CollectionRef, request: &BrowseRequest) -> Result<Page> {
        let offset: usize = match &request.cursor {
            Some(cursor) => cursor.parse().context("invalid cursor")?,
            None => 0,
        };
        let mut params = Vec::new();
        let mut sql = format!("SELECT * FROM {}", self.table(collection));
        if !request.filters.is_empty() {
            let conditions = request
                .filters
                .iter()
                .map(|filter| self.filter_condition(filter, &mut params))
                .collect::<Vec<_>>();
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        let primary_key = self.primary_key(collection).await?;
        if !primary_key.is_empty() {
            let order = primary_key
                .iter()
                .map(|field| self.quote(field))
                .collect::<Vec<_>>()
                .join(", ");
            sql.push_str(&format!(" ORDER BY {order}"));
        }
        sql.push_str(&format!(" LIMIT {} OFFSET {offset}", request.page_size + 1));
        let mut rows = self.rows(&sql, &params).await?;
        let has_more = rows.rows.len() > request.page_size;
        rows.rows.truncate(request.page_size);
        Ok(Page {
            next_cursor: has_more.then(|| (offset + request.page_size).to_string()),
            rows,
        })
    }

    fn render_changes(&self, changes: &ChangeSet) -> Result<String> {
        Ok(self
            .statements(changes, true)?
            .iter()
            .map(|(sql, _)| format!("{sql};"))
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn apply(&self, changes: &ChangeSet) -> Result<u64> {
        let statements = self.statements(changes, false)?;
        self.ensure_no_open_transaction().await?;
        self.statement(self.begin()).await?;
        match self.run_changes(changes, &statements).await {
            Ok(affected) => {
                self.statement("COMMIT").await?;
                Ok(affected)
            }
            Err(error) => {
                self.statement("ROLLBACK").await?;
                Err(error)
            }
        }
    }

    async fn rehearse(&self, changes: &ChangeSet, hold: bool) -> Result<Rehearsal> {
        let statements = self.statements(changes, false)?;
        if self.kind == DatabaseKind::Mysql {
            let lasting = self
                .mysql_non_transactional_tables(&changes.collection)
                .await?;
            if !lasting.is_empty() {
                // A rehearsal there would write for real: these engines
                // ignore ROLLBACK. Nothing runs until the user approves.
                return Ok(Rehearsal {
                    notes: vec![format!(
                        "Not rehearsed: {} can't roll back a change, so it runs only after you approve.",
                        lasting.join(", ")
                    )],
                    affected: 0,
                    held: false,
                });
            }
        }
        self.ensure_no_open_transaction().await?;
        self.statement(self.begin()).await?;
        let counted = self.kind == DatabaseKind::Postgres;
        let result = async {
            if hold && counted {
                // If Cue stops answering, the server ends the transaction
                // itself instead of holding its locks indefinitely.
                self.statement("SET LOCAL idle_in_transaction_session_timeout = '3min'")
                    .await?;
            }
            let before = if counted {
                self.row_counters().await?
            } else {
                Vec::new()
            };
            let affected = self.run_changes(changes, &statements).await?;
            let side_effects = if counted {
                counter_changes(&before, &self.row_counters().await?)
            } else {
                Vec::new()
            };
            anyhow::Ok((affected, side_effects))
        }
        .await;
        if !hold || result.is_err() {
            self.statement("ROLLBACK").await?;
        }
        let (affected, side_effects) = result?;
        let mut notes = vec![if hold {
            format!(
                "Rehearsed: {affected} {} in {}, waiting in an open transaction. Approving commits exactly this; rejecting rolls it back.",
                plural(affected, "row"),
                changes.collection.qualified_name()
            )
        } else {
            format!(
                "Rehearsed and rolled back: {affected} {} in {}.",
                plural(affected, "row"),
                changes.collection.qualified_name()
            )
        }];
        for (table, counts) in side_effects {
            if table == changes.collection {
                continue;
            }
            notes.push(format!(
                "Also {counts} in {} (triggers or foreign keys).",
                table.qualified_name()
            ));
        }
        if self.kind == DatabaseKind::Mysql {
            // MySQL keeps no per-transaction row counts, so name what will
            // run instead of counting what it did.
            let mut events = changes
                .changes
                .iter()
                .map(|change| match change {
                    Change::Insert { .. } => "INSERT",
                    Change::Update { .. } => "UPDATE",
                    Change::Delete { .. } => "DELETE",
                })
                .collect::<Vec<_>>();
            events.sort_unstable();
            events.dedup();
            for event in events {
                match self.mysql_side_effects(&changes.collection, event).await {
                    Ok(found) => notes.extend(found),
                    Err(error) => notes.push(format!(
                        "Could not list the triggers and cascades: {error:#}"
                    )),
                }
            }
        }
        Ok(Rehearsal {
            notes,
            affected,
            held: hold,
        })
    }

    async fn settle(&self, commit: bool) -> Result<()> {
        self.statement(if commit { "COMMIT" } else { "ROLLBACK" })
            .await
    }

    async fn assess(&self, text: &str) -> Result<Vec<String>> {
        let text = text.trim().trim_end_matches(';').trim();
        let tokens = top_level_tokens(self.kind, text);
        let statements = tokens
            .split(|token| token == ";")
            .filter(|statement| !statement.is_empty())
            .collect::<Vec<_>>();
        let mut notes = statements
            .iter()
            .flat_map(|statement| dangers(self.kind, statement))
            .collect::<Vec<_>>();
        let [statement] = statements.as_slice() else {
            return Ok(notes);
        };
        if self.kind == DatabaseKind::Mysql {
            notes.extend(self.assess_mysql(text, statement).await);
            return Ok(notes);
        }
        if self.in_open_transaction().await? {
            notes.push(
                "This session has an open transaction, so the impact was not estimated.".into(),
            );
            return Ok(notes);
        }
        let verb = top_level_verb(statement);
        if matches!(
            verb.as_deref(),
            Some("INSERT" | "UPDATE" | "DELETE" | "MERGE")
        ) {
            match self.estimate_write(text).await {
                Ok(estimate) => notes.extend(estimate),
                Err(error) => {
                    notes.push(format!("Could not estimate the rows affected: {error:#}"))
                }
            }
        }
        if let Some(target) = alter_table_target(text) {
            match self.forecast_rewrite(text, target).await {
                Ok(forecast) => notes.push(forecast),
                Err(error) => notes.push(format!(
                    "Could not tell whether this rewrites the table: {error:#}"
                )),
            }
        }
        Ok(notes)
    }
}

impl SqlSource {
    /// Runs a change set's statements inside a transaction the caller opened
    /// and ends. Every update and delete must hit exactly one row.
    async fn run_changes(
        &self,
        changes: &ChangeSet,
        statements: &[(String, Vec<Value>)],
    ) -> Result<u64> {
        let mut affected = 0;
        for (change, (sql, params)) in changes.changes.iter().zip(statements) {
            let rows = self
                .rows(sql, params)
                .await?
                .affected_rows
                .unwrap_or_default();
            // A key that turns out not to be unique would change every
            // matching row. Undo the whole change set instead.
            if rows > 1 {
                bail!("refusing to commit: `{sql}` would change {rows} rows, not one");
            }
            if rows == 0 {
                self.explain_missed_change(&changes.collection, change)
                    .await?;
            }
            affected += rows;
        }
        Ok(affected)
    }

    /// Errors when an update or delete matched no row because the row is gone
    /// or another session changed it since it was read. MySQL also reports 0
    /// rows for an update that leaves every value as it was, which is fine.
    async fn explain_missed_change(
        &self,
        collection: &CollectionRef,
        change: &Change,
    ) -> Result<()> {
        let (key, expected) = match change {
            Change::Update { key, expected, .. } => (key, checkable(expected)),
            Change::Delete { key } => (key, Vec::new()),
            Change::Insert { .. } => return Ok(()),
        };
        let mut params = Vec::new();
        let condition = self.key_condition(key, &mut params, false);
        let sql = format!("SELECT 1 FROM {} WHERE {condition}", self.table(collection));
        let row_exists = !self.rows(&sql, &params).await?.rows.is_empty();
        let row = key
            .iter()
            .map(|(field, value)| format!("{field} {}", value.display()))
            .collect::<Vec<_>>()
            .join(", ");
        if !row_exists {
            bail!(
                "Not saved: the row with {row} no longer exists; another session deleted it or changed its key. Nothing was written."
            );
        }
        if !expected.is_empty() {
            bail!(
                "Not saved: the row with {row} changed after it was read; another session saved a new value where this change would write. Nothing was written. Reload to see the current values."
            );
        }
        Ok(())
    }

    /// PostgreSQL's per-table counts of rows inserted, updated and deleted
    /// that this session has not yet reported. They count what triggers and
    /// cascading foreign keys did too. They can still hold earlier
    /// transactions' counts, so only a difference means anything.
    async fn row_counters(&self) -> Result<Vec<(CollectionRef, [i64; 3])>> {
        let result = self
            .rows(
                "-- cue: rows changed by a rehearsal\n\
                 SELECT schemaname::text, relname::text, n_tup_ins, n_tup_upd, n_tup_del \
                 FROM pg_stat_xact_user_tables \
                 WHERE n_tup_ins + n_tup_upd + n_tup_del > 0",
                &[],
            )
            .await?;
        let count = |value: &Value| match value {
            Value::Int(count) => *count,
            _ => 0,
        };
        Ok(result
            .rows
            .iter()
            .filter_map(|row| {
                let [schema, table, inserted, updated, deleted] = row.as_slice() else {
                    return None;
                };
                Some((
                    CollectionRef::new(Some(&schema.display()), &table.display()),
                    [count(inserted), count(updated), count(deleted)],
                ))
            })
            .collect())
    }

    /// Refuses to start a transaction on a session that already has one. The
    /// session is shared with the query tab, where the user may have run
    /// `BEGIN`: Postgres would fold ours into theirs and then end both, and
    /// MySQL would silently commit theirs at our `START TRANSACTION`.
    async fn ensure_no_open_transaction(&self) -> Result<()> {
        let open = match self.kind {
            DatabaseKind::Postgres => self.in_open_transaction().await?,
            // `SET TRANSACTION` fails while a transaction is in progress,
            // for any user. Otherwise it only marks the next transaction,
            // the one about to start, as read-write.
            _ => match self.statement("SET TRANSACTION READ WRITE").await {
                Ok(()) => false,
                Err(error) if format!("{error:#}").contains("transaction is in progress") => true,
                Err(error) => return Err(error),
            },
        };
        if open {
            bail!(
                "This connection's session has an open transaction, probably from a BEGIN in a query tab. Commit or roll it back first; Cue won't end it for you. Nothing was written."
            );
        }
        Ok(())
    }

    /// Tables a change to `table` could write that ignore ROLLBACK. With
    /// triggers on `table`, any table in its schema could be written, so all
    /// of them count.
    async fn mysql_non_transactional_tables(&self, table: &CollectionRef) -> Result<Vec<String>> {
        let schema = table.namespace.clone().map_or(Value::Null, Value::Text);
        let name = Value::Text(table.name.clone());
        let result = self
            .rows(
                "/* cue: tables a rehearsal could not roll back */ \
                 SELECT CONCAT(TABLE_NAME, ' (', ENGINE, ')') FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_TYPE = 'BASE TABLE' \
                 AND ENGINE NOT IN ('InnoDB', 'ndbcluster') \
                 AND (TABLE_NAME = ? OR EXISTS (SELECT 1 FROM information_schema.TRIGGERS \
                 WHERE EVENT_OBJECT_SCHEMA = COALESCE(?, DATABASE()) AND EVENT_OBJECT_TABLE = ?)) \
                 ORDER BY 1",
                &[schema.clone(), name.clone(), schema, name],
            )
            .await?;
        Ok(text_column(&result))
    }

    /// A savepoint can only be set inside a transaction block, so whether
    /// setting one fails says whether a block is open. (Comparing `now()`
    /// with `statement_timestamp()` doesn't work: the driver's extended
    /// protocol starts the implicit transaction before the statement.)
    async fn in_open_transaction(&self) -> Result<bool> {
        match self.statement("SAVEPOINT cue_probe").await {
            Ok(()) => {
                self.statement("RELEASE SAVEPOINT cue_probe").await?;
                Ok(true)
            }
            Err(error) if format!("{error:#}").contains("transaction block") => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Runs `body` in a transaction with tight limits, so a check never waits
    /// on a busy table, and always rolls it back.
    async fn checked_transaction<T>(
        &self,
        read_only: bool,
        body: impl AsyncFnOnce() -> Result<T>,
    ) -> Result<T> {
        let begin = if read_only {
            "BEGIN READ ONLY"
        } else {
            "BEGIN"
        };
        self.statement(begin).await?;
        let result = async {
            self.statement("SET LOCAL statement_timeout = '2s'").await?;
            self.statement("SET LOCAL lock_timeout = '500ms'").await?;
            body().await
        }
        .await;
        self.statement("ROLLBACK").await?;
        result
    }

    /// The planner's estimate of the rows a write touches, plus the triggers
    /// it fires and the rows a delete cascades to. Nothing is executed:
    /// `EXPLAIN` without `ANALYZE` only plans, inside a read-only transaction.
    async fn estimate_write(&self, text: &str) -> Result<Vec<String>> {
        self.checked_transaction(true, async || {
            let plan = self
                .rows(&format!("EXPLAIN (VERBOSE, FORMAT JSON) {text}"), &[])
                .await?;
            let plan = match first_value(&plan) {
                Some(Value::Json(plan)) => plan.clone(),
                Some(Value::Text(plan)) => serde_json::from_str(plan)?,
                _ => bail!("EXPLAIN returned no plan"),
            };
            let Some(write) = WritePlan::find(&plan) else {
                return Ok(Vec::new());
            };
            let table = format!("{}.{}", write.schema, write.table);
            let mut notes = vec![format!(
                "{} about {} {} in {table} (planner estimate).",
                write.operation,
                write.rows,
                plural(write.rows, "row")
            )];
            let params = [
                Value::Text(write.schema.clone()),
                Value::Text(write.table.clone()),
            ];
            let triggers = self
                .rows(
                    "-- cue: triggers a write fires\n\
                     SELECT tgname::text FROM pg_trigger \
                     WHERE tgrelid = to_regclass(format('%I.%I', $1::text, $2::text)) \
                     AND NOT tgisinternal AND tgenabled <> 'D' \
                     AND tgtype::bigint & $3::bigint <> 0 ORDER BY 1",
                    &[
                        params[0].clone(),
                        params[1].clone(),
                        Value::Int(write.trigger_mask()),
                    ],
                )
                .await?;
            let triggers = text_column(&triggers);
            if !triggers.is_empty() {
                notes.push(format!(
                    "Fires {} {} on {table}: {}.",
                    triggers.len(),
                    plural(triggers.len() as u64, "trigger"),
                    triggers.join(", ")
                ));
            }
            if write.operation == "Deletes" {
                let cascades = self
                    .rows(
                        "-- cue: foreign keys a delete acts through\n\
                         SELECT conrelid::regclass::text, confdeltype::text FROM pg_constraint \
                         WHERE contype = 'f' \
                         AND confrelid = to_regclass(format('%I.%I', $1::text, $2::text)) \
                         AND confdeltype IN ('c', 'n', 'd') ORDER BY 1",
                        &params,
                    )
                    .await?;
                for row in &cascades.rows {
                    let [referencing, action] = row.as_slice() else {
                        continue;
                    };
                    let action = match action.display().as_str() {
                        "c" => "deletes the rows that reference it in",
                        "n" => "sets the referencing columns to NULL in",
                        _ => "sets the referencing columns to their default in",
                    };
                    notes.push(format!(
                        "Each deleted row also {action} {}.",
                        referencing.display()
                    ));
                }
            }
            Ok(notes)
        })
        .await
    }

    /// Whether an `ALTER TABLE` rewrites its table, asked of PostgreSQL
    /// itself: the statement runs against an empty temporary copy, and a
    /// rewrite gives the copy a new file. The real table is never altered.
    async fn forecast_rewrite(&self, text: &str, target: Range<usize>) -> Result<String> {
        let table = text[target.clone()].to_string();
        let altered = format!(
            "{}pg_temp.cue_forecast{}",
            &text[..target.start],
            &text[target.end..]
        );
        self.checked_transaction(false, async || {
            let size = self
                .rows(
                    "-- cue: size of a table an ALTER rewrites\n\
                     SELECT pg_size_pretty(pg_total_relation_size(relation)), \
                     (SELECT count(*) FROM pg_index WHERE indrelid = relation) \
                     FROM to_regclass($1::text) AS relation",
                    &[Value::Text(table.clone())],
                )
                .await?;
            let (size, indexes) = match size.rows.first().map(Vec::as_slice) {
                Some([size, indexes]) if !size.is_nullish() => (size.display(), indexes.display()),
                _ => bail!("no table {table}"),
            };
            self.statement(&format!(
                "CREATE TEMPORARY TABLE cue_forecast (LIKE {table} INCLUDING ALL)"
            ))
            .await?;
            let file = "SELECT pg_relation_filenode('pg_temp.cue_forecast')::text";
            let before = text_column(&self.rows(file, &[]).await?);
            self.statement(&altered).await?;
            let after = text_column(&self.rows(file, &[]).await?);
            Ok(if before == after {
                format!("Changes only the catalog; {table} is not rewritten.")
            } else {
                let indexes = match indexes.as_str() {
                    "0" => String::new(),
                    "1" => " and rebuilds its index".to_string(),
                    count => format!(" and rebuilds its {count} indexes"),
                };
                format!(
                    "Rewrites {table} ({size}){indexes}; reads and writes on it wait until that finishes."
                )
            })
        })
        .await
    }
}

impl SqlSource {
    /// MySQL's side of `assess`. Plain `EXPLAIN` only plans (MySQL refuses
    /// it inside a read-only transaction), and the rest reads the catalog,
    /// so nothing here can change data.
    async fn assess_mysql(&self, text: &str, statement: &[String]) -> Vec<String> {
        let mut notes = Vec::new();
        let first = statement.first().map(|token| token.to_ascii_uppercase());
        let verb = top_level_verb(statement);
        if first.as_deref() != Some("EXPLAIN")
            && matches!(verb.as_deref(), Some("INSERT" | "UPDATE" | "DELETE"))
        {
            match self.estimate_mysql_write(text).await {
                Ok(estimate) => notes.extend(estimate),
                Err(error) => {
                    notes.push(format!("Could not estimate the rows affected: {error:#}"))
                }
            }
        }
        if let Some(target) = alter_table_target(text) {
            match self.forecast_mysql_alter(&text[target], statement).await {
                Ok(forecast) => notes.push(forecast),
                Err(error) => notes.push(format!(
                    "Could not tell whether this copies the table: {error:#}"
                )),
            }
        }
        notes
    }

    async fn estimate_mysql_write(&self, text: &str) -> Result<Vec<String>> {
        let plan = self
            .with_short_lock_wait(async || {
                self.rows(&format!("EXPLAIN FORMAT=JSON {text}"), &[]).await
            })
            .await?;
        let plan = match first_value(&plan) {
            Some(Value::Json(plan)) => plan.clone(),
            Some(Value::Text(plan)) => serde_json::from_str(plan)?,
            _ => bail!("EXPLAIN returned no plan"),
        };
        let Some(write) = MySqlWrite::find(&plan) else {
            return Ok(Vec::new());
        };
        let mut notes = Vec::new();
        if let Some(rows) = write.rows_examined {
            let action = match write.event {
                "UPDATE" => "update",
                "DELETE" => "delete",
                _ => "write",
            };
            notes.push(format!(
                "MySQL expects to examine about {rows} {} of {} to find the rows to {action} (planner estimate).",
                plural(rows, "row"),
                write.table,
            ));
        }
        notes.extend(
            self.mysql_side_effects(&CollectionRef::new(None, &write.table), write.event)
                .await?,
        );
        Ok(notes)
    }

    /// The triggers an `event` on `table` fires and, for deletes, the foreign
    /// keys it acts through, read from the catalog.
    async fn mysql_side_effects(&self, table: &CollectionRef, event: &str) -> Result<Vec<String>> {
        let schema = table.namespace.clone().map_or(Value::Null, Value::Text);
        let name = Value::Text(table.name.clone());
        let mut notes = Vec::new();
        let triggers = self
            .rows(
                "/* cue: triggers a write fires */ \
                 SELECT TRIGGER_NAME FROM information_schema.TRIGGERS \
                 WHERE EVENT_OBJECT_SCHEMA = COALESCE(?, DATABASE()) AND EVENT_OBJECT_TABLE = ? \
                 AND EVENT_MANIPULATION = ? ORDER BY 1",
                &[schema.clone(), name.clone(), Value::Text(event.to_string())],
            )
            .await?;
        let triggers = text_column(&triggers);
        if !triggers.is_empty() {
            notes.push(format!(
                "Fires {} {} on {}: {}.",
                triggers.len(),
                plural(triggers.len() as u64, "trigger"),
                table.name,
                triggers.join(", ")
            ));
        }
        if event == "DELETE" {
            let cascades = self
                .rows(
                    "/* cue: foreign keys a delete acts through */ \
                     SELECT TABLE_NAME, DELETE_RULE FROM information_schema.REFERENTIAL_CONSTRAINTS \
                     WHERE UNIQUE_CONSTRAINT_SCHEMA = COALESCE(?, DATABASE()) \
                     AND REFERENCED_TABLE_NAME = ? \
                     AND DELETE_RULE IN ('CASCADE', 'SET NULL', 'SET DEFAULT') ORDER BY 1",
                    &[schema, name],
                )
                .await?;
            for row in &cascades.rows {
                let [referencing, rule] = row.as_slice() else {
                    continue;
                };
                let action = match rule.display().as_str() {
                    "CASCADE" => "deletes the rows that reference it in",
                    "SET NULL" => "sets the referencing columns to NULL in",
                    _ => "sets the referencing columns to their default in",
                };
                notes.push(format!(
                    "Each deleted row also {action} {}.",
                    referencing.display()
                ));
            }
        }
        Ok(notes)
    }

    /// MySQL can't try an online `ALTER` on a temporary copy (temporary
    /// tables only support `ALGORITHM=COPY`), and trying it on the real table
    /// would run it. So this follows MySQL's online DDL rules instead.
    async fn forecast_mysql_alter(&self, table: &str, statement: &[String]) -> Result<String> {
        let parts = table
            .split('.')
            .map(|part| part.trim_matches('`').replace("``", "`"))
            .collect::<Vec<_>>();
        let (schema, name) = match parts.as_slice() {
            [schema, name] => (Value::Text(schema.clone()), name.clone()),
            [name] => (Value::Null, name.clone()),
            _ => bail!("unexpected table name {table}"),
        };
        let size = self
            .rows(
                "/* cue: size of a table an ALTER copies */ \
                 SELECT DATA_LENGTH + INDEX_LENGTH FROM information_schema.TABLES \
                 WHERE TABLE_SCHEMA = COALESCE(?, DATABASE()) AND TABLE_NAME = ?",
                &[schema, Value::Text(name.clone())],
            )
            .await?;
        let size = match first_value(&size) {
            Some(Value::Int(bytes)) => format_bytes(*bytes as u64),
            Some(Value::Text(bytes)) => format_bytes(bytes.parse().unwrap_or_default()),
            _ => bail!("no table {table}"),
        };
        Ok(match mysql_alter_cost(statement) {
            AlterCost::Copy => format!(
                "Likely copies {name} ({size}): changing a column's type, its character set, \
                 the primary key or the engine rebuilds the table, and writes to it wait until \
                 that finishes. Renaming a column, changing its default or widening a VARCHAR \
                 are the exceptions."
            ),
            AlterCost::Rebuild => format!(
                "Rebuilds {name} ({size}) in place; reads and writes keep working, but it takes \
                 time and I/O."
            ),
            AlterCost::Index => {
                format!(
                    "Builds an index on {name} ({size}) in place; reads and writes keep working."
                )
            }
            AlterCost::Instant => {
                format!(
                    "Changes only metadata; MySQL normally does this instantly, without copying {name}."
                )
            }
        })
    }

    /// Bounds how long a check waits for a metadata lock (MySQL waits a year
    /// by default), restoring the session's setting afterwards.
    async fn with_short_lock_wait<T>(&self, body: impl AsyncFnOnce() -> Result<T>) -> Result<T> {
        self.statement(
            "SET @cue_lock_wait_timeout = @@SESSION.lock_wait_timeout, SESSION lock_wait_timeout = 2",
        )
        .await?;
        let result = body().await;
        self.statement("SET SESSION lock_wait_timeout = @cue_lock_wait_timeout")
            .await?;
        result
    }
}

/// The table a MySQL `EXPLAIN FORMAT=JSON` plan writes to.
#[derive(Debug, PartialEq)]
struct MySqlWrite {
    /// INSERT, UPDATE or DELETE, as `information_schema.TRIGGERS` spells it.
    event: &'static str,
    table: String,
    rows_examined: Option<u64>,
}

impl MySqlWrite {
    fn find(plan: &serde_json::Value) -> Option<Self> {
        match plan {
            serde_json::Value::Array(items) => items.iter().find_map(Self::find),
            serde_json::Value::Object(object) => {
                let event = [
                    ("update", "UPDATE"),
                    ("delete", "DELETE"),
                    ("insert", "INSERT"),
                ]
                .into_iter()
                .find(|(flag, _)| object.get(*flag).and_then(|value| value.as_bool()) == Some(true))
                .map(|(_, event)| event);
                if let (Some(event), Some(table)) = (
                    event,
                    object.get("table_name").and_then(|name| name.as_str()),
                ) {
                    return Some(Self {
                        event,
                        table: table.to_string(),
                        rows_examined: object
                            .get("rows_examined_per_scan")
                            .and_then(|rows| rows.as_u64()),
                    });
                }
                object.values().find_map(Self::find)
            }
            _ => None,
        }
    }
}

#[derive(Debug, PartialEq, PartialOrd)]
enum AlterCost {
    Instant,
    Index,
    Rebuild,
    Copy,
}

/// The most expensive thing a MySQL `ALTER TABLE` does, from InnoDB's online
/// DDL rules. MODIFY and CHANGE count as copies: telling a type change from
/// a rename would need the old and new column definitions compared.
fn mysql_alter_cost(statement: &[String]) -> AlterCost {
    let words = statement
        .iter()
        .map(|token| token.to_ascii_uppercase())
        .collect::<Vec<_>>();
    let mut cost = AlterCost::Instant;
    for (index, word) in words.iter().enumerate() {
        let next = words.get(index + 1).map(String::as_str);
        let this = match (word.as_str(), next) {
            ("MODIFY" | "CHANGE" | "CONVERT" | "ENGINE", _) => AlterCost::Copy,
            ("ADD" | "DROP", Some("PRIMARY")) => AlterCost::Copy,
            ("ADD", Some("FULLTEXT" | "SPATIAL")) => AlterCost::Copy,
            ("FORCE" | "ROW_FORMAT", _) => AlterCost::Rebuild,
            ("ADD", Some("INDEX" | "KEY" | "UNIQUE")) => AlterCost::Index,
            _ => AlterCost::Instant,
        };
        if this > cost {
            cost = this;
        }
    }
    cost
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["bytes", "KB", "MB", "GB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} bytes")
    } else {
        format!("{size:.1} {}", UNITS[unit])
    }
}

/// The write node of an `EXPLAIN (VERBOSE, FORMAT JSON)` plan.
#[derive(Debug, PartialEq)]
struct WritePlan {
    operation: &'static str,
    schema: String,
    table: String,
    rows: u64,
}

impl WritePlan {
    fn find(plan: &serde_json::Value) -> Option<Self> {
        let node = match plan {
            serde_json::Value::Array(items) => return items.iter().find_map(Self::find),
            serde_json::Value::Object(object) => object,
            _ => return None,
        };
        if let Some(inner) = node.get("Plan") {
            return Self::find(inner);
        }
        if node.get("Node Type").and_then(|kind| kind.as_str()) == Some("ModifyTable") {
            let operation = match node.get("Operation")?.as_str()? {
                "Insert" => "Inserts",
                "Update" => "Updates",
                "Delete" => "Deletes",
                _ => "Changes",
            };
            // The node itself plans 0 rows unless it returns some; its input
            // is what it writes.
            let rows = node
                .get("Plans")
                .and_then(|plans| plans.get(0))
                .and_then(|input| input.get("Plan Rows"))
                .and_then(|rows| rows.as_f64())
                .unwrap_or_default();
            return Some(Self {
                operation,
                schema: node.get("Schema")?.as_str()?.to_string(),
                table: node.get("Relation Name")?.as_str()?.to_string(),
                rows: rows.round() as u64,
            });
        }
        node.get("Plans").and_then(Self::find)
    }

    /// `pg_trigger.tgtype` bits for the events this write raises.
    fn trigger_mask(&self) -> i64 {
        const INSERT: i64 = 1 << 2;
        const DELETE: i64 = 1 << 3;
        const UPDATE: i64 = 1 << 4;
        match self.operation {
            "Inserts" => INSERT,
            "Updates" => UPDATE,
            "Deletes" => DELETE,
            _ => INSERT | DELETE | UPDATE,
        }
    }
}

/// Per table, what changed between two readings of `row_counters`, as
/// "2 rows inserted, 1 row deleted".
fn counter_changes(
    before: &[(CollectionRef, [i64; 3])],
    after: &[(CollectionRef, [i64; 3])],
) -> Vec<(CollectionRef, String)> {
    let mut changes = after
        .iter()
        .filter_map(|(table, counts)| {
            let previous = before
                .iter()
                .find(|(earlier, _)| earlier == table)
                .map_or([0; 3], |(_, counts)| *counts);
            let summary = counts
                .iter()
                .zip(previous)
                .zip(["inserted", "updated", "deleted"])
                .filter_map(|((count, previous), verb)| {
                    let delta = count - previous;
                    (delta > 0).then(|| format!("{delta} {} {verb}", plural(delta as u64, "row")))
                })
                .collect::<Vec<_>>();
            (!summary.is_empty()).then(|| (table.clone(), summary.join(", ")))
        })
        .collect::<Vec<_>>();
    changes.sort_by(|left, right| left.0.cmp(&right.0));
    changes
}

fn plural(count: u64, noun: &str) -> String {
    if count == 1 {
        noun.to_string()
    } else {
        format!("{noun}s")
    }
}

/// The first top-level keyword that says what a statement does, looking past
/// the common table expressions of a `WITH`.
fn top_level_verb(tokens: &[String]) -> Option<String> {
    let mut depth = 0usize;
    for token in tokens {
        match token.as_str() {
            "(" => depth += 1,
            ")" => depth = depth.saturating_sub(1),
            word if depth == 0 => {
                let word = word.to_ascii_uppercase();
                if word != "WITH" && word != "RECURSIVE" && is_verb(&word) {
                    return Some(word);
                }
            }
            _ => {}
        }
    }
    None
}

fn is_verb(word: &str) -> bool {
    matches!(
        word,
        "SELECT"
            | "INSERT"
            | "UPDATE"
            | "DELETE"
            | "MERGE"
            | "TRUNCATE"
            | "DROP"
            | "ALTER"
            | "CREATE"
            | "COPY"
            | "VALUES"
            | "TABLE"
    )
}

/// Warnings for statements that destroy data or block a table, read from the
/// statement alone. Each is a reason to stop and look before running it.
fn dangers(kind: DatabaseKind, tokens: &[String]) -> Vec<String> {
    let top_level = {
        let mut depth = 0usize;
        tokens
            .iter()
            .filter_map(|token| match token.as_str() {
                "(" => {
                    depth += 1;
                    None
                }
                ")" => {
                    depth = depth.saturating_sub(1);
                    None
                }
                word if depth == 0 => Some(word.to_ascii_uppercase()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let has = |word: &str| top_level.iter().any(|token| token == word);
    let first = top_level.first().map(String::as_str).unwrap_or_default();
    let mut notes = Vec::new();
    match top_level_verb(tokens).as_deref() {
        Some(verb @ ("UPDATE" | "DELETE")) if !has("WHERE") => {
            let effect = if verb == "UPDATE" {
                "changes"
            } else {
                "removes"
            };
            notes.push(format!(
                "This {verb} has no WHERE clause: it {effect} every row."
            ));
        }
        Some("TRUNCATE") => notes.push("TRUNCATE removes every row.".into()),
        Some("DROP") if first == "DROP" => {
            let object = top_level.get(1).map(String::as_str).unwrap_or("object");
            notes.push(format!(
                "DROP {object} deletes it, and any data it holds, for good."
            ));
        }
        Some("ALTER")
            if first == "ALTER" && top_level.get(1).map(String::as_str) == Some("TABLE") =>
        {
            if has("DROP") {
                notes.push("This ALTER TABLE drops a column or constraint.".into());
            }
        }
        Some("CREATE")
            if kind == DatabaseKind::Postgres && has("INDEX") && !has("CONCURRENTLY") =>
        {
            notes.push(
                "CREATE INDEX blocks writes to the table while it builds; CREATE INDEX CONCURRENTLY does not."
                    .into(),
            );
        }
        Some("COPY") if first == "COPY" => {
            if has("PROGRAM") {
                notes.push("This COPY runs a program on the database server.".into());
            } else if let Some(to) = top_level.iter().position(|token| token == "TO")
                && top_level.get(to + 1).map(String::as_str) != Some("STDOUT")
            {
                notes.push("This COPY writes a file on the database server.".into());
            }
        }
        _ => {}
    }
    notes
}

/// The byte range of the table named by an `ALTER TABLE` statement, so it
/// can be swapped for another. `None` for anything else, including
/// `ALTER TABLE ALL IN TABLESPACE`, which names no single table.
fn alter_table_target(text: &str) -> Option<Range<usize>> {
    let tokens = head_tokens(text, 12);
    let word_at = |index: usize, word: &str| matches!(tokens.get(index), Some((_, HeadToken::Word(found))) if found.eq_ignore_ascii_case(word));
    if !word_at(0, "ALTER") || !word_at(1, "TABLE") {
        return None;
    }
    let mut index = 2;
    if word_at(index, "IF") && word_at(index + 1, "EXISTS") {
        index += 2;
    }
    if word_at(index, "ONLY") {
        index += 1;
    }
    if word_at(index, "ALL") {
        return None;
    }
    let name = |index: usize| match tokens.get(index) {
        Some((range, HeadToken::Word(_) | HeadToken::Quoted)) => Some(range.clone()),
        _ => None,
    };
    let mut target = name(index)?;
    index += 1;
    while matches!(tokens.get(index), Some((_, HeadToken::Dot)))
        && let Some(next) = name(index + 1)
    {
        target.end = next.end;
        index += 2;
    }
    Some(target)
}

#[derive(Debug, PartialEq)]
enum HeadToken {
    Word(String),
    Quoted,
    Dot,
    Other,
}

/// The first `limit` tokens of a Postgres statement with their byte ranges,
/// skipping whitespace and comments.
fn head_tokens(text: &str, limit: usize) -> Vec<(Range<usize>, HeadToken)> {
    let mut tokens = Vec::new();
    let mut characters = text.char_indices().peekable();
    while tokens.len() < limit {
        let Some((start, character)) = characters.next() else {
            break;
        };
        let next = characters.peek().map(|(_, next)| *next);
        if character.is_whitespace() {
            continue;
        }
        if character == '-' && next == Some('-') {
            while characters
                .next_if(|(_, character)| *character != '\n')
                .is_some()
            {}
            continue;
        }
        if character == '/' && next == Some('*') {
            characters.next();
            let mut previous = ' ';
            for (_, character) in characters.by_ref() {
                if previous == '*' && character == '/' {
                    break;
                }
                previous = character;
            }
            continue;
        }
        // `"` quotes identifiers in Postgres, `` ` `` in MySQL.
        let token = if character == '"' || character == '`' {
            let quote = character;
            loop {
                match characters.next() {
                    Some((_, found))
                        if found == quote
                            && characters.next_if(|(_, next)| *next == quote).is_some() => {}
                    Some((_, found)) if found == quote => break,
                    None => break,
                    Some(_) => {}
                }
            }
            HeadToken::Quoted
        } else if is_identifier_start(character) {
            while characters
                .next_if(|(_, next)| {
                    is_identifier_start(*next) || next.is_ascii_digit() || *next == '$'
                })
                .is_some()
            {}
            HeadToken::Word(String::new())
        } else if character == '.' {
            HeadToken::Dot
        } else {
            HeadToken::Other
        };
        let end = characters.peek().map_or(text.len(), |(index, _)| *index);
        let token = match token {
            HeadToken::Word(_) => HeadToken::Word(text[start..end].to_string()),
            token => token,
        };
        tokens.push((start..end, token));
    }
    tokens
}

/// Maps Echo's TLS mode names onto each driver's spelling.
fn driver_ssl_mode(kind: DatabaseKind, mode: &str) -> String {
    let mode = mode.to_ascii_lowercase().replace('-', "_");
    match kind {
        DatabaseKind::Mysql => match mode.as_str() {
            "disable" => "DISABLED",
            "prefer" => "PREFERRED",
            "require" => "REQUIRED",
            "verify_ca" => "VERIFY_CA",
            "verify_full" => "VERIFY_IDENTITY",
            other => return other.to_ascii_uppercase(),
        }
        .to_string(),
        _ => match mode.as_str() {
            "verify_ca" => "verify-ca".to_string(),
            "verify_full" => "verify-full".to_string(),
            other => other.to_string(),
        },
    }
}

fn truncate(mut result: ResultSet, limit: usize) -> ResultSet {
    // ponytail: the drivers buffer whole results, so the limit only bounds
    // what is kept and sent on. Streaming cursors would bound memory too.
    if result.rows.len() > limit {
        result.rows.truncate(limit);
        result.truncated = true;
    }
    result
}

fn first_value(result: &ResultSet) -> Option<&Value> {
    result.rows.first().and_then(|row| row.first())
}

fn value_is_true(value: &Value) -> bool {
    match value {
        Value::Bool(value) => *value,
        Value::Int(value) => *value != 0,
        Value::Text(value) => matches!(value.as_str(), "1" | "t" | "true" | "on" | "ON"),
        _ => false,
    }
}

fn text_column(result: &ResultSet) -> Vec<String> {
    result
        .rows
        .iter()
        .filter_map(|row| row.first().map(Value::display))
        .collect()
}

pub(crate) fn quote_identifier(kind: DatabaseKind, identifier: &str) -> String {
    match kind {
        DatabaseKind::Postgres => format!("\"{}\"", identifier.replace('"', "\"\"")),
        _ => format!("`{}`", identifier.replace('`', "``")),
    }
}

/// Renders a value as a SQL literal for display. Statements are executed
/// with bound parameters, never with these literals.
fn sql_literal(kind: DatabaseKind, value: &Value) -> String {
    match value {
        Value::Missing | Value::Null => "NULL".to_string(),
        Value::Bool(value) => if *value { "TRUE" } else { "FALSE" }.to_string(),
        Value::Int(value) => value.to_string(),
        Value::Float(value) => value.to_string(),
        Value::Bytes(bytes) => {
            let hex = bytes
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            match kind {
                DatabaseKind::Postgres => format!("'\\x{hex}'"),
                _ => format!("X'{hex}'"),
            }
        }
        Value::Text(_) | Value::Json(_) => {
            let text = match value {
                Value::Text(text) => text.clone(),
                _ => value.display(),
            };
            let escaped = match kind {
                DatabaseKind::Mysql => text.replace('\\', "\\\\").replace('\'', "''"),
                _ => text.replace('\'', "''"),
            };
            format!("'{escaped}'")
        }
    }
}

/// Whether `text` is a single reading statement, as far as a lexer can tell.
/// Used to decide when to ask before running a human's query on prod.
pub fn is_reading_statement(kind: DatabaseKind, text: &str) -> bool {
    check_read_only_statement(kind, text.trim().trim_end_matches(';')).is_ok()
}

/// Accepts a single statement that starts with a reading keyword. This is a
/// guard in front of the read-only transaction, not a substitute for it:
/// `WITH ... DELETE` and `EXPLAIN ANALYZE DELETE` pass here and are stopped by
/// the database.
fn check_read_only_statement(kind: DatabaseKind, text: &str) -> Result<()> {
    let tokens = top_level_tokens(kind, text);
    if let Some(position) = tokens.iter().position(|token| token == ";")
        && tokens[position + 1..].iter().any(|token| token != ";")
    {
        bail!("read-only queries run one statement at a time");
    }
    let first = tokens
        .iter()
        .find(|token| token.as_str() != "(")
        .map(|token| token.to_ascii_uppercase())
        .unwrap_or_default();
    let allowed: &[&str] = match kind {
        DatabaseKind::Postgres => &["SELECT", "WITH", "VALUES", "TABLE", "SHOW", "EXPLAIN"],
        _ => &[
            "SELECT", "WITH", "VALUES", "TABLE", "SHOW", "DESCRIBE", "DESC", "EXPLAIN",
        ],
    };
    if !allowed.contains(&first.as_str()) {
        bail!("only reading statements run here; `{first}` would need a writable query");
    }
    Ok(())
}

/// Splits SQL into words and punctuation outside strings, quoted
/// identifiers and comments. Only `;`, parentheses and words matter to the
/// callers, so everything else is dropped.
fn top_level_tokens(kind: DatabaseKind, text: &str) -> Vec<String> {
    let characters = text.chars().collect::<Vec<_>>();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        let next = characters.get(index + 1).copied();
        match character {
            '\'' | '"' | '`' => {
                index += 1;
                while index < characters.len() {
                    if characters[index] == '\\' && kind == DatabaseKind::Mysql {
                        index += 2;
                        continue;
                    }
                    if characters[index] == character {
                        if characters.get(index + 1) == Some(&character) {
                            index += 2;
                            continue;
                        }
                        break;
                    }
                    index += 1;
                }
                index += 1;
            }
            '-' if next == Some('-') => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '#' if kind == DatabaseKind::Mysql => {
                while index < characters.len() && characters[index] != '\n' {
                    index += 1;
                }
            }
            '/' if next == Some('*') => {
                index += 2;
                while index + 1 < characters.len()
                    && !(characters[index] == '*' && characters[index + 1] == '/')
                {
                    index += 1;
                }
                index += 2;
            }
            '$' if kind == DatabaseKind::Postgres => {
                // Dollar quoting: $tag$ ... $tag$.
                let tag_end = characters[index + 1..]
                    .iter()
                    .position(|candidate| {
                        !(is_identifier_start(*candidate) || candidate.is_ascii_digit())
                    })
                    .map(|offset| index + 1 + offset);
                match tag_end {
                    Some(tag_end) if characters[tag_end] == '$' => {
                        let tag = characters[index..=tag_end].iter().collect::<String>();
                        let body_start = tag_end + 1;
                        let rest = characters[body_start..].iter().collect::<String>();
                        index = match rest.find(&tag) {
                            Some(offset) => {
                                body_start + rest[..offset].chars().count() + tag.chars().count()
                            }
                            None => characters.len(),
                        };
                    }
                    _ => index += 1,
                }
            }
            ';' | '(' | ')' => {
                tokens.push(character.to_string());
                index += 1;
            }
            character if is_identifier_start(character) => {
                let start = index;
                while index < characters.len()
                    // Both engines allow `$` after the first character, so
                    // `x$a$` is one identifier, not a dollar quote.
                    && (is_identifier_start(characters[index])
                        || characters[index].is_ascii_digit()
                        || characters[index] == '$')
                {
                    index += 1;
                }
                tokens.push(characters[start..index].iter().collect());
            }
            _ => index += 1,
        }
    }
    tokens
}

/// Matches the engines' own lexers: an unquoted identifier may start with any
/// non-ASCII character, not only a letter. Anything narrower lets `x€$a$`
/// read as an identifier followed by a dollar quote.
fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || !character.is_ascii()
}

/// Extracts the role names from a `GRANT `role`@`host` TO ...` line.
fn granted_role(grant: &str) -> Option<String> {
    let rest = grant.strip_prefix("GRANT ")?;
    if rest.contains(" ON ") {
        return None;
    }
    let roles = rest.split(" TO ").next()?;
    Some(roles.trim().to_string())
}

fn mysql_grants_allow_writes(grants: &[String], database: Option<&str>) -> bool {
    grants.iter().any(|grant| {
        let Some(rest) = grant.strip_prefix("GRANT ") else {
            return false;
        };
        let Some((privileges, rest)) = rest.split_once(" ON ") else {
            return false;
        };
        let target = rest.split(" TO ").next().unwrap_or_default().trim();
        let privileges = privileges.to_ascii_uppercase();
        let writes = privileges.split(',').any(|privilege| {
            matches!(
                privilege.trim(),
                "ALL" | "ALL PRIVILEGES" | "INSERT" | "UPDATE" | "DELETE"
            )
        });
        if !writes {
            return false;
        }
        let target_database = target.split('.').next().unwrap_or_default();
        let target_database = target_database.trim_matches('`').replace("\\_", "_");
        match database {
            None => true,
            Some(database) => target_database == "*" || target_database == database,
        }
    })
}

fn to_driver_value(value: &Value) -> zqlz_core::Value {
    match value {
        Value::Missing | Value::Null => zqlz_core::Value::Null,
        Value::Bool(value) => zqlz_core::Value::Bool(*value),
        Value::Int(value) => zqlz_core::Value::Int64(*value),
        Value::Float(value) => zqlz_core::Value::Float64(*value),
        Value::Text(value) => zqlz_core::Value::String(value.clone()),
        Value::Bytes(value) => zqlz_core::Value::Bytes(value.clone()),
        Value::Json(value) => zqlz_core::Value::Json(value.clone()),
    }
}

fn from_driver_value(value: zqlz_core::Value) -> Value {
    use zqlz_core::Value as Driver;
    match value {
        Driver::Null => Value::Null,
        Driver::Bool(value) => Value::Bool(value),
        Driver::Int8(value) => Value::Int(value.into()),
        Driver::Int16(value) => Value::Int(value.into()),
        Driver::Int32(value) => Value::Int(value.into()),
        Driver::Int64(value) => Value::Int(value),
        Driver::Float32(value) => Value::Float(value.into()),
        Driver::Float64(value) => Value::Float(value),
        Driver::Decimal(value) => Value::Text(value),
        Driver::String(value) => Value::Text(value),
        Driver::Bytes(value) => match String::from_utf8(value) {
            Ok(text) => Value::Text(text),
            Err(error) => Value::Bytes(error.into_bytes()),
        },
        Driver::Uuid(value) => Value::Text(value.to_string()),
        Driver::Date(value) => Value::Text(value.to_string()),
        Driver::Time(value) => Value::Text(value.to_string()),
        Driver::DateTime(value) => Value::Text(value.format("%Y-%m-%d %H:%M:%S%.f").to_string()),
        Driver::DateTimeUtc(value) => Value::Text(value.to_rfc3339()),
        Driver::Json(value) => Value::Json(value),
        Driver::Array(values) => Value::Json(serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| from_driver_value(value).to_json())
                .collect(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mysql_grants() {
        let grants = |lines: &[&str]| {
            lines
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert!(!mysql_grants_allow_writes(
            &grants(&["GRANT SELECT ON *.* TO `reader`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT ALL PRIVILEGES ON *.* TO `root`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT SELECT, INSERT, UPDATE ON `ryse`.* TO `app`@`%`"]),
            Some("ryse")
        ));
        assert!(!mysql_grants_allow_writes(
            &grants(&["GRANT SELECT, INSERT ON `other`.* TO `app`@`%`"]),
            Some("ryse")
        ));
        assert!(mysql_grants_allow_writes(
            &grants(&["GRANT DELETE ON `ryse\\_db`.`seller` TO `app`@`%`"]),
            Some("ryse_db")
        ));
        assert_eq!(
            granted_role("GRANT `app_writer`@`%` TO `joshua`@`%`"),
            Some("`app_writer`@`%`".to_string())
        );
        assert_eq!(granted_role("GRANT SELECT ON *.* TO `u`@`%`"), None);
    }

    #[test]
    fn test_dangers() {
        let dangers = |kind: DatabaseKind, text: &str| dangers(kind, &top_level_tokens(kind, text));
        let postgres = |text: &str| dangers(DatabaseKind::Postgres, text);
        assert_eq!(
            postgres("DELETE FROM orders"),
            ["This DELETE has no WHERE clause: it removes every row."]
        );
        assert!(postgres("DELETE FROM orders WHERE id = 1").is_empty());
        // A WHERE inside a subquery does not limit the outer statement.
        assert_eq!(
            postgres("UPDATE orders SET total = (SELECT max(total) FROM orders WHERE id = 1)")
                .len(),
            1
        );
        assert_eq!(
            postgres("WITH paid AS (SELECT id FROM orders WHERE paid) DELETE FROM orders").len(),
            1
        );
        // A WHERE inside a string is not a WHERE.
        assert_eq!(postgres("UPDATE orders SET note = 'WHERE'").len(), 1);
        assert_eq!(postgres("TRUNCATE orders").len(), 1);
        assert_eq!(
            postgres("DROP TABLE orders"),
            ["DROP TABLE deletes it, and any data it holds, for good."]
        );
        assert_eq!(postgres("ALTER TABLE orders DROP COLUMN note").len(), 1);
        assert!(postgres("ALTER TABLE orders ADD COLUMN note text").is_empty());
        assert_eq!(postgres("CREATE INDEX ON orders (status)").len(), 1);
        assert!(postgres("CREATE INDEX CONCURRENTLY ON orders (status)").is_empty());
        assert_eq!(
            postgres("COPY orders TO PROGRAM 'gzip > /tmp/o.gz'").len(),
            1
        );
        assert_eq!(postgres("COPY (SELECT 1) TO '/tmp/o.csv'").len(), 1);
        assert!(postgres("COPY orders TO STDOUT").is_empty());
        assert!(postgres("INSERT INTO orders (total) VALUES (1)").is_empty());
        assert!(
            dangers(
                DatabaseKind::Mysql,
                "CREATE INDEX status ON orders (status)"
            )
            .is_empty()
        );
        assert!(
            dangers(
                DatabaseKind::Mysql,
                "ALTER TABLE orders MODIFY total DOUBLE"
            )
            .is_empty()
        );
    }

    #[test]
    fn test_alter_table_target() {
        let target = |text: &str| alter_table_target(text).map(|range| text[range].to_string());
        assert_eq!(
            target("ALTER TABLE orders ADD x int").as_deref(),
            Some("orders")
        );
        assert_eq!(
            target("alter table if exists only public.\"Order\"\"s\" alter column x type text")
                .as_deref(),
            Some("public.\"Order\"\"s\"")
        );
        assert_eq!(
            target("-- why\n/* note */ ALTER TABLE  \"a\".b DROP c").as_deref(),
            Some("\"a\".b")
        );
        assert_eq!(
            target("ALTER TABLE ALL IN TABLESPACE a SET TABLESPACE b"),
            None
        );
        assert_eq!(target("ALTER INDEX orders_pkey RENAME TO x"), None);
        assert_eq!(target("UPDATE orders SET x = 1"), None);
    }

    #[test]
    fn test_mysql_alter_cost() {
        let cost = |text: &str| mysql_alter_cost(&top_level_tokens(DatabaseKind::Mysql, text));
        assert_eq!(
            cost("ALTER TABLE t ADD COLUMN note TEXT"),
            AlterCost::Instant
        );
        assert_eq!(
            cost("ALTER TABLE t DROP COLUMN note, RENAME COLUMN a TO b"),
            AlterCost::Instant
        );
        assert_eq!(cost("ALTER TABLE t ADD INDEX (email)"), AlterCost::Index);
        assert_eq!(
            cost("ALTER TABLE t ADD UNIQUE KEY u (email), ADD COLUMN x INT"),
            AlterCost::Index
        );
        assert_eq!(
            cost("ALTER TABLE t MODIFY phone VARCHAR(64)"),
            AlterCost::Copy
        );
        assert_eq!(cost("ALTER TABLE t ADD PRIMARY KEY (id)"), AlterCost::Copy);
        assert_eq!(cost("ALTER TABLE t FORCE"), AlterCost::Rebuild);
        assert_eq!(
            cost("ALTER TABLE t ADD COLUMN `MODIFY` INT"),
            AlterCost::Instant
        );
        let text = "ALTER TABLE `echo_test`.`sel``ler` ADD x INT";
        assert_eq!(
            alter_table_target(text).map(|range| &text[range]),
            Some("`echo_test`.`sel``ler`")
        );
    }

    #[test]
    fn test_mysql_write_plan() {
        let plan = serde_json::json!({
            "query_block": { "select_id": 1, "table": {
                "delete": true, "table_name": "property", "rows_examined_per_scan": 7
            } }
        });
        assert_eq!(
            MySqlWrite::find(&plan),
            Some(MySqlWrite {
                event: "DELETE",
                table: "property".into(),
                rows_examined: Some(7)
            })
        );
        let insert = serde_json::json!({ "query_block": { "table": { "insert": true, "table_name": "t" } } });
        assert_eq!(
            MySqlWrite::find(&insert).map(|write| write.rows_examined),
            Some(None)
        );
    }

    #[test]
    fn test_write_plan() {
        let plan = serde_json::json!([{
            "Plan": {
                "Node Type": "ModifyTable",
                "Operation": "Update",
                "Relation Name": "orders",
                "Schema": "public",
                "Plan Rows": 0,
                "Plans": [{ "Node Type": "Seq Scan", "Plan Rows": 16705 }],
            }
        }]);
        assert_eq!(
            WritePlan::find(&plan),
            Some(WritePlan {
                operation: "Updates",
                schema: "public".into(),
                table: "orders".into(),
                rows: 16705,
            })
        );
        let nested = serde_json::json!([{
            "Plan": {
                "Node Type": "CTE Scan",
                "Plans": [{
                    "Node Type": "ModifyTable",
                    "Operation": "Delete",
                    "Relation Name": "orders",
                    "Schema": "public",
                    "Plans": [{ "Plan Rows": 3 }],
                }],
            }
        }]);
        assert_eq!(WritePlan::find(&nested).map(|plan| plan.rows), Some(3));
        assert_eq!(
            WritePlan::find(&serde_json::json!([{ "Plan": { "Node Type": "Result" } }])),
            None
        );
    }

    #[test]
    fn test_read_only_statement_check() {
        let check = |text: &str| check_read_only_statement(DatabaseKind::Postgres, text);
        assert!(check("SELECT 1").is_ok());
        assert!(check("  -- note\n (SELECT ';' FROM t) ;  ").is_ok());
        assert!(check("WITH x AS (SELECT 1) SELECT * FROM x").is_ok());
        assert!(check("SELECT $$;$$").is_ok());
        assert!(check("COMMIT; DROP TABLE seller").is_err());
        assert!(check("SELECT 1; DELETE FROM seller").is_err());
        assert!(check("DROP TABLE seller").is_err());
        assert!(check("/* SELECT */ DELETE FROM seller").is_err());
        // `x$a$` is one identifier, not the start of a dollar quote.
        assert!(check("SELECT 1 AS x$a$; COMMIT; DROP TABLE t; SELECT $a$").is_err());
        assert!(check("SELECT 1 AS x€$a$; COMMIT; DROP TABLE t; SELECT $a$").is_err());
        assert!(check("SELECT 1 AS €$a$; COMMIT; DROP TABLE t; SELECT $a$").is_err());
        assert!(check("SELECT $é$;$é$").is_ok());
        assert!(check_read_only_statement(DatabaseKind::Mysql, "SHOW TABLES").is_ok());
        assert!(check_read_only_statement(DatabaseKind::Mysql, "SELECT 'it\\'s;' # x").is_ok());
        assert!(
            check_read_only_statement(DatabaseKind::Mysql, "SELECT 1 # ;\n; DROP TABLE t").is_err()
        );
    }
}
