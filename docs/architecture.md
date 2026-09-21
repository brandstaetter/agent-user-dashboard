# Agent Usage Dashboard Architecture

> Contract and implementation architecture. Verified against the official
> sources listed at the end, Codex CLI 0.154.0, and Claude Code 2.1.278.

## Goals and non-goals

The application is one local Rust process that shows normalized quota windows
for Codex, Claude Code, GitHub Copilot, and Google Antigravity, retains a small local history, and
emits deduplicated local alerts. Provider acquisition, normalization, storage,
alert evaluation, and presentation are separate boundaries so a provider
contract change cannot silently redefine stored data.

V1 does not ask for, read, copy, export, log, or store OAuth/API credentials,
GitHub tokens, keychain contents, browser cookies, prompts, transcripts,
repositories, conversation identifiers, raw provider payloads, or account
identifiers. It does not call provider HTTP APIs directly, mutate
provider state, consume reset credits, synchronize remotely, or estimate message
counts from percentages. Codex app-server, Claude Code, GitHub's Copilot
runtime remain responsible for authentication and upstream communication.
Antigravity owns its own browser/keyring authentication and Google communication.

## Component boundaries

```text
codex child (stdio JSON-RPC) -> CodexAdapter ----+
                                                  v
Claude status-line stdin -> Relay/Ingest CLI -> Normalizer -> SQLite -> Query service -> TUI
                       \-> existing formatter -> unchanged status-line output
Copilot runtime (stdio RPC) -> CopilotAdapter ----+
Antigravity status-line stdin -> Relay/Ingest CLI -+
                                                  |                    |
                                                  +-> Alert engine ----+-> notifier
```

- `providers::codex` owns the child lifecycle, newline-delimited JSON transport,
  handshake, request correlation, tolerant decoding, and retry classification.
- `providers::claude` owns bounded parsing of one status-line JSON value.
  `status_line_relay` directly launches an explicitly supplied existing formatter,
  forwards the complete original stdin stream, preserves its stdout/stderr/exit
  status, and invokes a CLI-owned normalized-ingestion callback as an isolated
  best-effort side effect. Neither path launches Claude Code or Antigravity.
- `providers::github_copilot` owns the bounded SDK/runtime lifecycle, the single
  typed account-quota RPC, per-category validation, and sanitized error classes.
- `providers::antigravity` owns bounded typed parsing of one status-line JSON
  object and independently validates dynamic quota buckets.
- `domain` owns provider-independent validation and normalization.
- `storage` is the only SQLite caller. It receives normalized values, never raw JSON.
- `alerts` evaluates transitions against persisted alert state; it does not poll.
- `tui` reads query projections and sends refresh intents; it never talks directly
  to a provider. `notify` implements native/terminal outputs behind a trait.

One binary exposes `dashboard` (TUI), `ingest claude|antigravity`,
`relay claude|antigravity -- COMMAND [ARG...]`, poll-provider `refresh`/`watch`,
bounded `history`, and permit-listed
`config` subcommands. Concurrent processes use SQLite WAL mode, a busy
timeout, short transactions, and atomic upserts. The ingest/relay path must remain
quick because Claude cancels an in-flight status-line command when a newer update
arrives.

## Normalized quota model

`QuotaSnapshot` contains:

- `provider`: closed enum `codex | claude | github_copilot | google_antigravity`.
- `scope_key`: stable, non-secret provider bucket key. Codex uses a sanitized
  `limitId` when present; Claude uses `subscription`; Copilot uses a bounded
  runtime-defined quota category; Antigravity uses a bounded dynamic quota key.
  Never use an account ID.
- `window_kind`: `rolling_5h | rolling_7d | spend | monthly | other`.
- `limit_kind`: `limited | unlimited`. Existing Codex/Claude rows and finite
  Copilot categories are limited; Copilot entitlement `-1` is unlimited.
- `window_duration_seconds`: nullable positive integer. Classification comes from
  duration (Codex), not the transport labels `primary`/`secondary`.
- `used_percent`: finite decimal as reported; validation range is 0..=100 for
  subscription windows. Spend windows may exceed 100. Preserve precision.
