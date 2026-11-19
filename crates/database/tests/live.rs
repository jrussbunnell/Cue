//! Runs the engine against real databases. Skipped unless
//! `DATABASE_LIVE_TESTS=1`; see `script/database-test-containers` for the
//! containers and seed data these expect.

use std::sync::Arc;

use database::{
    BrowseRequest, Capability, Change, ChangeSet, CollectionRef, CompareOptions, CompareSide,
    ConnectionSpec, DatabaseEnvironment, DatabaseKind, Engine, Filter, FilterOp, Request, Response,
    ResultSet, Value, compare,
};

fn enabled() -> bool {
    std::env::var("DATABASE_LIVE_TESTS").is_ok_and(|value| value == "1")
}

fn engine() -> Engine {
    Engine::new(Arc::new(reqwest_client::ReqwestClient::new()))
}

fn spec(kind: DatabaseKind, port: u16, user: &str) -> ConnectionSpec {
    ConnectionSpec {
        kind,
        host: Some("127.0.0.1".into()),
        port: Some(port),
        database: Some("echo_test".into()),
        user: Some(user.into()),
        url: None,
        ssl_mode: Some("disable".into()),
        read_only: false,
        environment: DatabaseEnvironment::Dev,
    }
}

fn http_spec(kind: DatabaseKind, url: &str, user: Option<&str>) -> ConnectionSpec {
    ConnectionSpec {
        kind,
        host: None,
        port: None,
        database: None,
        user: user.map(str::to_string),
        url: Some(url.into()),
        ssl_mode: None,
        read_only: false,
        environment: DatabaseEnvironment::Dev,
    }
}

async fn connect(engine: &Engine, name: &str, spec: ConnectionSpec, secret: &str) -> Capability {
    match engine
        .handle(Request::Connect {
            connection: name.into(),
            spec,
            secret: Some(secret.into()),
        })
        .await
        .unwrap()
    {
        Response::Connected { capability, .. } => capability,
        other => panic!("unexpected {other:?}"),
    }
}

async fn query(
    engine: &Engine,
    name: &str,
    text: &str,
    read_only: bool,
) -> anyhow::Result<ResultSet> {
    match engine
        .handle(Request::Query {
            connection: name.into(),
            text: text.into(),
            limit: 100,
            read_only,
        })
        .await?
    {
        Response::Rows(rows) => Ok(rows),
        other => panic!("unexpected {other:?}"),
    }
}

async fn browse(
    engine: &Engine,
    name: &str,
    collection: &CollectionRef,
    filters: Vec<Filter>,
    cursor: Option<String>,
    page_size: usize,
) -> database::Page {
    match engine
        .handle(Request::Browse {
            connection: name.into(),
            collection: collection.clone(),
            request: BrowseRequest {
                filters,
                cursor,
                page_size,
            },
        })
        .await
        .unwrap()
    {
        Response::Page(page) => page,
        other => panic!("unexpected {other:?}"),
    }
}

async fn apply(engine: &Engine, name: &str, changes: ChangeSet) -> anyhow::Result<u64> {
    match engine
        .handle(Request::Apply {
            connection: name.into(),
            changes,
        })
        .await?
    {
        Response::Applied(count) => Ok(count),
        other => panic!("unexpected {other:?}"),
    }
}

