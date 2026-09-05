//! HTTP layer: routes, identity cookie, SSE fan-out, exports.

use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::{Arc, Mutex};

use askama::Template;
use axum::extract::{Form, Path, Query, Request, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event as SseEvent, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::{Extension, Router};
use axum_extra::extract::cookie::{Cookie, Key, SameSite, SignedCookieJar};
use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;
use tokio_stream::StreamExt as _;

use crate::clock::{self, format_utc, LOCK_AFTER};
use crate::store::{Message, RollInput, Room, Store, StoreError};
use crate::token::{new_client_id, new_token};
use crate::view::{display_name, Composer, EditForm, IndexPage, MessageView, NameForm, RoomPage};

const CLIENT_COOKIE: &str = "dice_chat_cid";

/// One fan-out event: a rendered HTML fragment plus its position in the
/// room's event sequence (the SSE event id).
#[derive(Clone)]
struct RoomEvent {
    id: i64,
    /// SSE event name: "msg" (new or edited message) or "locked".
    name: &'static str,
    html: String,
}

pub struct App {
    pub store: Arc<dyn Store>,
    pub cookie_key: Key,
    channels: Mutex<HashMap<i64, broadcast::Sender<RoomEvent>>>,
}

/// Local newtype so `Key: FromRef<AppState>` can be implemented here.
#[derive(Clone)]
pub struct AppState(pub Arc<App>);

impl std::ops::Deref for AppState {
    type Target = App;
    fn deref(&self) -> &App {
        &self.0
    }
}

impl AppState {
    pub fn new(store: Arc<dyn Store>, cookie_key: Key) -> AppState {
        AppState(Arc::new(App { store, cookie_key, channels: Mutex::new(HashMap::new()) }))
    }
}

impl App {

    fn channel(&self, room_id: i64) -> broadcast::Sender<RoomEvent> {
        let mut map = self.channels.lock().expect("channel map poisoned");
        map.entry(room_id)
            .or_insert_with(|| broadcast::channel(256).0)
            .clone()
    }

    /// Drop fan-out channels nobody listens to (called from the sweep task).
    pub fn prune_channels(&self) {
        let mut map = self.channels.lock().expect("channel map poisoned");
        map.retain(|_, tx| tx.receiver_count() > 0);
    }

    fn broadcast(&self, room_id: i64, id: i64, name: &'static str, html: String) {
        // A send error just means nobody is connected.
        let _ = self.channel(room_id).send(RoomEvent { id, name, html });
    }
}

impl axum::extract::FromRef<AppState> for Key {
    fn from_ref(state: &AppState) -> Key {
        state.0.cookie_key.clone()
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/rooms", post(create_room))
        .route("/r/{token}", get(room_page))
        .route("/r/{token}/name", post(set_name))
        .route("/r/{token}/messages", post(post_message))
        .route("/r/{token}/messages/{seq}", get(message_fragment).post(edit_message))
        .route("/r/{token}/messages/{seq}/edit", get(edit_form))
        .route("/r/{token}/lock", post(lock_room))
        .route("/r/{token}/events", get(events))
        .route("/r/{token}/export.txt", get(export_txt))
        .route("/r/{token}/export.json", get(export_json))
        .route("/static/{*path}", get(static_asset))
        .layer(axum::middleware::from_fn_with_state(state.clone(), identity))
        .with_state(state)
}

// ---------------------------------------------------------------- errors

pub struct AppError(String);

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        AppError(e.to_string())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        eprintln!("internal error: {}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "something went wrong").into_response()
    }
}

type AppResult<T> = Result<T, AppError>;

// -------------------------------------------------------------- identity

/// Best-effort persistent identity (SPEC.md §4): a signed cookie holding a
/// random client id, issued on first visit.
#[derive(Clone)]
pub struct ClientId(pub String);

async fn identity(
    jar: SignedCookieJar,
    mut req: Request,
    next: Next,
) -> Response {
    let (cid, set_jar) = match jar.get(CLIENT_COOKIE) {
        Some(c) => (c.value().to_string(), None),
        None => {
            let id = new_client_id();
            let cookie = Cookie::build((CLIENT_COOKIE, id.clone()))
                .path("/")
                .http_only(true)
                .same_site(SameSite::Lax)
                .max_age(cookie::time::Duration::days(400))
                .build();
            (id, Some(jar.add(cookie)))
        }
    };
    req.extensions_mut().insert(ClientId(cid));
    let resp = next.run(req).await;
    match set_jar {
        Some(jar) => (jar, resp).into_response(),
        None => resp,
    }
}

// ------------------------------------------------------------ room pages

fn html<T: Template>(t: &T) -> Html<String> {
    Html(t.render().expect("template render"))
}

async fn index() -> Html<String> {
    html(&IndexPage)
}

async fn create_room(State(app): State<AppState>) -> AppResult<Redirect> {
    let token = new_token();
    let room = app
        .store
        .create_room(&token, clock::now(), &crate::rng::RoomRng::fresh_state())
        .await?;
    Ok(Redirect::to(&format!("/r/{}", room.token)))
}

/// Look up a room by token, or 404. Possession of the link is the only
/// access control (SPEC.md §3).
async fn find_room(app: &App, token: &str) -> Result<Room, Response> {
    if !is_plausible_token(token) {
        return Err((StatusCode::NOT_FOUND, "no such room").into_response());
    }
    match app.store.room_by_token(token).await {
        Ok(Some(room)) => Ok(room),
        Ok(None) => Err((StatusCode::NOT_FOUND, "no such room").into_response()),
        Err(e) => Err(AppError::from(e).into_response()),
    }
}

fn is_plausible_token(token: &str) -> bool {
    token.len() <= 64 && token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn room_locked(room: &Room, now: i64) -> bool {
    room.locked_at.is_some() || now >= room.created_at + LOCK_AFTER
}

async fn room_page(
    State(app): State<AppState>,
    Extension(ClientId(cid)): Extension<ClientId>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let now = clock::now();
    let locked = room_locked(&room, now);
    let names = app.store.names(room.id).await.map_err(internal)?;
    let messages = app.store.list_messages(room.id).await.map_err(internal)?;

    let snapshot_seq = messages
        .iter()
        .map(|m| m.event_seq)
        .max()
        .unwrap_or(0)
        .max(room.event_counter);
    let rendered: Vec<String> = messages
        .iter()
        .map(|m| render_message(m, &token, &names, locked, false))
        .collect();

    let my_name = names.get(&cid).cloned().unwrap_or_default();
    let page = RoomPage {
        token: token.clone(),
        locked,
        created_fmt: format_utc(room.created_at),
        locks_at_fmt: format_utc(room.created_at + LOCK_AFTER),
        messages: rendered,
        snapshot_seq,
        name_form: NameForm { token: token.clone(), name: my_name, saved: false }
            .render()
            .expect("template render"),
        composer: Composer::fresh(&token, locked).render().expect("template render"),
    };
    Ok(html(&page).into_response())
}

fn internal(e: StoreError) -> Response {
    AppError::from(e).into_response()
}

fn render_message(
    msg: &Message,
    token: &str,
    names: &HashMap<String, String>,
    locked: bool,
    oob: bool,
) -> String {
    MessageView::build(msg, token, names, locked, oob)
        .render()
        .expect("template render")
}

// ----------------------------------------------------------------- names

#[derive(Deserialize)]
struct NameInput {
    name: String,
}

async fn set_name(
    State(app): State<AppState>,
    Extension(ClientId(cid)): Extension<ClientId>,
    Path(token): Path<String>,
    Form(input): Form<NameInput>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let now = clock::now();
    let name = input.name.trim();
    if room_locked(&room, now) || name.is_empty() || name.chars().count() > 50 {
        let current = app
            .store
            .names(room.id)
            .await
            .map_err(internal)?
            .get(&cid)
            .cloned()
            .unwrap_or_default();
        return Ok(html(&NameForm { token, name: current, saved: false }).into_response());
    }
    app.store.set_name(room.id, &cid, name, now).await.map_err(internal)?;
    Ok(html(&NameForm { token, name: name.to_string(), saved: true }).into_response())
}

// -------------------------------------------------------------- messages

/// The composer (and the edit form) post a roll expression and a comment;
/// either may be blank, but not both.
#[derive(Deserialize)]
struct MessageInput {
    #[serde(default)]
    expr: String,
    #[serde(default)]
    comment: String,
}

impl MessageInput {
    /// Trimmed `(expr, comment)`.
    fn trimmed(&self) -> (&str, &str) {
        (self.expr.trim(), self.comment.trim())
    }
}

/// Parse a roll expression, if there is one, into what the store evaluates.
fn parse_roll(expr: &str) -> Result<Option<RollInput>, String> {
    if expr.is_empty() {
        return Ok(None);
    }
    match dice::parse(expr) {
        Ok(parsed) => Ok(Some(RollInput::new(expr, &parsed))),
        Err(e) => Err(format!("{e}")),
    }
}

async fn post_message(
    State(app): State<AppState>,
    Extension(ClientId(cid)): Extension<ClientId>,
    Path(token): Path<String>,
    Form(input): Form<MessageInput>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let now = clock::now();
    if room_locked(&room, now) {
        return Ok(html(&Composer::fresh(&token, true)).into_response());
    }
    let (expr, comment) = input.trimmed();
    if expr.is_empty() && comment.is_empty() {
        return Ok(html(&Composer::fresh(&token, false)).into_response());
    }

    let roll = match parse_roll(expr) {
        Ok(roll) => roll,
        Err(e) => return Ok(composer_error(&token, e, expr, comment).into_response()),
    };
    let msg = match app
        .store
        .post_message(room.id, &cid, roll, comment, now)
        .await
        .map_err(internal)?
    {
        Ok(msg) => msg,
        Err(e) => return Ok(composer_error(&token, format!("{e}"), expr, comment).into_response()),
    };

    let names = app.store.names(room.id).await.map_err(internal)?;
    let fragment = render_message(&msg, &token, &names, false, false);
    app.broadcast(room.id, msg.event_seq, "msg", fragment);
    Ok(html(&Composer::fresh(&token, false)).into_response())
}

fn composer_error(token: &str, error: String, expr: &str, comment: &str) -> Html<String> {
    html(&Composer::with_error(token, error, expr, comment))
}

async fn message_fragment(
    State(app): State<AppState>,
    Path((token, seq)): Path<(String, i64)>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let locked = room_locked(&room, clock::now());
    let Some(msg) = app.store.get_message(room.id, seq).await.map_err(internal)? else {
        return Err((StatusCode::NOT_FOUND, "no such message").into_response());
    };
    let names = app.store.names(room.id).await.map_err(internal)?;
    Ok(Html(render_message(&msg, &token, &names, locked, false)).into_response())
}

async fn edit_form(
    State(app): State<AppState>,
    Path((token, seq)): Path<(String, i64)>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let Some(msg) = app.store.get_message(room.id, seq).await.map_err(internal)? else {
        return Err((StatusCode::NOT_FOUND, "no such message").into_response());
    };
    let names = app.store.names(room.id).await.map_err(internal)?;
    if room_locked(&room, clock::now()) {
        // The room locked since the page rendered; show the message instead.
        return Ok(Html(render_message(&msg, &token, &names, true, false)).into_response());
    }
    let form = EditForm {
        token,
        seq,
        expr: msg.expr.clone().unwrap_or_default(),
        comment: msg.comment.clone(),
        error: None,
    };
    Ok(html(&form).into_response())
}

async fn edit_message(
    State(app): State<AppState>,
    Extension(ClientId(cid)): Extension<ClientId>,
    Path((token, seq)): Path<(String, i64)>,
    Form(input): Form<MessageInput>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let now = clock::now();
    let names = app.store.names(room.id).await.map_err(internal)?;
    let Some(existing) = app.store.get_message(room.id, seq).await.map_err(internal)? else {
        return Err((StatusCode::NOT_FOUND, "no such message").into_response());
    };
    if room_locked(&room, now) {
        return Ok(Html(render_message(&existing, &token, &names, true, false)).into_response());
    }

    let (expr, comment) = input.trimmed();
    let edit_error = |error: String| {
        html(&EditForm {
            token: token.clone(),
            seq,
            expr: expr.to_string(),
            comment: comment.to_string(),
            error: Some(error),
        })
        .into_response()
    };
    if expr.is_empty() && comment.is_empty() {
        return Ok(edit_error("a message needs a roll or a comment".into()));
    }
    let roll = match parse_roll(expr) {
        Ok(roll) => roll,
        Err(e) => return Ok(edit_error(e)),
    };

    // Editing a roll re-rolls it with fresh randomness (SPEC.md §5).
    let msg = match app
        .store
        .edit_message(room.id, seq, &cid, roll, comment, now)
        .await
        .map_err(internal)?
    {
        Some(Ok(msg)) => msg,
        Some(Err(e)) => return Ok(edit_error(format!("{e}"))),
        None => return Err((StatusCode::NOT_FOUND, "no such message").into_response()),
    };

    // Everyone else gets the edit as an out-of-band swap; the editor gets it
    // as the direct response (the duplicate swap is idempotent).
    let oob = render_message(&msg, &token, &names, false, true);
    app.broadcast(room.id, msg.event_seq, "msg", oob);
    Ok(Html(render_message(&msg, &token, &names, false, false)).into_response())
}

// ---------------------------------------------------------------- locking

async fn lock_room(
    State(app): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let event_seq = app.store.lock_room(room.id, clock::now()).await.map_err(internal)?;
    app.broadcast(room.id, event_seq, "locked", crate::view::locked_composer(&token));
    Ok(Html(
        r#"<span class="locked-badge" title="No new messages, edits, or rolls">read-only</span>"#,
    )
    .into_response())
}

// -------------------------------------------------------------------- SSE

#[derive(Deserialize)]
struct EventsQuery {
    last: Option<i64>,
}

/// Live updates (SPEC.md §6): rendered HTML fragments over SSE. On connect
/// (and on reconnect, via `Last-Event-ID`) clients behind the room's event
/// counter get one "refresh" snapshot, then live events.
async fn events(
    State(app): State<AppState>,
    Path(token): Path<String>,
    Query(query): Query<EventsQuery>,
    headers: HeaderMap,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let last = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok())
        .or(query.last)
        .unwrap_or(0);

    let rx = app.channel(room.id).subscribe();

    // Snapshot strictly after subscribing, so nothing falls in the gap:
    // anything newer than the snapshot arrives on the channel.
    let room = find_room(&app, &token).await?;
    let now = clock::now();
    let locked = room_locked(&room, now);
    let messages = app.store.list_messages(room.id).await.map_err(internal)?;
    let snapshot_seq = messages
        .iter()
        .map(|m| m.event_seq)
        .max()
        .unwrap_or(0)
        .max(room.event_counter);

    let mut initial = Vec::new();
    if last < snapshot_seq {
        let names = app.store.names(room.id).await.map_err(internal)?;
        let mut html = String::new();
        for msg in &messages {
            html.push_str(&render_message(msg, &token, &names, locked, false));
        }
        initial.push(Ok::<_, Infallible>(
            SseEvent::default()
                .event("refresh")
                .id(snapshot_seq.to_string())
                .data(html),
        ));
    }

    let live = tokio_stream::wrappers::BroadcastStream::new(rx)
        // A lagged receiver has missed events; end the stream so the
        // browser reconnects and catches up via the snapshot.
        .take_while(|r| r.is_ok())
        .filter_map(move |r| {
            let ev = r.expect("checked by take_while");
            if ev.name == "msg" && ev.id <= snapshot_seq {
                return None; // already covered by the snapshot
            }
            Some(Ok(SseEvent::default()
                .event(ev.name)
                .id(ev.id.to_string())
                .data(ev.html)))
        });

    let stream = tokio_stream::iter(initial).chain(live);
    Ok(Sse::new(stream).keep_alive(KeepAlive::default()).into_response())
}

// ---------------------------------------------------------------- exports

#[derive(Serialize)]
struct ExportRoom<'a> {
    token: &'a str,
    created_at: i64,
    locked: bool,
    names: &'a HashMap<String, String>,
    messages: Vec<ExportMessage<'a>>,
}