- `remaining_percent`: derived at read time as `clamp(100 - used_percent, 0, 100)`,
  not independently persisted. Limited rows display a fixed-width progress bar
  plus used and remaining percentages. Unlimited rows display `Unlimited` with
  no percentage bar; `limit_kind` is authoritative.
- `resets_at`: nullable UTC Unix seconds; `observed_at`: local UTC Unix milliseconds.
- `availability`: `allowed | blocked | unknown`, derived only from an explicit
  provider availability/reached signal; percentages never imply recovery.
- `source_version`: CLI version observed at adapter startup, nullable for piped
  fixtures; `quality`: `fresh | partial`; `source_sequence`: one UUID per input
  snapshot, used only to group rows.

Unknown durations are stored as `other`, displayed with their duration, and are
not relabeled as weekly/monthly. Null reset timestamps remain unknown except for
the explicitly labeled, presentation-only Copilot monthly expectation described
below. Percentages, provider reset times, and durations are provider claims, not
guarantees. A Codex
`ordinaryUsageAllowed=false` or explicit reached state is authoritative for
availability; absence of such metadata must not be interpreted as recovery.

### SQLite schema

```sql
CREATE TABLE snapshots (
  id INTEGER PRIMARY KEY,
  provider TEXT NOT NULL CHECK (provider IN ('codex','claude','github_copilot','google_antigravity')),
  scope_key TEXT NOT NULL,
  window_kind TEXT NOT NULL CHECK (window_kind IN ('rolling_5h','rolling_7d','spend','monthly','other')),
  limit_kind TEXT NOT NULL CHECK (limit_kind IN ('limited','unlimited')),
  window_duration_seconds INTEGER,
  used_percent REAL NOT NULL,
  resets_at INTEGER,
  observed_at INTEGER NOT NULL,
  availability TEXT NOT NULL CHECK (availability IN ('allowed','blocked','unknown')),
  source_version TEXT,
  quality TEXT NOT NULL CHECK (quality IN ('fresh','partial')),
  source_sequence TEXT NOT NULL,
  CHECK (used_percent >= 0),
  UNIQUE(provider, scope_key, window_kind, observed_at)
);
CREATE INDEX snapshots_latest
  ON snapshots(provider, scope_key, window_kind, observed_at DESC);

CREATE TABLE provider_state (
  provider TEXT PRIMARY KEY,
  last_success_at INTEGER,
  last_attempt_at INTEGER,
  consecutive_failures INTEGER NOT NULL DEFAULT 0,
  last_error_class TEXT
);

CREATE TABLE alert_state (
  provider TEXT NOT NULL,
  scope_key TEXT NOT NULL,
  window_kind TEXT NOT NULL,
  alert_kind TEXT NOT NULL,
  cycle_key TEXT NOT NULL,
  fired_at INTEGER NOT NULL,
  PRIMARY KEY(provider, scope_key, window_kind, alert_kind, cycle_key)
);
```

Migrations use `PRAGMA user_version`. Schema v2 transactionally added Copilot's
explicit limit classification. Schema v3 transactionally rebuilds the constrained
snapshot table to permit `google_antigravity`, with direct v1-to-v3 and v2-to-v3
paths that preserve snapshot values plus provider/alert state. Retention is
configurable (default 30 days) and pruning occurs after successful inserts. SQLite and config live under the OS
application-data directory, not the repository or provider directories.

## Codex app-server contract

### Verified lifecycle

Spawn the configured executable directly (no shell) as `codex app-server
--listen stdio://`; pipe stdin/stdout, continuously drain bounded stderr without
persisting it, and kill/reap the child on shutdown or protocol timeout. Messages
are one JSON object per line. Immediately send exactly one request:

```json
{"method":"initialize","id":1,"params":{"clientInfo":{"name":"agent_usage_dashboard","title":"Agent Usage Dashboard","version":"<app-version>"}}}
```

After its successful response, send the notification
`{"method":"initialized","params":{}}`. The official contract rejects requests
before this sequence and rejects a repeated initialize. No experimental capability
is required for the documented stable rate-limit read.

Then send `{"method":"account/rateLimits/read","id":2}` (params omitted) and
correlate the numeric/string ID without assuming response order. Never invoke
login, token-refresh, email, credit-consumption, or other mutation methods.

