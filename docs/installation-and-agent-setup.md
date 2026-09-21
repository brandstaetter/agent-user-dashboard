# Installation and agent setup

This guide installs `agent-usage-dashboard` from a local checkout, makes the
installed executable available to newly started processes, and connects the four
supported agents: Codex, Claude Code, GitHub Copilot, and Google Antigravity.

The dashboard does not install or authenticate a provider CLI. Install and sign
in to Codex, Claude Code, GitHub Copilot, or Antigravity using that tool's own instructions
first. The dashboard also never edits shell profiles, `PATH`, Claude settings,
or provider authentication automatically; every persistent change below is an
explicit command or edit you control.

## Prerequisites

- Rust 1.94 or newer (`rustc --version` and `cargo --version`). The pinned
  Copilot SDK requires Rust 1.94.
- A checkout of this repository.
- For Codex data, a locally installed and authenticated `codex` command.
- For Claude data, Claude Code 2.1.251 or newer and an eligible account that
  supplies `rate_limits`. Claude Code can omit `rate_limits` until after the
  first API response, and some accounts may not supply it at all.
- For Copilot data, an active Copilot entitlement. If an organization or
  enterprise supplies it, that administrator must enable the Copilot CLI policy.
- For Antigravity data, the official `agy` CLI authenticated by the user. Google
  ended consumer-tier Gemini CLI access on June 18, 2026, so the dashboard does
  not implement a Gemini CLI provider.

The Copilot SDK's default feature downloads and embeds a version-matched runtime
and supporting assets during the build. Expect the initial build/download and
the installed release artifact to be larger than a build without Copilot support.

## Install the dashboard on `PATH`

From the repository directory containing `Cargo.toml`, run:

```text
cargo install --path .
```

Cargo installs the executable into the `bin` directory under its effective
install root. That root can be changed by `CARGO_INSTALL_ROOT` or Cargo
configuration, so do not blindly assume a fixed directory. The final
`Installing` or `Replacing` path printed by `cargo install` is authoritative for
this installation. The steps below first ask the shell for the executable and,
if it is not already on `PATH`, ask you to paste that printed path.

### Windows PowerShell

Run this in the same PowerShell window after `cargo install --path .`:

```powershell
$DashboardExe = Get-Command agent-usage-dashboard -CommandType Application -ErrorAction SilentlyContinue |
  Select-Object -First 1 -ExpandProperty Source

if (-not $DashboardExe) {
  $DashboardExe = Read-Host 'Paste the full agent-usage-dashboard.exe path printed by cargo install'
}

if (-not (Test-Path -LiteralPath $DashboardExe -PathType Leaf)) {
  throw "Dashboard executable not found: $DashboardExe"
}

$CargoBin = Split-Path -Parent $DashboardExe
$CargoBin
```

`$CargoBin` is the actual binary directory for this installation. Add it to the
current user's `PATH` (no administrator shell is required):

```powershell
$UserPath = [Environment]::GetEnvironmentVariable('Path', 'User')
$UserEntries = @($UserPath -split ';' | Where-Object { $_ })

if ($UserEntries -notcontains $CargoBin) {
  [Environment]::SetEnvironmentVariable(
    'Path',
    (($UserEntries + $CargoBin) -join ';'),
    'User'
  )
}

if (($env:Path -split ';') -notcontains $CargoBin) {
  $env:Path = "$CargoBin;$env:Path"
}
```

The final block updates only the current PowerShell process; the user-level
change is inherited by processes started later. Verify this window now:

```powershell
Get-Command agent-usage-dashboard -CommandType Application | Select-Object -ExpandProperty Source
agent-usage-dashboard --version
agent-usage-dashboard --help
```

### macOS and Linux (zsh or bash)

Run this in the same shell after `cargo install --path .`:

