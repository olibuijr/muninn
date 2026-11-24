// Live test (scratch database only, MUNINN_TEST_PG_URL): stores must survive a Postgres restart.
// Before this, a terminated backend left the long-lived Client closed and every call failed with
// "connection closed" until the daemon was restarted.
use muninn_core::pg::{Conn, PgStore};
use postgres::{Client, NoTls};
use std::sync::{Mutex, MutexGuard};

/// Each test terminates every other session in the database, so running them in parallel kills
/// the other test's connection mid-call. Hold this for the whole test.
fn serial() -> MutexGuard<'static, ()> {
    static SERIAL: Mutex<()> = Mutex::new(());
    SERIAL.lock().unwrap_or_else(|p| p.into_inner())
}

fn kill_other_sessions(url: &str) {
    let mut admin = Client::connect(url, NoTls).unwrap();
    admin
        .execute(
            "select pg_terminate_backend(pid) from pg_stat_activity
             where datname = current_database() and pid <> pg_backend_pid()",
            &[],
        )
        .unwrap();
}

#[test]
fn conn_reconnects_after_backend_is_terminated() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let _serial = serial();
    let conn = Conn::connect(&url).unwrap();
    conn.lock().execute("select 1", &[]).unwrap();
    kill_other_sessions(&url);
    let _ = conn.lock().execute("select 1", &[]); // may fail: this is how the client learns it is closed
    conn.lock()
        .execute("select 1", &[])
        .expect("second call after a restart must reconnect");
}

#[test]
fn symbol_lookup_recovers_after_backend_is_terminated() {
    let Ok(url) = std::env::var("MUNINN_TEST_PG_URL") else {
        return;
    };
    let _serial = serial();
    let store = PgStore::connect(&url).unwrap();
    store.symbol_lookup("Fit", "def", None, 5).unwrap();
    kill_other_sessions(&url);
    let _ = store.symbol_lookup("Fit", "def", None, 5);
    store
        .symbol_lookup("Fit", "def", None, 5)
        .expect("symbol_lookup must recover after a Postgres restart");
}