### Response assumptions and tolerant parsing

The documented result has a backward-compatible `rateLimits` snapshot and may
also have `rateLimitsByLimitId`. A snapshot may contain nullable `primary` and
`secondary` windows. Each present window has integer `usedPercent` and nullable
`windowDurationMins` and `resetsAt` (Unix seconds). Snapshot metadata and newer
top-level fields are optional and unknown fields are ignored.

Prefer every valid entry in `rateLimitsByLimitId`; otherwise use `rateLimits`.
Deduplicate equal bucket/window observations. Classify 300 minutes as 5-hour and
10,080 minutes as 7-day; preserve other durations without guessing. Reject only
the invalid row, not the entire response, when siblings remain usable. A response
with no valid windows is a successful-but-empty/partial observation and does not
erase the last good snapshot. Do not store raw `accountId`, credit details, plan
metadata, upsell data, or unknown fields.

The app-server also documents sparse `account/rateLimits/updated` notifications.
V1 may merge a notification into the most recent same-bucket snapshot or simply
trigger a bounded refetch; refetch is safer because nullable notification metadata
does not clear prior values. Polling remains the source of record.

Version-sensitive point: fields beyond the minimal window shape can change. At
build/test time generate schemas from the installed CLI into a temporary directory
with `codex app-server generate-json-schema --out <temp>` and compare fixtures;
do not ship or persist generated payloads. The upstream Rust protocol source is
the type-level reference for the tested revision.

## Claude Code status-line ingestion contract

Claude Code runs a configured status-line command locally, sends one JSON session
object on stdin, and consumes stdout as display text. Runs are event-driven,
debounced by 300 ms, may also use `refreshInterval`, and an overlapping update
cancels the old invocation. `rate_limits` requires Claude Code 2.1.251+, is only
present for eligible claude.ai Pro/Max users or gateway spend limits, and appears
only after the first API response. Therefore absence is normal and must never
overwrite a prior snapshot with zero usage.

Exact minimum accepted input is one top-level JSON object containing a
`rate_limits` object with at least one recognized window:

```json
{"rate_limits":{"five_hour":{"used_percentage":42.5,"resets_at":1780000000},"seven_day":{"used_percentage":17,"resets_at":1780500000}}}
```

Accepted keys are `five_hour`, `seven_day`, and `spend_limit`. Each accepted
window requires a finite numeric `used_percentage`; `resets_at` is accepted only
as a nonnegative integer Unix second and may be absent/null, producing a partial
row. Subscription percentages must be 0..=100; `spend_limit` may exceed 100.
At least one valid window is required for an insert. Unknown/malformed siblings,
all other session fields, and unknown top-level keys are ignored and never logged.
Input is size-capped (recommended 1 MiB), parsed once, and discarded immediately.

Users explicitly configure Claude's `statusLine.command` to invoke either this
binary's ingest subcommand or its transparent relay around an existing formatter.
The relay executes a program plus argument vector without constructing a shell
command, forwards the complete input even when it exceeds the ingestion cap, and
treats the wrapped program's stdout, stderr, and exit status as authoritative.
Parse, config, clock, database, alert, and retention failures only skip the
best-effort side effect; they cannot replace the formatter's result. The application
must not edit `~/.claude/settings.json` automatically. There is no supported
independent Claude poll in this architecture.

## Google Antigravity status-line ingestion contract

Antigravity is a push-only provider. It runs a user-configured status-line
command whenever agent state changes and supplies one JSON object on stdin. The
dashboard exposes `ingest antigravity` and
`relay antigravity -- PROGRAM [ARG ...]`; it does not expose refresh, watch, or
polling modes and does not launch `agy`. A user can open Antigravity's `/usage`
panel to refresh quota, after which the next status-line update is consumed.

The adapter reads at most 1 MiB and uses a narrow typed envelope plus per-bucket
`RawValue` parsing. Only `product`, allowlisted `version`, and the dynamic `quota`
map are inspected. If present, `product` must be exactly `antigravity`. Unknown
identity, workspace, conversation, transcript, model/context, VCS, task, and
agent-state fields are ignored and never reach domain objects, diagnostics, or
storage. Missing or null quota is a successful empty observation; malformed
quota shapes are sanitized document failures.

