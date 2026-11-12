# baby-mcp

A baby log for one household, used mainly through Claude (MCP): parents tell
Claude what happened ("drank 60 ml of pumped milk and had a wet diaper"),
Claude logs it, and anyone can ask "when was the last feed?" or "how much
pumped milk was given today?". A small web UI adds an at-a-glance status
page, charts, and a capture form that works without Claude.

Status: built and in daily use. This document describes the design and the
choices behind it; decisions marked *(v1)* may change later.

## Goals

- **MCP-first.** The agent is the main way in. An entry is what the user said,
  plus structured fields the agent extracts while logging.
- **Nothing is lost.** The text is kept next to the fields, so information that
  has no field yet survives, and fields added later can be filled in from old
  entries.
- **Statistics are easy.** Fields are plain, typed, queryable values: "ml of
  pumped milk per day" is a single SQL query.
- **Capture works without an agent.** A dumb web form stores text and marks
  it for an agent to process later.
- **Every change is kept.** Edits and deletes never destroy earlier versions.
- **One Rust binary, one SQLite file**, behind a TLS reverse proxy (Caddy).

Non-goals (v1): several children or other subjects; sync with other apps;
a public instance; tracking sleep (only the side she is put down facing).

## Architecture

```
claude.ai / Claude Code ──MCP (OAuth)──┐
phone / tablet browser ──HTTPS (passkey session)──┤
                                        Caddy (baby.example.com, TLS)
                                                  │
                                       baby-mcp on 127.0.0.1:8095
                                                  │
                                    /var/lib/baby-mcp/baby.sqlite
```

The host is dedicated to this app, so OAuth discovery documents live at its
root. Endpoints:

