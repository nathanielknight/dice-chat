//! SQLite backend (rusqlite, bundled).
//!
//! One connection behind a mutex, driven from `spawn_blocking`. Plenty for
//! the intended scale, and it makes SQLite's serialization trivial.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior};

use super::{
    eval_with_state, Message, MessageAttempt, Room, RollInput, Store, StoreError, StoreResult,
};

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError(format!("sqlite: {e}"))
    }
}

pub struct SqliteStore {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS rooms (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    token         TEXT NOT NULL UNIQUE,
    created_at    INTEGER NOT NULL,
    locked_at     INTEGER,
    rng_state     BLOB NOT NULL,
    seq_counter   INTEGER NOT NULL DEFAULT 0,
    event_counter INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE TABLE IF NOT EXISTS members (
    room_id    INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    client_id  TEXT NOT NULL,
    name       TEXT NOT NULL,
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (room_id, client_id)
) STRICT;
CREATE TABLE IF NOT EXISTS messages (
    room_id           INTEGER NOT NULL REFERENCES rooms(id) ON DELETE CASCADE,
    seq               INTEGER NOT NULL,
    author            TEXT NOT NULL,
    expr              TEXT,
    comment           TEXT NOT NULL,
    roll_json         TEXT,
    total             INTEGER,
    created_at        INTEGER NOT NULL,
    updated_at        INTEGER NOT NULL,
    updated_by        TEXT NOT NULL,
    created_event_seq INTEGER NOT NULL,
    event_seq         INTEGER NOT NULL,
    PRIMARY KEY (room_id, seq)
) STRICT;
";

impl SqliteStore {
    /// Open (creating if needed) the database at `path`; `:memory:` works.
    pub fn open(path: &str) -> StoreResult<Self> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL").ok(); // no-op in memory
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.execute_batch(SCHEMA)?;
        migrate(&conn)?;
        Ok(SqliteStore { conn: Arc::new(Mutex::new(conn)) })
    }

    async fn call<T, F>(&self, f: F) -> StoreResult<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> StoreResult<T> + Send + 'static,
    {
        let conn = Arc::clone(&self.conn);
        tokio::task::spawn_blocking(move || {
            let mut guard = conn.lock().expect("sqlite mutex poisoned");
            f(&mut guard)
        })
        .await
        .map_err(|e| StoreError(format!("sqlite task join: {e}")))?
    }
}

/// Bring a pre-`expr`/`comment` database (messages as `kind` + `body`)
/// forward: rolls keep their expression, text messages become comments.
fn migrate(conn: &Connection) -> StoreResult<()> {
    let legacy: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('messages') WHERE name = 'kind'",
        [],
        |row| row.get(0),
    )?;
    if legacy == 0 {
        return Ok(());
    }
    conn.execute_batch(
        "BEGIN;
         ALTER TABLE messages ADD COLUMN expr TEXT;
         ALTER TABLE messages ADD COLUMN comment TEXT NOT NULL DEFAULT '';
         UPDATE messages SET expr = body WHERE kind = 'roll';
         UPDATE messages SET comment = body WHERE kind = 'text';
         ALTER TABLE messages DROP COLUMN kind;
         ALTER TABLE messages DROP COLUMN body;
         COMMIT;",
    )?;
    Ok(())
}

fn room_from_row(row: &Row<'_>) -> rusqlite::Result<Room> {
    Ok(Room {
        id: row.get(0)?,
        token: row.get(1)?,
        created_at: row.get(2)?,
        locked_at: row.get(3)?,
        event_counter: row.get(4)?,
    })
}

fn message_from_row(row: &Row<'_>) -> rusqlite::Result<Message> {
    Ok(Message {
        room_id: row.get(0)?,
        seq: row.get(1)?,
        author: row.get(2)?,
        expr: row.get(3)?,
        comment: row.get(4)?,
        roll_json: row.get(5)?,
        total: row.get(6)?,
        created_at: row.get(7)?,
        updated_at: row.get(8)?,
        updated_by: row.get(9)?,
        created_event_seq: row.get(10)?,
        event_seq: row.get(11)?,
    })
}

const MESSAGE_COLS: &str = "room_id, seq, author, expr, comment, roll_json, total, \
     created_at, updated_at, updated_by, created_event_seq, event_seq";