Each quota key is independently bounded and validated. A finite
`remaining_fraction` in 0..=1 maps to
`used_percent = (1 - remaining_fraction) * 100`, `WindowKind::Other`, and a
limited row; zero remaining is blocked and positive remaining is allowed. A
valid RFC3339 `reset_time` is canonical. A nonnegative `reset_in_seconds` is a
checked fallback from the one captured observation time. Relative-only, missing,
or invalid-absolute fallback is partial. When valid absolute and relative values
differ by more than 60 seconds, the accepted row retains the absolute reset and
is marked partial; it is not rejected. Valid siblings survive invalid entries,
and all accepted rows share one observation time and source sequence.

The provider-neutral `status_line_relay` transport imports no provider parser or
storage code. It launches the selected formatter directly with an argument vector,
forwards complete stdin, concurrently drains byte-exact stdout/stderr, preserves
the child exit code, and invokes a bounded best-effort callback. Raw input exists
briefly in process memory; in relay mode the complete sensitive document is also
given to the trusted formatter. Parse/storage callback failures cannot replace
formatter output or exit status.

Durable Antigravity health reuses `provider_state` with push-ingest attempts and
successes. Failure classes are limited to `oversize`, `parse`, `storage`, and
`wrapped_command`. The configured stale threshold is independent of reset time,
defaults to 900 seconds, and is bounded to 60–86,400 seconds. The dashboard makes
no Google HTTP call and does not read Antigravity settings, keyrings, environment
credentials, browser state, histories, projects, or account identifiers.

## GitHub Copilot account-quota contract

### Verified SDK surface and lifecycle

Pin `github-copilot-sdk = "=1.0.13"` and raise the project/CI minimum to Rust
1.94.0. The published crate and upstream manifest both require Rust 1.94.0. The
official Rust example calls the server-scoped typed API directly as
`client.rpc().account().get_quota().await`; no `Session` is created and no
prompt, workspace, permission handler, tool, or repository access is needed.
The result is a dynamic `quota_snapshots` map. Each value supplies
`entitlement_requests` (`-1` means unlimited), `used_requests`,
`remaining_percentage`, and an optional ISO-8601 `reset_date`. Only the category
key, validated percentage/unlimited classification, reset time, and ordinary
normalized observation metadata may cross the adapter boundary; v1 does not
persist either request counter.

Each refresh creates a fresh `Client`, bounds `Client::start`, exactly one
`get_quota`, and `Client::stop`, and attempts stop even after an RPC failure.
This short lifecycle also avoids depending on process-lifetime quota freshness.
The adapter owns a Tokio current-thread runtime and remains synchronous to its
callers. It invokes no low-level RPC other than the typed quota method and never
calls `create_session` or `resume_session`.

### Runtime, authentication, and privacy boundary

Keep the crate's default `bundled-cli` feature. For managed child-process
transports, version 1.0.13 embeds a version-matched `copilot-runtime` wrapper,
adjacent `runtime.node`, and required assets, verifies release hashes at build
time, and uses stdio with Content-Length-framed JSON-RPC. Default resolution is
an explicit `CliProgram::Path`, then `COPILOT_CLI_PATH`, then the embedded
runtime; it does **not** scan `PATH`. Therefore a separately installed Copilot
CLI is an authentication/diagnostic convenience, not a runtime prerequisite.
An advanced configured runtime override must be passed as
`CliProgram::Path(PathBuf)` without a shell and is compatibility-sensitive.
The pinned build downloads and embeds runtime 1.0.83 artifacts, increasing build
network/download volume and installed release size; release packaging must treat
those assets as trusted provider binaries rather than ordinary Rust-only output.

Authentication and GitHub network traffic remain inside GitHub's child runtime.
The dashboard never asks for or accepts a token option, reads or copies a
keychain/config/auth file, calls GitHub HTTP directly, or logs/persists child
output or raw RPC values, account IDs, prompts, transcripts, or repositories.
GitHub documents that the runtime may select inherited token environment
variables before stored OAuth or `gh` credentials; this application neither
reads nor copies those values, and error mapping must discard SDK/RPC messages
in favor of stable permit-listed classes. The runtime is trusted provider code;
its stdio and responses are untrusted input. No TCP/external transport or
listening ingestion endpoint is permitted.

