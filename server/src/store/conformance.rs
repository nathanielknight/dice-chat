//! Shared conformance suite run against every `Store` implementation
//! (SPEC.md §7).
//!
//! SQLite runs in-memory on every `cargo test`. Postgres runs when
//! `DICE_CHAT_TEST_POSTGRES` holds a connection string, e.g.
//! `DICE_CHAT_TEST_POSTGRES=postgres://user:pw@localhost/dice_chat_test cargo test`.
//! The Postgres run truncates the store's tables first.

use super::*;
use crate::rng::RoomRng;

async fn mk_room(store: &dyn Store, now: i64) -> Room {
    let token = crate::token::new_token();
    store
        .create_room(&token, now, &RoomRng::fresh_state())
        .await
        .expect("create_room")
}

pub async fn run(store: &dyn Store) {
    rooms_and_tokens(store).await;
    names(store).await;
    text_messages_and_edits(store).await;
    rolls_advance_rng_in_transaction(store).await;
    roll_edit_rerolls(store).await;
    failed_roll_persists_nothing(store).await;
    locking(store).await;
    sweep(store).await;
    meta(store).await;
}

async fn rooms_and_tokens(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    assert_eq!(room.created_at, 1000);
    assert_eq!(room.locked_at, None);
    assert_eq!(room.event_counter, 0);

    let found = store.room_by_token(&room.token).await.unwrap().expect("room by token");
    assert_eq!(found.id, room.id);
    assert_eq!(found.token, room.token);
    assert!(store.room_by_token("no-such-token").await.unwrap().is_none());

    // Tokens are unique.
    assert!(store.create_room(&room.token, 1001, &RoomRng::fresh_state()).await.is_err());
}

async fn names(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    let other = mk_room(store, 1000).await;

    assert!(store.names(room.id).await.unwrap().is_empty());
    store.set_name(room.id, "client-a", "Alice", 1001).await.unwrap();
    store.set_name(room.id, "client-b", "Bob", 1002).await.unwrap();
    store.set_name(other.id, "client-a", "Someone Else", 1003).await.unwrap();

    let names = store.names(room.id).await.unwrap();
    assert_eq!(names.len(), 2);
    assert_eq!(names["client-a"], "Alice");
    assert_eq!(names["client-b"], "Bob");

    // Renaming is editable at any time and scoped per room.
    store.set_name(room.id, "client-a", "Alicia", 1004).await.unwrap();
    assert_eq!(store.names(room.id).await.unwrap()["client-a"], "Alicia");
    assert_eq!(store.names(other.id).await.unwrap()["client-a"], "Someone Else");
}

async fn text_messages_and_edits(store: &dyn Store) {
    let room = mk_room(store, 1000).await;

    let m1 = store.post_text(room.id, "client-a", "hello", 1001).await.unwrap();
    let m2 = store.post_text(room.id, "client-b", "hi back", 1002).await.unwrap();
    assert_eq!((m1.seq, m2.seq), (1, 2));
    assert_eq!(m1.kind, MessageKind::Text);
    assert_eq!(m1.body, "hello");
    assert_eq!(m1.author, "client-a");
    assert_eq!(m1.updated_by, "client-a");
    assert_eq!((m1.created_at, m1.updated_at), (1001, 1001));
    assert!(!m1.edited());
    assert!(m2.event_seq > m1.event_seq);

    // Anyone in the room may edit any message.
    let edited = store
        .edit_text(room.id, m1.seq, "client-b", "hello (fixed)", 1003)
        .await
        .unwrap()
        .expect("message exists");
    assert_eq!(edited.body, "hello (fixed)");
    assert_eq!(edited.author, "client-a"); // author unchanged
    assert_eq!(edited.updated_by, "client-b");
    assert_eq!(edited.created_at, 1001);
    assert_eq!(edited.updated_at, 1003);
    assert!(edited.edited());
    assert!(edited.event_seq > m2.event_seq);

    // Listing returns seq order with current bodies.
    let all = store.list_messages(room.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].body, "hello (fixed)");
    assert_eq!(all[1].body, "hi back");

    // get_message round-trips; absent seq is None.
    let got = store.get_message(room.id, m2.seq).await.unwrap().unwrap();
    assert_eq!(got.body, "hi back");
    assert!(store.get_message(room.id, 999).await.unwrap().is_none());

    // Editing a text message as a roll (or vice versa) is refused.
    let expr = dice::parse("d6").unwrap();
    assert!(store
        .edit_roll(room.id, m1.seq, "client-a", "d6", &expr, 1004)
        .await
        .unwrap()
        .is_none());
}

async fn rolls_advance_rng_in_transaction(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    let expr = dice::parse("4d6kh3+2").unwrap();

    let m = store
        .post_roll(room.id, "client-a", "4d6kh3+2", &expr, 1001)
        .await
        .unwrap()
        .expect("eval ok");
    assert_eq!(m.kind, MessageKind::Roll);
    assert_eq!(m.body, "4d6kh3+2");
    let outcome: dice::Outcome = serde_json::from_str(m.roll_json.as_deref().unwrap()).unwrap();
    assert_eq!(Some(outcome.value), m.total);
    let total = m.total.unwrap();
    assert!((5..=20).contains(&total), "4d6kh3+2 out of range: {total}");

    // Successive rolls draw fresh randomness: 100 d20 rolls can't all match.
    let expr = dice::parse("d20").unwrap();
    let mut totals = std::collections::HashSet::new();
    for i in 0..100 {
        let m = store
            .post_roll(room.id, "client-a", "d20", &expr, 1002 + i)
            .await
            .unwrap()
            .unwrap();
        let t = m.total.unwrap();
        assert!((1..=20).contains(&t));
        totals.insert(t);
    }
    assert!(totals.len() > 1, "RNG state does not advance");
}

