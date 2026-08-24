//! Render models and askama templates.
//!
//! Message fragments are rendered once and reused everywhere: full page
//! loads, htmx responses, and SSE event payloads. An SSE fragment for an
//! *edited* message carries `hx-swap-oob` so htmx replaces it in place;
//! new messages append via the container's `hx-swap="beforeend"`.

use std::collections::HashMap;

use askama::Template;

use crate::clock::format_utc;
use crate::store::{Message, MessageKind};

/// Display name for a client id, falling back to a stable anonymous handle.
pub fn display_name(names: &HashMap<String, String>, client_id: &str) -> String {
    match names.get(client_id) {
        Some(name) => name.clone(),
        None => format!("anon-{}", &client_id[..client_id.len().min(4)]),
    }
}

#[derive(Template)]
#[template(path = "index.html")]
pub struct IndexPage;

#[derive(Template)]
#[template(path = "room.html")]
pub struct RoomPage {
    pub token: String,
    pub locked: bool,
    pub created_fmt: String,
    pub locks_at_fmt: String,
    /// Pre-rendered message fragments in seq order.
    pub messages: Vec<String>,
    /// Room event counter covering everything rendered into this page.
    pub snapshot_seq: i64,
    pub name_form: String,
    pub composer: String,
}

#[derive(Template)]
#[template(path = "composer.html")]
pub struct Composer {
    pub token: String,
    pub locked: bool,
    pub error: Option<String>,
    /// Preserved input on error, so nothing typed is lost.
    pub draft: String,
}

impl Composer {
    pub fn fresh(token: &str, locked: bool) -> Self {
        Composer { token: token.into(), locked, error: None, draft: String::new() }
    }

    pub fn with_error(token: &str, error: String, draft: String) -> Self {
        Composer { token: token.into(), locked: false, error: Some(error), draft }
    }
}

#[derive(Template)]
#[template(path = "name_form.html")]
pub struct NameForm {
    pub token: String,
    pub name: String,
    pub saved: bool,
}

#[derive(Template)]
#[template(path = "edit_form.html")]
pub struct EditForm {
    pub token: String,
    pub seq: i64,
    pub is_roll: bool,
    /// Current text body, or the roll expression without the `/roll` prefix.
    pub value: String,
    pub error: Option<String>,
}

#[derive(Template)]
#[template(path = "message.html")]
pub struct MessageView {
    pub token: String,
    pub seq: i64,
    pub author_name: String,
    pub created_fmt: String,
    /// Name of the last editor, when the message has been edited.
    pub edited_by: Option<String>,
    pub body: String,
    pub roll: Option<RollView>,
    /// Render as an out-of-band swap (SSE edit events).
    pub oob: bool,
    /// Hide edit affordances in locked rooms.
    pub locked: bool,
}

pub struct RollView {
    pub expr: String,
    pub total: i64,
    pub parts: Vec<RollPart>,
}

pub struct RollPart {
    /// Operator joining this part to the previous one: "", "+", "-", "*".
    pub op: &'static str,
    /// Canonical term notation, e.g. `4d6kh3`; the value itself for constants.
    pub label: String,
    pub dice: Vec<DieView>,
    pub value: i64,
    pub is_const: bool,
    /// The term counts successes rather than summing faces.
    pub counts_successes: bool,
}

pub struct DieView {
    pub value: i64,
    pub dropped: bool,
    pub exploded: bool,
    pub success: bool,
    pub rerolled: Vec<i64>,
}

impl MessageView {
    pub fn build(
        msg: &Message,
        token: &str,
        names: &HashMap<String, String>,
        locked: bool,
        oob: bool,
    ) -> Self {
        let roll = match msg.kind {
            MessageKind::Text => None,
            MessageKind::Roll => Some(roll_view(msg)),
        };
        MessageView {
            token: token.into(),
            seq: msg.seq,
            author_name: display_name(names, &msg.author),
            created_fmt: format_utc(msg.created_at),
            edited_by: msg.edited().then(|| display_name(names, &msg.updated_by)),
            body: msg.body.clone(),
            roll,
            oob,
            locked,
        }
    }
}

fn roll_view(msg: &Message) -> RollView {
    let total = msg.total.unwrap_or(0);
    let outcome: Option<dice::Outcome> =
        msg.roll_json.as_deref().and_then(|j| serde_json::from_str(j).ok());
    let mut parts = Vec::new();
    if let Some(outcome) = &outcome {
        for (i, (op, terms)) in outcome.products.iter().enumerate() {
            for (j, term) in terms.iter().enumerate() {
                let op_str = match (i, j, op) {
                    (_, 1.., _) => "*",
                    (0, _, dice::AddOp::Add) => "",
                    (0, _, dice::AddOp::Sub) => "-",
                    (_, _, dice::AddOp::Add) => "+",
                    (_, _, dice::AddOp::Sub) => "-",
                };
                parts.push(roll_part(op_str, term));
            }
        }
    }
    RollView { expr: msg.body.clone(), total, parts }
}

fn roll_part(op: &'static str, term: &dice::TermOutcome) -> RollPart {
    match term {
        dice::TermOutcome::Const(n) => RollPart {
            op,
            label: n.to_string(),
            dice: Vec::new(),
            value: *n,
            is_const: true,
            counts_successes: false,
        },
        dice::TermOutcome::Dice { term, dice, value } => RollPart {
            op,
            label: term.to_string(),
            dice: dice
                .iter()
                .map(|d| DieView {
                    value: d.value,
                    dropped: d.dropped,
                    exploded: d.exploded,
                    success: d.success,
                    rerolled: d.rerolled.clone(),
                })
                .collect(),
            value: *value,
            is_const: false,
            counts_successes: term.success.is_some(),
        },
    }
}

/// The locked-room banner that replaces the composer (also sent over SSE
/// when someone locks the room).
pub fn locked_composer(token: &str) -> String {
    Composer::fresh(token, true).render().expect("render composer")
}