### Normalization and test seams

Treat every `quota_snapshots` key as runtime-defined and independently validate
and bound it as a non-secret scope key. For finite rows, require a finite
`remaining_percentage` in 0..=100 and derive `used_percent = 100 - remaining`;
do not recompute it from counters. `entitlement_requests == -1` maps to explicit
unlimited state; values below -1 reject that row. A missing reset date yields a
partial monthly row, while a malformed non-empty date rejects only that row.
The bundled runtime 1.0.83 can stamp `reset_date` with its quota-fetch time
instead of the documented monthly reset. Because `observed_at` is captured
before the bounded startup/RPC sequence, a parsed reset within 30 seconds of
that observation is treated conservatively as missing. This prevents the last
poll time from appearing as a reset while retaining genuinely future dates.
An empty map is a successful empty observation. Valid siblings survive malformed
entries, and no account identifier or unknown field is retained.

Put the SDK behind an injected quota-client trait whose production implementation
owns start/get/stop. Unit tests use hand-written typed fixtures and a fake client
to assert one start, one quota call, one stop attempt, zero session calls, phase
timeouts, empty/dynamic maps, finite boundaries, unlimited state, reset parsing,
unsafe keys, sibling preservation, and sanitized errors. Poller tests use fake
workers/clocks so Copilot failure cannot block Codex or Claude. Storage, alert,
and TUI tests consume normalized rows only. CI and automated tests must not need
network access, an installed runtime, a GitHub account, environment token,
keychain, or live credentials; build with a fixture/fake boundary that does not
start the SDK runtime.

## Privacy and threat boundaries

Trusted: this binary, its config/database, the exact configured `codex`
executable, the bundled or explicitly overridden Copilot runtime, and locally
launched Claude/Antigravity status-line invocations and selected formatters.
Untrusted: all provider JSON, stderr,
filesystem paths from environment, database contents after tampering, and
notification text consumers.

- Resolve executable paths from explicit config or PATH, spawn with an argument
  vector and `shell=false`, and never interpolate provider/user data into commands.
- Allow only loopback process pipes; do not expose an HTTP/socket ingestion server.
- Permit-list persisted fields. No raw JSON/debug dumps; errors contain method,
  class, and request ID only. Redact/truncate child stderr in memory.
- Create app directories/files with user-only permissions where supported; SQLite
  encryption is not claimed. Treat notification bodies as public screen content.
- Cap line/input length, nesting, numeric magnitude, pending request count, and
  parse time. Malformed input cannot clear good state or crash the TUI.
- Never scan Codex/Claude/Copilot/Antigravity config, auth, keychains, history, projects,
  repositories, logs, browser profiles, prompts, or transcripts. Provider
  children may inherit or retrieve provider auth under provider control; the
  dashboard does not inspect it.

## Refresh, backoff, and staleness

Codex default polling is 60 seconds, configurable but clamped to 30 seconds–15
minutes. Only one request is in flight; each has a 10-second timeout. On process
exit/protocol/transport failure, restart with full jitter exponential backoff
(1, 2, 4 ... capped at 5 minutes), resetting after a successful read. Authentication
or unsupported-method errors back off to 15 minutes and surface action-required.
Manual refresh bypasses the delay once but never the single-flight or 5-second
anti-spam guard.

Copilot has its own worker, stop signal, provider health, and configurable
60–3,600 second cadence (300 seconds by default). `refresh copilot` performs one
read-only observation; `watch copilot` repeats at that cadence. Global
`--no-copilot-poll` disables only that worker, and the exact process-local
`--copilot-runtime PATH` override is advanced-only and never persisted. Copilot
failures are reduced to `spawn`, `timeout`, `not_authenticated`, `not_entitled`,
`protocol`, `rpc`, or `storage`, without provider message bodies.