#[derive(Serialize)]
struct ExportMessage<'a> {
    seq: i64,
    author: &'a str,
    author_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    expr: Option<&'a str>,
    comment: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    total: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    roll: Option<serde_json::Value>,
    created_at: i64,
    updated_at: i64,
    updated_by: &'a str,
    edited: bool,
}

async fn export_json(
    State(app): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let names = app.store.names(room.id).await.map_err(internal)?;
    let messages = app.store.list_messages(room.id).await.map_err(internal)?;
    let export = ExportRoom {
        token: &room.token,
        created_at: room.created_at,
        locked: room_locked(&room, clock::now()),
        names: &names,
        messages: messages
            .iter()
            .map(|m| ExportMessage {
                seq: m.seq,
                author: &m.author,
                author_name: display_name(&names, &m.author),
                expr: m.expr.as_deref(),
                comment: &m.comment,
                total: m.total,
                roll: m.roll_json.as_deref().and_then(|j| serde_json::from_str(j).ok()),
                created_at: m.created_at,
                updated_at: m.updated_at,
                updated_by: &m.updated_by,
                edited: m.edited(),
            })
            .collect(),
    };
    let body = serde_json::to_string_pretty(&export).map_err(|e| AppError(e.to_string()).into_response())?;
    Ok(([(header::CONTENT_TYPE, "application/json")], body).into_response())
}