/// Evaluate a message's roll (if any) against the room's RNG, returning the
/// columns to write and the successor RNG state. `Ok(Err(_))` is an eval
/// error: the caller drops the transaction, persisting nothing.
type RollColumns = (Option<String>, Option<i64>, Option<[u8; crate::rng::STATE_LEN]>);

fn eval_roll(
    conn: &Connection,
    room_id: i64,
    roll: &Option<RollInput>,
) -> StoreResult<Result<RollColumns, dice::EvalError>> {
    let Some(roll) = roll else {
        return Ok(Ok((None, None, None)));
    };
    let state: Vec<u8> = conn.query_row(
        "SELECT rng_state FROM rooms WHERE id = ?1",
        [room_id],
        |row| row.get(0),
    )?;
    let (outcome, next_state) = match eval_with_state(&state, &roll.expr)? {
        Ok(ok) => ok,
        Err(e) => return Ok(Err(e)),
    };
    let roll_json = serde_json::to_string(&outcome)
        .map_err(|e| StoreError(format!("serialize outcome: {e}")))?;
    Ok(Ok((Some(roll_json), Some(outcome.value), Some(next_state))))
}

/// Bump the room's counters, returning `(next_seq, next_event_seq)`.
fn bump_counters(
    conn: &Connection,
    room_id: i64,
    new_message: bool,
) -> StoreResult<(i64, i64)> {
    let seq_incr = i64::from(new_message);
    let out = conn
        .query_row(
            "UPDATE rooms SET seq_counter = seq_counter + ?1,
                              event_counter = event_counter + 1
             WHERE id = ?2
             RETURNING seq_counter, event_counter",
            (seq_incr, room_id),
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError(format!("no room {room_id}")))?;
    Ok(out)
}

fn get_message_tx(conn: &Connection, room_id: i64, seq: i64) -> StoreResult<Option<Message>> {
    Ok(conn
        .query_row(
            &format!("SELECT {MESSAGE_COLS} FROM messages WHERE room_id = ?1 AND seq = ?2"),
            (room_id, seq),
            message_from_row,
        )
        .optional()?)
}

#[async_trait::async_trait]
impl Store for SqliteStore {
    async fn create_room(&self, token: &str, now: i64, rng_state: &[u8]) -> StoreResult<Room> {
        let token = token.to_owned();
        let rng_state = rng_state.to_owned();
        self.call(move |conn| {
            Ok(conn.query_row(
                "INSERT INTO rooms (token, created_at, rng_state) VALUES (?1, ?2, ?3)
                 RETURNING id, token, created_at, locked_at, event_counter",
                (&token, now, &rng_state),
                room_from_row,
            )?)
        })
        .await
    }

    async fn room_by_token(&self, token: &str) -> StoreResult<Option<Room>> {
        let token = token.to_owned();
        self.call(move |conn| {
            Ok(conn
                .query_row(
                    "SELECT id, token, created_at, locked_at, event_counter
                     FROM rooms WHERE token = ?1",
                    [&token],
                    room_from_row,
                )
                .optional()?)
        })
        .await
    }