Freshness is based on `observed_at`, never inferred from `resets_at`. Codex becomes
stale after `max(2 * interval, 3 minutes)`; Claude after 15 minutes by default
(configurable), because it cannot be polled. Copilot uses the same polling rule,
`max(2 * copilot interval, 3 minutes)`. Antigravity is also push-only and becomes
stale after 15 minutes by default, configurable from 60 through 86,400 seconds.
The TUI displays age and last error,
keeps the last good values, and never advances percentages locally. A known future
reset may count down visually, but crossing it marks the row expired/stale rather
than asserting quota recovery until a new provider observation arrives.

## Alert semantics

Low-remaining fires only on a transition from above to at/below the configured
threshold (default 20%), per provider/scope/window/reset cycle. Reset-soon fires
once when a known future reset enters the configured horizon (default 10 minutes).
Reset-observed fires only when a fresh observation demonstrates a new cycle
(`resets_at` advances materially and usage falls), never merely when wall time
passes the old reset. Stale/partial data cannot generate reset or recovery alerts.
Only finite rows participate in percentage/reset threshold evaluation. Unlimited
Copilot categories emit no low-remaining or reset-soon event and display no
misleading percentage state. Persisted claims deduplicate finite Copilot alerts
with the same provider/scope/window/cycle identity used for other providers.

`cycle_key` is the reset timestamp when known, otherwise a conservative observed-
hour bucket scoped by the provider/scope/window alert-state key. Persisted `alert_state`
prevents restarts from duplicating alerts. Threshold configuration changes do not
retroactively alert until a subsequent fresh observation. Clock jumps are handled
by reevaluating timestamps but never deleting dedupe records.

## History and TUI presentation

History and the TUI consume validated storage projections only; neither layer
parses provider data. The config stores permit-listed `provider_preferences.order`
and `provider_preferences.hidden` provider-key arrays. Reconciliation keeps the
first occurrence of each known configured key, ignores unknown keys, appends newly
known providers in the default Codex, Claude, GitHub Copilot, Google AI ·
Antigravity order, and retains hidden providers in the management list. Legacy or
empty preferences therefore show every provider in the default order.

The TUI applies this order and visibility only when laying out top-level provider
panels. `m` enters provider management; `Up`/`Down` or `j`/`k` changes selection;
`[` and `]` move a selected visible provider among visible panels; `v` toggles its
visibility; `Esc` or `m` leaves management; and `q` quits from either mode. Outside
management, `Esc` also quits. Every known provider remains selectable when all are
hidden, and the empty presentation directs the user back to `m` to restore one.
Changes are written to the resolved local config path as durable preferences
without persisting transient CLI overrides.

Visibility filtering occurs after acquisition, normalization, storage, and alert
evaluation. Hiding a provider does not disable polling or status-line ingestion,
remove normalized rows, suppress alerts or notifications, or alter history. The
polling flags remain the only dashboard controls that disable poll workers.
Providers with no quota rows do not render a panel. Configured poll-provider
health remains visible even when that provider has no panel, including stable
error class and consecutive-failure count. Snapshot age, quality, availability,
expired reset, and configured staleness remain visible without altering stored
values.

Runtime-defined Copilot category keys remain unchanged in SQLite and are
humanized only for presentation (`premium_interactions` becomes `Premium
interactions`). Limited monthly rows show the normal bar, used/left percentages,
reset, age, and stale/error styling. Unlimited rows show `Unlimited` and never a
percentage bar. History follows the same finite/unlimited and humanized-category
rules while printing only normalized bounded fields.

Antigravity quota keys remain unchanged in SQLite and are humanized only for
display (`gemini-weekly` becomes `Gemini weekly`). Empty observations update
push-ingest health without deleting or fabricating snapshots. Health distinguishes
waiting for a first observation, last-success age, and sanitized degraded state;
it never implies a polling worker.

For a Copilot monthly row with no retained provider reset, history and the TUI
show the first day of the next UTC calendar month at `00:00:00` as an explicitly
`expected` reset. The boundary is derived from that row's `observed_at`, not the
display clock, so historical rows keep stable meaning. Any retained provider
timestamp takes precedence, including a surprising or expired value, and keeps
the existing unqualified wording. Missing resets for other providers or window
kinds remain `unknown`; an unrepresentable observation also fails closed to
`unknown`. This policy-derived value is presentation-only: it is not persisted,
does not change quality, availability, freshness, or stale evaluation, and never
participates in reset alerts or alert deduplication.

