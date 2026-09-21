# Credential safety and local privacy

Agent Usage Dashboard is designed to display quota data without taking ownership of provider credentials. It is one local Rust program: it normalizes a small set of usage fields into a local SQLite database and does not include a direct Codex, Claude, GitHub, or Google HTTP client.

This document describes the implemented boundary, not an absolute security guarantee. The dashboard, its database, the selected Codex executable, the bundled or overridden Copilot runtime, and any status-line formatter still run with your local user permissions.

## How quota data reaches the dashboard

### Codex: locally authenticated, read-only app-server child

The dashboard starts the configured `codex` executable directly as a child process:

```text
codex app-server --listen stdio://
```

It communicates over the child's standard input and output, performs the required `initialize` / `initialized` handshake, and sends only `account/rateLimits/read`. It does not send login, token-refresh, credit-consumption, or other provider mutation requests. The child is stopped after the bounded request.

Authentication remains under Codex control. The dashboard does not open Codex authentication or configuration files and does not receive or persist an OAuth token or API key. The locally installed Codex program may use its own authentication and network connection to answer the request; that program is therefore part of the trusted local boundary.

Codex protocol lines are size-bounded. The dashboard extracts permitted quota fields from the response and discards the parsed response. Codex child stderr is drained only to prevent the child from blocking; its contents are not logged or stored.

### GitHub Copilot: provider-owned runtime and account quota

Each Copilot refresh starts a short-lived, version-matched provider runtime through the pinned official SDK, calls only the typed account-quota RPC, and attempts to stop the runtime. It does not create or resume a chat session, enable tools, access a workspace, or request repository contents. The dynamic quota map is independently validated per category, converted into normalized monthly limited/unlimited rows, and then discarded.

The dashboard never asks for, reads, copies, logs, or persists GitHub tokens, keychain contents, cookies, account IDs, prompts, transcripts, repositories, or raw quota payloads. It has no GitHub token argument or configuration field. Authenticate through Copilot CLI—normally by launching `copilot` and using `/login` on first use—and never place tokens in dashboard arguments, config files, or examples.

GitHub's runtime may use its own keychain or inherited authentication environment and makes GitHub network requests. The dashboard neither inspects that authentication nor makes direct provider HTTP requests. The optional exact `--copilot-runtime PATH` selects a trusted compatible executable; it is not an authentication mechanism. Copilot CLI's `/usage` reports current-session statistics, while the dashboard's account-quota RPC reports monthly account allowance.

### Claude Code: bounded status-line input

Claude Code invokes the configured status-line command and supplies one JSON value on standard input. The dashboard accepts at most 1 MiB for ingestion, parses the `rate_limits` object once, extracts recognized `five_hour`, `seven_day`, and `spend_limit` windows, and discards the parsed value and input buffer after that invocation. Other session fields and unknown keys are ignored and do not reach SQLite.

There are two supported local paths:

- `ingest claude` consumes the bounded value for quota ingestion.
- `relay claude -- PROGRAM [ARG ...]` forwards the complete original input to the configured formatter, then performs the same bounded ingestion as a best-effort side effect. If the input exceeds the cap, it is still forwarded to the formatter but is not ingested.

The relay launches the formatter directly with an argument vector rather than constructing a shell command. It passes through the formatter's stdout, stderr, and exit status without storing them. Because the formatter receives the complete Claude status-line value, only wrap a local formatter you trust.

### Google Antigravity: bounded status-line input

Antigravity invokes the user-configured status-line command and supplies one JSON object on standard input when agent state changes. `ingest antigravity` accepts at most 1 MiB, inspects only `product`, an allowlisted CLI `version`, and the dynamic `quota` map, then discards the input. Quota buckets are validated independently. The dashboard never retains email, plan tier, model/context data, working or project paths, VCS data, conversation/session IDs, transcript paths, or other unknown fields.

`relay antigravity -- PROGRAM [ARG ...]` uses the same provider-neutral transport as Claude. It forwards the complete original stream to the chosen formatter and preserves the formatter's stdout, stderr, and exit status; bounded dashboard ingestion is silent and best effort. Because the formatter receives every sensitive field in the original Antigravity payload, it is inside the trusted local boundary. The relay starts the program directly with a separate argument vector and never builds a shell command.