    async fn lock_room(&self, room_id: i64, now: i64) -> StoreResult<i64> {
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            tx.execute(
                "UPDATE rooms SET locked_at = ?1 WHERE id = ?2 AND locked_at IS NULL",
                (now, room_id),
            )?;
            let (_, event_seq) = bump_counters(&tx, room_id, false)?;
            tx.commit()?;
            Ok(event_seq)
        })
        .await
    }

    async fn set_name(&self, room_id: i64, client_id: &str, name: &str, now: i64) -> StoreResult<()> {
        let (client_id, name) = (client_id.to_owned(), name.to_owned());
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO members (room_id, client_id, name, updated_at)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (room_id, client_id)
                 DO UPDATE SET name = excluded.name, updated_at = excluded.updated_at",
                (room_id, &client_id, &name, now),
            )?;
            Ok(())
        })
        .await
    }

    async fn names(&self, room_id: i64) -> StoreResult<HashMap<String, String>> {
        self.call(move |conn| {
            let mut stmt =
                conn.prepare("SELECT client_id, name FROM members WHERE room_id = ?1")?;
            let map = stmt
                .query_map([room_id], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<HashMap<_, _>, _>>()?;
            Ok(map)
        })
        .await
    }

    async fn post_message(
        &self,
        room_id: i64,
        author: &str,
        roll: Option<RollInput>,
        comment: &str,
        now: i64,
    ) -> StoreResult<MessageAttempt> {
        let (author, comment) = (author.to_owned(), comment.to_owned());
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            let (roll_json, total, next_state) = match eval_roll(&tx, room_id, &roll)? {
                Ok(cols) => cols,
                Err(e) => return Ok(Err(e)), // tx dropped → rollback
            };
            let (seq, event_seq) = bump_counters(&tx, room_id, true)?;
            if let Some(next_state) = next_state {
                tx.execute(
                    "UPDATE rooms SET rng_state = ?1 WHERE id = ?2",
                    (&next_state[..], room_id),
                )?;
            }
            tx.execute(
                "INSERT INTO messages (room_id, seq, author, expr, comment, roll_json, total,
                     created_at, updated_at, updated_by, created_event_seq, event_seq)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?3, ?9, ?9)",
                (
                    room_id,
                    seq,
                    &author,
                    roll.as_ref().map(|r| &r.src),
                    &comment,
                    &roll_json,
                    total,
                    now,
                    event_seq,
                ),
            )?;
            let msg = get_message_tx(&tx, room_id, seq)?
                .ok_or_else(|| StoreError("inserted message vanished".into()))?;
            tx.commit()?;
            Ok(Ok(msg))
        })
        .await
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
        let (editor, comment) = (editor.to_owned(), comment.to_owned());
        self.call(move |conn| {
            let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
            if get_message_tx(&tx, room_id, seq)?.is_none() {
                return Ok(None);
            }
            // Editing a roll re-rolls it with fresh randomness (SPEC.md §5).
            let (roll_json, total, next_state) = match eval_roll(&tx, room_id, &roll)? {
                Ok(cols) => cols,
                Err(e) => return Ok(Some(Err(e))),
            };
            let (_, event_seq) = bump_counters(&tx, room_id, false)?;
            if let Some(next_state) = next_state {
                tx.execute(
                    "UPDATE rooms SET rng_state = ?1 WHERE id = ?2",
                    (&next_state[..], room_id),
                )?;
            }
            tx.execute(
                "UPDATE messages SET expr = ?1, comment = ?2, roll_json = ?3, total = ?4,
                     updated_at = ?5, updated_by = ?6, event_seq = ?7
                 WHERE room_id = ?8 AND seq = ?9",
                (
                    roll.as_ref().map(|r| &r.src),
                    &comment,
                    &roll_json,
                    total,
                    now,
                    &editor,
                    event_seq,
                    room_id,
                    seq,
                ),
            )?;
            let msg = get_message_tx(&tx, room_id, seq)?
                .ok_or_else(|| StoreError("edited message vanished".into()))?;
            tx.commit()?;
            Ok(Some(Ok(msg)))
        })
        .await
    }

    async fn get_message(&self, room_id: i64, seq: i64) -> StoreResult<Option<Message>> {
        self.call(move |conn| get_message_tx(conn, room_id, seq)).await
    }

    async fn list_messages(&self, room_id: i64) -> StoreResult<Vec<Message>> {
        self.call(move |conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {MESSAGE_COLS} FROM messages WHERE room_id = ?1 ORDER BY seq"
            ))?;
            let msgs = stmt
                .query_map([room_id], message_from_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(msgs)
        })
        .await
    }

    async fn sweep(&self, cutoff: i64) -> StoreResult<u64> {
        self.call(move |conn| {
            let n = conn.execute("DELETE FROM rooms WHERE created_at < ?1", [cutoff])?;
            Ok(n as u64)
        })
        .await
    }

    async fn meta_get(&self, key: &str) -> StoreResult<Option<String>> {
        let key = key.to_owned();
        self.call(move |conn| {
            Ok(conn
                .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| row.get(0))
                .optional()?)
        })
        .await
    }

    async fn meta_set(&self, key: &str, value: &str) -> StoreResult<()> {
        let (key, value) = (key.to_owned(), value.to_owned());
        self.call(move |conn| {
            conn.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
                (&key, &value),
            )?;
            Ok(())
        })
        .await
    }
}
