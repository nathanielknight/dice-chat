# Dice Chat

One-off chat rooms with integrated dice rolling, for casual tabletop play.
See [SPEC.md](SPEC.md) for the full specification.

A single binary plus a SQLite file is a complete installation. Rooms are
joined via share link — no accounts. Rooms become read-only 14 days after
creation and are deleted after a year.

## Running

```sh
cargo build --release
./target/release/dice-chat                # sqlite://dice-chat.db, 127.0.0.1:8080
```

Configuration is via environment variables:

| Variable       | Default               | Notes |
|----------------|-----------------------|-------|
| `DATABASE_URL` | `sqlite://dice-chat.db` | `sqlite://<path>` or `postgres://user:pw@host/db` |
| `BIND_ADDR`    | `127.0.0.1:8080`      | listen address |

The storage backend is selected by the connection string; the schema is
created automatically on first start. A database written by a version that
still used `/roll` is migrated in place on startup: old text messages become
comments, old rolls keep their expressions.

## Rolling

A room's composer has two inputs: one for a dice roll, one for a comment.
Fill in either or both — a bare roll, a line of chat, or a roll with a note
attached ("Attack" + `d20adv + 4`). Rolling needs no command or prefix.

The notation (SPEC.md §8) is a subset of the de facto Roll20 conventions:

- `2d6+3`, `1d8 + 1d6 + 2`, `2d6*3` — arithmetic (`*` binds tighter)
- `d20adv` / `d20dis` — advantage / disadvantage
- `4d6kh3`, `4d6dl1` — keep/drop highest/lowest
- `10d6!` — exploding dice; `r2` / `ro2` — reroll faces ≤ 2 (indefinitely / once)
- `8d10>=7` — count successes (`>` and `<` are strict)
- `dF` — Fate dice, `d%` — percentile

In the composer's roll box, Up/Down arrows scroll back through your recent
rolls (shell-style; kept per browser, per room).

Editing a message replaces its comment and **re-rolls** any roll it has with
fresh randomness; an edit can also add a roll to a comment, or drop one.
Anyone in the room can edit any message — it's a casual-play trust model.

## Development

```sh
cargo test          # dice crate + storage conformance (SQLite in-memory)
DICE_CHAT_TEST_POSTGRES=postgres://user:pw@localhost/dice_chat_test cargo test
```

The workspace has two crates: `dice` (parser/evaluator, no I/O) and
`server` (axum web app, SQLite/Postgres storage behind a `Store` trait).
Live updates are server-sent events carrying rendered HTML fragments that
htmx swaps in place; static assets (htmx and its SSE extension) are
embedded in the binary.