async fn export_txt(
    State(app): State<AppState>,
    Path(token): Path<String>,
) -> Result<Response, Response> {
    let room = find_room(&app, &token).await?;
    let names = app.store.names(room.id).await.map_err(internal)?;
    let messages = app.store.list_messages(room.id).await.map_err(internal)?;

    let mut out = format!(
        "# Dice Chat transcript — room {}\n# created {}\n\n",
        room.token,
        format_utc(room.created_at)
    );
    for m in &messages {
        let who = display_name(&names, &m.author);
        let when = format_utc(m.created_at);
        match &m.expr {
            None => out.push_str(&format!("[{when}] {who}: {}", m.comment)),
            Some(expr) => {
                let faces = m
                    .roll_json
                    .as_deref()
                    .and_then(|j| serde_json::from_str::<dice::Outcome>(j).ok())
                    .map(|o| outcome_txt(&o))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "[{when}] {who} rolled {expr}:{faces} = {}",
                    m.total.unwrap_or(0)
                ));
                if !m.comment.is_empty() {
                    out.push_str(&format!(" — {}", m.comment));
                }
            }
        }
        if m.edited() {
            out.push_str(&format!(" (edited by {})", display_name(&names, &m.updated_by)));
        }
        out.push('\n');
    }
    Ok(([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], out).into_response())
}

/// Plain-text face detail: dropped faces in ~tildes~, explosions marked `!`.
fn outcome_txt(outcome: &dice::Outcome) -> String {
    let mut out = String::new();
    for (_, terms) in &outcome.products {
        for term in terms {
            if let dice::TermOutcome::Dice { dice, .. } = term {
                let faces: Vec<String> = dice
                    .iter()
                    .map(|d| {
                        let mut s = d.value.to_string();
                        if d.exploded {
                            s.push('!');
                        }
                        if d.dropped {
                            s = format!("~{s}~");
                        }
                        s
                    })
                    .collect();
                out.push_str(&format!(" [{}]", faces.join(" ")));
            }
        }
    }
    out
}

// ----------------------------------------------------------------- static

#[derive(rust_embed::Embed)]
#[folder = "static"]
struct Assets;

async fn static_asset(Path(path): Path<String>) -> Response {
    let Some(file) = Assets::get(&path) else {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    };
    let mime = match path.rsplit('.').next() {
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    (
        [
            (header::CONTENT_TYPE, mime),
            (header::CACHE_CONTROL, "public, max-age=3600"),
        ],
        file.data,
    )
        .into_response()
}