Authentication and Google network access remain entirely under `agy` control. The user launches `agy` and completes its browser/keyring sign-in. The dashboard does not open Antigravity settings, inspect the OS keyring, scan environment credentials, invoke authentication, or contact Google. Raw status JSON exists briefly in bounded process memory and, in relay mode, is also passed to the user-selected formatter before being discarded.

## What is stored

`usage.sqlite3` contains only normalized operational records. The snapshot allowlist is:

- `id`: a local SQLite row ID;
- `provider`: `codex`, `claude`, `github_copilot`, or `google_antigravity`;
- `scope_key`: a sanitized, non-account provider category/bucket key;
- `window_kind`, `limit_kind`, and `window_duration_seconds`: the classified rolling, spend, monthly, or other window; its `limited`/`unlimited` state; and its optional duration;
- `used_percent`: the validated normalized used percentage for a finite allowance (unlimited rows retain an explicit limit classification rather than presenting this as consumption);
- `resets_at` and `observed_at`: the optional reset time and local observation time;
- `availability`: `allowed`, `blocked`, or `unknown`;
- `source_version`: the optional source CLI version;
- `quality`: `fresh` or `partial`; and
- `source_sequence`: a generated UUID used to group rows from one observation.

The database also keeps bounded operational state:

- Provider health: `provider`, `last_success_at`, `last_attempt_at`, `consecutive_failures`, and `last_error_class`. Raw error text is not stored.
- Alert deduplication: `provider`, `scope_key`, `window_kind`, `alert_kind`, `cycle_key`, and `fired_at`.

The dashboard configuration file is separate from SQLite. Its complete allowlist is `codex_refresh_seconds`, `copilot_refresh_seconds`, `claude_stale_after_seconds`, `antigravity_stale_after_seconds`, `retention_days`, `low_remaining_percent`, `reset_soon_seconds`, `native_notifications`, and the provider-key arrays `provider_preferences.order` and `provider_preferences.hidden`. The preference arrays control presentation only. The file has no fields for provider credentials, runtime paths, or provider authentication. Antigravity's stale threshold defaults to 900 seconds and is bounded to 60–86,400 seconds.

## What is excluded

The dashboard does not read provider auth/config files, keychains, browser profiles, provider histories, projects, prompts, or transcripts. In particular, it never asks for, reads, copies, logs, or persists GitHub tokens, keychain contents, cookies, account identifiers, prompts, transcripts, repositories, or raw quota payloads. It does not intentionally collect or persist:

- passwords, credentials, API keys, OAuth tokens, or refresh tokens;
- browser cookies or browser storage;
- prompts, responses, transcripts, conversations, or message histories;
- account, user, email, conversation, project, organization, or repository identifiers;
- raw Codex responses, raw Claude or Antigravity status-line payloads, or raw Copilot quota/RPC payloads;
- provider configuration or provider history;
- Codex child stderr or wrapped formatter stderr; or
- unknown provider fields and raw error bodies.

There is no direct provider HTTP client in the dashboard, no remote synchronization or cloud dashboard service, and no listening HTTP, socket, or other network ingestion server. Acquisition is limited to the directly spawned Codex stdio child, explicit Claude Code and Antigravity status-line invocations, and the provider-owned Copilot runtime's local stdio RPC. Provider-owned processes—not the dashboard—own authentication and upstream network traffic.

## Residual privacy risks

