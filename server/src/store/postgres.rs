//! Postgres backend (tokio-postgres).
//!
//! A single connection behind an async mutex — transactions need exclusive
//! access, and the intended scale doesn't call for a pool. Roll transactions
//! take `FOR UPDATE` on the room row, so concurrent rolls serialize correctly
//! even across multiple server processes.

use std::collections::HashMap;

use tokio_postgres::{Client, NoTls, Row};

use super::{
    eval_with_state, Message, MessageAttempt, Room, RollInput, Store, StoreError, StoreResult,
};

impl From<tokio_postgres::Error> for StoreError {
    fn from(e: tokio_postgres::Error) -> Self {
        StoreError(format!("postgres: {e}"))
    }
}

pub struct PostgresStore {
    pub(crate) client: tokio::sync::Mutex<Client>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS rooms (
    id            BIGSERIAL PRIMARY KEY,
    token         TEXT NOT NULL UNIQUE,
    created_at    BIGINT NOT NULL,
    locked_at     BIGINT,
    rng_state     BYTEA NOT NULL,
    seq_counter   BIGINT NOT NULL DEFAULT 0,
    event_counter BIGINT NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS members (
    room_id    BIGINT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    client_id  TEXT NOT NULL,
    name       TEXT NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (room_id, client_id)
);
CREATE TABLE IF NOT EXISTS messages (
    room_id           BIGINT NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    seq               BIGINT NOT NULL,
    author            TEXT NOT NULL,
    expr              TEXT,
    comment           TEXT NOT NULL,
    roll_json         TEXT,
    total             BIGINT,
    created_at        BIGINT NOT NULL,
    updated_at        BIGINT NOT NULL,
    updated_by        TEXT NOT NULL,
    created_event_seq BIGINT NOT NULL,
    event_seq         BIGINT NOT NULL,
    PRIMARY KEY (room_id, seq)
);
";

impl PostgresStore {
    /// Connect using a `postgres://…` connection string and ensure the schema.
    pub async fn connect(conn_str: &str) -> StoreResult<Self> {
        let (client, connection) = tokio_postgres::connect(conn_str, NoTls).await?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("postgres connection error: {e}");
            }
        });
        client.batch_execute(SCHEMA).await?;
        migrate(&client).await?;
        Ok(PostgresStore { client: tokio::sync::Mutex::new(client) })
    }
}

/// Bring a pre-`expr`/`comment` database (messages as `kind` + `body`)
/// forward: rolls keep their expression, text messages become comments.
async fn migrate(client: &Client) -> StoreResult<()> {
    let legacy: bool = client
        .query_one(
            "SELECT EXISTS (SELECT 1 FROM information_schema.columns
             WHERE table_name = 'messages' AND column_name = 'kind')",
            &[],
        )
        .await?
        .get(0);
    if !legacy {
        return Ok(());
    }
    client
        .batch_execute(
            "BEGIN;
             ALTER TABLE messages ADD COLUMN expr TEXT;
             ALTER TABLE messages ADD COLUMN comment TEXT NOT NULL DEFAULT '';
             UPDATE messages SET expr = body WHERE kind = 'roll';
             UPDATE messages SET comment = body WHERE kind = 'text';
             ALTER TABLE messages DROP COLUMN kind;
             ALTER TABLE messages DROP COLUMN body;
             COMMIT;",
        )
        .await?;
    Ok(())
}

fn room_from_row(row: &Row) -> Room {
    Room {
        id: row.get(0),
        token: row.get(1),
        created_at: row.get(2),
        locked_at: row.get(3),
        event_counter: row.get(4),
    }
}

fn message_from_row(row: &Row) -> Message {
    Message {
        room_id: row.get(0),
        seq: row.get(1),
        author: row.get(2),
        expr: row.get(3),
        comment: row.get(4),
        roll_json: row.get(5),
        total: row.get(6),
        created_at: row.get(7),
        updated_at: row.get(8),
        updated_by: row.get(9),
        created_event_seq: row.get(10),
        event_seq: row.get(11),
    }
}

const MESSAGE_COLS: &str = "room_id, seq, author, expr, comment, roll_json, total, \
     created_at, updated_at, updated_by, created_event_seq, event_seq";

/// Evaluate a message's roll (if any) against the room's RNG, returning the
/// columns to write and the successor RNG state. `Ok(Err(_))` is an eval
/// error: the caller drops the transaction, persisting nothing.
type RollColumns = (Option<String>, Option<i64>, Option<[u8; crate::rng::STATE_LEN]>);