```sh
dashboard_exe=$(command -v agent-usage-dashboard 2>/dev/null || true)

if [ -z "$dashboard_exe" ]; then
  printf '%s\n' 'Paste the full agent-usage-dashboard path printed by cargo install:'
  IFS= read -r dashboard_exe
fi

case "$dashboard_exe" in
  /*) ;;
  *) printf '%s\n' "Expected an absolute executable path: $dashboard_exe" >&2; exit 1 ;;
esac

if [ ! -x "$dashboard_exe" ]; then
  printf '%s\n' "Dashboard executable is missing or not executable: $dashboard_exe" >&2
  exit 1
fi

cargo_bin=$(dirname "$dashboard_exe")
printf 'Cargo binary directory: %s\n' "$cargo_bin"
```

Choose the startup file for the current shell, append the discovered directory
once, and update this shell:

```sh
case "${SHELL##*/}" in
  zsh) profile="$HOME/.zshrc" ;;
  bash)
    if [ "$(uname -s)" = Darwin ]; then
      profile="$HOME/.bash_profile"
    else
      profile="$HOME/.bashrc"
    fi
    ;;
  *) printf '%s\n' 'Add the printed Cargo binary directory to your shell PATH manually.' >&2; exit 1 ;;
esac

path_line="export PATH=\"$cargo_bin:\$PATH\""
touch "$profile"
grep -Fqx "$path_line" "$profile" || printf '\n%s\n' "$path_line" >> "$profile"
export PATH="$cargo_bin:$PATH"
```

Verify this shell now:

```sh
command -v agent-usage-dashboard
agent-usage-dashboard --version
agent-usage-dashboard --help
```

### Restart processes after a PATH change

Close and reopen the terminal before relying on the persistent `PATH` change.
Also fully restart already-running Codex, Claude Code, or Copilot processes: child
processes inherit their parent's environment at startup and cannot see a later
user `PATH` update. Repeat the three verification commands in the new terminal.

## GitHub Copilot setup

Install the official Copilot CLI so you can authenticate and diagnose the same
local GitHub user context used by the provider runtime. Choose one official
installation route for your platform:

### Windows

WinGet is the native choice:

```powershell
winget install GitHub.Copilot
```

The cross-platform npm package is also supported and requires Node.js 22 or
newer:

```powershell
npm install -g @github/copilot
```

### macOS

Use Homebrew:

```sh
brew install --cask copilot-cli
```

Alternatively use the Node.js 22+ npm package or GitHub's install script:

```sh
npm install -g @github/copilot
curl -fsSL https://gh.io/copilot-install | bash
```

### Linux

Use Homebrew on Linux, the Node.js 22+ npm package, or GitHub's install script:

```sh
brew install --cask copilot-cli
npm install -g @github/copilot
curl -fsSL https://gh.io/copilot-install | bash
```