## Cross-platform filesystem and notifications

Use the Rust `directories`/`ProjectDirs` abstraction for config and data roots:
roaming/local app-data on Windows, Application Support on macOS, and XDG config/
data roots on Linux. Offer `--data-dir` for tests and portable use. Paths are
canonicalized where possible, directories are created narrowly, and SQLite uses
one database per OS user.

Default alert output is an in-TUI event plus terminal bell when attached to a TTY.
Native notifications are opt-in and best-effort behind a `Notifier` trait: Windows
toast, macOS notification API/helper, and Linux freedesktop notification service.
No shell-form command construction is allowed. Unsupported/headless systems fall
back to the TUI/bell and record a nonfatal capability message.

## Contract tests and fixture discovery

- Hand-written minimal, full, null, unknown-field, malformed, oversized, negative,
  fractional, multi-bucket, and future-version JSON fixtures for provider adapters.
- A fake newline-delimited app-server verifies initialize-before-read, exactly one
  initialization, ID correlation, timeout, stderr draining, child exit/restart,
  partial notification/refetch, and forbidden-method absence.
- Claude tests pipe a whole status-line object, missing `rate_limits`, each single
  window, spend >100, missing/null reset, unknown fields, and concurrent SQLite
  writers; assert unrelated session fields never reach storage/log output.
- Golden normalization tests classify by duration and prove primary/secondary are
  not semantic labels. SQLite migration/retention/WAL and alert transition/dedupe/
  staleness tests use temporary directories and a fake clock/notifier.
- A non-CI discovery command generates the installed Codex schema in a temp dir
  and captures a manually sanitized live response only with explicit user action.
  A Claude discovery command prints the accepted minimal shape and lets the user
  pipe an actual invocation through a sanitizer that writes quota fields only.
  Sanitized fixtures require human diff review before commit; CI never needs login.
- Pin fixture provenance (CLI version, upstream commit/date, source URL) in fixture
  metadata. Unknown fields must pass; missing required minimal fields must fail.
- Copilot adapter, alert, history, and TUI automation uses normalized typed
  fixtures plus injected fake clients/clocks only. It never constructs a live
  SDK/runtime, reads authentication, or uses provider network access. Authenticated
  Copilot rendering remains an advisory manual release gate.
- Antigravity parser, CLI, relay, health, alert, history, and TUI tests use
  sanitized status fixtures, fake clocks/formatters, and temporary config/data
  paths. They never start `agy`, authenticate, or contact Google. Authenticated
  Antigravity rendering remains an advisory unclaimed manual release gate.

## Failure modes

Missing/old CLI, not logged in, API-key-only Codex auth, method/schema drift,
offline startup, child crash/hang, malformed or interleaved lines, and permission
denial all retain last-good data and expose a classified, non-secret error. Disk
full, corruption, lock contention, migration failure, or read-only app data puts
storage in degraded mode; never delete/recreate automatically. Clock skew, a reset
timestamp in the past, or a duration mismatch marks the row partial. Claude
inactivity, ineligible plans, pre-first-response payloads, workspace-trust policy,
or cancellation produces absence/staleness rather than zero usage. Notification
denial is nonfatal and falls back locally.
Copilot spawn/timeout/authentication/entitlement/protocol/RPC failures update only
Copilot health, retain last-good quota rows, and never expose raw runtime errors.
Antigravity oversize, document/product parse, storage, and wrapped-formatter
failures update only permit-listed push health. Missing/partial reset data remains
visible as partial; empty observations do not fabricate zero usage.

## Evaluated alternatives

1. **Codex acquisition. Chosen: supervised stdio app-server child.** It is the
   documented read-only integration, keeps credentials inside Codex, and is easy
   to fake in tests. Rejected: reading auth/config databases or calling private
   HTTP endpoints (credential exposure and unstable contract); parsing `/status`
   terminal text (locale/format fragile and less complete).
