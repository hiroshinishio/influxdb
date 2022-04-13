//! Helpers for initializing the shared database connection

use assert_cmd::Command;
use once_cell::sync::Lazy;
use sqlx::{migrate::MigrateDatabase, Postgres};
use std::{collections::BTreeSet, sync::Mutex};

// I really do want to block everything until the database is initialized...
#[allow(clippy::await_holding_lock)]
static DB_INITIALIZED: Lazy<Mutex<BTreeSet<String>>> = Lazy::new(|| Mutex::new(BTreeSet::new()));

/// Performs once-per-process database initialization, if necessary
pub async fn initialize_db(dsn: &str, schema_name: &str) {
    let mut init = DB_INITIALIZED.lock().expect("Mutex poisoned");

    // already done
    if init.contains(schema_name) {
        return;
    }

    println!("Initializing database...");

    // Create the catalog database if it doesn't exist
    if !Postgres::database_exists(dsn).await.unwrap() {
        println!("Creating database...");
        Postgres::create_database(dsn).await.unwrap();
    }

    // Set up the catalog
    Command::cargo_bin("influxdb_iox")
        .unwrap()
        .arg("catalog")
        .arg("setup")
        .env("INFLUXDB_IOX_CATALOG_DSN", dsn)
        .env("INFLUXDB_IOX_CATALOG_POSTGRES_SCHEMA_NAME", schema_name)
        .ok()
        .unwrap();

    // Create the shared Kafka topic in the catalog
    Command::cargo_bin("influxdb_iox")
        .unwrap()
        .arg("catalog")
        .arg("topic")
        .arg("update")
        .arg("iox-shared")
        .env("INFLUXDB_IOX_CATALOG_DSN", dsn)
        .env("INFLUXDB_IOX_CATALOG_POSTGRES_SCHEMA_NAME", schema_name)
        .ok()
        .unwrap();

    init.insert(schema_name.into());
}