async fn roll_edit_rerolls(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    let expr = dice::parse("d6").unwrap();
    let m = store
        .post_roll(room.id, "client-a", "d6", &expr, 1001)
        .await
        .unwrap()
        .unwrap();

    // Editing a roll re-rolls it — possibly with a different expression.
    let expr2 = dice::parse("2d20+100").unwrap();
    let edited = store
        .edit_roll(room.id, m.seq, "client-b", "2d20+100", &expr2, 1002)
        .await
        .unwrap()
        .expect("roll exists")
        .expect("eval ok");
    assert_eq!(edited.seq, m.seq);
    assert_eq!(edited.body, "2d20+100");
    assert_eq!(edited.updated_by, "client-b");
    assert!(edited.edited());
    let t = edited.total.unwrap();
    assert!((102..=140).contains(&t));
    // Prior results are replaced, not kept.
    let stored = store.get_message(room.id, m.seq).await.unwrap().unwrap();
    assert_eq!(stored.total, edited.total);
    assert_eq!(stored.body, "2d20+100");

    // Editing a roll as text is refused.
    assert!(store
        .edit_text(room.id, m.seq, "client-a", "not a roll", 1003)
        .await
        .unwrap()
        .is_none());

    // Editing a nonexistent message is None.
    assert!(store
        .edit_roll(room.id, 999, "client-a", "d6", &expr, 1004)
        .await
        .unwrap()
        .is_none());
}

async fn failed_roll_persists_nothing(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    // d6r6 parses but can never terminate → eval error inside the tx.
    let expr = dice::parse("d6r6").unwrap();
    let attempt = store
        .post_roll(room.id, "client-a", "d6r6", &expr, 1001)
        .await
        .unwrap();
    assert_eq!(attempt.unwrap_err(), dice::EvalError::RerollUnsatisfiable);

    // Nothing persisted: no message, no seq consumed, no event.
    assert!(store.list_messages(room.id).await.unwrap().is_empty());
    let ok = store
        .post_roll(room.id, "client-a", "d6", &dice::parse("d6").unwrap(), 1002)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ok.seq, 1);

    let room = store.room_by_token(&room.token).await.unwrap().unwrap();
    assert_eq!(room.event_counter, ok.event_seq);
}

async fn locking(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    store.post_text(room.id, "client-a", "hi", 1001).await.unwrap();

    let before = store.room_by_token(&room.token).await.unwrap().unwrap();
    let event_seq = store.lock_room(room.id, 2000).await.unwrap();
    assert!(event_seq > before.event_counter);

    let locked = store.room_by_token(&room.token).await.unwrap().unwrap();
    assert_eq!(locked.locked_at, Some(2000));
    assert_eq!(locked.event_counter, event_seq);

    // Idempotent: locked_at keeps its original timestamp.
    store.lock_room(room.id, 3000).await.unwrap();
    let still = store.room_by_token(&room.token).await.unwrap().unwrap();
    assert_eq!(still.locked_at, Some(2000));

    // The link keeps working for reading.
    assert_eq!(store.list_messages(room.id).await.unwrap().len(), 1);
}

async fn sweep(store: &dyn Store) {
    let old = mk_room(store, 100).await;
    let new = mk_room(store, 5000).await;
    store.post_text(old.id, "client-a", "doomed", 101).await.unwrap();
    store.set_name(old.id, "client-a", "Ghost", 101).await.unwrap();

    let n = store.sweep(1000).await.unwrap();
    assert_eq!(n, 1);
    assert!(store.room_by_token(&old.token).await.unwrap().is_none());
    assert!(store.room_by_token(&new.token).await.unwrap().is_some());
    // Messages and members go with the room.
    assert!(store.list_messages(old.id).await.unwrap().is_empty());
    assert!(store.names(old.id).await.unwrap().is_empty());
}

async fn meta(store: &dyn Store) {
    assert_eq!(store.meta_get("cookie_key").await.unwrap(), None);
    store.meta_set("cookie_key", "abc").await.unwrap();
    assert_eq!(store.meta_get("cookie_key").await.unwrap().as_deref(), Some("abc"));
    store.meta_set("cookie_key", "def").await.unwrap();
    assert_eq!(store.meta_get("cookie_key").await.unwrap().as_deref(), Some("def"));
}

#[tokio::test]
async fn sqlite_conformance() {
    let store = super::sqlite::SqliteStore::open(":memory:").unwrap();
    run(&store).await;
}

#[tokio::test]
async fn postgres_conformance() {
    let Ok(conn_str) = std::env::var("DICE_CHAT_TEST_POSTGRES") else {
        eprintln!("DICE_CHAT_TEST_POSTGRES not set; skipping Postgres conformance");
        return;
    };
    let store = super::postgres::PostgresStore::connect(&conn_str)
        .await
        .expect("connect to test postgres");
    {
        let client = store.client.lock().await;
        client
            .batch_execute("TRUNCATE messages, members, rooms, meta RESTART IDENTITY CASCADE")
            .await
            .expect("truncate test tables");
    }
    run(&store).await;
}