2. **Claude acquisition. Chosen: explicit status-line helper ingestion.** It uses
   the documented quota JSON without credential access and naturally follows
   Claude activity. Rejected: polling Claude/private web APIs (no documented local
   quota RPC and would require credentials); scraping terminal output/history
   (privacy violation and brittle). Tradeoff: Claude state can be stale when idle.
3. **Persistence/notification. Chosen: normalized append-only SQLite plus optional
   native notifier trait and default TUI/bell.** It supports history, concurrency,
   durable dedupe, and graceful platform fallback. Rejected: raw JSON files
   (over-collection, weak queries/migrations); native-only notifications (poor
   headless portability); terminal-only alerts (easy to miss when unfocused).
4. **Copilot acquisition. Chosen: official SDK account-quota RPC through the
   bundled local runtime.** It reports account-level monthly entitlement without
   giving the dashboard a token or creating a session. Rejected: `/usage`
   scraping (session-only and unstable), local database/keychain inspection
   (privacy violation), and GitHub billing HTTP APIs (different scope and direct
   credential handling).
5. **Antigravity acquisition. Chosen: documented bounded status-line ingestion
   plus provider-neutral formatter relay.** It exposes quota without dashboard
   credentials or polling. Rejected: Gemini CLI consumer integration (deprecated),
   `/usage` scraping, headless prompts, Antigravity settings/history discovery,
   and direct Google APIs. Those alternatives are unstable, consume quota, or
   cross credential/product boundaries.

## Verified facts, assumptions, and sources

Verified from official documentation/upstream source: the Codex handshake ordering,
newline JSON transport, method name and response window fields; sparse update
semantics; Claude stdin/status-line execution model, update/cancellation behavior,
version/eligibility caveats, and quota field names/types; Antigravity status-line
quota fields, installation/authentication, migration, and consumer Gemini CLI
deprecation. Local commands verified
`codex-cli 0.154.0`, `claude 2.1.278`, stdio app-server support, and schema generation.

Architecture choices (poll intervals, staleness limits, schemas, retention,
classification, alert defaults, and native notification adapters) are project
assumptions, not provider promises. The meaning of Codex primary/secondary slots,
future bucket IDs, and payload evolution must be discovered via generated schemas
and sanitized fixtures rather than inferred.

Sources consulted (accessed 2026-09-20):

- OpenAI, **Codex App Server**: https://developers.openai.com/codex/app-server/
- OpenAI upstream account protocol source:
  https://github.com/openai/codex/blob/main/codex-rs/app-server-protocol/src/protocol/v2/account.rs
- Anthropic, **Customize your status line**:
  https://code.claude.com/docs/en/statusline
- Google, **Antigravity status-line customization**:
  https://antigravity.google/docs/cli/statusline/
- Google, **Antigravity CLI installation and authentication**:
  https://antigravity.google/docs/cli-install
- Google, **Migrating from Gemini CLI**:
  https://antigravity.google/docs/cli/gcli-migration/
- Google, **Gemini Code Assist consumer account deprecation**:
  https://developers.google.com/gemini-code-assist/docs/deprecations/code-assist-individuals
- GitHub, **Copilot SDK usage and billing metrics**:
  https://docs.github.com/en/copilot/how-tos/copilot-sdk/features/usage-and-billing
- GitHub, **Copilot SDK authentication**:
  https://docs.github.com/en/copilot/how-tos/copilot-sdk/auth/authenticate
- GitHub, **Installing GitHub Copilot CLI**:
  https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/install-copilot-cli
- GitHub, **Copilot CLI command reference** (`/usage` session metrics):
  https://docs.github.com/en/copilot/reference/copilot-cli-reference/cli-command-reference
- GitHub, **official Rust SDK 1.0.13 documentation and crate metadata**:
  https://docs.rs/crate/github-copilot-sdk/1.0.13
- GitHub, **official Rust SDK manifest and runtime documentation**:
  https://raw.githubusercontent.com/github/copilot-sdk/main/rust/Cargo.toml and
  https://raw.githubusercontent.com/github/copilot-sdk/main/rust/README.md
- Locally installed CLI help/version output: `codex --version`, `codex app-server
  --help`, `codex app-server generate-json-schema --help`, `claude --version`, and
  `claude --help`.