- **SQLite is not encrypted by this application.** Anyone or any process that can read the database files can inspect the normalized records. Disk encryption supplied by the operating system is outside the dashboard's control.
- **Usage and timestamps are still personal operational data.** They can reveal when a provider was used, quota consumption patterns, reset schedules, provider health, and alert activity even though they contain no prompt text or account identifier.
- **Terminal and notification text can be observed.** Screenshots, scrollback, screen sharing, terminal capture, native notification history, and people near the display may expose usage and reset information.
- **Configured executables are trusted local programs.** The selected Codex executable runs under your account and controls its own authentication. A wrapped Claude or Antigravity formatter receives the full original status-line value. PATH replacement or an untrusted executable can exceed the dashboard's privacy boundary.
- **Raw status input exists briefly in memory.** Claude and Antigravity documents are bounded, parsed once, and discarded, but could still appear in process memory or a crash dump. Relay mode necessarily gives the complete document to the formatter you selected.
- **The bundled or overridden Copilot runtime is trusted provider code.** It runs with your local user permissions, may access its own credential store and network, and is larger than the dashboard-only code. An exact runtime override expands this trust decision.
- **Copilot quota keys can evolve.** Category keys are runtime-defined. The dashboard validates and stores bounded keys, but an SDK/runtime upgrade can add, remove, or rename categories and change what appears in history.
- **Local deletion is not secure erasure.** SQLite pages, WAL/SHM files, filesystem snapshots, backups, crash dumps, and storage-device behavior may retain older bytes after records or files are deleted.

## Choose and review a private data directory

Use an absolute `--data-dir` that belongs to your user account, is not inside a shared or synchronized folder, and is not a repository you may publish. The directory contains `usage.sqlite3` and may temporarily contain SQLite `-wal` and `-shm` companion files.

Windows PowerShell example:

```powershell
$dashboardData = Join-Path $env:LOCALAPPDATA "AgentUsageDashboardPrivate"
New-Item -ItemType Directory -Force -Path $dashboardData
icacls $dashboardData
agent-usage-dashboard --data-dir $dashboardData dashboard
```

`icacls` shows the effective inherited ACL entries. Confirm that unexpected local users or groups do not have read access; apply your organization's Windows ACL policy if the inherited permissions are too broad.

macOS example:

```sh
dashboard_data="$HOME/Library/Application Support/Agent Usage Dashboard Private"
mkdir -p "$dashboard_data"
chmod 700 "$dashboard_data"
ls -lde "$dashboard_data"
agent-usage-dashboard --data-dir "$dashboard_data" dashboard
```

Linux example:

```sh
dashboard_data="${XDG_DATA_HOME:-$HOME/.local/share}/agent-usage-dashboard-private"
install -d -m 700 "$dashboard_data"
ls -ld "$dashboard_data"
agent-usage-dashboard --data-dir "$dashboard_data" dashboard
```

These checks review the directory itself; also review parent-directory access, operating-system backup policy, synchronization software, and any endpoint-management rules that apply on your machine. The application creates missing directories but does not promise to replace or harden inherited platform permissions.

Use the same `--data-dir` before every command that should share this database, including `relay`, `ingest`, `history`, and the dashboard. If it is part of a Claude Code or Antigravity status-line command, place the global option before the subcommand.

## Sharing, backups, retention, and removal

Before sharing a screenshot, terminal transcript, `history` output, diagnostic bundle, or database, review it for usage percentages, times, bucket keys, CLI versions, provider health, alert history, and any surrounding terminal content. Prefer sharing a narrowly cropped or manually redacted view instead of the database.

Treat backups of the data directory as having the same sensitivity as the live database. To make a simple file-level backup, first stop the dashboard and other writers, including Claude status-line ingestion, then copy the dedicated data directory. Protect the destination with appropriate access controls and encryption. Copying only `usage.sqlite3` while a writer is active may omit data still represented by SQLite's WAL files.

`retention_days` controls pruning of snapshot rows and alert-deduplication records after a successful observation; the default is 30 days. It does not promise immediate byte removal, wipe SQLite free pages, clear backups, or remove provider-health state. Choose a shorter supported value if you need less operational history.

To remove local dashboard data:

1. Remove or disable the Claude and Antigravity status-line integrations and stop dashboard/watch processes so the files are not recreated.
2. Confirm the exact active data directory. Do not delete a broad parent directory that contains unrelated files.
3. Delete `usage.sqlite3` and any matching `usage.sqlite3-wal` and `usage.sqlite3-shm` files, or delete the dedicated `--data-dir` if it contains nothing else.
4. Delete any backups separately. Delete the dashboard config only if you also want to remove its non-secret operational settings.

Ordinary file deletion is not a secure-erasure guarantee. For stronger disposal requirements, follow the operating system, encrypted-volume, backup, and device-retirement procedures appropriate to your threat model.