async fn eval_roll(
    tx: &tokio_postgres::Transaction<'_>,
    room_id: i64,
    roll: &Option<RollInput>,
) -> StoreResult<Result<RollColumns, dice::EvalError>> {
    let Some(roll) = roll else {
        return Ok(Ok((None, None, None)));
    };
    let state = rng_state_for_update(tx, room_id).await?;
    let (outcome, next_state) = match eval_with_state(&state, &roll.expr)? {
        Ok(ok) => ok,
        Err(e) => return Ok(Err(e)),
    };
    let roll_json = serde_json::to_string(&outcome)
        .map_err(|e| StoreError(format!("serialize outcome: {e}")))?;
    Ok(Ok((Some(roll_json), Some(outcome.value), Some(next_state))))
}

/// Bump the room's counters, returning `(next_seq, next_event_seq)`.
async fn bump_counters(
    tx: &tokio_postgres::Transaction<'_>,
    room_id: i64,
    new_message: bool,
) -> StoreResult<(i64, i64)> {
    let seq_incr = i64::from(new_message);
    let row = tx
        .query_opt(
            "UPDATE rooms SET seq_counter = seq_counter + $1,
                              event_counter = event_counter + 1
             WHERE id = $2
             RETURNING seq_counter, event_counter",
            &[&seq_incr, &room_id],
        )
        .await?
        .ok_or_else(|| StoreError(format!("no room {room_id}")))?;
    Ok((row.get(0), row.get(1)))
}

async fn get_message_tx(
    tx: &tokio_postgres::Transaction<'_>,
    room_id: i64,
    seq: i64,
) -> StoreResult<Option<Message>> {
    let row = tx
        .query_opt(
            &format!("SELECT {MESSAGE_COLS} FROM messages WHERE room_id = $1 AND seq = $2"),
            &[&room_id, &seq],
        )
        .await?;
    Ok(row.as_ref().map(message_from_row))
}

/// Lock the room row and return its RNG state.
async fn rng_state_for_update(
    tx: &tokio_postgres::Transaction<'_>,
    room_id: i64,
) -> StoreResult<Vec<u8>> {
    let row = tx
        .query_opt("SELECT rng_state FROM rooms WHERE id = $1 FOR UPDATE", &[&room_id])
        .await?
        .ok_or_else(|| StoreError(format!("no room {room_id}")))?;
    Ok(row.get(0))
}

#[async_trait::async_trait]
impl Store for PostgresStore {
    async fn create_room(&self, token: &str, now: i64, rng_state: &[u8]) -> StoreResult<Room> {
        let client = self.client.lock().await;
        let row = client
            .query_one(
                "INSERT INTO rooms (token, created_at, rng_state) VALUES ($1, $2, $3)
                 RETURNING id, token, created_at, locked_at, event_counter",
                &[&token, &now, &rng_state],
            )
            .await?;
        Ok(room_from_row(&row))
    }

    async fn room_by_token(&self, token: &str) -> StoreResult<Option<Room>> {
        let client = self.client.lock().await;
        let row = client
            .query_opt(
                "SELECT id, token, created_at, locked_at, event_counter
                 FROM rooms WHERE token = $1",
                &[&token],
            )
            .await?;
        Ok(row.as_ref().map(room_from_row))
    }

