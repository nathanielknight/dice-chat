# Dice Chat — Specification (v0.1)

A small self-hosted web app for one-off chat rooms with integrated dice
rolling, aimed at casual tabletop play.

## 1. Goals & priorities

- **Ease of deployment**: a single binary plus a SQLite file is a complete
  installation. No external services required.
- **Minimal memory footprint.**
- **Zero-friction access**: rooms are joined via share link; no accounts.
- Postgres supported from the start as an alternative backend.

## 2. Stack

| Concern      | Choice |
|--------------|--------|
| Language     | Rust |
| Web          | axum + tokio |
| Templates    | askama (compile-time checked) |
| Frontend     | Server-rendered HTML + htmx (SSE extension) |
| Static assets| Embedded in the binary via rust-embed |
| Storage      | Trait with SQLite (rusqlite) and Postgres (tokio-postgres) implementations |
| Live updates | Server-sent events; writes via ordinary POSTs |

Backend selection at startup by connection string (`sqlite://path.db` or
`postgres://…`).

## 3. Rooms & access

- Anyone with access to the instance can create a room.
- A room's share link contains an unguessable token (≥128 bits, URL-safe).
  Possession of the link is the only access control.
- Rooms are one-off: intended for a session or campaign, not a permanent
  community.

### 3.1 Lifecycle

- **Lock**: 14 days after creation a room becomes read-only — no new
  messages, edits, or rolls. The link keeps working for reading. Any
  member may also lock the room proactively at any time.
- **Delete**: one year after creation the room and its messages are
  permanently deleted (background sweep).
- **Export**: a transcript export is available at any time, including
  while locked (plain-text and JSON formats).

## 4. Identity

- On first visit a client is issued a signed cookie holding a random client
  id — best-effort persistent identity, no login.
- Each client sets a display name per room (editable at any time). Messages
  render the display name current at render time is **not** required;
  messages store the author's client id and the room stores the id → name
  mapping.
- Losing the cookie means a new identity. That's acceptable.

## 5. Messages

- All clients in a room can view, post, and **edit** messages (any message,
  including others' — casual-play trust model).
- A message carries a **roll**, a **comment**, or both:
  - The **roll** is a dice expression (§8); the message stores the
    expression as typed, structured results, and the rendered total.
  - The **comment** is free text. On a message with a roll it labels the
    roll ("Attack", "Start of combat"); on its own it is plain chat.
  - At least one of the two must be present.
- The composer has a separate input for each, so rolling — the common case
  — takes no prefix or command. There are no chat commands; a leading
  slash is ordinary text.
- Editing replaces the comment, and **re-rolls** any roll the saved message
  has (e.g. to fix a fat-fingered die size): fresh randomness is drawn and
  previous results are replaced. An edit may also add a roll to a message
  that had none, or drop the roll from one that did.
- Messages are ordered by server-assigned sequence per room.
- Every message tracks `created_at`, `updated_at`, and `updated_by`
  (client id). Edited messages show an indicator in the UI (e.g.
  "edited by <name>"); prior versions are not kept.

## 6. Real-time

- Each room fans out over a `tokio::sync::broadcast` channel.
- Clients hold one SSE connection per open room; events carry rendered HTML
  fragments (new message, edited message) that htmx swaps in place.
- SSE auto-reconnect + a `Last-Event-ID`-style catch-up query on reconnect
  cover dropped connections.

## 7. Storage

- A `Store` trait abstracts persistence; SQLite and Postgres each implement
  it. Queries are fully typed per backend — no lowest-common-denominator
  driver.
- A shared conformance test suite runs against both implementations.
- **Room RNG state**: each room persists an opaque RNG state. Every roll
  (including a re-roll on edit) reads the state, produces results, and
  writes the successor state in the same transaction as the message write,
  serializing concurrent rolls. No audit/replay guarantees — this is for
  casual play.

## 8. Dice notation

A documented subset of the de facto Roll20 conventions. No external spec is
normative; this section is.

### 8.1 Grammar

```
expr    := product (("+" | "-") product)*
product := term ("*" term)*
term    := dice | integer
dice    := [count] "d" sides suffix*
sides   := integer | "F" | "%"
suffix  := keep | reroll | explode | success | sugar
keep    := ("kh" | "kl" | "dh" | "dl") integer
reroll  := ("r" | "ro") integer
explode := "!"
success := (">=" | "<=" | ">" | "<") integer
sugar   := "adv" | "dis"
```

- `count` defaults to 1 (`d20` ≡ `1d20`).
- `dF` = Fate/Fudge die (−1, 0, +1). `d%` ≡ `d100`.
- Whitespace is permitted around operators, not within a dice term.

### 8.2 Semantics

- A plain dice term evaluates to the **sum** of its (kept) faces.
- `kh n` / `kl n` keep the n highest/lowest; `dh` / `dl` drop them.
- `r n` rerolls faces ≤ n indefinitely; `ro n` rerolls each such face once.
- `!` explodes: a maximum face is rolled again and added, recursively.
- A success suffix changes the term's value to the **count of faces**
  passing the comparison. Comparisons are exact: `>` and `<` are strict;
  use `>=` / `<=` for inclusive. (Deliberate deviation from Roll20's
  inclusive `>`/`<`.)
- Suffixes apply in fixed order regardless of writing order:
  **reroll → explode → keep/drop → success**. (Matches Roll20/Foundry
  convention of exploding before dropping.)
- `adv` ≡ `kh1` on a doubled pool; `dis` ≡ `kl1`. Only valid on a
  single-die term: `d20adv` ≡ `2d20kh1`.
- A success-counting term participates in arithmetic by its count:
  `8d10>=7 + 2` = successes + 2.

### 8.3 Result rendering

Rolls display the expression, each die face (dropped faces struck through,
exploded faces marked), and the final value. Structured results are stored
as JSON alongside the message.

### 8.4 Limits

- ≤ 100 dice per term, ≤ 10,000 sides, explosion depth ≤ 20,
  ≤ 20 terms per expression.
- Violations are parse/eval errors returned to the poster; nothing is
  persisted.

### 8.5 Errors

The parser reports position-anchored messages (e.g.
`expected die size after 'd' at column 4`) echoed back inline in the
compose area.

## 9. Out of scope (v1)

- Roll20 grouped rolls `{…}` and computed dice counts `(N+2)dX`.
- Accounts, permissions, moderation.
- Audit/replayable randomness.
- File uploads, reactions, threading.

## 10. Open questions

- Pagination: full history on join vs windowed with lazy load.
- Abuse: per-client rate limiting on posts and room creation.
- Name for the project.