async fn exercise_sql(kind: DatabaseKind, port: u16, namespace: &str) {
    let engine = engine();
    assert_eq!(
        connect(
            &engine,
            "admin",
            spec(
                kind,
                port,
                if kind == DatabaseKind::Mysql {
                    "root"
                } else {
                    "postgres"
                }
            ),
            "echo"
        )
        .await,
        Capability::Writable
    );
    assert!(matches!(
        connect(&engine, "reader", spec(kind, port, "reader"), "reader").await,
        Capability::ReadOnly { .. }
    ));

    let Response::Collections(collections) = engine
        .handle(Request::Collections {
            connection: "admin".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    let seller = CollectionRef::new(Some(namespace), "seller");
    assert!(
        collections
            .iter()
            .any(|collection| collection.reference == seller),
        "{collections:?}"
    );

    let Response::Schema(schema) = engine
        .handle(Request::Describe {
            connection: "admin".into(),
            collection: seller.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(schema.primary_key, vec!["id".to_string()]);
    assert!(
        schema
            .referenced_by
            .iter()
            .any(|reference| reference.source.name == "property"
                && reference.fields == vec!["seller_id".to_string()]),
        "{schema:?}"
    );

    let rows = query(
        &engine,
        "admin",
        "SELECT id, email FROM seller ORDER BY id;",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0][1], Value::Text("a@x.com".into()));

    // A read-only query on a writable connection must not write.
    let attempt = query(
        &engine,
        "admin",
        "INSERT INTO seller (id, email) VALUES (99, 'no@x.com')",
        true,
    )
    .await;
    assert!(attempt.is_err(), "read-only query wrote: {attempt:?}");
    let count = query(&engine, "admin", "SELECT COUNT(*) FROM seller", false)
        .await
        .unwrap();
    assert!(matches!(count.rows[0][0], Value::Int(3)), "{count:?}");
    // Escapes: DDL (MySQL commits implicitly before it) and statement chains.
    for escape in [
        "CREATE TABLE escaped (id INT)",
        "COMMIT; CREATE TABLE escaped (id INT)",
        "SELECT 1; DROP TABLE property",
        "/* SELECT */ DROP TABLE property",
    ] {
        assert!(
            query(&engine, "admin", escape, true).await.is_err(),
            "{escape} ran read-only"
        );
    }
    let escaped = query(
        &engine,
        "admin",
        "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'escaped'",
        false,
    )
    .await
    .unwrap();
    assert!(matches!(escaped.rows[0][0], Value::Int(0)), "{escaped:?}");
    query(&engine, "admin", "SELECT COUNT(*) FROM property", false)
        .await
        .unwrap();
    // And the session is writable again afterwards.
    query(&engine, "admin", "SELECT 1", false).await.unwrap();
    query(&engine, "admin", "CREATE TABLE scratch (id INT)", false)
        .await
        .unwrap();
    query(&engine, "admin", "DROP TABLE scratch", false)
        .await
        .unwrap();

    let first = browse(&engine, "admin", &seller, Vec::new(), None, 2).await;
    assert_eq!(first.rows.rows.len(), 2);
    let second = browse(
        &engine,
        "admin",
        &seller,
        Vec::new(),
        first.next_cursor.clone(),
        2,
    )
    .await;
    assert_eq!(second.rows.rows.len(), 1);
    assert!(second.next_cursor.is_none());
    let filtered = browse(
        &engine,
        "admin",
        &CollectionRef::new(Some(namespace), "property"),
        vec![Filter {
            field: "seller_id".into(),
            op: FilterOp::Eq,
            value: Value::Int(1),
        }],
        None,
        50,
    )
    .await;
    assert_eq!(filtered.rows.rows.len(), 2);

    let changes = ChangeSet {
        collection: seller.clone(),
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(2))],
            set: vec![("phone".into(), Value::Text("it's 5".into()))],
            expected: vec![],
        }],
    };
    let Response::Text(preview) = engine
        .handle(Request::RenderChanges {
            connection: "admin".into(),
            changes: changes.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(preview.contains("'it''s 5'"), "{preview}");
    assert_eq!(apply(&engine, "admin", changes).await.unwrap(), 1);
    let phone = query(
        &engine,
        "admin",
        "SELECT phone FROM seller WHERE id = 2",
        false,
    )
    .await
    .unwrap();
    assert_eq!(phone.rows[0][0], Value::Text("it's 5".into()));

    // An edit made against a stale read must not overwrite the newer value.
    let edit = |expected: &str, new: &str| ChangeSet {
        collection: seller.clone(),
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(2))],
            set: vec![("phone".into(), Value::Text(new.into()))],
            expected: vec![("phone".into(), Value::Text(expected.into()))],
        }],
    };
    let error = apply(&engine, "admin", edit("stale", "lost"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed after it was read"),
        "{error}"
    );
    let phone = query(
        &engine,
        "admin",
        "SELECT phone FROM seller WHERE id = 2",
        false,
    )
    .await
    .unwrap();
    assert_eq!(phone.rows[0][0], Value::Text("it's 5".into()));
    assert_eq!(
        apply(&engine, "admin", edit("it's 5", "it's 6"))
            .await
            .unwrap(),
        1
    );
    let error = apply(
        &engine,
        "admin",
        ChangeSet {
            collection: seller.clone(),
            changes: vec![Change::Delete {
                key: vec![("id".into(), Value::Int(999))],
            }],
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no longer exists"), "{error}");

    assert!(
        apply(
            &engine,
            "reader",
            ChangeSet {
                collection: seller.clone(),
                changes: vec![Change::Delete {
                    key: vec![("id".into(), Value::Int(2))]
                }],
            }
        )
        .await
        .is_err()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mysql() {
    if !enabled() {
        return;
    }
    exercise_sql(DatabaseKind::Mysql, 13306, "echo_test").await;

    // A key matching two rows must roll the whole change set back.
    let engine = engine();
    connect(
        &engine,
        "admin",
        spec(DatabaseKind::Mysql, 13306, "root"),
        "echo",
    )
    .await;
    let error = apply(
        &engine,
        "admin",
        ChangeSet {
            collection: CollectionRef::new(Some("echo_test"), "tag"),
            changes: vec![Change::Update {
                key: vec![("name".into(), Value::Text("dup".into()))],
                set: vec![("color".into(), Value::Text("green".into()))],
                expected: vec![],
            }],
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("2 rows"), "{error}");
    let colors = query(
        &engine,
        "admin",
        "SELECT color FROM tag ORDER BY color",
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        colors.rows,
        vec![
            vec![Value::Text("blue".into())],
            vec![Value::Text("red".into())]
        ]
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_postgres() {
    if !enabled() {
        return;
    }
    exercise_sql(DatabaseKind::Postgres, 15432, "public").await;

    // `x$a$` is one identifier to Postgres; a lexer that reads `$a$` as a
    // dollar quote would hide the COMMIT and DROP that follow.
    let engine = engine();
    connect(
        &engine,
        "admin",
        spec(DatabaseKind::Postgres, 15432, "postgres"),
        "echo",
    )
    .await;
    query(
        &engine,
        "admin",
        "CREATE TABLE IF NOT EXISTS canary (id int)",
        false,
    )
    .await
    .unwrap();
    let escape = "SELECT 1 AS x$a$; COMMIT; DROP TABLE canary; SELECT $a$";
    assert!(
        query(&engine, "admin", escape, true).await.is_err(),
        "{escape} ran read-only"
    );
    query(&engine, "admin", "SELECT COUNT(*) FROM canary", false)
        .await
        .expect("the canary table survives");
}

async fn notes(engine: &Engine, request: Request) -> Vec<String> {
    match engine.handle(request).await.unwrap() {
        Response::Notes(notes) => notes,
        other => panic!("unexpected {other:?}"),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn test_mysql_assess() {
    if !enabled() {
        return;
    }
    let engine = engine();
    let mut prod = spec(DatabaseKind::Mysql, 13306, "root");
    prod.environment = DatabaseEnvironment::Prod;
    connect(&engine, "prod", prod, "echo").await;
    for statement in [
        "DROP TABLE IF EXISTS shipment_line, shipment_audit, shipment, ledger",
        "CREATE TABLE shipment (id INT PRIMARY KEY, status VARCHAR(16))",
        "CREATE TABLE shipment_audit (shipment_id INT, status VARCHAR(16))",
        "CREATE TABLE shipment_line (id INT, shipment_id INT, \
         FOREIGN KEY (shipment_id) REFERENCES shipment(id) ON DELETE CASCADE)",
        "INSERT INTO shipment VALUES (1, 'paid'), (2, 'paid'), (3, 'open')",
        "CREATE TRIGGER shipment_audit AFTER UPDATE ON shipment FOR EACH ROW \
         INSERT INTO shipment_audit VALUES (NEW.id, NEW.status)",
    ] {
        query(&engine, "prod", statement, false).await.unwrap();
    }
    let assess = |text: &str| Request::Assess {
        connection: "prod".into(),
        text: text.into(),
    };
    let update = notes(
        &engine,
        assess("UPDATE shipment SET status = 'shipped' WHERE status = 'paid'"),
    )
    .await;
    assert!(
        update[0].starts_with("MySQL expects to examine about")
            && update[0].contains(" of shipment to find the rows to update"),
        "{update:?}"
    );
    assert!(
        update
            .iter()
            .any(|note| note == "Fires 1 trigger on shipment: shipment_audit."),
        "{update:?}"
    );
    let delete = notes(&engine, assess("DELETE FROM shipment")).await;
    assert!(delete[0].contains("no WHERE clause"), "{delete:?}");
    assert!(
        delete.iter().any(|note| note
            == "Each deleted row also deletes the rows that reference it in shipment_line."),
        "{delete:?}"
    );
    let add = notes(&engine, assess("ALTER TABLE shipment ADD COLUMN note TEXT")).await;
    assert!(add[0].starts_with("Changes only metadata"), "{add:?}");
    let retype = notes(
        &engine,
        assess("ALTER TABLE `echo_test`.`shipment` MODIFY status TEXT"),
    )
    .await;
    assert!(
        retype[0].starts_with("Likely copies shipment ("),
        "{retype:?}"
    );

    // Nothing changed, and the session's lock wait is back to normal.
    let state = query(
        &engine,
        "prod",
        "SELECT (SELECT COUNT(*) FROM shipment WHERE status = 'paid'), \
         (SELECT COUNT(*) FROM information_schema.COLUMNS WHERE TABLE_SCHEMA = DATABASE() \
         AND TABLE_NAME = 'shipment' AND COLUMN_NAME = 'note'), @@SESSION.lock_wait_timeout, \
         @@SESSION.innodb_lock_wait_timeout",
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        state.rows[0],
        [
            Value::Int(2),
            Value::Int(0),
            Value::Int(31536000),
            Value::Int(10)
        ]
    );

    let shipment = CollectionRef::new(Some("echo_test"), "shipment");
    let (rehearsal, _) = rehearse(&engine, "prod", ship(&shipment, 1), false).await;
    assert_eq!(
        rehearsal,
        [
            "Rehearsed and rolled back: 1 row in echo_test.shipment.",
            "Fires 1 trigger on shipment: shipment_audit.",
        ]
    );
    exercise_held_rehearsal(
        &engine,
        "prod",
        &shipment,
        "SELECT COUNT(*) FROM shipment_audit",
    )
    .await;

    // MyISAM ignores ROLLBACK, so a rehearsal there would write for real.
    for statement in [
        "DROP TABLE IF EXISTS ledger",
        "CREATE TABLE ledger (id INT PRIMARY KEY, status VARCHAR(16)) ENGINE = MyISAM",
        "INSERT INTO ledger VALUES (1, 'paid')",
    ] {
        query(&engine, "prod", statement, false).await.unwrap();
    }
    let ledger = CollectionRef::new(Some("echo_test"), "ledger");
    let (notes, token) = rehearse(&engine, "prod", ship(&ledger, 1), true).await;
    assert_eq!(token, None);
    assert!(
        notes[0].starts_with("Not rehearsed: ledger (MyISAM)"),
        "{notes:?}"
    );
    let status = query(
        &engine,
        "prod",
        "SELECT status FROM ledger WHERE id = 1",
        false,
    )
    .await
    .unwrap();
    assert_eq!(status.rows[0][0], Value::Text("paid".into()), "nothing ran");
    // `shipment` has a trigger, so any MyISAM table in the schema could be
    // written by it; with `ledger` around, its rehearsals are skipped too.
    let (notes, _) = rehearse(&engine, "prod", ship(&shipment, 1), true).await;
    assert!(
        notes[0].starts_with("Not rehearsed: ledger (MyISAM)"),
        "{notes:?}"
    );
    query(&engine, "prod", "DROP TABLE ledger", false)
        .await
        .unwrap();

    exercise_open_transaction_guard(
        &engine,
        "prod",
        &shipment,
        "INSERT INTO shipment_audit VALUES (0, 'mine')",
        "SELECT COUNT(*) FROM shipment_audit WHERE status = 'mine'",
    )
    .await;
}

/// Writes refuse to run on a session where the user left a transaction
/// open, and leave that transaction as it was.
async fn exercise_open_transaction_guard(
    engine: &Engine,
    name: &str,
    table: &CollectionRef,
    mine: &str,
    count_mine: &str,
) {
    query(engine, name, "BEGIN", false).await.unwrap();
    query(engine, name, mine, false).await.unwrap();
    let error = apply(engine, name, ship(table, 3)).await.unwrap_err();
    assert!(error.to_string().contains("open transaction"), "{error}");
    let error = engine
        .handle(Request::Rehearse {
            connection: name.into(),
            changes: ship(table, 3),
            hold: true,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("open transaction"), "{error}");
    let rows = query(engine, name, count_mine, false).await.unwrap();
    assert_eq!(
        rows.rows[0][0],
        Value::Int(1),
        "the user's transaction is intact"
    );
    query(engine, name, "ROLLBACK", false).await.unwrap();
    let rows = query(engine, name, count_mine, false).await.unwrap();
    assert_eq!(
        rows.rows[0][0],
        Value::Int(0),
        "and still the user's to end"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_postgres_prod_safety() {
    if !enabled() {
        return;
    }
    let engine = engine();
    let mut prod = spec(DatabaseKind::Postgres, 15432, "postgres");
    prod.environment = DatabaseEnvironment::Prod;
    connect(&engine, "prod", prod, "echo").await;
    let settings = query(
        &engine,
        "prod",
        "SELECT current_setting('lock_timeout'), current_setting('statement_timeout'), \
         current_setting('idle_in_transaction_session_timeout'), current_setting('application_name')",
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        settings.rows[0],
        ["10s", "15min", "10min", "Cue"].map(|value| Value::Text(value.into()))
    );

    for statement in [
        "DROP TABLE IF EXISTS shipment, shipment_audit, shipment_line CASCADE",
        "CREATE TABLE shipment (id int PRIMARY KEY, total numeric(10,2), status text)",
        "CREATE TABLE shipment_audit (shipment_id int, status text)",
        "CREATE TABLE shipment_line (id int, shipment_id int REFERENCES shipment(id) ON DELETE CASCADE)",
        "INSERT INTO shipment SELECT g, g, 'paid' FROM generate_series(1, 500) g",
        "CREATE OR REPLACE FUNCTION shipment_audit() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN INSERT INTO shipment_audit VALUES (NEW.id, NEW.status); RETURN NEW; END $$",
        "CREATE TRIGGER shipment_audit AFTER UPDATE ON shipment FOR EACH ROW EXECUTE FUNCTION shipment_audit()",
        "ANALYZE shipment",
    ] {
        query(&engine, "prod", statement, false).await.unwrap();
    }
    let assess = |text: &str| Request::Assess {
        connection: "prod".into(),
        text: text.into(),
    };

    let update = notes(
        &engine,
        assess("UPDATE shipment SET status = 'shipped' WHERE status = 'paid'"),
    )
    .await;
    assert!(
        update[0].starts_with("Updates about 500 rows in public.shipment"),
        "{update:?}"
    );
    assert!(
        update.iter().any(|note| note.contains("shipment_audit")),
        "{update:?}"
    );
    let delete = notes(&engine, assess("DELETE FROM shipment")).await;
    assert!(delete[0].contains("no WHERE clause"), "{delete:?}");
    assert!(
        delete
            .iter()
            .any(|note| note.contains("deletes the rows that reference it in shipment_line")),
        "{delete:?}"
    );
    let widen = notes(
        &engine,
        assess("ALTER TABLE shipment ALTER COLUMN total TYPE numeric(12,2)"),
    )
    .await;
    assert_eq!(
        widen,
        ["Changes only the catalog; shipment is not rewritten."]
    );
    let retype = notes(
        &engine,
        assess("ALTER TABLE public.shipment ALTER COLUMN total TYPE float8"),
    )
    .await;
    assert!(
        retype[0].starts_with("Rewrites public.shipment ("),
        "{retype:?}"
    );
    assert!(retype[0].contains("rebuilds its index"), "{retype:?}");

    // Assessing never changes anything.
    let state = query(
        &engine,
        "prod",
        "SELECT format_type(atttypid, atttypmod), (SELECT count(*) FROM shipment WHERE status = 'paid') \
         FROM pg_attribute WHERE attrelid = 'shipment'::regclass AND attname = 'total'",
        true,
    )
    .await
    .unwrap();
    assert_eq!(
        state.rows[0],
        [Value::Text("numeric(10,2)".into()), Value::Int(500)]
    );

    // Inside the user's own transaction, assessing must not end it.
    query(&engine, "prod", "BEGIN", false).await.unwrap();
    query(
        &engine,
        "prod",
        "INSERT INTO shipment_audit VALUES (0, 'mine')",
        false,
    )
    .await
    .unwrap();
    let inside = notes(
        &engine,
        assess("UPDATE shipment SET status = 'x' WHERE id = 1"),
    )
    .await;
    assert_eq!(
        inside,
        ["This session has an open transaction, so the impact was not estimated."]
    );
    let mine = query(
        &engine,
        "prod",
        "SELECT count(*) FROM shipment_audit",
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        mine.rows[0][0],
        Value::Int(1),
        "the user's transaction is intact"
    );
    query(&engine, "prod", "ROLLBACK", false).await.unwrap();

    let shipment = CollectionRef::new(Some("public"), "shipment");
    let (rehearsal, token) = rehearse(&engine, "prod", ship(&shipment, 1), false).await;
    assert_eq!(token, None);
    assert_eq!(
        rehearsal,
        [
            "Rehearsed and rolled back: 1 row in public.shipment.",
            "Also 1 row inserted in public.shipment_audit (triggers or foreign keys).",
        ]
    );
    let audit = query(&engine, "prod", "SELECT count(*) FROM shipment_audit", true)
        .await
        .unwrap();
    assert_eq!(
        audit.rows[0][0],
        Value::Int(0),
        "the rehearsal was rolled back"
    );

    exercise_held_rehearsal(
        &engine,
        "prod",
        &shipment,
        "SELECT count(*) FROM shipment_audit",
    )
    .await;
    exercise_open_transaction_guard(
        &engine,
        "prod",
        &shipment,
        "INSERT INTO shipment_audit VALUES (0, 'mine')",
        "SELECT count(*) FROM shipment_audit WHERE status = 'mine'",
    )
    .await;
}

fn ship(table: &CollectionRef, id: i64) -> ChangeSet {
    ChangeSet {
        collection: table.clone(),
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(id))],
            set: vec![("status".into(), Value::Text("shipped".into()))],
            expected: vec![],
        }],
    }
}

async fn rehearse(
    engine: &Engine,
    name: &str,
    changes: ChangeSet,
    hold: bool,
) -> (Vec<String>, Option<u64>) {
    match engine
        .handle(Request::Rehearse {
            connection: name.into(),
            changes,
            hold,
        })
        .await
        .unwrap()
    {
        Response::Rehearsed { notes, token } => (notes, token),
        other => panic!("unexpected {other:?}"),
    }
}

async fn settle(engine: &Engine, name: &str, token: u64, commit: bool) -> anyhow::Result<u64> {
    match engine
        .handle(Request::Settle {
            connection: name.into(),
            token,
            commit,
        })
        .await?
    {
        Response::Applied(count) => Ok(count),
        other => panic!("unexpected {other:?}"),
    }
}

/// A held rehearsal keeps its transaction until it is settled: approving
/// commits exactly what was rehearsed, rejecting leaves nothing behind, and
/// the connection refuses other work meanwhile instead of hanging.
async fn exercise_held_rehearsal(engine: &Engine, name: &str, table: &CollectionRef, audit: &str) {
    let audited = async || {
        let rows = query(engine, name, audit, false).await.unwrap();
        rows.rows[0][0].clone()
    };
    let before = audited().await;

    let (notes, token) = rehearse(engine, name, ship(table, 2), true).await;
    let token = token.expect("SQL rehearsals can be held");
    assert!(notes[0].starts_with("Rehearsed: 1 row in "), "{notes:?}");
    let busy = query(engine, name, "SELECT 1", true).await.unwrap_err();
    assert!(busy.to_string().contains("waiting for approval"), "{busy}");
    assert_eq!(settle(engine, name, token, false).await.unwrap(), 0);
    assert_eq!(
        audited().await,
        before,
        "rejecting rolled the rehearsal back"
    );
    let stale = settle(engine, name, token, true).await.unwrap_err();
    assert!(stale.to_string().contains("expired"), "{stale}");

    let (_, token) = rehearse(engine, name, ship(table, 2), true).await;
    assert_eq!(settle(engine, name, token.unwrap(), true).await.unwrap(), 1);
    let status = query(
        engine,
        name,
        &format!("SELECT status FROM {} WHERE id = 2", table.name),
        false,
    )
    .await
    .unwrap();
    assert_eq!(status.rows[0][0], Value::Text("shipped".into()));
    assert_ne!(
        audited().await,
        before,
        "the approved change and its trigger committed"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_held_rehearsal_expires() {
    if !enabled() {
        return;
    }
    let engine = engine().with_held_rehearsal_timeout(std::time::Duration::from_millis(300));
    connect(
        &engine,
        "admin",
        spec(DatabaseKind::Postgres, 15432, "postgres"),
        "echo",
    )
    .await;
    let seller = CollectionRef::new(Some("public"), "seller");
    let change = ChangeSet {
        collection: seller,
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(3))],
            set: vec![("phone".into(), Value::Text("expired".into()))],
            expected: vec![],
        }],
    };
    let (_, token) = rehearse(&engine, "admin", change, true).await;
    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    let error = settle(&engine, "admin", token.unwrap(), true)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("expired"), "{error}");
    let phone = query(
        &engine,
        "admin",
        "SELECT phone FROM seller WHERE id = 3",
        false,
    )
    .await
    .expect("the connection is free again");
    assert_eq!(
        phone.rows[0][0],
        Value::Text("555".into()),
        "the expired rehearsal rolled back"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn test_clickhouse() {
    if !enabled() {
        return;
    }
    let engine = engine();
    let url = "http://127.0.0.1:18123";
    assert_eq!(
        connect(
            &engine,
            "admin",
            http_spec(DatabaseKind::Clickhouse, url, Some("default")),
            "echo"
        )
        .await,
        Capability::Writable
    );
    assert!(matches!(
        connect(
            &engine,
            "reader",
            http_spec(DatabaseKind::Clickhouse, url, Some("reader")),
            "reader"
        )
        .await,
        Capability::ReadOnly { .. }
    ));
    let rows = query(
        &engine,
        "reader",
        "SELECT id, kind FROM default.events ORDER BY id",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.rows.len(), 3);
    assert_eq!(rows.rows[0][0], Value::Int(1));

    for escape in [
        "INSERT INTO default.events VALUES (9, 9, 'x')",
        "CREATE TABLE default.escaped (id UInt8) ENGINE = Memory",
        "INSERT INTO default.events SETTINGS readonly = 0 VALUES (9, 9, 'x')",
    ] {
        assert!(
            query(&engine, "admin", escape, true).await.is_err(),
            "{escape} ran read-only"
        );
    }
    let count = query(
        &engine,
        "admin",
        "SELECT count() FROM default.events",
        false,
    )
    .await
    .unwrap();
    assert_eq!(count.rows[0][0], Value::Int(3));

    let Response::Schema(schema) = engine
        .handle(Request::Describe {
            connection: "reader".into(),
            collection: CollectionRef::new(Some("default"), "events"),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(schema.primary_key, vec!["id".to_string()]);
    let page = browse(
        &engine,
        "reader",
        &CollectionRef::new(Some("default"), "events"),
        vec![Filter {
            field: "seller_id".into(),
            op: FilterOp::Eq,
            value: Value::Int(1),
        }],
        None,
        10,
    )
    .await;
    assert_eq!(page.rows.rows.len(), 2);

    // Stale edits are refused before any mutation runs.
    let events = CollectionRef::new(Some("default"), "events");
    let edit = |expected: &str, new: &str| ChangeSet {
        collection: events.clone(),
        changes: vec![Change::Update {
            key: vec![("id".into(), Value::Int(3))],
            set: vec![("kind".into(), Value::Text(new.into()))],
            expected: vec![("kind".into(), Value::Text(expected.into()))],
        }],
    };
    let error = apply(&engine, "admin", edit("stale", "lost"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed after it was read"),
        "{error}"
    );
    assert_eq!(
        apply(&engine, "admin", edit("signup", "renamed"))
            .await
            .unwrap(),
        1
    );
    let kind = query(
        &engine,
        "admin",
        "SELECT kind FROM default.events WHERE id = 3",
        false,
    )
    .await
    .unwrap();
    assert_eq!(kind.rows[0][0], Value::Text("renamed".into()));
    assert_eq!(
        apply(&engine, "admin", edit("renamed", "signup"))
            .await
            .unwrap(),
        1
    );
    let error = apply(
        &engine,
        "admin",
        ChangeSet {
            collection: events.clone(),
            changes: vec![Change::Delete {
                key: vec![("id".into(), Value::Int(999))],
            }],
        },
    )
    .await
    .unwrap_err();
    assert!(error.to_string().contains("no longer exists"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn test_convex() {
    if !enabled() {
        return;
    }
    let Ok(key) = std::fs::read_to_string("/tmp/echo-db-convex-key") else {
        panic!("generate a key with script/database-test-containers");
    };
    let engine = engine();
    let url = "http://127.0.0.1:13210";
    assert_eq!(
        connect(
            &engine,
            "atlas",
            http_spec(DatabaseKind::Convex, url, None),
            key.trim()
        )
        .await,
        Capability::Writable
    );

    let links = CollectionRef::new(None, "ryseSellerUserLinks");
    let first = browse(&engine, "atlas", &links, Vec::new(), None, 2).await;
    assert_eq!(first.rows.columns[0].name, "_id");
    assert!(first.next_cursor.is_some());
    let filtered = browse(
        &engine,
        "atlas",
        &links,
        vec![Filter {
            field: "ryseSellerId".into(),
            op: FilterOp::Eq,
            value: Value::Float(2.0),
        }],
        None,
        10,
    )
    .await;
    assert_eq!(filtered.rows.rows.len(), 1);
    let id_index = filtered.rows.column_index("_id").unwrap();
    let id = filtered.rows.rows[0][id_index].clone();

    let changes = ChangeSet {
        collection: links.clone(),
        changes: vec![Change::Update {
            key: vec![("_id".into(), id.clone())],
            set: vec![("email".into(), Value::Text("b2@x.com".into()))],
            expected: vec![],
        }],
    };
    assert_eq!(apply(&engine, "atlas", changes).await.unwrap(), 1);
    let after = browse(
        &engine,
        "atlas",
        &links,
        vec![Filter {
            field: "_id".into(),
            op: FilterOp::Eq,
            value: id,
        }],
        None,
        10,
    )
    .await;
    let email = after.rows.column_index("email").unwrap();
    assert_eq!(after.rows.rows[0][email], Value::Text("b2@x.com".into()));

    // A stale edit is refused; one made against the current value applies.
    let edit = |expected: &str, new: &str| ChangeSet {
        collection: links.clone(),
        changes: vec![Change::Update {
            key: vec![(
                "_id".into(),
                after.rows.rows[0][after.rows.column_index("_id").unwrap()].clone(),
            )],
            set: vec![("email".into(), Value::Text(new.into()))],
            expected: vec![("email".into(), Value::Text(expected.into()))],
        }],
    };
    let error = apply(&engine, "atlas", edit("stale@x.com", "lost@x.com"))
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("changed after it was read"),
        "{error}"
    );
    assert_eq!(
        apply(&engine, "atlas", edit("b2@x.com", "b3@x.com"))
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        apply(&engine, "atlas", edit("b3@x.com", "b2@x.com"))
            .await
            .unwrap(),
        1
    );

    // History: walk the snapshot, then deltas.
    let mut cursor = None;
    let mut entries = 0;
    for _ in 0..10 {
        let Response::History(batch) = engine
            .handle(Request::History {
                connection: "atlas".into(),
                cursor: cursor.clone(),
            })
            .await
            .unwrap()
        else {
            panic!()
        };
        entries += batch.entries.len();
        let done = batch.cursor.starts_with("delta:") && !batch.has_more;
        cursor = Some(batch.cursor);
        if done {
            break;
        }
    }
    assert!(entries >= 3, "{entries}");
    assert!(cursor.unwrap().starts_with("delta:"));
}

#[tokio::test(flavor = "multi_thread")]
async fn test_compare_mysql_against_convex() {
    if !enabled() {
        return;
    }
    let key = std::fs::read_to_string("/tmp/echo-db-convex-key").unwrap();
    let engine = engine();
    connect(
        &engine,
        "ryse",
        spec(DatabaseKind::Mysql, 13306, "reader"),
        "reader",
    )
    .await;
    connect(
        &engine,
        "atlas",
        http_spec(DatabaseKind::Convex, "http://127.0.0.1:13210", None),
        key.trim(),
    )
    .await;
    let left = query(&engine, "ryse", "SELECT id, email FROM seller", true)
        .await
        .unwrap();
    let right = browse(
        &engine,
        "atlas",
        &CollectionRef::new(None, "ryseSellerUserLinks"),
        Vec::new(),
        None,
        100,
    )
    .await
    .rows;
    let left_key = ["id".to_string()];
    let right_key = ["ryseSellerId".to_string()];
    let outcome = compare(
        CompareSide {
            rows: &left,
            key: &left_key,
        },
        CompareSide {
            rows: &right,
            key: &right_key,
        },
        &CompareOptions {
            fields: vec![("email".into(), "email".into())],
        },
    )
    .unwrap();
    // Seller 1 matches, 2 and 3 exist on both sides; nothing is one-sided.
    assert_eq!(outcome.left_only(), 0, "{outcome:?}");
    assert_eq!(outcome.right_only(), 0, "{outcome:?}");
}

/// Serves the shapes Bartr's `warehouse` relay returns: `POST /query` with
/// `{sql}` answers `{meta, rows}`, `GET /catalog/*` answers its catalog, and
/// everything needs the `x-relay-secret` header.
fn fake_relay() -> String {
    use std::io::{BufRead, BufReader, Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut content_length = 0;
            let mut secret_ok = false;
            loop {
                let mut header = String::new();
                reader.read_line(&mut header).unwrap();
                let header = header.trim().to_ascii_lowercase();
                if header.is_empty() {
                    break;
                }
                if let Some(length) = header.strip_prefix("content-length:") {
                    content_length = length.trim().parse().unwrap();
                }
                secret_ok |= header == "x-relay-secret: s3cret";
            }
            let mut body = vec![0; content_length];
            reader.read_exact(&mut body).unwrap();
            let body = String::from_utf8(body).unwrap();
            let (status, response) = if !secret_ok {
                (
                    "403 Forbidden",
                    r#"{"error":"Invalid relay secret"}"#.to_string(),
                )
            } else if request_line.starts_with("GET /catalog/tables") {
                ("200 OK", r#"{"tables":[{"database":"catalog","table":"listings","engine":"ReplacingMergeTree"}]}"#.to_string())
            } else if request_line.starts_with("GET /catalog/describe-table") {
                ("200 OK", r#"{"columns":[{"name":"id","type":"UInt64"},{"name":"city","type":"Nullable(String)"}]}"#.to_string())
            } else if body.contains("boom") {
                (
                    "400 Bad Request",
                    r#"{"error":"Unknown identifier 'boom'"}"#.to_string(),
                )
            } else {
                ("200 OK", r#"{"meta":[{"name":"id","type":"UInt64"},{"name":"city","type":"String"}],"rows":[{"id":"1","city":"Austin"},{"id":"2"}],"rowCount":2}"#.to_string())
            };
            write!(
                stream,
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{response}",
                response.len()
            )
            .unwrap();
        }
    });
    format!("http://{address}/query")
}

#[tokio::test(flavor = "multi_thread")]
async fn test_clickhouse_relay() {
    if !enabled() {
        return;
    }
    let engine = engine();
    let url = fake_relay();
    assert!(matches!(
        connect(
            &engine,
            "bartr",
            http_spec(DatabaseKind::ClickhouseRelay, &url, None),
            "s3cret"
        )
        .await,
        Capability::ReadOnly { .. }
    ));
    let rows = query(
        &engine,
        "bartr",
        "SELECT id, city FROM catalog.listings",
        false,
    )
    .await
    .unwrap();
    assert_eq!(rows.columns.len(), 2);
    assert_eq!(
        rows.rows[0],
        vec![Value::Text("1".into()), Value::Text("Austin".into())]
    );
    assert_eq!(rows.rows[1][1], Value::Missing);
    let error = query(&engine, "bartr", "SELECT boom", false)
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("Unknown identifier 'boom'"),
        "{error}"
    );

    let Response::Collections(collections) = engine
        .handle(Request::Collections {
            connection: "bartr".into(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(
        collections[0].reference,
        CollectionRef::new(Some("catalog"), "listings")
    );
    let Response::Schema(schema) = engine
        .handle(Request::Describe {
            connection: "bartr".into(),
            collection: collections[0].reference.clone(),
        })
        .await
        .unwrap()
    else {
        panic!()
    };
    assert_eq!(schema.fields.len(), 2);
    assert!(schema.fields[1].nullable);
    assert_eq!(schema.writable, Some(false));
    assert!(
        apply(
            &engine,
            "bartr",
            ChangeSet {
                collection: collections[0].reference.clone(),
                changes: vec![Change::Delete {
                    key: vec![("id".into(), Value::Int(1))]
                }],
            }
        )
        .await
        .is_err()
    );

    let wrong_secret = engine
        .handle(Request::Connect {
            connection: "bad".into(),
            spec: http_spec(DatabaseKind::ClickhouseRelay, &url, None),
            secret: Some("nope".into()),
        })
        .await
        .unwrap_err();
    assert!(
        wrong_secret.to_string().contains("Invalid relay secret"),
        "{wrong_secret}"
    );
}
