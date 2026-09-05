//! Persistence (SPEC.md §7).
//!
//! A `Store` trait abstracts persistence; SQLite and Postgres each implement
//! it with fully typed queries. Rolls read the room's RNG state, produce
//! results, and write the successor state in the same transaction as the
//! message write, serializing concurrent rolls.

pub mod postgres;
pub mod sqlite;

use std::collections::HashMap;

#[derive(Debug)]
pub struct StoreError(pub String);

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "store error: {}", self.0)
    }
}
impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, Clone)]
pub struct Room {
    pub id: i64,
    pub token: String,
    /// Unix seconds.
    pub created_at: i64,
    /// Set when a member locked the room proactively.
    pub locked_at: Option<i64>,
    /// Monotonic per-room event counter; the SSE stream's event id.
    pub event_counter: i64,
}

/// A message carries a roll, a comment, or both (SPEC.md §5).
#[derive(Debug, Clone)]
pub struct Message {
    pub room_id: i64,
    /// Server-assigned per-room sequence; orders the transcript.
    pub seq: i64,
    /// Client id of the original author.
    pub author: String,
    /// The roll expression as typed, when the message has a roll.
    pub expr: Option<String>,
    /// Free-text comment; empty when the message is a bare roll.
    pub comment: String,
    /// For rolls: structured results (`dice::Outcome`) as JSON.
    pub roll_json: Option<String>,
    /// For rolls: the rendered total.
    pub total: Option<i64>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Client id of the last editor (the author, if never edited).
    pub updated_by: String,
    /// Room event counter value when the message was created / last touched.
    pub created_event_seq: i64,
    pub event_seq: i64,
}

impl Message {
    pub fn edited(&self) -> bool {
        self.event_seq != self.created_event_seq
    }

    pub fn is_roll(&self) -> bool {
        self.expr.is_some()
    }
}

/// A roll to evaluate inside the store transaction: the expression as typed
/// alongside its parsed form.
#[derive(Debug, Clone)]
pub struct RollInput {
    pub src: String,
    pub expr: dice::Expr,
}

impl RollInput {
    pub fn new(src: &str, expr: &dice::Expr) -> Self {
        RollInput { src: src.to_owned(), expr: expr.clone() }
    }
}

/// Outcome of a write whose roll is evaluated inside the transaction.
pub type MessageAttempt = Result<Message, dice::EvalError>;

#[async_trait::async_trait]
pub trait Store: Send + Sync {
    /// Create a room with the given share token and initial RNG state.
    async fn create_room(
        &self,
        token: &str,
        now: i64,
        rng_state: &[u8],
    ) -> StoreResult<Room>;

    async fn room_by_token(&self, token: &str) -> StoreResult<Option<Room>>;

    /// Lock the room (idempotent). Returns the room's new event counter.
    async fn lock_room(&self, room_id: i64, now: i64) -> StoreResult<i64>;

    /// Set the display name this client uses in this room.
    async fn set_name(
        &self,
        room_id: i64,
        client_id: &str,
        name: &str,
        now: i64,
    ) -> StoreResult<()>;

    /// The room's client id → display name mapping.
    async fn names(&self, room_id: i64) -> StoreResult<HashMap<String, String>>;

    /// Append a message. When `roll` is present it is evaluated with the
    /// room's RNG, whose state advances in the same transaction; an eval
    /// error rolls everything back and persists nothing.
    async fn post_message(
        &self,
        room_id: i64,
        author: &str,
        roll: Option<RollInput>,
        comment: &str,
        now: i64,
    ) -> StoreResult<MessageAttempt>;

    /// Replace a message's roll and comment. A roll is re-evaluated with
    /// fresh randomness, whatever the message held before, so an edit can
    /// add, change, or drop a roll. `None` if no such message.
    async fn edit_message(
        &self,
        room_id: i64,
        seq: i64,
        editor: &str,
        roll: Option<RollInput>,
        comment: &str,
        now: i64,
    ) -> StoreResult<Option<MessageAttempt>>;

    async fn get_message(&self, room_id: i64, seq: i64) -> StoreResult<Option<Message>>;

    /// All messages in seq order (full history; rooms are one-off).
    async fn list_messages(&self, room_id: i64) -> StoreResult<Vec<Message>>;

    /// Delete rooms created before `cutoff` (and their messages/members).
    /// Returns how many rooms were deleted.
    async fn sweep(&self, cutoff: i64) -> StoreResult<u64>;

    /// Instance-level key/value metadata (e.g. the cookie signing key).
    async fn meta_get(&self, key: &str) -> StoreResult<Option<String>>;
    async fn meta_set(&self, key: &str, value: &str) -> StoreResult<()>;
}

/// Evaluate a roll against an opaque RNG state, returning the outcome and the
/// successor state. Shared by both backends inside their transactions.
pub(crate) fn eval_with_state(
    state: &[u8],
    expr: &dice::Expr,
) -> StoreResult<Result<(dice::Outcome, [u8; crate::rng::STATE_LEN]), dice::EvalError>> {
    let mut rng = crate::rng::RoomRng::from_state(state)
        .ok_or_else(|| StoreError("corrupt room RNG state".into()))?;
    match expr.eval(&mut rng) {
        Ok(outcome) => Ok(Ok((outcome, rng.state()))),
        Err(e) => Ok(Err(e)),
    }
}

#[cfg(test)]
pub mod conformance;