    async fn lock_room(&self, room_id: i64, now: i64) -> StoreResult<i64> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        tx.execute(
            "UPDATE rooms SET locked_at = $1 WHERE id = $2 AND locked_at IS NULL",
            &[&now, &room_id],
        )
        .await?;
        let (_, event_seq) = bump_counters(&tx, room_id, false).await?;
        tx.commit().await?;
        Ok(event_seq)
    }

    async fn set_name(&self, room_id: i64, client_id: &str, name: &str, now: i64) -> StoreResult<()> {
        let client = self.client.lock().await;
        client
            .execute(
                "INSERT INTO members (room_id, client_id, name, updated_at)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (room_id, client_id)
                 DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
                &[&room_id, &client_id, &name, &now],
            )
            .await?;
        Ok(())
    }

    async fn names(&self, room_id: i64) -> StoreResult<HashMap<String, String>> {
        let client = self.client.lock().await;
        let rows = client
            .query("SELECT client_id, name FROM members WHERE room_id = $1", &[&room_id])
            .await?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1))).collect())
    }

    async fn post_message(
        &self,
        room_id: i64,
        author: &str,
        roll: Option<RollInput>,
        comment: &str,
        now: i64,
    ) -> StoreResult<MessageAttempt> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        let (roll_json, total, next_state) = match eval_roll(&tx, room_id, &roll).await? {
            Ok(cols) => cols,
            Err(e) => return Ok(Err(e)), // tx dropped → rollback
        };
        let (seq, event_seq) = bump_counters(&tx, room_id, true).await?;
        if let Some(next_state) = next_state {
            tx.execute(
                "UPDATE rooms SET rng_state = $1 WHERE id = $2",
                &[&&next_state[..], &room_id],
            )
            .await?;
        }
        let expr_src = roll.as_ref().map(|r| r.src.clone());
        let row = tx
            .query_one(
                &format!(
                    "INSERT INTO messages (room_id, seq, author, expr, comment, roll_json, total,
                         created_at, updated_at, updated_by, created_event_seq, event_seq)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $8, $3, $9, $9)
                     RETURNING {MESSAGE_COLS}"
                ),
                &[
                    &room_id,
                    &seq,
                    &author,
                    &expr_src,
                    &comment,
                    &roll_json,
                    &total,
                    &now,
                    &event_seq,
                ],
            )
            .await?;
        let msg = message_from_row(&row);
        tx.commit().await?;
        Ok(Ok(msg))
    }

    async fn edit_message(
        &self,
        room_id: i64,
        seq: i64,
        editor: &str,
        roll: Option<RollInput>,
        comment: &str,
        now: i64,
    ) -> StoreResult<Option<MessageAttempt>> {
        let mut client = self.client.lock().await;
        let tx = client.transaction().await?;
        if get_message_tx(&tx, room_id, seq).await?.is_none() {
            return Ok(None);
        }
        // Editing a roll re-rolls it with fresh randomness (SPEC.md §5).
        let (roll_json, total, next_state) = match eval_roll(&tx, room_id, &roll).await? {
            Ok(cols) => cols,
            Err(e) => return Ok(Some(Err(e))),
        };
        let (_, event_seq) = bump_counters(&tx, room_id, false).await?;
        if let Some(next_state) = next_state {
            tx.execute(
                "UPDATE rooms SET rng_state = $1 WHERE id = $2",
                &[&&next_state[..], &room_id],
            )
            .await?;
        }
        let expr_src = roll.as_ref().map(|r| r.src.clone());
        let row = tx
            .query_one(
                &format!(
                    "UPDATE messages SET expr = $1, comment = $2, roll_json = $3, total = $4,
                         updated_at = $5, updated_by = $6, event_seq = $7
                     WHERE room_id = $8 AND seq = $9
                     RETURNING {MESSAGE_COLS}"
                ),
                &[
                    &expr_src, &comment, &roll_json, &total, &now, &editor, &event_seq, &room_id,
                    &seq,
                ],
            )
            .await?;
        let msg = message_from_row(&row);
        tx.commit().await?;
        Ok(Some(Ok(msg)))
    }

    async fn get_message(&self, room_id: i64, seq: i64) -> StoreResult<Option<Message>> {
        let client = self.client.lock().await;
        let row = client
            .query_opt(
                &format!("SELECT {MESSAGE_COLS} FROM messages WHERE room_id = $1 AND seq = $2"),
                &[&room_id, &seq],
            )
            .await?;
        Ok(row.as_ref().map(message_from_row))
    }

    async fn list_messages(&self, room_id: i64) -> StoreResult<Vec<Message>> {
        let client = self.client.lock().await;
        let rows = client
            .query(
                &format!("SELECT {MESSAGE_COLS} FROM messages WHERE room_id = $1 ORDER BY seq"),
                &[&room_id],
            )
            .await?;
        Ok(rows.iter().map(message_from_row).collect())
    }

    async fn sweep(&self, cutoff: i64) -> StoreResult<u64> {
        let client = self.client.lock().await;
        Ok(client
            .execute("DELETE FROM rooms WHERE created_at < $1", &[&cutoff])
            .await?)
    }

    async fn meta_get(&self, key: &str) -> StoreResult<Option<String>> {
        let client = self.client.lock().await;
        let row = client
            .query_opt("SELECT value FROM meta WHERE key = $1", &[&key])
            .await?;
        Ok(row.map(|r| r.get(0)))
    }

    async fn meta_set(&self, key: &str, value: &str) -> StoreResult<()> {
        let client = self.client.lock().await;
        client
            .execute(
                "INSERT INTO meta (key, value) VALUES ($1, $2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                &[&key, &value],
            )
            .await?;
        Ok(())
    }
}