| path | what |
|---|---|
| `/mcp` | MCP, streamable HTTP; bearer token required |
| `/.well-known/oauth-protected-resource/mcp`, `/.well-known/oauth-authorization-server` | OAuth discovery |
| `/oauth/register`, `/oauth/authorize`, `/oauth/token` | OAuth 2.1 provider |
| `/login`, `/logout`, `/enroll` | passkey login and enrollment for people |
| `/` | status page (session) |
| `/charts` | charts (session) |
| `/add` | capture form (session) |
| `/manifest.webmanifest`, `/icon-*.png` | home-screen app: manifest and icons (public; the icon's source is `static/icon.svg`) |
| `/api/*` | JSON for the pages (session) |

## Stack

- Rust, edition 2024.
- `axum` + `tokio` for HTTP.
- MCP: a small hand-written JSON-RPC layer (`initialize`, `ping`,
  `tools/list`, `tools/call`; stateless, JSON responses), not `rmcp`: the
  endpoint is simple enough, and the SDK's API still moves.
- `rusqlite` with the `bundled` feature (SQLite 3.45+, for `->>` and JSON
  checks).
- `webauthn-rs` for passkeys.
- `serde`, `serde_json`, `toml` for data and config.
- `jiff` for time zones (Europe/Amsterdam, DST-correct day boundaries), with
  the time zone database bundled.
- `rand`, `sha2`, `hmac`, `base64` for tokens.
- Web pages: server-rendered HTML with a little plain JavaScript; no
  framework, no build step. Charts are SVG rendered by the server *(v1)*.

Keep dependencies few; prefer small hand-written code over a crate for small
things.

## Data model

SQLite, one file. Times are Unix seconds (UTC); everything shown to people is
Europe/Amsterdam.

### Entries

An *entry* is one event or episode: a feed, a diaper, a measurement, or
several of these at the same moment. Sleep is not tracked (too much hassle).
It has a start time, an optional
duration, the text the user said (or the agent's paraphrase when it was a
conversation), and a JSON object of fields.

**Two kinds of "none".** In `fields`, a key that is absent means *unknown*:
not stated, or the field did not exist when the entry was logged. A key with
value `null` means *deliberately nothing* ("no bottle this time"). `false` and
`0` are ordinary values.

**Needing attention.** `needs_attention_by` is a bitmap: 1 = a person,
2 = an agent. `needs_attention_why` says why, in free text, for example
`fields not extracted yet` (logged on the web form). Code must not match on
the wording.

**Versions.** `entry` holds only current versions. Before every update, a
trigger copies the old row into `entry_old` (same columns, plus
`superseded_at`). Deleting is a new version with `deleted = 1`; real `DELETE`
is refused. Restoring version *n* is a new version with the contents of *n*.

### Schema

```sql
PRAGMA user_version = 4;   -- migrations in db.rs

CREATE TABLE user (
  id           INTEGER PRIMARY KEY,
  name         TEXT    NOT NULL UNIQUE,     -- login name, e.g. 'bas'
  display_name TEXT    NOT NULL,
  created_at   INTEGER NOT NULL,
  role         TEXT    NOT NULL DEFAULT 'parent' CHECK (role IN ('parent', 'viewer'))
);

CREATE TABLE entry (
  entry_id            INTEGER PRIMARY KEY,  -- the id agents and pages use
  version             INTEGER NOT NULL DEFAULT 1,
  at                  INTEGER NOT NULL,     -- start of the event
  duration_s          INTEGER CHECK (duration_s >= 0),
  text                TEXT    NOT NULL,
  fields              TEXT    NOT NULL DEFAULT '{}'
                      CHECK (json_valid(fields) AND json_type(fields) = 'object'),
  needs_attention_by  INTEGER NOT NULL DEFAULT 0 CHECK (needs_attention_by BETWEEN 0 AND 3),
  needs_attention_why TEXT,
  deleted             INTEGER NOT NULL DEFAULT 0 CHECK (deleted IN (0, 1)),
  changed_at          INTEGER NOT NULL,     -- when this version was made
  changed_by          INTEGER NOT NULL REFERENCES user(id),
  CHECK ((needs_attention_by = 0) = (needs_attention_why IS NULL))
  -- plus one generated column per registry field, see below
);
CREATE INDEX entry_at        ON entry(at) WHERE deleted = 0;
CREATE INDEX entry_attention ON entry(at) WHERE needs_attention_by != 0 AND deleted = 0;

CREATE TABLE entry_old (
  entry_id            INTEGER NOT NULL REFERENCES entry(entry_id),
  version             INTEGER NOT NULL,
  at                  INTEGER NOT NULL,
  duration_s          INTEGER,
  text                TEXT    NOT NULL,
  fields              TEXT    NOT NULL,
  needs_attention_by  INTEGER NOT NULL,
  needs_attention_why TEXT,
  deleted             INTEGER NOT NULL,
  changed_at          INTEGER NOT NULL,
  changed_by          INTEGER NOT NULL REFERENCES user(id),
  superseded_at       INTEGER NOT NULL,
  PRIMARY KEY (entry_id, version)
  -- plus the same generated columns
);

CREATE TRIGGER entry_keep_old BEFORE UPDATE ON entry
BEGIN
  SELECT RAISE(ABORT, 'version must increase by 1') WHERE NEW.version != OLD.version + 1;
  INSERT INTO entry_old (entry_id, version, at, duration_s, text, fields,
                         needs_attention_by, needs_attention_why, deleted,
                         changed_at, changed_by, superseded_at)
  VALUES (OLD.entry_id, OLD.version, OLD.at, OLD.duration_s, OLD.text, OLD.fields,
          OLD.needs_attention_by, OLD.needs_attention_why, OLD.deleted,
          OLD.changed_at, OLD.changed_by, NEW.changed_at);
END;

CREATE TRIGGER entry_no_delete BEFORE DELETE ON entry
BEGIN
  SELECT RAISE(ABORT, 'entries are not deleted; set deleted = 1');
END;

CREATE TABLE suggestion (
  id     INTEGER PRIMARY KEY,
  at     INTEGER NOT NULL,
  by     INTEGER NOT NULL REFERENCES user(id),
  text   TEXT    NOT NULL,
  status TEXT    NOT NULL DEFAULT 'open' CHECK (status IN ('open', 'done', 'rejected')),
  note   TEXT
);
```

### Field registry

The registry is a static table in the code: name, type, unit or choices,
allowed range, and a description for the agent. It is the only place fields
are defined. The server validates every incoming value against it, and the
`fields` MCP tool returns it.

For each field, the program adds a virtual generated column to both `entry`
and `entry_old` at startup if it is missing, e.g.
`bottle_breastmilk_ml REAL GENERATED ALWAYS AS (fields ->> '$.bottle_breastmilk_ml') VIRTUAL`.
Queries then read fields like plain columns, and indexes can be added on them
when needed. In these columns both kinds of "none" are NULL; queries that must
tell them apart use `json_type(fields, '$.name')`. A test checks that `entry`
and `entry_old` have identical columns.

Registry v1:

| field | type | unit / choices | range |
|---|---|---|---|
| `bottle_breastmilk_ml` | number | ml of expressed breast milk given (bottle, cup, finger, syringe) | 0–300 |
| `bottle_formula_ml` | number | ml of formula given | 0–300 |
| `breast_left_min` | integer | minutes at the left breast | 0–90 |
| `breast_right_min` | integer | minutes at the right breast | 0–90 |
| `latch` | choice | `with_aid` (e.g. nipple shield), `with_difficulty`, `without_difficulty`, `none` | |
| `wet` | boolean | wet diaper | |
| `dirty` | boolean | dirty diaper | |
| `stool_color` | choice | `meconium`, `green`, `brown`, `yellow`, `transitional`, `other` | |
| `vomit` | boolean | spit up / vomited | |
| `temp_c` | number | °C | 34–42 |
| `weight_g` | number | grams | 1000–15000 |
| `sleep_facing` | choice | `left` or `right`: the way the face is turned when put down to sleep; alternated after every feed (`status` and the feed tile show the next side, assuming it was alternated after each feed logged since the last logged side) | |
| `bath` | choice | `with_soap`, `without_soap` (in the bath) or `sponge` (washcloth only); log shows 🛁🧼, 🛁, 🧽. Goal: two baths a week, alternating: a bath is due 3½ days after the last one (sponge washes don't count), of the other kind than the last; the status page has a 🛁 tile with the day and whether to use soap | |
| `vitamin_k`, `vitamin_d` | bool | drops given. RIVM: K daily from day 8 until 12 weeks, D daily from day 8 until 4 years; a 💊 tile says what is still to give today (orange from 18:00), and the "Vitamins" notification fires at 18:00 if something wasn't given | |

The log is about the baby: pumped milk counts when she is given it
(`bottle_breastmilk_ml`), not when it is pumped. A breastfeeding at the breast
is `breast_*_min` and/or `latch`; a bottle is `bottle_*_ml`; a feed can have
both.

## MCP

The `initialize` result answers with a protocol version the server speaks
(the client's, if it is one of them, else the newest) and carries
instructions: what an entry is, the two kinds
of "none", "extract every registry field the user mentions, store the user's
own words as the text", the bare-time rule, and "call `suggest` when something
does not fit".

Tools:

| tool | what |
|---|---|
| `log` | New entry: `text`, `at` (default now), optional `duration_min`, `fields`, `needs_attention_by/why`. Warns instead of logging when an entry within 15 minutes shares a field group (feed, diaper, vomit, measurement), unless `confirm` is set. Returns the entry. |
| `update` | Change an entry by id: any of `at`, `duration_min`, `text`, `fields` (merge; a key with `null` sets deliberately-none, `remove` lists keys to make unknown again), attention. |
| `delete` / `undelete` | Set or clear `deleted`. |
| `entries` | Entries in a time range (default: today), oldest first; at most `limit` (default 200, at most 500), with `truncated` and `next_from` to continue. |
| `history` | All versions of one entry. |
| `attention` | Entries that need attention, by person and/or agent. |
| `status` | Overview: the last feed, diapers, temperature and weight, with how long ago; the next feed (`next_feed_estimate`: the end of the last feed + the median recent gap, at least 2 hours and at most the longest gap the feeding plan allows; `next_feed_start` moved up by the preparation time); the wet-diaper warning; feeding guidance; the sleep side; bath and vitamins; growth percentiles; today's and last 24 h totals; entries needing attention. Diapers aren't predicted (they're checked at feeds). |
| `stats` | Per-day totals for a range: ml of each kind, breast minutes, feeds, diapers, temperature range, weight vs birth weight. |
| `fields` | The field registry. |
| `sql` | Read-only SQL for questions the other tools don't answer: a separate read-only connection, an SQLite authorizer that only allows reading `entry`, `entry_old`, `suggestion`, `json_each`/`json_tree` and `user` (id and names) — never credentials or tokens — 500 rows, 5 seconds. |
| `suggest` | Free-text suggestion for the developer: what the user wanted, what was missing, what the agent did instead. |

Bare clock times ("at 3") mean today, or yesterday if that would be more than
15 minutes in the future; in `update`, the entry's own day. A time more than
15 minutes in the future is refused. A day in statistics runs midnight to midnight,
Europe/Amsterdam.

## Web UI

Passkey session required. English labels. *(v1)*

- **Status** (`/`): entries that need attention (with a link to each); a
  timeline from the day of birth to the hours ahead, scrolling sideways
  (logged entries as emojis on lanes for feeds, diapers and the rest;
  expected ones faded, overdue ones ringed just after now), loaded a day at
  a time from `/timeline`; pinch (or − / +) zooms out to a summary per day
  and per week of life. "What's next" tiles (feed, diaper, bath, vitamins)
  under today's heading; below, the log of the day in view (`/?day=`,
  swapped in from `/log` while scrolling), or at the week zoom the week.
  Refreshes every minute. A *viewer* sees only the charts.
- **Charts** (`/charts`): weight change since birth and the WHO growth chart
  (see Growth), ml per day by kind (pumped breast milk, formula), minutes at the breast per day (left and right), a per-kg chart (see
  Feeding guidance), feeds and
  diapers per day (each diaper once: wet, wet + dirty striped, dirty), with
  today's bar extended by a dashed outline to the value expected by midnight
  (so far today + the last 24 hours' total × the part of the day still to
  come), and temperature readings of the last 14 days (not before
  birth) against the usual newborn range of 36.5–37.5 °C, readings outside
  it in red.
- **Capture** (`/add`): one free-text box and Save; the entry is logged as
  now with `needs_attention_by = 2` and `fields not extracted yet`. The agent
  extracts the fields and the time (if the text mentions one) via `attention`
  and `update`. Pages show such entries as "waiting for Claude".

## Authentication

Same model as [healthsoft-mcp](https://github.com/bwesterb/healthsoft-mcp)'s, written in Rust:

- **People** log in with passkeys (`webauthn-rs`). `baby-mcp user add NAME
  "Display name"` creates a user and prints a single-use enrollment link
  (30 minutes). A person can enroll several passkeys. Passkeys are
  discoverable (resident): login asks the browser for any passkey of this
  site, so the page never lists the enrolled credential ids, and the user
  handle in the answer must belong to the passkey's user. Web sessions are random
  ids in a cookie (`Secure`, `HttpOnly`, `SameSite=Lax`), stored server-side;
  one expires 30 days after it was last used (renewed, cookie and all, at
  most once a day).
- **Claude** connects through a minimal OAuth 2.1 provider: dynamic client
  registration, redirect URIs restricted to https on claude.ai / claude.com
  or http on localhost; authorization code with PKCE (S256) only; the
  consent page requires a passkey login; access tokens 1 h, HMAC-signed and
  bound to the `/mcp` resource; refresh tokens 30 days, stored hashed,
  rotated on every use.
- Every entry version, suggestion and token records which user it belongs to.
- Admin commands: list and remove passkeys, revoke a user's tokens or all
  tokens.

Credentials and tokens live in the same SQLite file, in tables of their own
(`passkey`, `session`, `oauth_client`, `refresh_token`), defined with the
authentication code.

## Configuration

A TOML file, `/etc/baby-mcp/config.toml`, unknown keys rejected:

```toml
origin  = "https://baby.example.com"          # passkey RP; effectively permanent
listen  = "127.0.0.1:8095"
state   = "/var/lib/baby-mcp"                 # database
tz      = "Europe/Amsterdam"
oauth_redirect_hosts = ["claude.ai", "claude.com"]

[baby]
name           = "…"
birth          = 2026-01-02T03:04:00+01:00
birth_weight_g = 3500
```

The baby's details exist only in this file, never in the code.

## Import from the kraamweek

`baby-mcp import-healthsoft FILE --as USER`, run once: reads the JSON of the
portal's `GET /client/dossiers/{id}/medical` from a file (fetched with
healthsoft-mcp's login) and creates one entry per measure, with the portal's
notes as text. (Reading a file keeps an HTTP client out of baby-mcp.) Mapping: `feeding` ml with
`feedingType` BREAST → `bottle_breastmilk_ml`, FORMULA → `bottle_formula_ml`,
MIXED → the split from the notes where stated (one stated kind plus "the
rest" is the other kind); when the notes give no amounts the total stays in
the text only ("split not recorded"); notes whose amounts don't add up get
`needs_attention_by = 2`;
`breastType` → `latch`; minutes, urine, feces, colour, vomited, temperature,
weight map directly.

## Operations

- **Deploy**: build on the Mac (cross-compile for x86_64 Linux), copy the
  binary to `/usr/local/bin/baby-mcp` on the server, restart.
- **Service**: system user `baby-mcp`, hardened systemd unit as for
  healthsoft-mcp, state in `/var/lib/baby-mcp`.
- **Caddy**: a site block for the app's host name proxying everything
  to `127.0.0.1:8095`.
- **DNS**: A/AAAA records for that host name, pointing at the server.
- **Backup**: none of its own; the database file in the state directory is
  covered by the host's backups. `baby-mcp export` writes all entries,
  versions and suggestions as JSON.
- **Admin commands** run as the service user (`sudo -u baby-mcp baby-mcp …`),
  so SQLite never creates files the service cannot open; they refuse to run
  as root. The config file holds no secrets.
- **Suggestions**: `baby-mcp suggestions [--all]` lists them;
  `baby-mcp done ID [--note …]` and `baby-mcp reject ID [--note …]` close them.
- **Logs**: ids, tool names, users and timings; never entry text or field
  values.

## Growth

Weight is compared with the WHO Child Growth Standards (weight-for-age),
selected by `baby.sex` in the config. The WHO's table of LMS parameters per
day of age (0–1826 days, from the WHO's `anthro` package) is **not part of
this repository**: the server reads it at startup from `growth_table`
(default `<state>/who-weight-for-age.tsv`) and runs without percentiles if it
is missing; `data/README.md` says where to download it. `growth.rs` computes the z-score with the LMS
method, including the WHO's adjustment beyond ±3 SD, and the percentile.

- `status`: percentile of the latest weight and of the birth weight, and
  whether (and when) birth weight was regained.
- `stats`: the percentile of each day's weight.
- Status and day pages: the percentile next to the weight.
- Charts: a growth chart over age since birth with the P3–P97 and P15–P85
  bands and the median, birth weight at day 0 and every weighing.

The standard does not model the normal weight loss in the first days, so a
percentile dip in the first week is expected. Percentiles are a reference,
not a diagnosis; the pages and tools say so.

**Weight change in the first month.** The WHO standard ignores the normal
weight loss of the first days, and that loss depends on delivery mode. For
the first 30 days the percentage change since birth is also compared with
percentiles per delivery mode (`baby.delivery`), from a local table read off
the figure in Paul et al., Pediatrics 2016 (see `data/README.md`; not part of
this repository). The percentile of a value lies between two curves and is
interpolated on the normal (z) scale. `status` reports it as
`weight_change_percentile`; the status and day pages show it next to the
weight; the charts page has a weight-change chart with the P5–P95 and
P25–P75 bands and the median. (The Newborn Weight Tool, Newt, is based on the
same research; its terms forbid copying or analysing it, so it is not used.)

Later: length and head circumference (fields and WHO tables), once measured.

## Feeding guidance

Feeds are counted as episodes: a feed entry within 1 hour of the previous one
is a top-up of the same feed, as long as that feed started at most 2 hours
before (so cluster feeding counts as several feeds).

`feeding.rs` gives a guide for how much and when, from rules of thumb (no
official guideline; Dutch hospital and kraamzorg information): in the first
days 10 ml × day of life per feed up to 10 ml more (day 1 = the birth day, as
the kraamzorg counts), for the planned number of feeds (the aim, else 8, at
least the NVK minimum), then 150–180 ml per kg per day, the build-up applying
until it reaches the per-kg level. The target is the middle of that band (as
drawn on the bottle chart). Feeds per day: the median of the last three full
days, kept between the NVK minimum and 10.

The amount comes from a plan for the feeding day (07:00 to 07:00). The night
(23:00–07:00) is left to her: nobody wakes her, she feeds when she wakes. Her
expected night intake is the median of the last three nights. Each feed's
millilitres are spread over a triangle 1.5 h either side, so a feed just
before or after 23:00 or 07:00 counts partly for both; the same goes for
what she had since 07:00. What remains of the target after the night and
since 07:00 is split into feeds of at most what she usually manages (the
75th percentile of her bottle-only feeds of the last 2 days, top-ups
included). The amount is that share (at least a normal feed, the target ÷
feeds per day, and at most the 75th percentile); `day_plan.feed_by` is when
the first of those feeds is due for the last one to be at 23:00 with 2 hours
between them, never later than the NVK's latest. When that is earlier than
her rhythm's estimate (below), it is the time on the feed tile, in the
notifications and on the timeline ("🎯 N feeds of ~X ml still to go before
23:00"). The rhythm estimate is `status`'s next-feed estimate: the end of the last feed plus the median recent gap, at
least 2 hours (snacks and cluster feeding shorten the median) and at most 24
hours ÷ the planned number of feeds (and the NVK's longest pause),
given as `next_feed.max_gap`; "at the latest" is the last feed plus the NVK's longest pause, or earlier,
the moment the last 24 hours would hold fewer than the NVK minimum number of
feeds (when the minimum-th most recent feed turns 24 h old).
The minimum is the NVK advice for healthy term babies ("Advies ten aanzien
van wakker maken van pasgeborenen voor voeding", 2016): with a normal birth
weight, at least 6 feeds per 24 h and pauses of at most 6 h until 14 days old
and back above birth weight, then 5 and 8 h until 8 weeks, then 4 and 12 h
(under 2500 g: 8 and 3 h; 2500–3000 g: 7 and 6 h). It assumes the baby grows
and drinks well. On top of that, `aim_feeds_per_day` in the config
(e.g. 7) raises the planned number of feeds. Warnings (Feed tile, `status`,
and the "Feeding minimum" notification, once each): from 1 hour before the
longest pause since the last feed, and when (within the hour) the last 24 hours
would hold fewer than the minimum number of feeds; once per last feed. `feed_prep_minutes` (e.g. 15)
moves the feed tile, "start by" and the "feed soon" notification up by that
much, because the routine starts with the diaper; "feed overdue" and the NVK
limits stay about the feed itself. Which breast to start with: the side with fewer
minutes over the last 24 hours when clearly less (≥ 10 min and ≥ 20% of the
total), else the other side than the one used most at the last breastfeed
(feed tile, "feed soon" notification, `feeding_guidance.breast_side`). Growth is a check, not a
steering wheel: a weight change below P25 for the delivery mode is flagged
with the advice to discuss it with the midwife or consultatiebureau; the
amounts are not raised automatically.

`status` returns it as `feeding_guidance`; the feed tile shows the suggested
amount. A separate guide card, a text analysis and target lines on the
bottle chart were tried and dropped as too much.

The charts page shows, per full day since birth, bottle ml per kg and grams
gained per kg (weights interpolated linearly between weighings; the birth
day and today are partial and left out; days with breastfeeding are marked,
since it isn't measured). From day 8 on (before that intake follows the
build-up, not need) it fits `ml/kg = baseline + cost × g/kg` by least
squares: the baseline is the bottle intake at zero growth, the cost the
extra ml per gram gained. It is shown only with at least 5 such days, at
least 5 g/kg/day between the slowest and fastest, and a positive baseline
and cost; otherwise the card says why not. Every place says it is not
medical advice, that a plan from the nurse, midwife or doctor takes
precedence, and that intake at the breast is not measured (so with
breastfeeding the bottle total looks lower). `baby-mcp status` prints the
same overview on the server.

## Notifications

Web Push (RFC 8030) with aes128gcm message encryption (RFC 8291) and VAPID
(RFC 8292), hand-written on p256/aes-gcm/hmac; the VAPID key is in `kv`.
`/notifications` turns push on or off for the device at hand (a service
worker at `/sw.js`; on iOS only from the home-screen app), lists the user's
devices, sends a test, and has per-user choices (default on):

- **Feed soon**: 15 minutes before it's time to start the next feed (the
  estimate less `feed_prep_minutes`, as on the feed tile);
- **Feed overdue**: 30 minutes after the estimate (not after 3 hours: nobody
  is logging);
- **Feeding minimum**: an NVK warning (see Feeding guidance), once each;
- **Entries need attention**: an entry marked for the user (🙋) and left
  unchanged for 20 minutes (whoever just made it knows).
- **Any change** (off by default): when someone else logs, changes or deletes
  an entry (each version once, linking to the entry).
- **Vitamins**: at 18:00 if vitamin K or D (whichever applies at her age)
  hasn't been given that day; once per day.

Notifications are grouped: feeding (soon, overdue, minimum), attention,
changes, vitamins. A group with something new goes out as one notification
listing everything current in it (most important first), and replaces the
group's previous notification on the device (its tag, and the push topic).
A task checks once a minute; `push_sent` keeps each (user, kind, key) to
one push (the key is the last feed's time, or entry id and version).
Pushes are content-free: the encrypted payload is only a random message id;
the service worker fetches the text from `/push/message/{id}` (unguessable,
expires after a day) over our own TLS. So Apple/Google relay an id, not
"Eva needs a feed", and the classical P-256 push encryption guards nothing
of lasting value. Endpoints answering 404/410 are removed.

Wet diapers: a warning (💧 tile, badge, `status.wet_diaper_warning`) when the
last 24 hours had fewer wet diapers than usual for her age: at least as many
as the day of life up to day 5, then at least 6 (breastfeeding and kraamzorg
guidance). Not before day 2, and only while someone is logging.

App badge: the home-screen icon shows a count (Badging API; iOS 16.4+ for
home-screen apps, with notifications allowed) of entries needing the parents
(🙋) plus one each for feeding (due within 15 minutes, overdue, or an NVK
warning), vitamins not given after 18:00, and a bath due today (from 18:00)
or overdue. Pages set it
on load from `/push/badge`; with the app closed it only changes when a push
arrives (the message fetch carries the current count), since iOS allows no
silent background updates.

## Deferred

- More notification kinds (e.g. an open feed whose duration was never
  finalised), quiet hours.
- Several children; a separate mother subject.
- Server-side extraction (calling Claude from the server) instead of an agent
  working through `attention`.
- Fixed reason codes for `needs_attention_why`.
