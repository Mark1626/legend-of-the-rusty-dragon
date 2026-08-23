# CLAUDE.md

## Project overview

A Rust port of [Legend of the Pink Dragon](https://gitlab.com/baruchel/pink-dragon),
an IRC hack'n'slash, moved onto a serverless model (Vercel + Neon Postgres).

The reference is a long-lived Python process: a self-rescheduling timer drives
the world and output is pushed at an IRC socket as it happens. Neither
assumption survives hosting where nothing outlives a request, so the port
restructures both.

Reference project: `/Users/nimalanm/Documents/opensource/personal/dragon-ref/pink-dragon`
(needed by `tools/gen-assets`; not needed to build or run.)

## Build & run

```bash
cargo test --workspace          # database tests skip without TEST_DATABASE_URL
cargo clippy --workspace --all-targets
cargo build --release --bin index      # what Vercel builds
```

Running locally needs Postgres. **Ask before starting Docker/Colima or any
service** — write the code, gate the tests, then ask.

```bash
docker run -d --name dragon-pg \
  -e POSTGRES_USER=dragon -e POSTGRES_PASSWORD=dragon -e POSTGRES_DB=dragon \
  -p 55432:5432 postgres:16-alpine

export DATABASE_URL="postgres://dragon:dragon@localhost:55432/dragon?sslmode=disable"
export INVITE_KEY="local-dev"          # without this nobody can sign up
export STATIC_DIR=public               # serve the client too
cargo run -p dragon-api --bin dragon-server
```

Pacing is measured, not guessed — sweep it without a rebuild:

```bash
cargo run --release -p dragon-core --example simulate -- \
  --pacing web --players 12 --days 14 [--tick S --idle-base S --idle-coeff N ...]
```

## Layout

| | |
|---|---|
| `crates/dragon-core` | the whole game. No I/O, no async, no clock. |
| `crates/dragon-api` | Postgres, HTTP, tokens. Knows no game rules. |
| `api/index.rs` | Vercel entry point; the root package is a shim around it |
| `public/` | the web client — no build step, three files |
| `tools/gen-assets` | extracts game data out of the reference Python |
| `attic/pre-port` | an early sketch, kept for reference. Not built. |

### dragon-core

- `step.rs` — **the only entry point.** `step(&mut GameState, Input, now) -> Turn`, plus the catch-up replay
- `state.rs` — `GameState`: everything persisted, in one serialisable value
- `config.rs` — `Pacing`; `Pacing::IRC` is the original cadence, `Pacing::WEB` the retune
- `battle.rs` — D&D5 combat resolver, bounded
- `event.rs` — all 26 random events
- `quest.rs` `bboard.rs` `shop.rs` `user.rs` — the subsystems
- `out.rs` — `Line`/`Span`: structured output replacing IRC control codes
- `rng.rs` — hand-rolled xoshiro256++; `numeric.rs` — `bit_length`, `ability_modifier`, `Cr`
- `assets.rs` — 325 monsters and 40 items, `include_str!`-compiled

### dragon-api

- `store.rs` — the locked turn, feed append, history reads, pool configuration
- `routes.rs` — nine endpoints; `auth.rs` — tokens and the invitation gate
- `config.rs` — environment settings; `error.rs` — one error type rendered as JSON

## Architecture

```
request ──> load the single `game` row (SELECT … FOR UPDATE)
            │
            ├─ settle the clock: replay owed ticks at their virtual times
            ├─ dragon_core::step(&mut state, input, now)
            │
            ├─ write the row back        ┐
            ├─ append Out::feed to `feed`│ one transaction
            └─ commit                    ┘
                                          
Out::feed  ──> the `feed` table, polled by every client
Out::reply ──> the HTTP response body of the caller who acted
```

Three things replaced their IRC equivalents:

- **`broadcast()` became an append-only feed.** Clients poll `/api/feed?since=`.
- **`reply()` became the HTTP response.** Private replies are never persisted.
- **The timer became lazy ticking.** No background process: each request first
  settles whatever the clock owes, replaying missed ticks *dated to the moments
  they would have happened*, capped by `Pacing::max_catchup_ticks`.

## Invariants — these are load-bearing

**`dragon-core` stays pure.** No I/O, no async, no `SystemTime`, no trait
objects for output. Time and randomness arrive as arguments or live in
`GameState`. This is what makes a turn reproducible and the hosting layer thin;
adding `async` to the core would unravel the whole design.

**Durations are wall-clock seconds, never tick counts.** The reference purges at
896 ticks and runs event cooldowns at 16/24/32/64/256 ticks, which is safe only
while the tick length never moves. It moved. A new cooldown expressed in ticks
would silently rescale when `TICK_SECS` is retuned. Only genuine *cycle
positions* (the store's 32-step restock rhythm) stay counters.

**The RNG is hand-rolled on purpose.** `rng_seed` is persisted in `GameState`, so
an in-flight game must keep producing the same stream across dependency
upgrades. Do not replace `GameRng` with `rand`.

**`ability_modifier` floors, deliberately.** `floor((score - 10) / 2)` is the
published D&D table — 1 → −5, 8-9 → −1. Rust's `/` truncates and would misprice
every odd score below 10; 36 of 325 monsters sit in that range. A test asserts
the two disagree so the choice stays pinned.

**Feed inserts stay chunked.** Postgres encodes a statement's bind-parameter
count as an `int16`, so 65535 is the ceiling — 16383 lines at four parameters
each. Exceeding it fails *inside* the transaction, rolls the turn back, and the
next request recomputes the identical catch-up and fails identically. Nothing
ever commits again, and no API call can repair it. `tests/large_turns.rs` guards
this.

**Turns are globally serialized** on one row lock. Keep work inside a turn
bounded; never add an unbounded loop or a network call. Reads must not take the
lock — `/api/feed` never does, and `/api/state` escalates only when the clock
genuinely owes a tick.

**Assets are generated.** Do not hand-edit `crates/dragon-core/assets/*.json`;
change `tools/gen-assets/gen_assets.py` and regenerate:

```bash
uv run tools/gen-assets/gen_assets.py --ref ../../dragon-ref/pink-dragon/pink_dragon
```

Use `uv` for Python, never `pip`.

**The client never uses `innerHTML`.** Every node is built with `textContent`.
The API's XSS guarantee rests on one server-side validator (`validate_nick`);
if that ever relaxes to allow Unicode display names, a string-concatenating
client would go from safe to stored-XSS everywhere at once.

**Never hand-write crypto.** Use a vetted crate and pin it if stored data
depends on stability. `sha2 = "=0.11.0"` is pinned because token digests are
stored; a published test vector guards the algorithm.

## Security posture

Defaults fail **closed**, and should stay that way:

| | when unset |
|---|---|
| `INVITE_KEY` | nobody can sign up — never that anybody can |
| `ADMIN_SECRET` | `/api/admin` refused outright |
| `CRON_SECRET` | `/api/tick` unauthenticated, which is *safe* — a heartbeat that finds no tick owed takes no lock and writes nothing |

Signup attempts are rate-limited per caller and **failures count** — a wrong
key, a taken name and a malformed name all spend budget. That is what bounds a
leaked invitation key and closes the name-enumeration oracle. Locally every
request shares the `ip:unattributed` bucket (no proxy header), so 10 signups
exhaust the default; raise `JOIN_ATTEMPTS_PER_WINDOW` for development.

A `DATABASE_URL` with no `sslmode` gets `verify-full` appended. sqlx defaults to
`prefer` and installs a verifier that accepts *any* certificate for every mode
below `verify-ca`, so Neon's suggested `sslmode=require` encrypts without
authenticating the server.

`vercel.json` sets `outputDirectory: public`. Without it Vercel serves the
*repository root* statically — with no framework preset the output directory is
`public` if it exists and `.` otherwise.

## Testing

```bash
cargo test --workspace                 # 306
export TEST_DATABASE_URL="postgres://dragon:dragon@localhost:55432/dragon"
cargo test --workspace                 # 306, and 51 of them actually run
```

**A green run without `TEST_DATABASE_URL` proves less than it looks.** The 51
database tests return early when the variable is absent and are counted as
*passing*, so the total is 306 either way. Any change touching SQL, the turn
transaction, or an endpoint has to be run against a real Postgres to have been
tested at all.

| | |
|---|---|
| `dragon-core` unit | 219 — the game rules |
| `dragon-api` unit | 36 — config, auth, error mapping, line rendering |
| `tests/store.rs` | 16 — persistence, feed paging, concurrency |
| `tests/http.rs` | 29 — every endpoint, status codes, the invitation gate |
| `tests/contention.rs` | 5 — a stranded lock, the tick-boundary herd |
| `tests/large_turns.rs` | 1 — the bind-parameter ceiling |

Database tests gate on `TEST_DATABASE_URL` and **skip cleanly** without it, so
`cargo test` stays useful on a machine with no Postgres. Each runs in its own
schema, so they are parallel-safe and never touch a real deployment.

## Deliberate departures from the reference

Do not "fix" these back — each is commented at its site.

- **Event 24 (stealing) never ran.** `event.py:444` reads `u.dexterity`, unbound
  on that branch — an `UnboundLocalError` every time it was drawn. Ported as the
  thief's dexterity. A player also can no longer pick their own pocket.
- **The battle loop is bounded** — the reference's own standing TODO. Capped,
  with deadlock detection ahead of it. `battle`'s `return warriors[0]` on an
  empty roster is a `None` rather than an `IndexError`.
- **Ability-check tie-breaks are capped.**
- **The periodic board and store dumps left the feed.** IRC had to broadcast the
  whole board because a channel cannot be asked; `GET /api/state` can. The feed
  keeps the news, drops the recitals.
- **Alignment/size compatibility asserts full pair coverage** rather than
  reproducing the reference's fall-through on an unmatched pair.

## Known unfixed

From the security review, in rough priority order:

- The RNG seed is *disclosed*: it equals `started_at` when `GAME_SEED` is unset,
  and `/api/state` publishes uptime. Fix: seed from the CSPRNG.
- `/api/state` returns every player unpaginated.
- `/api/quest` reflects unbounded raw input into its error message.
- No `DefaultBodyLimit`; axum's 2 MB default is far larger than anything needed.
- `Config` derives `Debug` over three secrets — not currently logged, but a
  single future `?config` would dump them.
- `/api/admin` deserialises its body before checking the secret.
- The `player` table is never trimmed; nicks are reserved forever.
- `SCHEMA_VERSION` is written but never read, and there is no `CatchPanicLayer`.
- The purge narrates one feed line per departing player, uncapped. Chunking
  makes this commit correctly, but it is still a lot of noise for one tick.

## Code style

- `anyhow::Result` in `dragon-api`; `dragon-core` returns values, not errors
- `tracing` for logging — never log a token, a secret, or a request body
- `BTreeMap`, not `HashMap`, anywhere inside `GameState`: serialisation and
  iteration must be deterministic
- Comments explain *why*, especially where a line looks wrong but is deliberate
- Test names are sentences describing the behaviour, not the function under test
