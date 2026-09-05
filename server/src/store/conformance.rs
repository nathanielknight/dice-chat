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

fn roll(src: &str) -> RollInput {
    RollInput::new(src, &dice::parse(src).expect("parse"))
}

/// Post a comment-only message.
async fn post_comment(store: &dyn Store, room_id: i64, author: &str, comment: &str, now: i64) -> Message {
    store
        .post_message(room_id, author, None, comment, now)
        .await
        .expect("store")
        .expect("no roll to fail")
}

pub async fn run(store: &dyn Store) {
    rooms_and_tokens(store).await;
    names(store).await;
    comments_and_edits(store).await;
    rolls_advance_rng_in_transaction(store).await;
    roll_edit_rerolls(store).await;
    rolls_carry_comments(store).await;
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

async fn comments_and_edits(store: &dyn Store) {
    let room = mk_room(store, 1000).await;

    let m1 = post_comment(store, room.id, "client-a", "hello", 1001).await;
    let m2 = post_comment(store, room.id, "client-b", "hi back", 1002).await;
    assert_eq!((m1.seq, m2.seq), (1, 2));
    assert!(!m1.is_roll());
    assert_eq!(m1.comment, "hello");
    assert_eq!(m1.total, None);
    assert_eq!(m1.author, "client-a");
    assert_eq!(m1.updated_by, "client-a");
    assert_eq!((m1.created_at, m1.updated_at), (1001, 1001));
    assert!(!m1.edited());
    assert!(m2.event_seq > m1.event_seq);

    // Anyone in the room may edit any message.
    let edited = store
        .edit_message(room.id, m1.seq, "client-b", None, "hello (fixed)", 1003)
        .await
        .unwrap()
        .expect("message exists")
        .expect("no roll to fail");
    assert_eq!(edited.comment, "hello (fixed)");
    assert_eq!(edited.author, "client-a"); // author unchanged
    assert_eq!(edited.updated_by, "client-b");
    assert_eq!(edited.created_at, 1001);
    assert_eq!(edited.updated_at, 1003);
    assert!(edited.edited());
    assert!(edited.event_seq > m2.event_seq);

    // Listing returns seq order with current comments.
    let all = store.list_messages(room.id).await.unwrap();
    assert_eq!(all.len(), 2);
    assert_eq!(all[0].comment, "hello (fixed)");
    assert_eq!(all[1].comment, "hi back");

    // get_message round-trips; absent seq is None.
    let got = store.get_message(room.id, m2.seq).await.unwrap().unwrap();
    assert_eq!(got.comment, "hi back");
    assert!(store.get_message(room.id, 999).await.unwrap().is_none());

    // An edit can attach a roll to a message that had none.
    let now_a_roll = store
        .edit_message(room.id, m1.seq, "client-a", Some(roll("d6")), "hello (fixed)", 1004)
        .await
        .unwrap()
        .expect("message exists")
        .expect("eval ok");
    assert_eq!(now_a_roll.expr.as_deref(), Some("d6"));
    assert_eq!(now_a_roll.comment, "hello (fixed)");
    assert!((1..=6).contains(&now_a_roll.total.unwrap()));
}

async fn rolls_advance_rng_in_transaction(store: &dyn Store) {
    let room = mk_room(store, 1000).await;

    let m = store
        .post_message(room.id, "client-a", Some(roll("4d6kh3+2")), "", 1001)
        .await
        .unwrap()
        .expect("eval ok");
    assert!(m.is_roll());
    assert_eq!(m.expr.as_deref(), Some("4d6kh3+2"));
    assert_eq!(m.comment, "");
    let outcome: dice::Outcome = serde_json::from_str(m.roll_json.as_deref().unwrap()).unwrap();
    assert_eq!(Some(outcome.value), m.total);
    let total = m.total.unwrap();
    assert!((5..=20).contains(&total), "4d6kh3+2 out of range: {total}");

    // Successive rolls draw fresh randomness: 100 d20 rolls can't all match.
    let mut totals = std::collections::HashSet::new();
    for i in 0..100 {
        let m = store
            .post_message(room.id, "client-a", Some(roll("d20")), "", 1002 + i)
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
    let m = store
        .post_message(room.id, "client-a", Some(roll("d6")), "", 1001)
        .await
        .unwrap()
        .unwrap();

    // Editing a roll re-rolls it — possibly with a different expression.
    let edited = store
        .edit_message(room.id, m.seq, "client-b", Some(roll("2d20+100")), "", 1002)
        .await
        .unwrap()
        .expect("roll exists")
        .expect("eval ok");
    assert_eq!(edited.seq, m.seq);
    assert_eq!(edited.expr.as_deref(), Some("2d20+100"));
    assert_eq!(edited.updated_by, "client-b");
    assert!(edited.edited());
    let t = edited.total.unwrap();
    assert!((102..=140).contains(&t));
    // Prior results are replaced, not kept.
    let stored = store.get_message(room.id, m.seq).await.unwrap().unwrap();
    assert_eq!(stored.total, edited.total);
    assert_eq!(stored.expr.as_deref(), Some("2d20+100"));

    // An edit can drop the roll, leaving a comment-only message.
    let dropped = store
        .edit_message(room.id, m.seq, "client-a", None, "never mind", 1003)
        .await
        .unwrap()
        .expect("message exists")
        .expect("no roll to fail");
    assert!(!dropped.is_roll());
    assert_eq!(dropped.comment, "never mind");
    assert_eq!(dropped.total, None);
    assert!(dropped.roll_json.is_none());

    // Editing a nonexistent message is None.
    assert!(store
        .edit_message(room.id, 999, "client-a", Some(roll("d6")), "", 1004)
        .await
        .unwrap()
        .is_none());
}

/// A message can be a roll, a comment, or both (SPEC.md §5).
async fn rolls_carry_comments(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    let m = store
        .post_message(room.id, "client-a", Some(roll("d20adv+4")), "Attack", 1001)
        .await
        .unwrap()
        .expect("eval ok");
    assert_eq!(m.expr.as_deref(), Some("d20adv+4"));
    assert_eq!(m.comment, "Attack");
    assert!((5..=24).contains(&m.total.unwrap()));

    // The comment survives a re-roll, and can be edited on its own.
    let edited = store
        .edit_message(room.id, m.seq, "client-a", Some(roll("d20dis+4")), "Attack (with cover)", 1002)
        .await
        .unwrap()
        .expect("message exists")
        .expect("eval ok");
    assert_eq!(edited.expr.as_deref(), Some("d20dis+4"));
    assert_eq!(edited.comment, "Attack (with cover)");

    let all = store.list_messages(room.id).await.unwrap();
    assert_eq!(all.len(), 1);
}

async fn failed_roll_persists_nothing(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    // d6r6 parses but can never terminate → eval error inside the tx.
    let attempt = store
        .post_message(room.id, "client-a", Some(roll("d6r6")), "here goes", 1001)
        .await
        .unwrap();
    assert_eq!(attempt.unwrap_err(), dice::EvalError::RerollUnsatisfiable);

    // Nothing persisted: no message, no seq consumed, no event.
    assert!(store.list_messages(room.id).await.unwrap().is_empty());
    let ok = store
        .post_message(room.id, "client-a", Some(roll("d6")), "", 1002)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ok.seq, 1);

    let room = store.room_by_token(&room.token).await.unwrap().unwrap();
    assert_eq!(room.event_counter, ok.event_seq);
}

async fn locking(store: &dyn Store) {
    let room = mk_room(store, 1000).await;
    post_comment(store, room.id, "client-a", "hi", 1001).await;

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
    post_comment(store, old.id, "client-a", "doomed", 101).await;
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