Review downloaded scripts before executing them when required by your local
security policy. GitHub also publishes platform executables for manual download.
The current official installation choices are maintained in
[Installing GitHub Copilot CLI](https://docs.github.com/en/copilot/how-tos/copilot-cli/set-up-copilot-cli/install-copilot-cli).

Verify the command, then authenticate through the interactive CLI:

```text
copilot --version
copilot
```

On first launch, enter `/login` when prompted and follow GitHub's on-screen
device-flow instructions. If Copilot is provided by an organization or
enterprise and login or use is denied, ask its administrator to enable the
Copilot CLI policy. Do not put a GitHub token in dashboard arguments, config
files, or command examples; the dashboard has no token option.

Run the dashboard's read-only account-quota check after login:

```text
agent-usage-dashboard refresh copilot
agent-usage-dashboard history --limit 10
agent-usage-dashboard
```

The dashboard calls the supported account-quota RPC and displays monthly quota
categories for the authenticated user. Copilot CLI's interactive `/usage`
command instead reports statistics for the current CLI session, including
per-model token totals; it is not the account-quota source used here.

The default path uses the SDK's bundled, version-matched runtime. Only advanced
compatibility diagnosis should select another runtime, and the global option
must precede the subcommand:

```text
agent-usage-dashboard --copilot-runtime "/absolute/path/to/copilot-runtime" refresh copilot
```

The override is an exact executable path, is passed directly without a shell,
and must be protocol-compatible with the pinned SDK. It is not a login or token
option. Use `--no-copilot-poll` to disable automatic Copilot polling, or
`watch copilot` for the non-TUI polling form.

### Copilot failure classes

Copilot failures expose only stable classes and retain the last good snapshot:

- `spawn`: the selected or bundled runtime could not start;
- `timeout`: bounded startup, quota, or shutdown work exceeded its deadline;
- `not_authenticated`: authenticate using Copilot CLI and `/login`;
- `not_entitled`: confirm the subscription and organization policy;
- `protocol` or `rpc`: the runtime and pinned SDK may be incompatible or the
  provider service may be unavailable; retry with the bundled runtime first;
- `storage`: the local SQLite operation failed; check the selected data directory.

The production SDK currently maps undocumented authentication/entitlement RPC
details conservatively to `rpc`, so begin with the same login, policy, and
connectivity checks. Provider error bodies are intentionally not displayed or
stored.

## Codex setup

Codex owns its installation, login, tokens, and account state. Confirm that the
same environment which will launch the dashboard can resolve the authenticated
CLI:

```text
codex --version
```

By default, a one-shot refresh and the dashboard resolve `codex` from `PATH`.
The dashboard directly starts `codex app-server --listen stdio://` and performs
its bounded, read-only rate-limit request; it does not read Codex credential
files or authenticate on Codex's behalf.

Perform a live one-shot refresh, then inspect the result:

```text
agent-usage-dashboard refresh codex
agent-usage-dashboard history --limit 10
agent-usage-dashboard
```

This live check requires your locally authenticated Codex installation. A
successful refresh should add Codex quota rows to history and the dashboard.

If `codex` is not on the dashboard process's `PATH`, use its absolute executable
path. The one-shot command has a provider-specific `--executable` option:

```text
agent-usage-dashboard refresh codex --executable "/absolute/path/to/codex"
```

The continuously polling dashboard instead uses the global
`--codex-executable` option:

```text
agent-usage-dashboard --codex-executable "/absolute/path/to/codex" dashboard
```

On Windows, quote a path containing spaces, for example
`"C:\Program Files\Codex\codex.exe"`. These overrides select a trusted local
program; they do not change Codex authentication or provider configuration.

## Claude Code status-line setup

Claude Code supplies one status-line JSON object on standard input. The
dashboard can either consume it directly or transparently relay it through your
existing status-line formatter. There is no independent Claude polling command.

Claude Code's user settings file is normally:

- Windows: `%USERPROFILE%\.claude\settings.json`
- macOS and Linux: `~/.claude/settings.json`

The snippets below are complete minimal JSON documents. If your file already
has other settings, merge the `statusLine` member into its existing top-level
object instead of replacing the file. JSON does not allow comments or trailing
commas.

### Direct dashboard status line

Use this when the dashboard's concise Claude summary should be the status line:

```json
{
  "statusLine": {
    "type": "command",
    "command": "agent-usage-dashboard ingest claude"
  }
}
```

Because this command uses the installed name, Claude Code must be restarted
after the PATH installation so it inherits the new environment.

### Preserve an existing formatter

Use `relay` when an existing formatter should remain authoritative. For example,
this sends the exact Claude input to `ccstatusline`, preserves that program's
stdout, stderr, and exit status, and independently attempts dashboard ingestion:

```json
{
  "statusLine": {
    "type": "command",
    "command": "agent-usage-dashboard relay claude -- npx -y ccstatusline@latest"
  }
}
```

Everything after `--` is the formatter executable and its argument vector.
Replace the example with a formatter you trust. The relay does not construct a
shell command from provider data, and an ingestion or database failure does not
replace the formatter's result.

### Absolute executable fallback

Some GUI launchers and long-running agent processes inherit a narrower `PATH`
than an interactive terminal. Use the exact executable path discovered during
installation when `agent-usage-dashboard` cannot be resolved.

Windows JSON requires each backslash to be doubled and embedded quotes to be
escaped. This example also protects a path containing spaces:

```json
{
  "statusLine": {
    "type": "command",
    "command": "\"C:\\full path\\agent-usage-dashboard.exe\" relay claude -- npx -y ccstatusline@latest"
  }
}
```

On macOS or Linux, single quotes inside the JSON string protect a path containing
spaces without needing JSON backslash escapes:

```json
{
  "statusLine": {
    "type": "command",
    "command": "'/full path/agent-usage-dashboard' relay claude -- npx -y ccstatusline@latest"
  }
}
```

The same leading absolute path can be used with `ingest claude` instead of
`relay claude -- ...`. If you use `--data-dir`, place this global option before
`ingest` or `relay` and use the same directory when opening the dashboard, for
example:

```text
agent-usage-dashboard --data-dir "/absolute/private/path" ingest claude
```

### Verify Claude ingestion

1. Save valid `settings.json`, then fully quit and restart Claude Code.
2. Make one normal Claude API request. The `rate_limits` field may not appear
   until after that response.
3. Open `agent-usage-dashboard` (or use
   `agent-usage-dashboard --no-codex-poll --no-copilot-poll` when testing Claude alone).
4. Confirm Claude rows appear, then run
   `agent-usage-dashboard history --limit 10` to confirm a recent observation.

If no Claude rows appear, check `claude --version`, confirm the account emits
`rate_limits`, validate the JSON file, try the absolute executable form, and
confirm that the status-line command and dashboard use the same `--data-dir`.
Claude becomes stale while idle because fresh data arrives only when Claude Code
runs the status-line command.

## Google Antigravity CLI setup

Google states that Gemini Code Assist for individuals, Google AI Pro, and Google
AI Ultra stopped serving requests through Gemini CLI on June 18, 2026. Affected
individual users should migrate to Antigravity; this dashboard deliberately has
no Gemini provider. Read Google's [deprecation notice](https://developers.google.com/gemini-code-assist/docs/deprecations/code-assist-individuals)
and [Antigravity migration guide](https://antigravity.google/docs/cli/gcli-migration/).
Antigravity's first launch can offer user-controlled conversion of eligible
legacy configuration. Its documented manual plugin conversion command is
`agy plugin import gemini`. The dashboard neither runs that command nor reads or
modifies either product's settings.

### Install and authenticate `agy`

Use Google's current [Antigravity installation and authentication guide](https://antigravity.google/docs/cli-install).
Review remote installer scripts as required by your security policy.

On macOS or Linux, Google's installer places `agy` under `~/.local/bin`:

```sh
curl -fsSL https://antigravity.google/cli/install.sh | bash
command -v agy
agy --version
```

On Windows PowerShell, Google's installer registers the binary under the current
user's local application data:

```powershell
irm https://antigravity.google/cli/install.ps1 | iex
Get-Command agy -CommandType Application | Select-Object -ExpandProperty Source
agy --version
```

If `agy` is not found, restart the terminal first. On macOS/Linux, confirm
`$HOME/.local/bin` is on `PATH`; on Windows, confirm
`$env:LOCALAPPDATA\agy\bin` is on the user `PATH`. Fully restart an already
running Antigravity process after changing `PATH`, because it retains the
environment inherited at startup.

Launch `agy` yourself to authenticate. The CLI uses the operating system keyring
when an existing session is available; otherwise it opens a user-controlled
browser sign-in flow. The dashboard does not initiate sign-in, inspect the
keyring, read Antigravity settings, or scan environment credentials.

### Recommended direct status line

Inside the Antigravity TUI, configure the concise dashboard status line:

```text
/statusline agent-usage-dashboard ingest antigravity
```

Antigravity pipes one JSON document to the command whenever agent state changes.
The dashboard accepts at most 1 MiB, persists only normalized quota fields, and
prints a sanitized provider plus accepted/rejected bucket count. It never echoes
bucket keys or the raw document.

Antigravity is push-only. Open `/usage` to ask Antigravity to refresh model quota;
the dashboard receives that state on the next status-line update. There is no
dashboard `refresh antigravity` or `watch antigravity` command. `/credits` is not
included because the documented status JSON does not expose a machine-readable
credit balance.

### JSON configuration and absolute-path fallback

As an alternative to `/statusline`, merge a `statusLine` member into the
Antigravity settings document described by Google's [status-line guide](https://antigravity.google/docs/cli/statusline/). Do not
replace unrelated settings. A PATH-based command is:

```json
{
  "statusLine": {
    "type": "command",
    "command": "agent-usage-dashboard ingest antigravity"
  }
}
```

If a GUI or long-running process cannot resolve the dashboard, use the exact
executable path discovered during installation. Windows JSON requires doubled
backslashes and escaped quotes around a path containing spaces:

```json
{
  "statusLine": {
    "type": "command",
    "command": "\"C:\\full path\\agent-usage-dashboard.exe\" ingest antigravity"
  }
}
```

On macOS or Linux, single quotes inside the JSON string can protect a path with
spaces:

```json
{
  "statusLine": {
    "type": "command",
    "command": "'/full path/agent-usage-dashboard' ingest antigravity"
  }
}
```

Place global dashboard options before the subcommand. For example, use the same
private directory for the status line and interactive dashboard:

```text
agent-usage-dashboard --data-dir "/absolute/private/path" ingest antigravity
```

The persisted `antigravity_stale_after_seconds` setting defaults to 900 and
accepts 60 through 86,400. An explicit override also precedes the subcommand:

```text
agent-usage-dashboard --antigravity-stale-after-seconds 600 ingest antigravity
```

### Preserve an existing formatter

Use the provider-neutral relay when another formatter must remain authoritative:

```text
agent-usage-dashboard relay antigravity -- /absolute/path/to/formatter --its-argument
```

The relay launches that executable directly with its argument vector—never
through a constructed shell command—and preserves its stdout, stderr, and exit
status. Local parsing, storage, and alerts are best effort. The formatter receives
the complete original Antigravity JSON, including sensitive identity, workspace,
conversation, transcript, and model fields. Only wrap a formatter you trust, and
do not put credentials in its arguments.

### Verify and troubleshoot Antigravity ingestion

Automated project verification uses sanitized fixtures and temporary config/data
paths only. It proves parsing, persistence, current/history/health rendering,
staleness, and relay stream/exit behavior without authenticating or contacting
Google. Live authenticated rendering remains an advisory manual release check
and is not claimed by those tests.

For a user-run live check:

1. Confirm both `agy --version` and `agent-usage-dashboard --version` resolve in
   a newly opened terminal, then restart Antigravity.
2. Configure direct ingestion or the trusted relay, open `/usage`, and trigger
   the next status-line update.
3. Open the dashboard and run `agent-usage-dashboard history --limit 10`; confirm
   Google Antigravity current, history, push-health, and stale behavior.
4. Review the SQLite directory and output only as normalized operational data;
   do not expect raw JSON, account identity, workspace, or conversation fields.

Troubleshooting:

- `oversize` means the document exceeded 1 MiB and was not ingested.
- `parse` means the document shape/product was invalid or no quota bucket was
  usable. Invalid siblings are counted without exposing their keys; valid
  siblings can still persist.
- Missing reset data and relative-only resets remain usable but are marked
  partial. If absolute and relative resets differ by more than 60 seconds, the
  canonical absolute reset is retained and the row is marked partial.
- `wrapped_command` means the trusted formatter failed or returned nonzero;
  verify its executable path and arguments. Its streams and exit remain
  authoritative even when dashboard ingestion succeeds.
- No data or stale rows usually mean no recent status-line update. Open `/usage`,
  then cause a new update; an empty quota observation updates health but does not
  fabricate or delete snapshots.
- If either executable is missing, restart the terminal and Antigravity, verify
  `PATH`, then use the dashboard's absolute-path JSON form.

## Settings and argument safety

The application never edits Claude or Antigravity settings automatically. Back up and review
your existing settings before changing them. Do not put API keys, tokens,
passwords, cookies, or other secrets in relay arguments: command arguments may
be visible to local process-inspection tools, and the trailing arguments name a
local program that you are choosing to trust.

Provider credentials remain under provider-owned runtimes and CLIs. The Copilot
runtime may use its own keychain or inherited authentication and makes GitHub
network requests; the dashboard neither inspects that authentication nor makes
direct provider HTTP requests. For the
full data-flow and local-storage boundaries, see
[Credential safety](credential-safety.md).
