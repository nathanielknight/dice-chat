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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageKind {
    Text,
    Roll,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub room_id: i64,
    /// Server-assigned per-room sequence; orders the transcript.
    pub seq: i64,
    /// Client id of the original author.
    pub author: String,
    pub kind: MessageKind,
    /// Text body, or the roll expression as typed.
    pub body: String,
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
}

/// Outcome of a roll attempted inside a store transaction.
pub type RollAttempt = Result<Message, dice::EvalError>;

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

    async fn post_text(
        &self,
        room_id: i64,
        author: &str,
        body: &str,
        now: i64,
    ) -> StoreResult<Message>;

    /// Roll `expr` with the room's RNG and append the result; RNG state
    /// advances in the same transaction. An eval error rolls everything back.
    async fn post_roll(
        &self,
        room_id: i64,
        author: &str,
        expr_src: &str,
        expr: &dice::Expr,
        now: i64,
    ) -> StoreResult<RollAttempt>;

    /// Replace a text message's body. `None` if no such text message.
    async fn edit_text(
        &self,
        room_id: i64,
        seq: i64,
        editor: &str,
        body: &str,
        now: i64,
    ) -> StoreResult<Option<Message>>;

    /// Re-roll a roll message with a (possibly new) expression, drawing fresh
    /// randomness from the room RNG. `None` if no such roll message.
    async fn edit_roll(
        &self,
        room_id: i64,
        seq: i64,
        editor: &str,
        expr_src: &str,
        expr: &dice::Expr,
        now: i64,
    ) -> StoreResult<Option<RollAttempt>>;

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
