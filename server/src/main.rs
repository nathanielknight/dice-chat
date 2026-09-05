//! Dice Chat: one-off chat rooms with integrated dice rolling.
//!
//! A single binary plus a SQLite file is a complete installation; Postgres
//! is supported as an alternative backend, selected by connection string:
//!
//! ```text
//! dice-chat                                   # sqlite://dice-chat.db
//! DATABASE_URL=sqlite://game.db dice-chat
//! DATABASE_URL=postgres://user:pw@host/db dice-chat
//! BIND_ADDR=0.0.0.0:8080 dice-chat
//! ```

mod clock;
mod rng;
mod store;
mod token;
mod view;
mod web;

use std::sync::Arc;

use axum_extra::extract::cookie::Key;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;

use store::Store;

const COOKIE_KEY_META: &str = "cookie_key";

#[tokio::main]
async fn main() {
    let db_url =
        std::env::var("DATABASE_URL").unwrap_or_else(|_| "sqlite://dice-chat.db".to_string());
    let bind = std::env::var("BIND_ADDR").unwrap_or_else(|_| "127.0.0.1:8080".to_string());

    let store: Arc<dyn Store> = if let Some(path) = db_url.strip_prefix("sqlite://") {
        Arc::new(store::sqlite::SqliteStore::open(path).unwrap_or_else(|e| {
            eprintln!("failed to open {path}: {e}");
            std::process::exit(1);
        }))
    } else if db_url.starts_with("postgres://") || db_url.starts_with("postgresql://") {
        Arc::new(store::postgres::PostgresStore::connect(&db_url).await.unwrap_or_else(|e| {
            eprintln!("failed to connect to postgres: {e}");
            std::process::exit(1);
        }))
    } else {
        eprintln!("DATABASE_URL must start with sqlite:// or postgres:// (got {db_url})");
        std::process::exit(1);
    };

    let key = cookie_key(store.as_ref()).await.unwrap_or_else(|e| {
        eprintln!("failed to load cookie key: {e}");
        std::process::exit(1);
    });

    let app = web::AppState::new(store, key);

    // Background sweep (SPEC.md §3.1): rooms are permanently deleted one
    // year after creation. Locking needs no sweep — it's computed from age.
    let sweeper = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
        loop {
            tick.tick().await;
            match sweeper.store.sweep(clock::now() - clock::DELETE_AFTER).await {
                Ok(0) => {}
                Ok(n) => println!("swept {n} expired room(s)"),
                Err(e) => eprintln!("sweep failed: {e}"),
            }
            sweeper.prune_channels();
        }
    });

    let router = web::router(app);
    let listener = tokio::net::TcpListener::bind(&bind).await.unwrap_or_else(|e| {
        eprintln!("failed to bind {bind}: {e}");
        std::process::exit(1);
    });
    println!("dice-chat listening on http://{bind}");
    axum::serve(listener, router).await.expect("server error");
}

/// The cookie signing key lives with the data, so a restart (or a second
/// process on the same database) keeps existing identities valid.
async fn cookie_key(store: &dyn Store) -> Result<Key, store::StoreError> {
    if let Some(encoded) = store.meta_get(COOKIE_KEY_META).await? {
        if let Ok(bytes) = BASE64.decode(&encoded) {
            if let Ok(key) = Key::try_from(&bytes[..]) {
                return Ok(key);
            }
        }
        eprintln!("stored cookie key is corrupt; issuing a fresh one (existing identities reset)");
    }
    let key = Key::generate();
    store.meta_set(COOKIE_KEY_META, &BASE64.encode(key.master())).await?;
    Ok(key)
}
