# ✨ Osmium — Windows Service Generator Framework

<p align="center">
  <img src="https://img.shields.io/github/followers/NXRKYMANE?style=social" />
  <img src="https://img.shields.io/github/forks/NXRKYMANE/Osmium" />
  <img src="https://img.shields.io/github/stars/NXRKYMANE/Osmium" />
  <img src="https://img.shields.io/badge/-Rust-000000?style=flat&logo=rust&logoColor=white" />
  <img src="https://img.shields.io/badge/Douyin-Ozones-000000?style=flat&logo=tiktok&logoColor=white" />
  <img src="https://img.shields.io/badge/QQ-946777609-12B7F5?style=flat&logo=tencentqq&logoColor=white" />
  <img src="https://komarev.com/ghpvc/?username=NXRKYMANE&repo=Osmium&label=Views&color=00BFFF&style=flat" />
</p>

Register any executable or script as a Win32 system service. [中文文档](Docs/README_CN.md)

> The project is based on [WinSW 2](https://github.com/winsw/winsw).
> Osmium keeps most of WinSW's features, written in **Rust**, with some advanced features provided through OSX plugins so they can be extended whenever needed.

> The project is now fairly stable, though a few minor issues may still surface. Thank you for your understanding, fellow developers.

## Rust Implementation

Osmium is written in modern Rust (edition 2024) and compiles into a standalone `osmium64.exe` (installed as `os.exe`) plus an official (yours truly) advanced plugin `osmium64-official-kits.osx`:

| Item        | Detail                                                                         |
| --- | --- |
| Language    | Rust 2024                                                                      |
| Artifacts   | `Publish\osmium64.exe` `Publish\osmium64-official-kits.osx`                                    |
| Size        | `osmium64.exe` ~3.6 MB, `osmium64-official-kits.osx` ~1.9 MB (size-first compile, opt-level=z) |
| UPX build   | `Publish\osmium64-upx.exe` (~1.1 MB)                                                 |
| Installer   | `osmium-win-x64-setup-v<VERSION>.exe` (non-UPX build)                          |
| Build tools | Rust stable + MSVC                                                             |

> Don't want the platform framework? Embedding into your own project? I'd recommend the UPX build (`osmium64-upx.exe`) — tiny, extensible and very lightweight, and cold start is barely different from the original.
> Missing a feature? The project is plugin-everything: write your own plugin in any language and place it under the executable (e.g. `exts\` on platform installs) — see the [Extension Guide](Docs/EXTENSION_EN.md) for full plugin development and usage; a green dot on `os --extend` means your plugin is usable.

> Note: platform deployment needs the framework installed via the installer; all lifecycle, logging and service management are done by the core program os.exe. Without it, services cannot start — reinstalling the framework restores everything.
> Relying on the framework keeps your own project and config simpler; if you're unsure or the project is important, use the embedded approach — plugins can be swapped freely and all operations and logs stay inside your project.
> An osiml file is actually just a toml file, renamed only for convenience.

## Quick Start

```powershell
# Install (requires administrator)
os --install <svc.toml>
# Quick install (--pth or --path): register a simple service by name + executable path (auto-generates config, deploys .osiml to the svc directory)
os --install <my-service> --pth C:\app\myapp.exe

# Manage services
os --start     <my-service>
os --stop      <my-service>
os --restart   <my-service>
os --refresh   <my-service>
os --kill      <my-service>
os --status    <my-service>
os --uninstall <my-service>
os --delete    <my-service>
os --list
```

## Commands

| Command                                    | Usage                                                                                                                                          |
| --- | --- |
| `--install <toml>`                         | Install / update a service                                                                                                                     |
| `--install <name> --pth/--path <exe path>` | Quick install: auto-generates config and deploys `.osiml` (not needed for embedded projects)                                                   |
| `--uninstall <name>`                       | Stop and uninstall a service                                                                                                                   |
| `--start <name>`                           | Start a service                                                                                                                                |
| `--stop <name>`                            | Stop a service                                                                                                                                 |
| `--restart <name>`                         | Restart a service                                                                                                                              |
| `--refresh <name>`                         | Refresh SCM service properties (display name / description / start type / account / recovery, etc.) from the deployed config without reinstalling |
| `--kill <name>`                            | Admin/dev tool: force-kill the service's target process tree (via `WINSGF_SERVICE_ID`; short alias `--kil`)                                          |
| `--status <name>`                          | Query service status                                                                                                                           |
| `--delete <name>`                          | Force delete (stop + uninstall)                                                                                                                |
| `--list`                                   | List all platform-deployed services (excludes inplace embedded services)                                                                       |
| `--extend`                                 | List installed plugins with availability check (green dot / red dot; short alias `--ext`; plugin dev: [Extension Guide](Docs/EXTENSION_EN.md)) |
| `--test <config>`                          | Run the service in a foreground console without installing (debug only; deploy dir = config dir, `%BASE%` points there; short alias `--tst`)   |
| `help` / `-h` / `--help`                   | Print help text                                                                                                                                |

> Management commands are equivalent to the legacy `-m --xxx` form (the prefix is optional); after a framework install you can use the `os` shortcut alias instead of `os.exe`.

> Every command has a short alias: `--ins` / `--uin` / `--str` / `--stp` / `--rst` / `--rfs` / `--kil` / `--sts` / `--del` / `--lst` / `--tst` / `--ext` (install / uninstall / start / stop / restart / refresh / kill / status / delete / list / test / extend).

> The service name `Osmium Service Refresher` is reserved; service names are validated: empty names, `.` / `..` (path traversal), path separators and control characters are rejected, length capped at 256.

## Config Reference

The config file is **TOML**. When a service is registered, the config is deployed as `<name>.osiml` to `C:\ProgramData\Osmium\svcs\<name>\` (shown with the Osmium service-config icon in Explorer); inplace embedded mode uses a `.toml` named after the exe.

### Required Fields

```toml
service_name = "My-Service"
service_display_name = "My Service"
service_description = "Service description"
service_executable_path = 'C:\app\myapp.exe'
```

> TOML note: paths containing backslashes must use **single-quoted literal strings** (as above) — in a basic string like `"C:\app\..."` the `\a` is an illegal escape and parsing fails.

### Basic Features

| Field                     | Type   | Default       | Description                                                                                                                                                                                                                                                                                                                                   |
| --- | --- | --- | --- |
| `service_executable_args` | string | `""`          | Command-line arguments for the target executable (passed through verbatim, quotes preserved)                                                                                                                                                                                                                                                  |
| `start_arguments`         | string | none          | Start-time arguments that override `service_executable_args` (WinSW `startarguments`)                                                                                                                                                                                                                                                         |
| `service_start_mode`      | string | `"automatic"` | Startup type: `automatic`, `delayed_auto`, `manual`, `disabled`                                                                                                                                                                                                                                                                               |
| `service_dependencies`    | string | none          | Semicolon-separated list of services that must start first (e.g. `"EventLog;WinRM"`)                                                                                                                                                                                                                                                          |
| `service_account`         | string | `LocalSystem` | Windows account to run the service as (e.g. `"NT AUTHORITY\\NetworkService"`)                                                                                                                                                                                                                                                                 |
| `service_password`        | string | `""`          | Password for `service_account` (only needed for user accounts)                                                                                                                                                                                                                                                                                |
| `env`                     | object | none          | Environment variables injected into the target process (values support `%VAR%` expansion; `%BASE%` means the deploy directory). The host also auto-injects `BASE` (deploy directory) and `WINSGF_SERVICE_ID` (service name, used by RunawayProcessKiller anti-miskill checks) — an explicit user `env` value for `BASE` wins over the default |
| `working_directory`       | string | exe dir       | Working directory for the target process; relative paths resolve against the service directory                                                                                                                                                                                                                                                |
| `process_priority`        | string | `normal`      | Target process priority: `idle` / `belownormal` / `normal` / `abovenormal` / `high` / `realtime`                                                                                                                                                                                                                                              |

**Config-wide expansion**: `%VAR%` environment variables and the special `%BASE%` (the service deploy/config directory) are expanded across the whole config — `service_executable_path`, `service_executable_args`, `start_arguments`, `working_directory`, `download_url`, `download_to`, `stop_executable`, `stop_arguments`, `log_dir`, `runaway_pid_file`, shared mapper paths, and `env` values (WinSW-compatible). Hook commands are shell commands and are not expanded. `%PID%` is a reserved placeholder: config expansion leaves it untouched, and it is replaced with the target process PID only when running the stop command (WinSW #217).

### Advanced — Lifecycle & Hooks

| Field                       | Type   | Default   | Description                                                                                                                                                                                                                                                                                                                        |
| --- | --- | --- | --- |
| `failure_reset_sec`         | int    | `86400`   | Failure counter reset period in seconds                                                                                                                                                                                                                                                                                            |
| `restart_delay_ms`          | int    | `60000`   | Delay before auto-restart after crash in milliseconds                                                                                                                                                                                                                                                                              |
| `kill_process_tree`         | bool   | `true`    | Whether to force-kill the whole process tree on stop                                                                                                                                                                                                                                                                               |
| `prestart_command`          | string | none      | Hook run before launching the target (`cmd /c` semantics, failure is non-fatal; killed after 60s timeout)                                                                                                                                                                                                                          |
| `poststop_command`          | string | none      | Hook run after the target stops (injects `WINSGF_CHILD_PID` / `WINSGF_CHILD_EXIT_CODE`)                                                                                                                                                                                                                                            |
| `auto_refresh`              | bool   | `false`   | Config hot-reload (WinSW `autoRefresh`): the host watches the config file's mtime and gracefully restarts the target when it changes; a failed reload keeps the previous configuration running                                                                                                                                     |
| `stop_executable`           | string | none      | Program run first when stopping the service (graceful drain; the host then waits for the target to exit)                                                                                                                                                                                                                           |
| `stop_arguments`            | string | `""`      | Command-line arguments for `stop_executable` (passed through verbatim, quotes preserved); `%PID%` is replaced with the target process PID and `WINSGF_CHILD_PID` is injected (WinSW #217)                                                                                                                                          |
| `interactive`               | bool   | `false`   | Register the service as interactive with the desktop (`SERVICE_INTERACTIVE_PROCESS`)                                                                                                                                                                                                                                               |
| `failure_action`            | string | `restart` | Failure-recovery action: `restart` / `reboot` / `none`                                                                                                                                                                                                                                                                             |
| `failure_actions`           | array  | none      | Failure-recovery action chain: `[{ action = "restart", delay_secs = 10 }, { action = "reboot" }]` — each failure takes the next action, the last one repeats; `restart` / `reboot` / `none` (invalid entries filtered). When absent, `failure_action` + `restart_delay_ms` build the chain (3 restarts then stop, legacy behavior) |
| `stop_timeout_secs`         | int    | `10`      | Graceful-stop timeout in seconds (WinSW `stoptimeout`)                                                                                                                                                                                                                                                                             |
| `hide_window`               | bool   | `true`    | Launch the target with `CreateNoWindow`; set `false` to let it create a console window (WinSW `hidewindow`)                                                                                                                                                                                                                        |
| `stop_parent_process_first` | bool   | `false`   | When force-killing, terminate the parent before its subtree (WinSW `stopparentprocessfirst`)                                                                                                                                                                                                                                       |
| `allow_service_logon`       | bool   | `false`   | When a custom service account is used, automatically grant it the "Log on as a service" right                                                                                                                                                                                                                                      |
| `event_log`                 | bool   | `false`   | Also write to the Windows Event Log (informational level, source `Osmium`)                                                                                                                                                                                                                                                         |
| `security_descriptor`       | string | none      | Service security descriptor (SDDL) applied to the service DACL at install — controls who can manage the service (WinSW `securityDescriptor`)                                                                                                                                                                                       |
| `preshutdown`               | bool   | `false`   | Advertise `SERVICE_ACCEPT_PRESHUTDOWN` so the SCM grants extra time for graceful shutdown                                                                                                                                                                                                                                          |
| `extensions`                | array  | none      | Extra lifecycle extension commands: `[{ phase = "start", command = "...", stdout_path?, stderr_path? }]` — `start` runs before launch, `start_after` after launch, `stop_before` before stop, `stop` after stop; failures are non-fatal. `stdout_path` / `stderr_path` redirect the hook output to standalone files                |
| `plugins`                   | array  | none      | Lifecycle plugin calls (`.osx` plugins next to the executable): `[{ kit, phase, payload?, fail_on_error? }]` — see the [Extension Guide](Docs/EXTENSION_EN.md)                                                                                                                                                                                              |

### Advanced — Resource Watchdog & Network Mapping

| Field                         | Type   | Default | Description                                                                                                                                                                                                                                                                                        |
| --- | --- | --- | --- |
| `runaway_cpu_limit`           | float  | none    | RunawayProcessKiller: kill the child when its CPU usage (kernel+user delta over wall time, all cores summed) exceeds this percentage                                                                                                                                                               |
| `runaway_memory_limit_mb`     | int    | none    | RunawayProcessKiller: kill the child when its working set exceeds this many MB                                                                                                                                                                                                                     |
| `runaway_check_interval_secs` | int    | `30`    | RunawayProcessKiller sampling interval in seconds                                                                                                                                                                                                                                                  |
| `runaway_pid_file`            | string | none    | PID file for startup cleanup: at service start, a leftover process with this PID is killed, then the new child PID is written and removed on stop. Only processes carrying this service's `WINSGF_SERVICE_ID` are killed (WinSW #237: prevents killing an unrelated process when a PID was reused) |
| `runaway_stop_timeout_ms`     | int    | `5000`  | Graceful-stop timeout for the leftover process during startup cleanup, then force-kill                                                                                                                                                                                                             |
| `runaway_stop_parent_first`   | bool   | `false` | During startup cleanup, kill the parent process before its children                                                                                                                                                                                                                                |
| `shared_directory_mappers`    | array  | none    | SharedDirectoryMapper: map network shares at service start and disconnect at stop: `[{ local_path = "Z:", remote_path = "\\\\server\\share", username?, password? }]`                                                                                                                              |

### Advanced — Pre-Start Download

| Field                    | Type   | Default        | Description                                                                                                                                                                                                                                                                                                                                                                 |
| --- | --- | --- | --- |
| `download_url`           | string | none           | URL to fetch the target executable before launch (when the target exists and no `download_sha256` is set, `If-Modified-Since` is sent and re-download is skipped on HTTP 304)                                                                                                                                                                                               |
| `download_to`            | string | none           | Download destination; relative paths resolve against the service directory                                                                                                                                                                                                                                                                                                  |
| `download_sha256`        | string | none           | SHA-256 of the downloaded file (lowercase hex)                                                                                                                                                                                                                                                                                                                              |
| `download_fail_on_error` | bool   | `true`         | Whether a failed download fails service startup                                                                                                                                                                                                                                                                                                                             |
| `download_auth`          | string | none           | Download authentication: `basic` (user/password), or `sspi` (Windows integrated Negotiate/NTLM/Kerberos) — `sspi` is handled by the official `osmium-kit-sspi` plugin (shipped in `osmium64-official-kits.osx`); without the plugin the download fails with a clear error                                                                                                             |
| `download_username`      | string | none           | Username for `basic` authentication                                                                                                                                                                                                                                                                                                                                         |
| `download_password`      | string | none           | Password for `basic` authentication                                                                                                                                                                                                                                                                                                                                         |
| `download_proxy`         | string | none           | Proxy used for downloads (http or https)                                                                                                                                                                                                                                                                                                                                    |
| `download_unzip`         | bool   | `false`        | Auto-extract the downloaded file when it is a zip (zip-slip traversal is blocked)                                                                                                                                                                                                                                                                                           |
| `download_stage`         | string | `before_start` | When the download runs: `before_start` (ensure the executable before launch), `after_start` (extra resource after the target launches), `after_stop` (extra resource after stop). Only `before_start` participates in startup executability checks                                                                                                                          |
| `download_threads`       | int    | `16`           | Max chunked-download thread count; `0`/`1` disables multi-threading (single-threaded fallback)                                                                                                                                                                                                                                                                              |
| `downloads`              | array  | none           | Multiple download entries (WinSW `download` list): `[{ from, to, sha256?, fail_on_error?, auth?, username?, password?, unsecure_auth?, proxy?, unzip?, stage? }]` — omitted fields fall back to the top-level `download_*` values; when configured, the array takes precedence over the single `download_url` entry and the executable path stays `service_executable_path` |
| `download_unsecure_auth` | bool   | `false`        | Explicitly allow `basic` authentication over plain `http://` (WinSW `unsecureAuth`); default refuses because credentials would be sent in cleartext                                                                                                                                                                                                                         |

> Security note: with `http://` and no `download_sha256`, `fail_on_error=true` refuses to start (protects against tampering in transit). `basic` auth over plain `http://` is refused unless `download_unsecure_auth = true`.
> Secrets: `service_password`, `download_password` and mapper `password` are DPAPI-encrypted (machine scope, ciphertext marked with the versioned `enc:OSMIUM1:` prefix) in the deployed `.osiml` — plaintext never lands on disk; legacy plaintext configs keep working.

### Advanced — Logging

| Field                  | Type   | Default    | Description                                                                                                                                                                                                                                                                               |
| --- | --- | --- | --- |
| `log_enabled`          | bool   | `true`     | Whether host logs are written                                                                                                                                                                                                                                                             |
| `log_dir`              | string | none       | Log directory; relative paths resolve against the service directory                                                                                                                                                                                                                       |
| `log_max_size_mb`      | int    | `0`        | Max log file size (MB) before rollover; `0` means unlimited                                                                                                                                                                                                                               |
| `log_max_backup_count` | int    | `5`        | Number of rolled-over backups to keep                                                                                                                                                                                                                                                     |
| `log_split_out_err`    | bool   | `false`    | Write child stderr to a separate `yyyy-MM-dd.err.log`                                                                                                                                                                                                                                     |
| `log_zip`              | bool   | `false`    | Zip a rolled-over backup, and expired logs during boot-time cleanup, into `.zip` archives before deleting them                                                                                                                                                                            |
| `log_reset`            | bool   | `false`    | Clear today's log files every time the service starts (WinSW log `reset` mode)                                                                                                                                                                                                            |
| `log_auto_roll_at`     | string | none       | Daily scheduled rollover at `"HH:mm:ss"`; today's log is renamed `{pattern}.{HHmmss}.log` and a fresh file starts                                                                                                                                                                         |
| `log_out_enabled`      | bool   | `true`     | Whether child stdout is logged; `false` discards it (no pipe, no file)                                                                                                                                                                                                                    |
| `log_err_enabled`      | bool   | `true`     | Whether child stderr is logged; `false` discards it                                                                                                                                                                                                                                       |
| `log_pattern`          | string | `%Y-%m-%d` | chrono date pattern used in log file names (e.g. `%Y%m%d`), safe chars only (`%`, alphanumeric, `-_.`); unsafe patterns fall back to the default                                                                                                                                          |
| `log_out_filename`     | string | none       | Custom main log file name overriding `{pattern}.log` (no date rolling; safe chars only)                                                                                                                                                                                                   |
| `log_err_filename`     | string | none       | Custom stderr log file name overriding `{pattern}.err.log` (requires `log_split_out_err = true`)                                                                                                                                                                                          |
| `log_mode`             | string | none       | WinSW log mode: `append` (default) / `reset` (clear on start) / `none` (disable logging) / `roll` (rename current logs to `.old` on start) / `roll-by-size` (size rollover, default threshold 10 MB) / `roll-by-time` (daily rollover, default period 1 day) / `roll-by-size-time` (both) |
| `log_roll_period_days` | int    | `0`        | Roll-by-time period in days; rolls when the log's last-modified date is ≥ N days old                                                                                                                                                                                                      |
| `log_zip_date_format`  | string | none       | chrono date format for `.zip` archive file names (e.g. `%Y%m%d`); empty keeps `{file}.zip`                                                                                                                                                                                                |

### Advanced — SCM Reporting

| Field               | Type | Default   | Description                                                                                                                                 |
| --- | --- | --- | --- |
| `scm_wait_hint_ms`  | int  | `3600000` | `dwWaitHint` reported to SCM in start/stop-pending states (WinSW `waitHint`) — how long SCM waits before declaring the service unresponsive |
| `scm_sleep_time_ms` | int  | `500`     | Host main-loop polling interval for SCM signals in ms (WinSW `sleepTime`)                                                                   |

### Advanced — Embedded Mode (inplace)

| Field            | Type | Default | Description                                                                                                                                                                                                                                                                                                                              |
| --- | --- | --- | --- |
| `deploy_inplace` | bool | `false` | Register the current `os.exe` in place instead of deploying to ProgramData; the TOML must be named after the exe and sit next to it (use the actual exe file name). Intended for embedding Osmium inside your own project; excluded from boot-time host upgrades and cleanup — upgrade the framework manually from the official Releases |

### Full Example

```toml
# Base config
service_name = "My-Service"
service_display_name = "My Service"
service_description = "My application service"
service_executable_path = 'C:\app\myapp.exe'
service_executable_args = "--mode production"
service_start_mode = "delayed_auto"
service_dependencies = "EventLog;WinRM"
service_account = 'NT AUTHORITY\NetworkService'

# Lifecycle & logging
failure_reset_sec = 86400
restart_delay_ms = 60000
kill_process_tree = true
prestart_command = 'echo pre-start >> C:\app\hook.log'
poststop_command = 'echo child=%WINSGF_CHILD_PID% >> C:\app\hook.log'
stop_executable = 'C:\app\graceful-drain.exe'
stop_arguments = '--drain 5000'
stop_timeout_secs = 20
failure_action = "restart"
# or use an action chain: restart 3 times then reboot
# [[failure_actions]]
# action = "restart"
# delay_secs = 10
# [[failure_actions]]
# action = "reboot"
log_enabled = true
log_dir = "logs"
log_max_size_mb = 10
log_max_backup_count = 5
log_split_out_err = true
log_zip = true
log_reset = false
log_auto_roll_at = "00:00:00"
log_pattern = "%Y-%m-%d"

# Process environment & behavior
working_directory = 'C:\app'
process_priority = "abovenormal"
event_log = true
hide_window = true
stop_parent_process_first = false

# Pre-start download (with auth & proxy)
download_url = "https://example.com/app.exe"
download_to = 'C:\app\myapp.exe'
download_sha256 = "<sha256>"
download_fail_on_error = true
download_unzip = true
download_username = "user"
download_password = "pass"
download_proxy = "http://127.0.0.1:8080"
download_threads = 16

# Environment variables (values support %VAR% expansion; %BASE% means the deploy directory)
[env]
MY_VAR = "%BASE%"
LOG_LEVEL = "info"

# Lifecycle extensions (start before launch, start_after after launch,
# stop_before before stop, stop after stop; stdout_path/stderr_path redirect output)
[[extensions]]
phase = "start"
command = 'echo start >> C:\app\hook.log'

# Lifecycle plugin calls (kit/phase/payload/fail_on_error — see Docs\EXTENSION_EN.md)
# [[plugins]]
# kit = "backup"
# phase = "start_after"
# payload = { mode = "full" }
# fail_on_error = false

# Resource watchdog: kill the child when memory exceeds 512 MB (RunawayProcessKiller)
runaway_memory_limit_mb = 512
runaway_check_interval_secs = 30

# Map a network share at start, disconnect at stop (SharedDirectoryMapper)
# [[shared_directory_mappers]]
# local_path = "Z:"
# remote_path = '\\server\share'
```

## Scripts as Services (Interpreter + Script Path)

Osmium treats the service target as an "executable". To run a .py / .ps1 / .bat / .cmd script as a service, simply put the **interpreter** in `service_executable_path` and the script path plus arguments in `service_executable_args` — the host manages it like any other process: exit codes, auto-restart, logging and graceful shutdown all work as usual.

> The service process default working directory is `C:\Windows\System32`; always use absolute paths inside scripts (or `cd` yourself).

### Python Scripts

```toml
service_name = "py-worker"
service_display_name = "Python Worker"
service_description = "Python script service"
service_executable_path = 'C:\Python312\python.exe'
service_executable_args = '"C:\app\worker.py --interval 30"'
service_start_mode = "automatic"

[env]
PYTHONUNBUFFERED = "1"    # disable output buffering so logs flush in real time
```

To bind a virtual environment, just swap the interpreter path: `service_executable_path = 'C:\app\.venv\Scripts\python.exe'`.

### PowerShell Scripts

```toml
service_name = "ps-worker"
service_display_name = "PS Worker"
service_description = "PowerShell script service"
service_executable_path = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
service_executable_args = '-NoProfile -ExecutionPolicy Bypass -File C:\app\worker.ps1'
```

For PowerShell 7 use `C:\Program Files\PowerShell\7\pwsh.exe` with the same arguments. Scripts must be pure background logic — avoid interactive calls like `Read-Host` (the service environment has no interactive session).

### Batch Scripts

```toml
service_name = "bat-worker"
service_display_name = "Bat Worker"
service_description = "Batch script service"
service_executable_path = 'C:\Windows\System32\cmd.exe'
service_executable_args = '/c cd /d C:\app && worker.bat'
```

Batch scripts should end with `exit /b <code>` to return a real exit code; otherwise the host sees the exit code of the last command.

### Java Applications

```toml
service_name = "java-worker"
service_display_name = "Java Worker"
service_description = "Java application service"
service_executable_path = 'C:\Program Files\Java\jdk-17\bin\java.exe'
service_executable_args = '-jar C:\app\myapp.jar --server.port=8080'
service_start_mode = "automatic"
working_directory = 'C:\app'    # relative file access inside the jar resolves here

[env]
JAVA_HOME = 'C:\Program Files\Java\jdk-26'
```

Java apps run through `java.exe` like any other executable: crash self-healing, graceful stop (`Ctrl+C` triggers JVM shutdown hooks), logging and env injection all work as usual. Set `working_directory` so `new File(".")`-style relative paths resolve to the app directory. Use full TOML registration — the quick install (`--pth`) cannot pass `-jar` arguments.

### Behavior & Notes

- **Exit-code restart**: when a script exits with a non-zero code, the host restarts it automatically (up to 3 times) and stops the service when the limit is exceeded; the SCM layer backs this up with `restart_delay_ms`.
- **Graceful shutdown**: on stop, the interpreter receives `Ctrl+C` (cmd / python forward it), force-killed after a 10-second timeout; `kill_process_tree=true` (default) also terminates the whole tree.
- **Quote nesting**: `service_executable_args` is spliced into the command line verbatim — keep inner quotes for paths with spaces, e.g. `service_executable_args = '"C:\Program Files\App\worker.py"'`.
- **Permissions**: when switching `service_account` (e.g. `NT AUTHORITY\NetworkService`), mind that account's read/write access to the script directory.

## How It Works

1. **Install**: Osmium stores the config as `<name>.osiml` under `C:\ProgramData\Osmium\svcs\<name>\` (directory ACL is tightened to SYSTEM / Administrators only) and registers it via the SCM API with the ImagePath pointing at the shared host: `"…\os.exe" -internal --run <name>`. All platform services share one host binary (no per-service copy), so the on-disk footprint stays flat no matter how many services are registered. Reinstalling an existing name compares the source (executable path + arguments); a different source is rejected to avoid hijacking an unrelated service.
2. **Runtime**: When SCM starts the service, Osmium reads `<name>.osiml` and launches the target as a child process. If `download_url` is set, the target file is ensured to be ready before launch (with SHA-256 verification).
3. **Logging**: Child stdout/stderr and host lifecycle events are written to `logs\yyyy-MM-dd.log` (concurrent writes serialized by a mutex; size rollover and stderr splitting supported).

### Service Recovery

- SCM layer: on a crash the service restarts after `restart_delay_ms` (up to 2 times), with the failure counter reset after `failure_reset_sec`;
- Host layer: when the child exits with a **non-zero exit code**, the host restarts it automatically (up to 3 times) and stops the service once the limit is exceeded.

### Hooks

- **prestart** (`prestart_command`): runs before the target is launched, `cmd /c` semantics (pipes / redirection supported); failure is non-fatal, and a 60-second timeout force-kills the whole hook tree (so a stuck hook cannot trip the SCM 30-second startup timeout).
- **poststop** (`poststop_command`): runs after the target stops, with `WINSGF_CHILD_PID` / `WINSGF_CHILD_EXIT_CODE` injected; failure is only a warning.

### Graceful Shutdown

On stop or system shutdown: GUI processes receive `WM_CLOSE` (sent to every top-level window) → console processes receive `Ctrl+C` (broadcast to the shared console; the host registers an ignore handler to avoid killing itself) → after a 10-second timeout the process is force-killed; `kill_process_tree=true` (default) also terminates the whole tree.

### Embedded Mode (inplace)

With `deploy_inplace: true`, `--install` registers the current exe **in place**:

- No copy to ProgramData; the ImagePath points directly at the current exe;
- `service_name` must equal the actual exe file name (e.g. `os`; if you rename the exe, use its actual name), otherwise SCM cannot dispatch the service;
- Designed for embedding Osmium into your own project; excluded from boot-time host upgrades and cleanup. Developers must manually upgrade `os.exe` from the [official Releases](https://github.com/NXRKYMANE/Osmium/releases).

### Service Refresher

The installer automatically registers a **Service Refresher** (`Osmium Service Refresher`) that performs boot-time maintenance and cleans up residue:

1. **Registration (install time)** — The Inno Setup installer calls `os.exe -internal --install-refresher`, registering itself with the `-internal --refresher` parameter as a Windows service with "Automatic (Delayed Start)" so host services start before the maintenance scan.
2. **Boot-time execution** — About 2 minutes after system startup, SCM launches the Service Refresher. It scans `C:\ProgramData\Osmium\svcs\` and cleans up stale services and orphaned directories. Since all platform services share one host binary in the install directory, the host is upgraded by reinstalling the installer over the framework — there is no per-service binary replacement.
3. **Stale-service cleanup** — Removes services with a missing `.osiml`, nonexistent target, or unparsable config (plus their host directories), and orphaned directories (SCM record gone but the `svcs` folder remains).
4. **Log cleanup** — Deletes logs older than 30 days in each service's log directory and the refresher's own (`%ProgramData%\Osmium\refresher\`), including `.err.log` split logs and `.N` rollover backups.
5. **Auto-stop** — The service stops itself after one full scan; it does not stay resident.
6. **Removal (uninstall time)** — The Inno Setup uninstaller calls `os.exe -internal --uninstall-refresher` to stop and remove the service.

> The Service Refresher runs at the next boot; the installer also restarts previously stopped services immediately after install.

## Build

The one-click build script produces 2 artifacts (executable + installer):

```powershell
.\BUILD.ps1
```

**Pipeline**: build → unit tests → compile the installer with ISCC (Inno Setup 7).

After the installer is built, the script asks whether to also produce an optional UPX-compressed build. Answering `y` rebuilds with `opt-level = "z"` (size-first) and compresses with UPX (`--ultra-brute --lzma`), outputting `Publish\osmium64-upx.exe` (~1.1 MB, down from ~3.6 MB) — the normal exe and installer are left untouched.

The script reads the version from `Project\Cargo.toml` and automatically syncs it (plus the copyright year) into `installer.iss`. A failing test aborts the pipeline; use `.\BUILD.ps1 -SkipTests` to skip testing.

**Code signing**: all artifacts (`osmium64.exe`, `osmium64-official-kits.osx`, the installer, `osmium64-upx.exe`) are Authenticode-signed (SHA256 + RFC 3161 timestamp) when a certificate is available. Certificate sources, in priority order: the `OSMIUM_CERT_PFX` environment variable (plus optional `OSMIUM_CERT_PASSWORD`), or the repo-local dev certificate `Misc\codesign.pfx` (self-signed, `Misc\codesign.pfx` is gitignored and never committed). Without a certificate the pipeline proceeds unsigned with a warning; use `.\BUILD.ps1 -SkipSign` to skip signing explicitly. The self-signed dev certificate produces valid signatures but is not trusted by other machines — for public releases that must clear SmartScreen, sign with a commercial certificate via `OSMIUM_CERT_PFX`.

### Build Individually

```powershell
Set-Location Project
cargo build --release                     # → Project\target\release\osmium64.exe
Copy-Item target\release\osmium64.exe ..\Publish\osmium64.exe
# build the kits → Extension\osmium64-official-kits.osx (see Extension\osmium-official-kits)
ISCC installer.iss                        # → Publish\osmium-win-x64-setup-v<VERSION>.exe
```

## Installer Deployment

Pre-built installers are available on the [Releases](https://github.com/NXRKYMANE/Osmium/releases) page.

### Installer

| Installer                             | Notes              |
| --- | --- |
| `osmium-win-x64-setup-v<VERSION>.exe` | Standard installer |

The installer places `os.exe` in `%ProgramFiles%\Osmium\` and registers the Control Panel uninstall entry and the boot-time Service Refresher.

### Installer Features

- Installs `os.exe` to `%ProgramFiles%\Osmium\` and adds it to the system PATH
- Component selection page: core (`os.exe`) is fixed; the official extension kit (`osmium64-official-kits.osx` → `Extension\`) is **unchecked by default** — tick it if you need the plugin features (sspi download / unzip / share mapping / reboot), usage: [Extension Guide](Docs/EXTENSION_EN.md)
- Automatically registers the boot-time Service Refresher (`--install-refresher`)
- Registers an uninstall entry in Windows Control Panel
- Auto-detects old versions: silently upgrades on newer, prompts to reinstall on identical, warns on downgrade
- Stops services that use `os.exe` before replacing it, then restarts them automatically after install — no reboot prompt

### Inno Setup Integration Tips

When embedding Osmium in your own Inno Setup installer, watch out for these pitfalls:

1. **TOML path backslashes** — use **single-quoted literal strings** (`'C:\Program Files\ASMMS'`) for install-directory paths, so `\P` is not treated as an escape.
2. **PATH staleness** — the installer process may not find `os.exe` even after installation; read the full path from registry: `HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\os.exe`.
3. **Elevated child process** — Inno's `Exec` returns `ERROR_ACCESS_DENIED` when directly starting a requireAdministrator child; route through `cmd.exe`.
4. **Silent-install language dialog** — `/VERYSILENT` silent installs must pass `/LANG=` explicitly (highest precedence); otherwise the language dialog still pops up and hangs.

## Project Structure

```
Osmium/
├── Cargo.toml                 # workspace config (members: Project / Extension/osmium-official-kits)
├── Cargo.lock                 # dependency lock file (workspace-wide)
├── Project/                   # Rust implementation
│   ├── build.rs               # EXE version info / icon / language metadata (winresource)
│   ├── Cargo.toml             # Project config (release speed optimizations)
│   ├── installer.iss          # Inno Setup install script
│   └── src/                   # Rust sources
│       ├── main.rs            # Entry: module wiring
│       ├── service_cli.rs     # CLI: terminal command parsing / routing / help
│       ├── service_core.rs    # Core: SCM API, deployment, Service Refresher, download engine
│       ├── service_host.rs    # Service host: launches target process + plugin calls
│       ├── service_config.rs  # TOML config model (serde)
│       └── service_tests.rs   # Unit tests (139, incl. process-tree integration)
├── Extension/                 # Official kits (external plugin executables, shipped as .osx)
│   └── osmium-official-kits/  # Single bin (built as osmium64-official-kits.osx)
│       ├── Cargo.toml         # Kit config (same format as Project)
│       ├── build.rs           # EXE version info / icon (Extension.ico)
│       └── src/
│           ├── main.rs        # Protocol entry: stdin JSON dispatch (kit field) → stdout JSON
│           ├── kits_core.rs   # Shared implementations (same as Project service_core.rs):
│           │                  # SSPI download / share mapping / unzip / reboot
│           └── kits_tests.rs  # Unit + integration tests (24 + 1 ignored)
├── Misc/                      # Icon resources (referenced by build.rs / installer)
│   ├── Osmium.ico             # Installer / distribution icon (SetupIconFile)
│   ├── Osmium.png             # Program icon source
│   ├── Osmium.bmp             # Installer wizard small image (WizardSmallImageFile)
│   ├── Background.bmp         # Installer wizard background (WizardImageFile)
│   ├── Setup.ico              # .osiml config icon (installed as icons\osiml.ico)
│   ├── Setup.png              # .osiml icon source
│   ├── Extension.ico          # .osx plugin icon (installed as icons\osx.ico)
│   └── Extension.png          # .osx icon source
├── Docs/                      # Documentation
│   ├── README_CN.md           # Chinese documentation
│   ├── EXTENSION_CN.md        # Plugin development & usage guide (CN)
│   └── EXTENSION_EN.md        # Plugin development & usage guide (EN)
├── Publish/                   # Build artifacts (exe + installer, not committed)
├── BUILD.ps1                  # One-click build script (Rust build & tests + installer)
├── .github/                   # GitHub community templates (issues / PR)
├── CLAUDE.md                  # AI collaboration rules
├── CODE_OF_CONDUCT.md         # Code of conduct
├── CONTRIBUTING.md            # Contributing guidelines
├── SECURITY.md                # Security policy
├── LICENSE                    # License
└── README.md                  # English documentation
```

## Testing

Rust automated tests cover input validation, startup-mode parsing, log cleanup, process-tree collection, ACL permission checks, downloading, and other core logic:

```powershell
# Rust (139 tests + 24 plugin tests + 1 ignored, incl. a real process-tree integration test)
Set-Location Project
cargo test
```

- Tests are consolidated in `Project\service_tests.rs`; the test build never ships in the release binary;
- Security boundaries such as path traversal, control-character injection, and SDDL permission checks are covered.

## Requirements

- Windows 10+ x64
- Administrator privileges
- Build tools (build only):
  - Rust stable (edition 2024) + MSVC linker (Visual Studio C++ Build Tools) — to build the Rust binary
  - Inno Setup 7 — to build the installer (default path `C:\Program Files\Inno Setup 7\ISCC.exe`)

## Development History

> Back in 2024, I had basically finished learning Python and wanted to build my own project, but my laptop was so weak — only 8GB of RAM — that I was constantly anxious about memory.
>
> Later I got into Minecraft Java Edition and came across the PCL2 launcher. Its memory-cleaning feature worked great, but I had to click it manually every time — until I found out I could launch PCL2 silently with the `--memory` parameter to run a single cleanup. That got me interested, so I wrote my first automation service in Python. But Python's Win32 service support was rough, PyInstaller kept failing after packaging, and the high school entrance exam was approaching, so I shelved the memory-cleanup project for a while.
>
> After the exams, I learned about a magic tool called WinSW that can wrap anything into a Windows service, so I built my first project on top of it. But just when I thought everything was going smoothly and packaged my first installer, it turned out it only installed successfully on my own computer — on other machines it failed with weird errors I couldn't make sense of.
>
> Realizing the problem, I decided to write an automated service management platform named WSF (Windows Service Framework). It was pure Python too, still calling WinSW underneath. As development went on, the framework turned out to be extremely bloated, and security issues were hard to handle — basically usable but crippled. And as a purely interpreted language, Python's cold start was painfully slow, and the packaged size was shocking.
>
> To fix this once and for all, during the summer of 2026 I went out of my way to learn Rust, and with the help of the mysterious fat blue fish that eats free meals plus the WinSW source code, I directly built the first truly usable framework. As a chemistry fan, I also picked a name rarely used in the open-source community — Silanes, the silicon hydrides — for the first generation. But after deep development to cover all of WinSW's features (details are all in CLAUDE.md), I felt Silanes didn't fit the project anymore, so I officially renamed it to Osmium (osmium, the element). At the same time, that half-rotten memory-management project evolved into a Rust-based project called Hydride.

## Sponsor

If this project helps you, consider [sponsoring](https://ifdian.net/a/NXRKYMANE).

## License

Copyright © 2026 NXRKYMANE SOFTWARE
