# Legend of the Rusty Dragon

A Rust port of [Legend of the Pink Dragon](https://gitlab.com/baruchel/pink-dragon),
moved off IRC and onto a serverless model.

## Running it locally

```sh
docker run -d --name dragon-pg \
  -e POSTGRES_USER=dragon -e POSTGRES_PASSWORD=dragon -e POSTGRES_DB=dragon \
  -p 55432:5432 postgres:16-alpine

# disable ssl for local server
export DATABASE_URL="postgres://dragon:dragon@localhost:55432/dragon?sslmode=disable"
export INVITE_KEY="local-dev"
export STATIC_DIR=public # To serve client
cargo run -p dragon-api --bin dragon-server
```

### Settings

| variable                   | meaning                                                        |
|----------------------------|----------------------------------------------------------------|
| `DATABASE_URL`             | required; use pooled endpoint for Neon                         |
| `INVITE_KEY`               | signup key                                                     |
| `JOIN_ATTEMPTS_PER_WINDOW` | signup rate limit, default 10/hour                             |
| `PACING`                   | `web` (default) or `irc` for the original cadence              |
| `TICK_SECS`                | override the heartbeat without a redeploy                      |
| `MAX_CATCHUP_TICKS`        | how much history one request will replay                       |
| `ADMIN_SECRET`             | enables `/api/admin`; admin is refused outright when unset     |
| `CRON_SECRET`              | guards `/api/tick`; unauthenticated when unset                 |
| `GAME_SEED`                | fixes the world's random stream                                |
| `STATIC_DIR`               | serve a client directory (local development only)              |
| `PORT`                     | default 3000                                                   |

## API

|                                   |                                                   |
|-----------------------------------|---------------------------------------------------|
| `POST /api/join`                  | `{nick, invite}` → a bearer token, shown **once** |
| `POST /api/quest`                 | `{quest, strategy?}`, authenticated               |
| `POST /api/buy`                   | `{item}`, authenticated                           |
| `GET /api/me`                     | character sheet, authenticated                    |
| `GET /api/state`                  | board, store, standings, feed cursor              |
| `GET /api/feed?since&limit&actor` | history, oldest first                             |
| `POST /api/tick`                  | optional heartbeat                                |
| `POST /api/admin`                 | the reference's admin commands                    |

### Signing up

The Realm is invitation-only. `POST /api/join` requires one of the keys in
`INVITE_KEY`.

```sh
curl -s -X POST localhost:3000/api/join \
  -H 'content-type: application/json' \
  -d '{"nick":"Absalom","invite":"the-key-you-shared"}'
```

Attempts are rate-limited per caller (10 an hour by default) and **failures
count** — a wrong key, a taken name and a malformed name all spend budget. That
is what bounds a leaked key, and what stops the taken-name reply being used to
enumerate the roster.

## Deploying to Vercel

One function serves everything. `vercel.json` rewrites `/api/(.*)` to
`api/index`, and a rewrite leaves the request's original path intact, so the
same axum router does the routing in both places — there is no second route
table to keep in step. If that ever stops holding, the 404 body names the path
it was actually given, so the first `curl` says what went wrong.

```sh
vercel link
vercel env add DATABASE_URL      # Neon's POOLED endpoint, not the direct one
vercel env add INVITE_KEY        # required; nobody can sign up without it
vercel env add ADMIN_SECRET      # optional; admin is refused outright without it
vercel deploy
```

The schema is created on the first cold start and the Realm opens itself on the
first request, so there is no migration step.

**Connection pooling is not optional.** Every concurrent invocation opens its
own pool, so point `DATABASE_URL` at Neon's pooled endpoint and leave
`DB_MAX_CONNECTIONS` small (it defaults to 3).

**Only `public/` is served.** `vercel.json` sets `outputDirectory`, so the web
root is a directory you deliberately put files in. Without it Vercel falls back
to serving the *repository root* — every source file, `Cargo.lock` and all —
because with no framework preset the output directory is `public` if it exists
and `.` otherwise.

**TLS is verified.** A `DATABASE_URL` with no `sslmode` gets `verify-full`
added. This is worth knowing about rather than trusting blindly: sqlx defaults
to `prefer`, and for every mode below `verify-ca` it installs a certificate
verifier that accepts anything — so Neon's suggested `sslmode=require` encrypts
the connection without authenticating the server. An explicit `sslmode` is
always respected, and anything weaker than `verify-full` logs a warning.

### About the heartbeat

The game ticks lazily, so **no cron is required** — any request settles the
clock first. `vercel.json` deliberately ships without a `crons` entry, because
Vercel's Hobby plan rejects any schedule that fires more than once a day and the
deployment would fail. On Pro, add one:

```json
"crons": [{ "path": "/api/tick", "schedule": "*/5 * * * *" }]
```

Set `CRON_SECRET` alongside it if you want the route authenticated; leaving it
unset is safe rather than dangerous, because a heartbeat that finds no tick owed
does no work at all. A heartbeat is worth having only so a Realm nobody is
looking at keeps living; without one, an empty Realm simply pauses until
somebody arrives, which is arguably what it should do.

## Tests

```sh
cargo test --workspace                     # core only; database tests skip
export TEST_DATABASE_URL="postgres://dragon:dragon@localhost:55432/dragon"
cargo test --workspace                     # everything, including SQL and HTTP
```

Database tests each run in their own schema, so they are safe to run in
parallel and never touch a real deployment. `tests/contention.rs` covers what
happens when several callers want the single game row at once — a stranded
lock, and the thundering herd at a tick boundary — and `tests/large_turns.rs`
guards the bind-parameter ceiling.

## Playtesting the pacing

The web retune's numbers came out of this rather than out of an argument:

```sh
cargo run --release -p dragon-core --example simulate -- --pacing web --players 12 --days 14
```

Every pacing knob is overridable (`--tick`, `--idle-base`, `--idle-coeff`,
`--idle-min`, `--idle-max`, `--relief`) so a candidate retune can be swept
without a rebuild. Measured with 12 bots questing around the clock:

| | level 10 | level 20 | quests/day | ascensions in 14 days |
|---|---|---|---|---|
| `web` | 1d 15h | 2d 12h | 95 | 0–1 |
| `irc` | 3d 00h | 4d 18h | 69 | 0 |

Those bots play optimally and never sleep, so treat the times as a floor for an
obsessive player rather than a typical experience.

## Deliberate departures from the reference

Three are bug fixes, and are commented at their sites:

* **Event 24 (stealing) never ran.** `event.py:444` reads `u.dexterity`, which
  is unbound on that branch — an `UnboundLocalError` every time it was drawn,
  one event in twenty-six. Ported as the thief's dexterity, which was plainly
  the intent. A player can also no longer pick their own pocket.
* **The battle loop was unbounded** — the reference's own standing TODO. Two
  combatants who cannot hurt each other spin forever, which under a request
  timeout is a lost turn. Now capped, with deadlock detection ahead of it.
  `battle`'s `return warriors[0]` on an empty roster is a `None` rather than an
  `IndexError`.
* **Ability-check tie-breaks could loop forever.** Capped.

And two are consequences of the move:

* **Durations are wall-clock, not tick counts.** The reference purges at 896
  ticks and runs event cooldowns at 16/24/32/64/256 ticks, which is only safe
  while the tick length never moves. It moved.
* **The periodic board and store dumps left the feed.** IRC had to broadcast the
  whole board because a channel cannot be asked; `GET /api/state` can. The feed
  keeps the news — what arrived, what sold, who won — and drops the recitals.

## Assets

Monsters, weapons and armor come from the D&D 5e SRD [dnd5eapi.co](https://www.dnd5eapi.co).
