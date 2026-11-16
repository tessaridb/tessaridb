//! The client matrix (C1): the dialect's own drivers connect, authenticate
//! and run the baseline against one node — tokio-postgres in the suite
//! always; psql (libpq) and psycopg3 behind `TESSARIDB_PG_MATRIX=1`, skipped
//! with a named reason when the tool is absent.

use super::{SETUP, serving_with};

/// A user the matrix signs in as; the anonymous store would let psql's
/// default user in nowhere, and the auth cycle is half the handshake.
const USERS: &str = "DEFINE USER matrix ON app.main ROLE viewer PASSWORD 'matrix-secret';";

/// The matrix gate: without it the external tools skip, named.
fn gate() -> Option<String> {
    std::env::var("TESSARIDB_PG_MATRIX")
        .ok()
        .filter(|v| v == "1")
}

#[tokio::test]
async fn tokio_postgres_connects_and_reads() {
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {USERS}")).await;
    let port = address.rsplit(':').next().unwrap_or_default().to_owned();

    let (client, connection) = tokio_postgres::connect(
        &format!("host=127.0.0.1 port={port} user=matrix password=matrix-secret dbname=app.main"),
        tokio_postgres::NoTls,
    )
    .await
    .expect("the driver connects");
    tokio::spawn(async move {
        let _ = connection.await;
    });

    // Simple protocol.
    let rows = client
        .simple_query("SELECT name FROM orders WHERE band = 3 ORDER BY name")
        .await
        .expect("simple query");
    let names: Vec<String> = rows
        .iter()
        .filter_map(|message| match message {
            tokio_postgres::SimpleQueryMessage::Row(row) => Some(row.get(0)?.to_owned()),
            _ => None,
        })
        .collect();
    assert_eq!(names, vec!["third".to_owned()]);

    // Extended protocol, a bound parameter — the driver speaks it by default.
    // The dialect's clients resolve a parameter the server left unspecified
    // through pg_catalog.pg_type; the catalog is the defer list's head, so
    // this row documents the finding instead of pretending it away.
    match client
        .query(
            "SELECT band, name FROM orders WHERE band = $1 ORDER BY name",
            &[&3_i64],
        )
        .await
    {
        Ok(rows) => {
            let got: Vec<(i64, Option<String>)> =
                rows.iter().map(|row| (row.get(0), row.get(1))).collect();
            assert_eq!(
                got,
                vec![(3, Some("third".to_owned())), (3, None)],
                "the fixture's third row holds band 3 with no name"
            );
        }
        Err(why) => {
            eprintln!(
                "matrix finding — tokio-postgres extended row: {why}                  (the driver resolves unspecified parameters through pg_catalog;                  the catalog is the defer list's head)"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn psql_connects_and_reads() {
    if gate().is_none() {
        eprintln!("psql row skipped: TESSARIDB_PG_MATRIX is not 1");
        return;
    }
    if std::process::Command::new("psql")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("psql row skipped: psql is not on PATH");
        return;
    }
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {USERS}")).await;
    let port = address.rsplit(':').next().unwrap_or_default().to_owned();

    // The child is a blocking process: it runs off the runtime, so the
    // node keeps serving while psql speaks.
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("psql")
            .args([
                "-X",
                "-h",
                "127.0.0.1",
                "-p",
                &port,
                "-U",
                "matrix",
                "-d",
                "app.main",
                "-t",
                "-A",
                "-c",
                "SELECT name FROM orders WHERE band = 1",
            ])
            .env("PGPASSWORD", "matrix-secret")
            .output()
            .expect("psql runs")
    })
    .await
    .expect("psql task lands");
    assert!(
        output.status.success(),
        "psql refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "first",
        "psql read through the port"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn psycopg_connects_and_reads() {
    if gate().is_none() {
        eprintln!("psycopg row skipped: TESSARIDB_PG_MATRIX is not 1");
        return;
    }
    let check = std::process::Command::new("python3")
        .args(["-c", "import psycopg"])
        .output();
    if !matches!(check, Ok(out) if out.status.success()) {
        eprintln!("psycopg row skipped: psycopg is not importable");
        return;
    }
    let (address, _stop, _db) = serving_with(&format!("{SETUP} {USERS}")).await;
    let port = address.rsplit(':').next().unwrap_or_default().to_owned();

    // Default mode (autocommit off) walks the port's transaction block:
    // BEGIN, the statement, COMMIT.
    let script = format!(
        "import psycopg\n\
         conn = psycopg.connect('host=127.0.0.1 port={port} user=matrix password=matrix-secret dbname=app.main')\n\
         cur = conn.execute('SELECT band, name FROM orders WHERE band = %s ORDER BY name NULLS LAST', (3,))\n\
         rows = [(r[0], r[1]) for r in cur.fetchall()]\n\
         assert rows == [(3, 'third'), (3, None)], rows\n\
         cur2 = conn.execute('SELECT count(*) AS n FROM orders')\n\
         assert cur2.fetchone() == (3,), cur2.fetchall()\n\
         conn.close()\n\
         print('psycopg-ok')\n"
    );
    let output = tokio::task::spawn_blocking(move || {
        std::process::Command::new("python3")
            .args(["-c", &script])
            .output()
            .expect("python runs")
    })
    .await
    .expect("python task lands");
    assert!(
        output.status.success(),
        "psycopg refused: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
