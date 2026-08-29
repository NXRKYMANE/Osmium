# ✨ Osmium — Windows Service Generative Framework

<p align="center">
  <img src="https://img.shields.io/github/followers/NXRKYMANE?style=social" />
  <img src="https://img.shields.io/github/forks/NXRKYMANE/Osmium" />
  <img src="https://img.shields.io/github/stars/NXRKYMANE/Osmium" />
  <img src="https://img.shields.io/badge/-Rust-FFFFFF?style=flat&logo=rust&logoColor=black" />
  <img src="https://img.shields.io/badge/Gitee-NXRKYMANE-FFFFFF?style=flat" />
  <img src="https://img.shields.io/badge/AtomGit-NXRKYMANEX-FFFFFF?style=flat" />
  <img src="https://img.shields.io/badge/Douyin-Ozones-FFFFFF?style=flat&logo=tiktok&logoColor=white" />
  <img src="https://vbr.nathanchung.dev/badge?page_id=NXRKYMANE.Osmium&color=FFFFFF&leftColor=555555&label=Views" />
</p>

Register any executable or script as a Win32 system service — a Windows service generative framework. [中文文档](README_CN.md)

> Osmium is written in **Rust**, with some advanced features provided through OSX plugins so they can be extended whenever needed.

> The project is now fairly stable, though a few minor issues may still surface. Thank you for your understanding, fellow developers.

## Rust Implementation

Osmium is written in modern Rust (edition 2024) and compiles into **both 64-bit and 32-bit builds** — standalone `osmium64.exe` / `osmium32.exe` (installed as `os.exe`) plus the matching official advanced plugins `osmium64-official-kits.osx` / `osmium32-official-kits.osx`:

| Item            | Detail                                                                                                                                                                                                                                                                                                                                                                                                                                       |
| --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Language        | Rust 2024 (same source, cross-compiled 64/32)                                                                                                                                                                                                                                                                                                                                                                                                |
| Artifacts       | `Publish\osmium64.exe` + `Publish\osmium32.exe` (x86); plugins `Publish\exts\osmium64-official-kits-v<KITS_VERSION>.osx` + `Publish\exts\osmium32-official-kits-v<KITS_VERSION>.osx` (after installation the suffix is dropped and it stays `osmium64-official-kits.osx` — the host only cares about `.osx` + kit name, not the filename, so upgrade overwrites never break calls)                                                           |
| Size            | 64-bit: `osmium64.exe` ~4.3 MB; 32-bit: `osmium32.exe` ~3.3 MB                                                                                                                                                                                                                                                                                                                                                                               |
| Plugin size     | 64-bit: `osmium64-official-kits.osx` ~0.9 MB; 32-bit: `osmium32-official-kits.osx` ~0.7 MB (compiled with opt-level=z + UPX)                                                                                                                                                                                                                                                                                                                 |
| UPX build       | `Publish\osmium64-upx.exe` (~1.4 MB) + `Publish\osmium32-upx.exe` (~1.2 MB)                                                                                                                                                                                                                                                                                                                                                                  |
| Installer       | `osmium-win-x64-setup-v<VERSION>.exe` (non-UPX build, 64-bit only; for 32-bit deploy the exe + plugin standalone)                                                                                                                                                                                                                                                                                                                            |
| Toolchain       | Rust stable + MSVC (i686 cross target)                                                                                                                                                                                                                                                                                                                                                                                                       |

> [!TIP]
> Don't want the platform framework? Embedding into your own project? I'd recommend the UPX builds (`osmium64-upx.exe` / `osmium32-upx.exe`) — tiny, extensible and very lightweight, and cold start is barely different from the original.
>
> Missing a feature? The project is plugin-everything: write your own plugin in any language and place it under the executable (e.g. `exts\` on platform installs) — see the [Extension Guide](#plugin-system) for full plugin development and usage; a green dot on `os --extend` means your plugin is usable.
>
> Platform deployment needs the framework installed via the installer — lifecycle / logging / management are done by `os.exe`, without it services cannot start; an `osiml` file is just TOML renamed for convenience.

## Quick Start

```powershell
# Install (requires administrator)
os --install <svc.toml>
# Quick install: name + executable path (auto-generates config)
os --install <my-service> --pth C:\app\myapp.exe

# Manage services
os --start      <my-service>
os --stop       <my-service>
os --status     <my-service>
os --uninstall  <my-service>
os --list
# Run in foreground for debugging (no install)
os --test <svc.toml>
```

> [!WARNING]
> Write operations (install / uninstall / start / stop / ...) require **Administrator privileges**; for platform deployment the framework must be installed first via the installer — without `os.exe` (which carries the lifecycle / logging / management logic) the service cannot start. For embedding into your own project use inplace mode, which works without the framework directory (see [Embedded Mode (inplace)](#embedded-mode-inplace)).

## Commands

| Command                                          | Usage                                                                                                                                                                                                                                                                   |
| ------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `--install <toml>`                               | Install / update a service                                                                                                                                                                                                                                              |
| `--install <name> --pth/--path <exe path>`       | Quick install: auto-generates config and deploys `.osiml` (not needed for embedded projects)                                                                                                                                                                            |
| `--import <config.osiml>`                        | Import a deployed config and re-register the service (same as `--install`, for restoring from an export; short alias `--imp`)                                                                                                                                           |
| `--export <name> <dest dir>`                     | Export the deployed config (`svcs\<name>\<name>.osiml`) to a directory for migration/backup (short alias `--exp`)                                                                                                                                                       |
| `--start <name>`                                 | Start a service                                                                                                                                                                                                                                                         |
| `--stop <name>`                                  | Stop a service                                                                                                                                                                                                                                                          |
| `--restart <name>`                               | Restart a service                                                                                                                                                                                                                                                       |
| `--status <name>`                                | Query service status + registration details (start type / account / failure actions) + child process PIDs + Job Object state (`ok` or `failed:<count>`) + last metrics line (when `metrics_file` is set)                                                                |
| `--kill <name>`                                  | Admin/dev tool: force-kill the service's target process tree (via `WINSGF_SERVICE_ID`; short alias `--kil`)                                                                                                                                                             |
| `--refresh <name>`                               | Refresh SCM service properties (display name / description / start type / account / recovery, etc.) from the deployed config without reinstalling                                                                                                                       |
| `--reload <name>`                                | Hot-reload the deployed config and gracefully restart the child (independent of `auto_refresh`; short alias `--rld`)                                                                                                                                                    |
| `--uninstall <name>`                             | Stop and uninstall a service                                                                                                                                                                                                                                            |
| `--delete <name>`                                | Force delete (stop + uninstall)                                                                                                                                                                                                                                         |
| `--test <config>`                                | Run the service in a foreground console without installing (debug only; deploy dir = config dir, `%BASE%` points there; short alias `--tst`)                                                                                                                            |
| `--check <config or service name>`               | Validate a config file **or a registered service name** (reads its deployed config) without installing — field legality / service name / path writability / download targets / plugins / SDDL / schedules / health-check target, prints `[OK]`/`[FAIL]` per item        |
| `--sign-config <config>`                         | Sign a config with the `osmium-sign.key` next to the executable (RSA-SHA256, writes `<config>.sig`; short alias `--sigc`)                                                                                                                                               |
| `--list`                                         | List all platform-deployed services (excludes inplace embedded services)                                                                                                                                                                                                |
| `--extend`                                       | List installed plugins with availability check (green dot / red dot; the name shows an architecture tag `[64]` / `[32]`, `[unknown]` for non-PE files; short alias `--ext`; plugin dev: [Extension Guide](#plugin-system))                                              |
| `--start-all`                                    | Start all services (short alias `--stra`)                                                                                                                                                                                                                               |
| `--stop-all`                                     | Stop all services (short alias `--stpa`)                                                                                                                                                                                                                                |
| `--restart-all`                                  | Restart all services (short alias `--rsta`)                                                                                                                                                                                                                             |
| `--status-all`                                   | Batch status: state / registration details / child PIDs / metrics summary for every registered service (short alias `--stsa`)                                                                                                                                           |
| `help` / `-h` / `--help`                         | Print help text                                                                                                                                                                                                                                                         |

> [!NOTE]
> Management commands are equivalent to the legacy `-m --xxx` form (the prefix is optional); after a framework install you can use the `os` shortcut alias instead of `os.exe`.

> [!NOTE]
> Read-only / local commands run without administrator: `--help`, `--list`, `--status`, `--status-all`, `--extend`, `--check`, `--test`, `--sign-config` (SCM queries use least-privilege access); all other commands (install/start/stop/uninstall and similar write operations) still require elevation.

> [!TIP]
> Every command has a short alias: `--ins` / `--imp` / `--exp` / `--str` / `--stp` / `--rst` / `--sts` / `--kil` / `--rfs` / `--rld` / `--uin` / `--del` / `--lst` (install / import / export / start / stop / restart / status / kill / refresh / reload / uninstall / delete / list); developer commands `--tst` / `--chk` / `--sigc` / `--ext` / `--stra` / `--stpa` / `--rsta` / `--stsa` (test / check / sign / extend / batch start / batch stop / batch restart / batch status).

> [!CAUTION]
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

> [!TIP]
> Paths containing backslashes must use **single-quoted literal strings** (as above) — in a basic string like `"C:\app\..."` the `\a` is an illegal escape and parsing fails.

> [!WARNING]
> In TOML, top-level keys written **after an array table** (`[[...]]`) become elements of that array — put all `[[extensions]]` / `[[plugins]]` / `[[schedules]]` / `[[failure_actions]]` / `[[downloads]]` tables at the **end** of the file (see the notes in the [full example](#full-example)).

### Basic Features

| Field                           | Type         | Default             | Description                                                                                                                                                                                                                                                                                                                                          |
| ------------------------------- | ------------ | ------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `service_executable_args`       | string       | `""`                | Command-line arguments for the target executable (passed through verbatim, quotes preserved)                                                                                                                                                                                                                                                         |
| `start_arguments`               | string       | none                | Start-time arguments that override `service_executable_args`                                                                                                                                                                                                                                                                                         |
| `service_start_mode`            | string       | `"automatic"`       | Startup type: `automatic`, `delayed_auto`, `manual`, `disabled`, `once` (stop the service when the child exits — no restart, no recovery)                                                                                                                                                                                                            |
| `service_dependencies`          | string       | none                | Semicolon-separated list of services that must start first (e.g. `"EventLog;WinRM"`)                                                                                                                                                                                                                                                                 |
| `service_account`               | string       | `LocalSystem`       | Windows account to run the service as (e.g. `"NT AUTHORITY\\NetworkService"`)                                                                                                                                                                                                                                                                        |
| `service_password`              | string       | `""`                | Password for `service_account` (only needed for user accounts)                                                                                                                                                                                                                                                                                       |
| `env`                           | object       | none                | Environment variables injected into the target process (values support `%VAR%` expansion; `%BASE%` means the deploy directory). The host also auto-injects `BASE` (deploy directory) and `WINSGF_SERVICE_ID` (service name, used by RunawayProcessKiller anti-miskill checks) — an explicit user `env` value for `BASE` wins over the default        |
| `working_directory`             | string       | exe dir             | Working directory for the target process; relative paths resolve against the service directory                                                                                                                                                                                                                                                       |
| `process_priority`              | string       | `normal`            | Target process priority: `idle` / `belownormal` / `normal` / `abovenormal` / `high` / `realtime`                                                                                                                                                                                                                                                     |
| `process_affinity`              | string       | none                | Target process CPU affinity: core list like `"0,1,2"` (out-of-range cores ignored, empty mask not applied, clamped to the system core count)                                                                                                                                                                                                         |
| `io_priority`                   | string       | `normal`            | Target process I/O priority: `idle` / `low` / `normal` / `high` (ProcessIoPriority, Windows 8+)                                                                                                                                                                                                                                                      |
| `job_object`                    | bool         | `true`              | Place the child in a Job Object (`KILL_ON_JOB_CLOSE`): if the host dies abnormally (including crashes) the whole child process tree is terminated by the system — no orphan processes. Normal shutdown still uses graceful stop                                                                                                                      |

**Config-wide expansion**: `%VAR%` environment variables and the special `%BASE%` (the service deploy/config directory) are expanded across the whole config — `service_executable_path`, `service_executable_args`, `start_arguments`, `working_directory`, `download_url`, `download_to`, `stop_executable`, `stop_arguments`, `log_dir`, `runaway_pid_file`, shared mapper paths, and `env` values. Hook commands are shell commands and are not expanded. `%PID%` is a reserved placeholder: config expansion leaves it untouched, and it is replaced with the target process PID only when running the stop command. Variable names must be valid identifiers (letter/underscore first); URL percent-escape sequences (e.g. `%20`, `%2F`, `%E4`) are preserved literally so download URLs stay intact. Relative paths must not escape the deployment directory after expansion — escaping download targets (`download_to` / `downloads[].to`) and working directories fail as config errors; the log directory falls back to the default `logs` subfolder, while metrics file / pid file / stop executable / extension redirections ignore that setting.

### Advanced — Lifecycle & Hooks

| Field                             | Type         | Default         | Description                                                                                                                                                                                                                                                                                                                               |
| --------------------------------- | ------------ | --------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `prestart_command`                | string       | none            | Hook run before launching the target (`cmd /c` semantics, failure is non-fatal; killed after 60s timeout)                                                                                                                                                                                                                                 |
| `poststop_command`                | string       | none            | Hook run after the target stops (injects `WINSGF_CHILD_PID` / `WINSGF_CHILD_EXIT_CODE`)                                                                                                                                                                                                                                                   |
| `auto_refresh`                    | bool         | `false`         | Config hot-reload: the host watches the config file's mtime and gracefully restarts the target when it changes; a failed reload keeps the previous configuration running                                                                                                                                                                  |
| `extensions`                      | array        | none            | Extra lifecycle extension commands: `[{ phase = "start", command = "...", stdout_path?, stderr_path? }]` — `start` runs before launch, `start_after` after launch, `stop_before` before stop, `stop` after stop; failures are non-fatal. `stdout_path` / `stderr_path` redirect the hook output to standalone files                       |
| `plugins`                         | array        | none            | Lifecycle plugin calls (`.osx` plugins next to the executable): `[{ kit, phase, payload?, fail_on_error? }]` — see the [Extension Guide](#plugin-system)                                                                                                                                                                                  |
| `require_signed_plugins`          | bool         | `false`         | Only execute plugins with a valid Authenticode signature (WinVerifyTrust); unsigned/invalid-signature plugins are refused (default false keeps ACL-based trust only)                                                                                                                                                                      |
| `stop_executable`                 | string       | none            | Program run first when stopping the service (graceful drain; the host then waits for the target to exit)                                                                                                                                                                                                                                  |
| `stop_arguments`                  | string       | `""`            | Command-line arguments for `stop_executable` (passed through verbatim, quotes preserved); `%PID%` is replaced with the target process PID and `WINSGF_CHILD_PID` is injected                                                                                                                                                              |
| `stop_timeout_secs`               | int          | `10`            | Graceful-stop timeout in seconds                                                                                                                                                                                                                                                                                                          |
| `hide_window`                     | bool         | `true`          | Launch the target with `CreateNoWindow`; set `false` to let it create a console window                                                                                                                                                                                                                                                    |
| `stop_parent_process_first`       | bool         | `false`         | When force-killing, terminate the parent before its subtree                                                                                                                                                                                                                                                                               |
| `kill_process_tree`               | bool         | `true`          | Whether to force-kill the whole process tree on stop                                                                                                                                                                                                                                                                                      |
| `failure_reset_sec`               | int          | `86400`         | Failure counter reset period in seconds                                                                                                                                                                                                                                                                                                   |
| `restart_delay_ms`                | int          | `60000`         | Delay before auto-restart after crash in milliseconds                                                                                                                                                                                                                                                                                     |
| `failure_action`                  | string       | `restart`       | Failure-recovery action: `restart` / `reboot` / `none`                                                                                                                                                                                                                                                                                    |
| `failure_actions`                 | array        | none            | Failure-recovery action chain: `[{ action = "restart", delay_secs = 10 }, { action = "reboot" }]` — each failure takes the next action, the last one repeats; `restart` / `reboot` / `none` (invalid entries filtered). When absent, `failure_action` + `restart_delay_ms` build the chain (3 restarts then stop, legacy behavior)        |
| `interactive`                     | bool         | `false`         | Register the service as interactive with the desktop (`SERVICE_INTERACTIVE_PROCESS`)                                                                                                                                                                                                                                                      |
| `allow_service_logon`             | bool         | `false`         | When a custom service account is used, automatically grant it the "Log on as a service" right                                                                                                                                                                                                                                             |
| `security_descriptor`             | string       | none            | Service security descriptor (SDDL) applied to the service DACL at install — controls who can manage the service                                                                                                                                                                                                                           |
| `preshutdown`                     | bool         | `false`         | Advertise `SERVICE_ACCEPT_PRESHUTDOWN` so the SCM grants extra time for graceful shutdown                                                                                                                                                                                                                                                 |
| `event_log`                       | bool         | `false`         | Also write to the Windows Event Log (source `Osmium`; structured event IDs: 1000 start / 1001 stop / 1002 crash / 1003 download failure / 1004 config error / 1005 config-change audit on install/update/refresh)                                                                                                                         |

### Advanced — Built-in Alert Channels

On a child-process crash the host automatically calls the official alert plugins — **no `[[plugins]]` needed** (enabled by any of `notify_url` / `smtp_host` / `syslog_host`; merged with `[[plugins]]` crash calls). The crash context (`service_name` / `exit_code` / `failures`) is injected for the plugins to build the default alert text:

| Field                | Type         | Default                                | Description                                                                                               |
| -------------------- | ------------ | -------------------------------------- | --------------------------------------------------------------------------------------------------------- |
| `notify_url`         | string       | none                                   | Webhook URL: POST a JSON message on crash (optional `notify_format` selects the platform format)          |
| `notify_format`      | string       | `"generic"`                            | notify message format: `generic` / `teams` / `discord` / `feishu`                                         |
| `smtp_host`          | string       | none                                   | SMTP server (`host:port`, default port 25); enables crash email alerts, requires `smtp_from` + `smtp_to`  |
| `smtp_from`          | string       | none                                   | Sender address (From header)                                                                              |
| `smtp_to`            | string       | none                                   | Recipient address(es) (To header, comma-separated)                                                        |
| `smtp_subject`       | string       | `"Osmium service notification"`        | Email subject                                                                                             |
| `smtp_username`      | string       | none                                   | SMTP auth username (optional, AUTH PLAIN)                                                                 |
| `smtp_password`      | string       | none                                   | SMTP auth password (DPAPI-encrypted when written to `.osiml`, never stored in plain text)                 |
| `syslog_host`        | string       | none                                   | Syslog server (`host:port`, default port 514); enables crash syslog alerts                                |
| `syslog_facility`    | int          | `3` (daemon)                           | Syslog facility number (0-23)                                                                             |
| `syslog_severity`    | int          | `5` (notice)                           | Syslog severity number (0-7)                                                                              |
| `syslog_tag`         | string       | `"Osmium"`                             | Syslog program-name TAG                                                                                   |

> [!TIP]
> Need a phase other than crash (e.g. notify after startup)? Use `[[plugins]]` to call these kits at any phase (see the [Extension Guide](#plugin-system)).

### Advanced — Resource Monitor & Network Mapping

| Field                                | Type         | Default       | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                                        |
| ------------------------------------ | ------------ | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `runaway_cpu_limit`                  | float        | none          | RunawayProcessKiller: kill the child when its CPU usage (kernel+user delta over wall time, all cores summed) exceeds this percentage                                                                                                                                                                                                                                                                                                                                                               |
| `runaway_memory_limit_mb`            | int          | none          | RunawayProcessKiller: kill the child when its working set exceeds this many MB                                                                                                                                                                                                                                                                                                                                                                                                                     |
| `runaway_check_interval_secs`        | int          | `30`          | RunawayProcessKiller sampling interval in seconds                                                                                                                                                                                                                                                                                                                                                                                                                                                  |
| `runaway_pid_file`                   | string       | none          | PID file for startup cleanup: at service start, a leftover process with this PID is killed, then the new child PID is written and removed on stop. Only processes carrying this service's `WINSGF_SERVICE_ID` are killed                                                                                                                                                                                                                                                                           |
| `runaway_stop_timeout_ms`            | int          | `5000`        | Graceful-stop timeout for the leftover process during startup cleanup, then force-kill                                                                                                                                                                                                                                                                                                                                                                                                             |
| `runaway_stop_parent_first`          | bool         | `false`       | During startup cleanup, kill the parent process before its children                                                                                                                                                                                                                                                                                                                                                                                                                                |
| `shared_directory_mappers`           | array        | none          | SharedDirectoryMapper: map network shares at service start and disconnect at stop: `[{ local_path = "Z:", remote_path = "\\\\server\\share", username?, password? }]`                                                                                                                                                                                                                                                                                                                              |
| `health_check_url`                   | string       | none          | Health check: poll this target while the child runs; after consecutive failures the child is treated as crashed and the failure-recovery flow runs (restart/alerts). Supports `http(s)://` (GET, expected status `health_check_expected_status`) and `tcp://host:port` (TCP connect succeeds = healthy, for non-HTTP services), and `osx://<kit>?<key=value&...>` (protocol probe via a plugin — e.g. `osx://probe?url=127.0.0.1%3A3306&probe_type=mysql` for MySQL/Redis handshake checks)        |
| `health_check_interval_secs`         | int          | `30`          | Health-check polling interval in seconds                                                                                                                                                                                                                                                                                                                                                                                                                                                           |
| `health_check_timeout_secs`          | int          | `5`           | Health-check request timeout in seconds                                                                                                                                                                                                                                                                                                                                                                                                                                                            |
| `health_check_failures`              | int          | `3`           | Consecutive failures that count as a crash                                                                                                                                                                                                                                                                                                                                                                                                                                                         |
| `health_check_expected_status`       | int          | `200`         | Expected HTTP status code (anything else counts as failure; unused for `tcp://` probes)                                                                                                                                                                                                                                                                                                                                                                                                            |

> [!IMPORTANT]
> When health checks fail past the threshold or the runaway killer terminates the child, the host does **not silently stop the service** — it treats the kill as an abnormal exit and runs the full failure-recovery flow: the `failure_actions` chain (restart by default), the crash alert plugins (built-in notify / smtp / syslog channels) and event-log 1002.

### Advanced — Scheduled Tasks

| Field             | Type        | Default       | Description                                                                                                                                                                                                                                                                                              |
| ----------------- | ----------- | ------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `schedules`       | array       | none          | Scheduled tasks: `[{ every_secs?, daily_at?, action?, command? }]` — `every_secs` (fixed interval) and `daily_at` (`"HH:mm:ss"`, daily at) are mutually exclusive; `action`: `restart` (restart the child, default) / `reload` (hot-reload the config) / `hook` (run `command`, cmd /c semantics)        |

### Advanced — Efficiency Mode (EcoQoS)

| Field                             | Type         | Default       | Description                                                                                                                                                                                |
| --------------------------------- | ------------ | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `eco_qos`                         | string       | `none`        | Child-process efficiency mode (Task Manager "efficiency mode", ProcessPowerThrottling): `none` (no interference) / `always` (on at start) / `auto` (enter when idle, exit when busy)       |
| `eco_qos_idle_cpu_pct`            | float        | `10`          | `auto`: enter efficiency mode after 2 consecutive samples below this CPU %                                                                                                                 |
| `eco_qos_busy_cpu_pct`            | float        | `30`          | `auto`: exit efficiency mode when CPU rises above this %                                                                                                                                   |
| `host_eco_qos`                    | string       | `none`        | Host process efficiency mode: `none` / `always` / `auto` (enter when the host's own CPU is low; exits when the host or the child gets busy)                                                |
| `host_eco_qos_idle_cpu_pct`       | float        | `5`           | `auto`: host enters after 2 consecutive samples below this CPU %                                                                                                                           |
| `host_eco_qos_busy_cpu_pct`       | float        | `20`          | `auto`: host exits when its own CPU rises above this %, or when the child exceeds `eco_qos_busy_cpu_pct` (linked exit during heavy work)                                                   |

### Advanced — Pre-Start Download

| Field                             | Type         | Default              | Description                                                                                                                                                                                                                                                                                                                                                |
| --------------------------------- | ------------ | -------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `download_url`                    | string       | none                 | URL to fetch the target executable before launch (when the target exists and no `download_sha256` is set, `If-Modified-Since` is sent and re-download is skipped on HTTP 304)                                                                                                                                                                              |
| `download_to`                     | string       | none                 | Download destination; relative paths resolve against the service directory (must not escape it)                                                                                                                                                                                                                                                            |
| `download_sha256`                 | string       | none                 | SHA-256 of the downloaded file (lowercase hex)                                                                                                                                                                                                                                                                                                             |
| `download_fail_on_error`          | bool         | `true`               | Whether a failed download fails service startup                                                                                                                                                                                                                                                                                                            |
| `download_auth`                   | string       | none                 | Download authentication: `basic` (user/password), or `sspi` (Windows integrated Negotiate/NTLM/Kerberos) — `sspi` is handled by the official plugin (`sspi` kit, shipped as: `osmium64-official-kits.osx` on 64-bit hosts, `osmium32-official-kits.osx` on 32-bit); without the plugin the download fails with a clear error                               |
| `download_username`               | string       | none                 | Username for `basic` authentication                                                                                                                                                                                                                                                                                                                        |
| `download_password`               | string       | none                 | Password for `basic` authentication                                                                                                                                                                                                                                                                                                                        |
| `download_proxy`                  | string       | none                 | Proxy used for downloads (http or https)                                                                                                                                                                                                                                                                                                                   |
| `download_unzip`                  | bool         | `false`              | Auto-extract the downloaded file when it is a zip (zip-slip traversal is blocked)                                                                                                                                                                                                                                                                          |
| `download_stage`                  | string       | `before_start`       | When the download runs: `before_start` (ensure the executable before launch), `after_start` (extra resource after the target launches), `after_stop` (extra resource after stop). Only `before_start` participates in startup executability checks                                                                                                         |
| `download_threads`                | int          | `16`                 | Max chunked-download thread count; `0`/`1` disables multi-threading (single-threaded fallback)                                                                                                                                                                                                                                                             |
| `download_retries`                | int          | `2`                  | Download retry count with exponential backoff (still fails after all retries); `0` disables retries                                                                                                                                                                                                                                                        |
| `download_retry_backoff_ms`       | int          | `2000`               | Exponential backoff base in ms (2s/4s/8s...), only used when `download_retries > 0`                                                                                                                                                                                                                                                                        |
| `downloads`                       | array        | none                 | Multiple download entries: `[{ from, to, sha256?, fail_on_error?, auth?, username?, password?, unsecure_auth?, proxy?, unzip?, stage? }]` — omitted fields fall back to the top-level `download_*` values; when configured, the array takes precedence over the single `download_url` entry and the executable path stays `service_executable_path`        |
| `download_unsecure_auth`          | bool         | `false`              | Explicitly allow `basic` authentication over plain `http://`; default refuses because credentials would be sent in cleartext                                                                                                                                                                                                                               |

> [!WARNING]
> With `http://` and no `download_sha256`, `fail_on_error=true` refuses to start (protects against tampering in transit). `basic` auth over plain `http://` is refused unless `download_unsecure_auth = true`. Redirects are followed manually: `https→http` downgrade is refused, and `basic` credentials are only re-sent to the same origin (scheme+host+port) — never forwarded to a cross-host redirect target. The `sspi` plugin follows redirects manually too (downgrades refused, negotiation restarts per origin, tokens never sent to redirect targets) and verifies response length against Content-Length. Probe requests for authenticated URLs retry once with credentials on 401/403, so authenticated large files get chunked parallel downloads as well.

> [!WARNING]
> `--export` writes the config **including DPAPI ciphertext** — machine-scoped ciphertext can be decrypted by any account on this machine, so the export directory must be restricted to SYSTEM / Administrators only (e.g. a protected directory under `C:\ProgramData`). Never export to shared or public locations.

> [!IMPORTANT]
> Secrets: `service_password`, `download_password` and mapper `password` are DPAPI-encrypted (machine scope, ciphertext marked with the versioned `enc:OSMIUM1:` prefix) in the deployed `.osiml` — plaintext never lands on disk; legacy plaintext configs keep working.

### Advanced — Logging

| Field                        | Type         | Default          | Description                                                                                                                                                                                                                                                                                |
| ---------------------------- | ------------ | ---------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `log_enabled`                | bool         | `true`           | Whether host logs are written                                                                                                                                                                                                                                                              |
| `log_dir`                    | string       | none             | Log directory; relative paths resolve against the service directory                                                                                                                                                                                                                        |
| `log_max_size_mb`            | int          | `0`              | Max log file size (MB) before rollover; `0` means unlimited                                                                                                                                                                                                                                |
| `log_max_backup_count`       | int          | `5`              | Number of rolled-over backups to keep                                                                                                                                                                                                                                                      |
| `log_split_out_err`          | bool         | `false`          | Write child stderr to a separate `yyyy-MM-dd.err.log`                                                                                                                                                                                                                                      |
| `log_zip`                    | bool         | `false`          | Zip a rolled-over backup, and expired logs during boot-time cleanup, into `.zip` archives before deleting them                                                                                                                                                                             |
| `log_reset`                  | bool         | `false`          | Clear today's log files every time the service starts                                                                                                                                                                                                                                      |
| `log_auto_roll_at`           | string       | none             | Daily scheduled rollover at `"HH:mm"` or `"HH:mm:ss"`; today's log is renamed `{pattern}.{HHmmss}.log` and a fresh file starts; invalid times are rejected by `--check`                                                                                                                    |
| `log_out_enabled`            | bool         | `true`           | Whether child stdout is logged; `false` discards it (no pipe, no file)                                                                                                                                                                                                                     |
| `log_err_enabled`            | bool         | `true`           | Whether child stderr is logged; `false` discards it                                                                                                                                                                                                                                        |
| `log_pattern`                | string       | `%Y-%m-%d`       | chrono date pattern used in log file names (e.g. `%Y%m%d`), safe chars only (`%`, alphanumeric, `-_.`); unsafe patterns fall back to the default                                                                                                                                           |
| `log_out_filename`           | string       | none             | Custom main log file name overriding `{pattern}.log` (no date rolling; safe chars only)                                                                                                                                                                                                    |
| `log_err_filename`           | string       | none             | Custom stderr log file name overriding `{pattern}.err.log` (requires `log_split_out_err = true`)                                                                                                                                                                                           |
| `log_mode`                   | string       | none             | Log mode: `append` (default) / `reset` (clear on start) / `none` (disable logging) / `roll` (rename current logs to `.old` on start) / `roll-by-size` (size rollover, default threshold 10 MB) / `roll-by-time` (daily rollover, default period 1 day) / `roll-by-size-time` (both)        |
| `log_roll_period_days`       | int          | `0`              | Roll-by-time period in days; rolls when the log's last-modified date is ≥ N days old                                                                                                                                                                                                       |
| `log_zip_date_format`        | string       | none             | chrono date format for `.zip` archive file names (e.g. `%Y%m%d`); empty keeps `{file}.zip`                                                                                                                                                                                                 |
| `log_redact`                 | array        | none             | Literal strings redacted from logs (matching substrings replaced with `***` before writing — keeps passwords/tokens out of logs), e.g. `log_redact = ["TOKEN-123"]`                                                                                                                        |

### Advanced — SCM Reporting

| Field                     | Type       | Default         | Description                                                                                                                     |
| ------------------------- | ---------- | --------------- | ------------------------------------------------------------------------------------------------------------------------------- |
| `scm_wait_hint_ms`        | int        | `3600000`       | `dwWaitHint` reported to SCM in start/stop-pending states — how long SCM waits before declaring the service unresponsive        |
| `scm_sleep_time_ms`       | int        | `500`           | Host main-loop polling interval for SCM signals in ms                                                                           |

### Advanced — Robustness & Scaling

| Field                              | Type         | Default                   | Description                                                                                                                                                                                                                                                                                                                                                                                                                                                                 |
| ---------------------------------- | ------------ | ------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `hook_prestart_timeout_secs`       | int          | `60`                      | Timeout for prestart / extension hooks in seconds (anti-hang)                                                                                                                                                                                                                                                                                                                                                                                                               |
| `hook_poststop_timeout_secs`       | int          | `30`                      | Timeout for the poststop hook in seconds                                                                                                                                                                                                                                                                                                                                                                                                                                    |
| `stop_cmd_timeout_secs`            | int          | `stop_timeout_secs`       | Timeout for `stop_executable` in seconds (defaults to `stop_timeout_secs`)                                                                                                                                                                                                                                                                                                                                                                                                  |
| `process_count`                    | int          | `1`                       | **Multi-process**: number of identical child instances the host supervises. With `>1`: a non-zero exit of any instance applies the failure-action chain (restart respawns **all** instances), a clean (0) exit only replenishes that instance (not counted as a failure), `none` stops the service; health checks / runaway sampling use the primary instance (identical config → identical behavior); `stop_executable` runs once per instance with its own `%PID%`        |
| `metrics_file`                     | string       | none                      | Metrics export file (relative to the deploy dir; skipped when it's a symlink): appends one JSON line every 30s (time / child PID / avg CPU% / working set MB / restart count / uptime), and appends a final line with the exit code when the child exits                                                                                                                                                                                                                    |
| `metrics_format`                   | string       | `json`                    | Metrics export format: `json` (one JSON object per line) or `prometheus` (Prometheus text format `# TYPE` lines, scrape-friendly)                                                                                                                                                                                                                                                                                                                                           |
| `require_signed_config`            | bool         | `false`                   | Require a valid RSA-SHA256 signature (`.sig` file) for the deployed config — missing/invalid signature rejects load (fail-closed). See [Config Signing](#config-signing)                                                                                                                                                                                                                                                                                                    |
| `download_rate_limit_kbps`         | int          | `0`                       | Download rate limit in Kbps (0 = unlimited); throttles both single-thread and chunked downloads so they don't saturate the link                                                                                                                                                                                                                                                                                                                                             |

### Config Signing

The deployed config can be signed with RSA-SHA256 so the host refuses to run tampered or forged configurations (defense-in-depth on top of the hardened directory ACL + DPAPI):

- **Key pair**: generate once with OpenSSL — `openssl genrsa -out osmium-sign.key 2048` then `openssl pkcs8 -topk8 -nocrypt -in osmium-sign.key -out osmium-sign.key` (PKCS#8 PEM), and `openssl rsa -in osmium-sign.key -pubout -out osmium-public.pem`. Put both **next to the host exe** (platform: `%ProgramFiles%\Osmium\`, inplace: your project dir).
- **Auto-sign on install**: when `osmium-sign.key` exists next to the exe, `--install` signs the deployed config automatically (`<name>.sig` for platform, `<exe-name>.toml.sig` for inplace). Manual signing is available via `--sign-config <config>`.
- **Enforcement**: set `require_signed_config = true` in the config — the host then verifies the signature with `osmium-public.pem` at start, hot-reload and crash-restart; a missing/invalid signature is logged and the service refuses to start (fail-closed).
- Keep `osmium-sign.key` private (Administrators only) — anyone with the key can sign configs the host will trust.

### Configuration Safety Notes

> [!WARNING]
> **Misspelled field names are silently ignored** (lenient unknown-key parsing, a TOML compatibility design) — a misspelled safety switch such as `require_signed_config = ture` silently falls back to the default; a misspelled enum (`download_stage` / `extensions.phase` / `plugins.phase` / `failure_actions` / `eco_qos` and friends) disables the whole feature chain. Always run `--check <config>` before installing (it validates every enum value and numeric range).

- **`security_descriptor` is additive only**: `--refresh` rewrites registration properties from the config, but removing the SDDL from the config keeps the previous DACL on the service (there is no safe "reset to default" semantics); the refresh prints a note. Reinstall the service to reset it.

### Developer Features — Embedded Mode (inplace)

| Field                  | Type       | Default       | Description                                                                                                                                                                                                                                                                                                                                     |
| ---------------------- | ---------- | ------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `deploy_inplace`       | bool       | `false`       | Register the current `os.exe` in place instead of deploying to ProgramData; the TOML must be named after the exe and sit next to it (use the actual exe file name). Intended for embedding Osmium inside your own project; excluded from boot-time host upgrades and cleanup — upgrade the framework manually from the official Releases        |

### Full Example

All config fields at a glance (the `service_*` quartet is required; everything else has a default):

```toml
# ==================== Base config (required) ====================
service_name = "My-Service"
service_display_name = "My Service"
service_description = "My application service"
service_executable_path = 'C:\app\myapp.exe'
service_executable_args = "--mode production"      # concatenated verbatim, quoting preserved
start_arguments = "--mode prod"                    # start-only args; overrides the args above

# ==================== Start type & account ====================
service_start_mode = "delayed_auto"                # automatic | delayed_auto | manual | disabled | once
service_dependencies = "EventLog;WinRM"            # semicolon-separated
service_account = 'NT AUTHORITY\NetworkService'    # default LocalSystem; "virtual" = NT SERVICE\<name> least privilege
service_password = "svc-pass"                      # custom-account password (DPAPI-encrypted on deploy)
allow_service_logon = false                        # auto-grant "log on as a service"
interactive = false                                # interactive-desktop service (LocalSystem only)
preshutdown = false                                # extra graceful time on system shutdown
security_descriptor = ""                           # service DACL (SDDL), e.g. 'D:(A;;RPWPCR;;;BA)'
deploy_inplace = false                             # true: register in place (no host copy, toml next to exe)

# ==================== Process environment & behavior ====================
working_directory = 'C:\app'                       # relative paths resolve against the deploy dir
process_priority = "abovenormal"                   # idle | belownormal | normal | abovenormal | high | realtime
process_affinity = "0,1,2"                         # CPU affinity (core-id list)
io_priority = "high"                               # idle | low | normal | high (Windows 8+)
job_object = true                                  # Job Object: system kills the child tree if the host dies
hide_window = true                                 # CreateNoWindow
stop_parent_process_first = false                  # kill the parent before its subtree on force-kill

# ==================== Lifecycle & hooks ====================
prestart_command = 'echo pre-start >> C:\app\hook.log'
poststop_command = 'echo child=%WINSGF_CHILD_PID% >> C:\app\hook.log'
auto_refresh = false                               # hot-reload config (change → graceful child restart)
stop_executable = 'C:\app\graceful-drain.exe'      # graceful drain program run before stopping
stop_arguments = '--drain 5000'                    # %PID% placeholder replaced with the child PID
stop_timeout_secs = 20                             # graceful stop timeout (seconds)
hook_prestart_timeout_secs = 60                    # prestart/extension hook timeout (seconds)
hook_poststop_timeout_secs = 30                    # poststop hook timeout (seconds)
stop_cmd_timeout_secs = 20                         # stop-command timeout (defaults to stop_timeout_secs)

# ==================== Failure recovery ====================
failure_reset_sec = 86400                          # failure counter reset period (seconds)
restart_delay_ms = 60000                           # restart delay after a crash (milliseconds)
kill_process_tree = true                           # force-kill the whole process tree on stop
failure_action = "restart"                         # restart | reboot | none
# or use an action chain ([[failure_actions]] below; mutually exclusive with failure_action)

# ==================== Logging ====================
log_enabled = true
log_dir = "logs"
log_max_size_mb = 10                               # 0 = unlimited
log_max_backup_count = 5
log_split_out_err = true                           # stderr into a separate yyyy-MM-dd.err.log
log_zip = true                                     # zip-archive evicted backups before deleting
log_reset = false                                  # clear today's log on startup
log_auto_roll_at = "00:00:00"                      # roll at a fixed daily time
log_out_enabled = true
log_err_enabled = true
log_pattern = "%Y-%m-%d"                           # chrono date format (safe chars only)
log_out_filename = ""                              # custom main log file name (overrides default, no date roll)
log_err_filename = ""                              # custom stderr log file name (needs split_out_err)
log_mode = "append"                                # append | reset | none | roll | roll-by-size | roll-by-time | roll-by-size-time
log_roll_period_days = 0                           # day-based roll period (days)
log_zip_date_format = "%Y%m%d"                     # date format for .zip archive names
log_redact = ["SECRET_TOKEN"]                      # redact these literals in logs (replaced with ***)

# ==================== Pre-start download ====================
download_url = "https://example.com/app.exe"
download_to = 'C:\app\myapp.exe'
download_sha256 = "<sha256>"                       # if absent, sends If-Modified-Since (304 skips re-download)
download_fail_on_error = true
download_auth = "basic"                            # basic | sspi (sspi via the official plugin)
download_username = "user"
download_password = "pass"                         # DPAPI-encrypted on deploy
download_proxy = "http://127.0.0.1:8080"
download_unzip = true                              # auto-extract zips (zip-slip blocked)
download_stage = "before_start"                    # before_start | after_start | after_stop
download_threads = 16                              # chunked-download threads; 0/1 disables chunking
download_retries = 2
download_retry_backoff_ms = 2000                   # exponential backoff 2s/4s/8s
download_rate_limit_kbps = 0                       # download throttle (Kbps, 0 = unlimited)
download_unsecure_auth = false                     # explicitly allow basic auth over plain http://
# or use multiple entries ([[downloads]] below; mutually exclusive with the single download_* fields)

# ==================== Resource monitor (RunawayProcessKiller) ====================
runaway_cpu_limit = 80.0                           # kill the child when CPU exceeds this (all-core %)
runaway_memory_limit_mb = 512                      # kill the child when the working set exceeds this (MB)
runaway_check_interval_secs = 30
runaway_pid_file = ""                              # startup-cleanup pid file (absolute path)
runaway_stop_timeout_ms = 5000                     # graceful-stop timeout for leftover processes (ms)
runaway_stop_parent_first = false

# ==================== Health check ====================
health_check_url = "http://127.0.0.1:8080/health"  # also tcp://host:port and osx://probe?...
health_check_interval_secs = 30
health_check_timeout_secs = 5
health_check_failures = 3                          # consecutive failures before considering a crash
health_check_expected_status = 200                 # expected HTTP status code

# ==================== Metrics export ====================
metrics_file = "metrics.json"                      # append one line every 30s
metrics_format = "json"                            # json | prometheus

# ==================== Multiple child processes ====================
process_count = 1                                  # 1..=64; any non-zero exit follows the recovery chain

# ==================== Efficiency mode (EcoQoS) ====================
eco_qos = "auto"                                   # none | always | auto (child)
eco_qos_idle_cpu_pct = 10
eco_qos_busy_cpu_pct = 30
host_eco_qos = "auto"                              # none | always | auto (host itself)
host_eco_qos_idle_cpu_pct = 5
host_eco_qos_busy_cpu_pct = 20

# ==================== SCM reporting ====================
scm_wait_hint_ms = 3600000                         # dwWaitHint reported during PENDING phases
scm_sleep_time_ms = 500                            # main-loop SCM signal poll interval (ms)

# ==================== Built-in alert channels (auto-called on crash, no [[plugins]] needed) ====================
notify_url = "https://hooks.example.com/osmium"    # webhook notification
notify_format = "generic"                          # generic | teams | discord | feishu
smtp_host = "mail.example.com:25"                  # SMTP email (needs smtp_from/smtp_to too)
smtp_from = "alerts@example.com"
smtp_to = "ops@example.com"
smtp_subject = "[Osmium] service crashed"
smtp_username = "smtp-user"                        # optional (AUTH PLAIN)
smtp_password = "smtp-pass"                        # DPAPI-encrypted on deploy
syslog_host = "192.168.1.10:514"                   # syslog (UDP RFC 5424)
syslog_facility = 3                                # 0-23, default 3 (daemon)
syslog_severity = 5                                # 0-7, default 5 (notice)
syslog_tag = "MyService"

# ==================== Security ====================
event_log = true                                   # also write the Windows event log (IDs 1000-1005)
require_signed_plugins = false                     # plugins must carry a valid Authenticode signature
require_signed_config = false                      # deployed config must carry a valid RSA-SHA256 signature (.sig)

# ==================== Environment variables (values support %VAR%; %BASE% = deploy dir) ====================
[env]
MY_VAR = "%BASE%"
LOG_LEVEL = "info"

# ==================== Array tables (must be at the end: keys after an array table belong to its elements!) ====================

# Lifecycle extension commands (start before launch / start_after / stop_before / stop)
[[extensions]]
phase = "start"
command = 'echo start >> C:\app\hook.log'

# Lifecycle plugin calls (generic channel — set kit to your own capability name)
[[plugins]]
kit = "your kit"               # placeholder: your capability name (the kit field of the plugin request JSON)
phase = "start_after"          # start | start_after | stop_before | stop | crash
payload = { mode = "full" }    # optional args (JSON object, merged into the request)
fail_on_error = false          # blocks startup when true and the plugin fails in the start phase

# Scheduled tasks (every_secs and daily_at are mutually exclusive)
[[schedules]]
every_secs = 3600
action = "hook"                # restart | reload | hook
command = 'echo scheduled tick >> C:\app\schedule.log'

# Failure-recovery action chain (mutually exclusive with the top-level failure_action; last action repeats)
[[failure_actions]]
action = "restart"
delay_secs = 10

# Network share mapping (map at start, disconnect at stop)
[[shared_directory_mappers]]
local_path = "Z:"
remote_path = '\\server\share'

# Multiple download entries (mutually exclusive with the single download_* fields;
# the executable path stays service_executable_path)
[[downloads]]
from = "https://example.com/extra.bin"
to = "extra.bin"
```

## Scripts as Services (Interpreter + Script Path)

Osmium treats the service target as an "executable". To run a .py / .jar / .js / .rb / .lua / .ps1 / .bat / .cmd script as a service, simply put the **interpreter** in `service_executable_path` and the script path plus arguments in `service_executable_args` — the host manages it like any other process: exit codes, auto-restart, logging and graceful shutdown all work as usual.

> [!TIP]
> The service process default working directory is `C:\Windows\System32`; always use absolute paths inside scripts (or `cd` yourself, or set `working_directory`).

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

### Node.js Scripts

```toml
service_name = "node-worker"
service_display_name = "Node.js Worker"
service_description = "Node.js script service"
service_executable_path = 'C:\Program Files\nodejs\node.exe'
service_executable_args = 'C:\app\worker.js'
service_start_mode = "automatic"
working_directory = 'C:\app'
```

The script must stay resident (the event loop must not exit) — don't write a one-shot script that finishes immediately. On graceful stop, `Ctrl+C` triggers a `process.on('SIGINT')` callback where you can clean up. Use the Windows `node.exe` (not `nodevars.bat`).

### Ruby Scripts

```toml
service_name = "ruby-worker"
service_display_name = "Ruby Worker"
service_description = "Ruby script service"
service_executable_path = 'C:\Ruby33-x64\bin\ruby.exe'
service_executable_args = 'C:\app\worker.rb'
service_start_mode = "automatic"
working_directory = 'C:\app'
```

Use RubyInstaller's `ruby.exe` (64-bit installs to `C:\Ruby33-x64\bin\ruby.exe`). Keep the script resident with `loop { sleep 1 ... }` or an event loop (e.g. `TCPServer` / Sinatra); on graceful stop `Ctrl+C` fires `Signal.trap('INT')` / `at_exit` cleanup; return a real exit code with `exit(code)` for the host's failure-recovery logic.

### Lua Scripts

```toml
service_name = "lua-worker"
service_display_name = "Lua Worker"
service_description = "Lua script service"
service_executable_path = 'C:\Program Files\Lua\5.4\lua.exe'
service_executable_args = 'C:\app\worker.lua'
service_start_mode = "automatic"
working_directory = 'C:\app'
```

Use the official Windows binaries (e.g. Lua for Windows' `lua.exe`). Keep the script resident with something like `while true do os.execute("sleep 1") ... end`, and return a real exit code with `os.exit(code)` for the host's failure-recovery logic. Lua 5.3+ `lua.exe` passes extra arguments through verbatim.

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

### Behavior & Notes

- **Exit-code restart**: when a script exits with a non-zero code, the host restarts it automatically (up to 3 times) and stops the service when the limit is exceeded; the SCM layer backs this up with `restart_delay_ms`.
- **Graceful shutdown**: on stop, the interpreter receives `Ctrl+C` (cmd / python forward it), force-killed after a 10-second timeout; `kill_process_tree=true` (default) also terminates the whole tree.
- **Quote nesting**: `service_executable_args` is spliced into the command line verbatim — keep inner quotes for paths with spaces, e.g. `service_executable_args = '"C:\Program Files\App\worker.py"'`.
- **Permissions**: when switching `service_account` (e.g. `NT AUTHORITY\NetworkService`), mind that account's read/write access to the script directory. `service_account = "virtual"` (NT SERVICE\<name>) is the least-privilege option: the host auto-grants access to its own deploy directory, but cannot read the hardened `exts` plugin directory (SYSTEM / Administrators only) — plugin calls degrade to non-fatal warnings under a virtual account. Use the default `LocalSystem` if plugins are required.

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

> [!IMPORTANT]
> The exe must live in a location only SYSTEM / Administrators can write (e.g. your own directory under Program Files) — writable locations such as Downloads / Public / a dev workspace are **refused at install time** (nobody must be able to swap the exe and gain SYSTEM execution); `service_name` must equal the exe file name (otherwise SCM cannot dispatch the service).

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

> [!NOTE]
> The Service Refresher runs at the next boot; the installer also restarts previously stopped services immediately after install.

## Plugin System

Osmium is plugin-everything: official advanced features and third-party extensions are all standalone executables (`.osx`) placed under the executable's directory (the platform installer uses `exts\`), launched by the host on demand. How plugins work, the protocol, and how to write one — it's all here.

## What a Plugin Is

> [!IMPORTANT]
> **The plugin architecture must match the host**: a 32-bit process cannot start a 64-bit executable (a 64-bit host can run 32-bit plugins) — on a 32-bit host use `osmium32-official-kits.osx` (or your own 32-bit plugin), otherwise calls fail outright (`--extend` red dot; the architecture tag `[64]` / `[32]` / `[unknown]` after the name lets you check).

- A plugin is just an ordinary program with its extension renamed to `.osx` (e.g. `osmium-kits.exe` → `osmium64-official-kits.osx`)
- Plugins live anywhere under the host exe's directory — the host recursively discovers every `.osx` (skipping dot-hidden folders), so standalone deployments can put plugins directly next to the exe; the platform installer still ships the official kit to `%ProgramFiles%\Osmium\exts\`
- At startup the host recursively scans every `.osx` under the executable's directory and dispatches requests by the `kit` field
- **Plugins are not resident**: each call launches a fresh process, which handles one request and exits

### The File Name Doesn't Matter (Renaming Doesn't Break Calls)

The host does **not** identify plugins by file name — it only cares about three things: the `kit` capability name, the `.osx` extension, and discoverability under the executable's directory. So renaming the official plugin (`osmium64-official-kits.osx`) to any other name (e.g. `my-tools.osx`, `whatever.osx`) keeps every feature working, as long as those three hold:

- Host built-in config fields keep working: `download_auth = "sspi"`, `download_unzip = true`, `shared_directory_mappers`, `failure_action = "reboot"`, `notify_url`, `smtp_host`, `syslog_host` — they call the kit names (`sspi`/`unzip`/`netmap`/`reboot`/`notify`/`smtp`/`syslog`), which have nothing to do with file names
- A `kit` declared in a `[[plugins]]` block still matches
- `--extend` still lists it (just showing the new file name)

The call chain looks like this:

```
run_plugin("sspi", ...)        # the host only cares about the kit name
  → discover_plugins()         # scans *.osx under the executable — no name matching, collects all
  → broadcast {"kit":"sspi"}   # the request carries the capability name, not a file name
  → the plugin claims it       # internal dispatch by the kit field — recognize and run
  → first ok wins
```

Because it resolves capabilities instead of files, you get:

- **Free renaming**: swap plugin names, versions, or upgrades — the host and config need zero changes
- **Multiple plugins coexist**: the official plugin and any number of third-party plugins can live in `exts\` side by side without interference
- **Multiple implementations of one capability**: when several plugins respond to the same kit, the host takes the first success in discovery order
- **One file, many capabilities**: the official plugin responds to nine kits (`ping`/`sspi`/`netmap`/`unzip`/`reboot`/`notify`/`probe`/`smtp`/`syslog`) from a single file

The only things to watch:

1. The extension must stay `.osx` (renaming to `.exe` or similar means `discover_plugins` can't find it)
2. It must be discoverable under the host exe's directory (any depth; dot-hidden folders are skipped)
3. The plugin's internal kit dispatch must not change (e.g. if you rename the `sspi` dispatch inside the plugin, a config writing `sspi` can't hit it anymore — only in that case do you need to update the config too)
4. With `require_signed_plugins = true`, plugins must also carry a valid Authenticode signature (verified via WinVerifyTrust); unsigned or invalid-signature plugins are refused (`--extend` shows a red dot) — for deployments with strict plugin origin requirements

### Checking Whether a Plugin Is Usable

```powershell
os --extend
# or the short alias
os --ext
```

Prints each plugin's status: **green dot ●** = usable, **red dot ●** = unusable (untrusted ACL / protocol not responding / broken).

## Official Plugin osmium64-official-kits.osx / osmium32-official-kits.osx

The official plugin ships in **both 64-bit and 32-bit builds** (`osmium64-official-kits-v<VERSION>.osx` and `osmium32-official-kits-v<VERSION>.osx`, suffix dropped after installation; the installer embeds the 64-bit one, the 32-bit one is available from the Release assets). Pick the one matching your host's bitness — a mismatched plugin cannot even start. Built-in capabilities:

| kit            | Feature                                                                             | Host built-in config field (easier)       |
| -------------- | ----------------------------------------------------------------------------------- | ----------------------------------------- |
| `ping`         | Availability probe (used by the host's `--extend` self-check)                       | nothing to configure                      |
| `sspi`         | Windows integrated-auth download (Negotiate/NTLM/Kerberos 401 challenge loop)       | `download_auth = "sspi"`                  |
| `netmap`       | Network share mapping / disconnecting                                               | `shared_directory_mappers`                |
| `unzip`        | zip extraction (zip-slip traversal blocked)                                         | `download_unzip = true`                   |
| `reboot`       | System reboot (failure-recovery action)                                             | `failure_action = "reboot"`               |
| `notify`       | Webhook notification: POST JSON to a URL (service-event push)                       | `notify_url = "https://..."`              |
| `smtp`         | SMTP email alerts (optional AUTH PLAIN, single message)                             | `smtp_host = "mail.example.com:25"`       |
| `syslog`       | Syslog alerts (UDP RFC 5424, configurable facility/severity)                        | `syslog_host = "192.168.1.10:514"`        |

### Two Ways to Use Official Features

1. **Host built-in fields** (easiest): unzip, share mapping, reboot, sspi download and crash alerts (webhook / email / syslog) all have ready-made config fields — the host calls the matching plugin automatically, no `[[plugins]]` needed:

```toml
# sspi-authenticated download (via the official sspi kit)
download_url = "https://server/app.exe"
download_auth = "sspi"

# auto-extract downloaded zip (via the unzip plugin)
download_unzip = true

# map shares at start, disconnect at stop (via the netmap plugin)
[[shared_directory_mappers]]
local_path = "Z:"
remote_path = '\\server\share'

# reboot the system after a crash (via the reboot plugin)
failure_action = "reboot"

# Webhook notification (via the notify plugin): POST {"text": ...} to the URL on crash
# (optional notify_format = "teams" | "discord" | "feishu")
notify_url = "https://hooks.example.com/osmium"

# SMTP email alert (via the smtp plugin): sent on crash; requires smtp_from/smtp_to,
# optional smtp_username/smtp_password/smtp_subject
smtp_host = "mail.example.com:25"
smtp_from = "alerts@example.com"
smtp_to = "ops@example.com"

# Syslog alert (via the syslog plugin): UDP RFC 5424 on crash
# (optional syslog_facility/syslog_severity/syslog_tag)
syslog_host = "192.168.1.10:514"
```

> [!NOTE]
> Alert channels (crash phase) automatically receive injected `service_name` / `exit_code` / `failures` fields — the plugin builds the default alert text from them (or use an explicit `text`).

2. **`plugins` config-driven calls** (generic channel — third-party plugins use this too): declare lifecycle calls in the service config, at any phase and for any plugin (including the official alert kits):

```toml
[[plugins]]
kit = "your kit"            # placeholder: your capability name (the kit field of the plugin request JSON)
phase = "start_after"       # start / start_after / stop_before / stop / crash
payload = { mode = "full" } # optional args, merged into the request JSON and passed to the plugin
fail_on_error = false       # optional; true blocks startup when the plugin fails in the start phase
```

For example, to notify not only on crash but also after a successful start:

```toml
# Notify on crash (equivalent to the built-in notify_url, but with a custom text)
[[plugins]]
kit = "notify"
phase = "crash"
payload = { url = "https://hooks.example.com/osmium", text = "my service died" }

# Also notify after startup succeeds
[[plugins]]
kit = "notify"
phase = "start_after"
payload = { url = "https://hooks.example.com/osmium", text = "my service started" }
```

## Plugin Protocol

All plugins share one protocol, independent of language (Rust / C / Go / Python packaging all work):

| Item              | Rule                                                                                                              |
| ----------------- | ----------------------------------------------------------------------------------------------------------------- |
| Invocation        | the host spawns the plugin process (no command-line args, `CREATE_NO_WINDOW`)                                     |
| Input             | one line of JSON on stdin, with the `kit` field (injected by the host) + business fields                          |
| Output            | one line of JSON on stdout: `{"ok": true}` or `{"ok": false, "error": "..."}`                                     |
| Exit code         | 0 = success, non-zero = failure (double-checked with the ok field)                                                |
| stderr            | human-readable error info (does not pollute the protocol; discarded by the host)                                  |
| Empty input       | exits silently (double-click scenario produces no output)                                                         |
| Limits            | stdin capped at 1MB; the host force-kills after a 5-second timeout (so a stuck plugin cannot hang the host)       |

## Third-Party Plugin Development

Writing a plugin is actually simple: implement the protocol, drop it into `exts\`, declare it in the config, and check the green dot with `--extend`. Below are complete examples in 10 languages — all implementing the same `backup` capability with identical logic; pick whichever you're comfortable with.

### Rust Example

```rust
use std::io::Read;
use serde_json::Value;

fn main() {
    let mut input = String::new();
    // cap input size: protect against an abnormal caller feeding oversized input
    let _ = std::io::stdin().take(1024 * 1024).read_to_string(&mut input);
    if input.trim().is_empty() {
        std::process::exit(0); // no caller (double-click): exit silently
    }
    let req: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => fail(&format!("invalid request: {e}")),
    };
    // dispatch by kit field: if it's not your capability, fail with a clear message
    match req["kit"].as_str().unwrap_or("") {
        "backup" => { /* do the business */ println!(r#"{{"ok":true}}"#); }
        other => fail(&format!("unknown kit: {other}")),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("osmium-kits error: {msg}");          // stderr: for humans
    println!(r#"{{"ok":false,"error":"{msg}"}}"#); // stdout: protocol response
    std::process::exit(1);
}
```

### C Example (standard library only)

Pure C11 with the standard library — a minimal hand-rolled `kit` extraction (no full JSON parsing); for production consider cJSON / jansson.

```c
// plugin.c — MSVC: cl /O2 /Fe:plugin.exe plugin.c    MinGW: gcc -O2 -o plugin.exe plugin.c
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

// minimal "kit":"xxx" extraction (no full JSON parsing, field order irrelevant)
static void extract_kit(const char *json, char *out, size_t out_size) {
    const char *p = strstr(json, "\"kit\"");
    if (!p) { out[0] = '\0'; return; }
    p = strchr(p, ':');  if (!p) { out[0] = '\0'; return; }
    p = strchr(p, '"');  if (!p) { out[0] = '\0'; return; }
    p++;
    const char *q = strchr(p, '"');
    size_t len = q ? (size_t)(q - p) : 0;
    if (len >= out_size) len = out_size - 1;
    memcpy(out, p, len);
    out[len] = '\0';
}

static int fail(const char *msg) {
    fprintf(stderr, "osmium-kits error: %s\n", msg);      // stderr: for humans
    printf("{\"ok\":false,\"error\":\"%s\"}\n", msg);    // stdout: protocol response
    return 1;
}

int main(void) {
    // cap input at 1MB (the host feeds at most 1MB; truncate if you want to be safe)
    char *buf = malloc(1024 * 1024);
    if (!buf) return 1;
    size_t n = fread(buf, 1, 1024 * 1024, stdin);
    buf[n] = '\0';
    char *input = buf;
    while (*input == ' ' || *input == '\t' || *input == '\r' || *input == '\n') input++;
    if (*input == '\0') { free(buf); return 0; }          // no caller (double-click): exit silently

    char kit[64];
    extract_kit(input, kit, sizeof(kit));
    if (strcmp(kit, "backup") == 0) {
        // do the business
        printf("{\"ok\":true}\n");
        free(buf);
        return 0;
    }
    free(buf);
    return fail("unknown kit");
}
```

### C++ Example (nlohmann/json)

Needs the single-header library [nlohmann/json](https://github.com/nlohmann/json); compiles with VS or MinGW.

```cpp
#include <iostream>
#include <string>
#include <nlohmann/json.hpp>

using json = nlohmann::json;

int fail(const std::string& msg) {
    std::cerr << "osmium-kits error: " << msg << std::endl;         // stderr: for humans
    std::cout << "{\"ok\":false,\"error\":\"" << msg << "\"}" << std::endl; // stdout: protocol response
    return 1;
}

int main() {
    // read all of stdin (the host only feeds up to 1MB; truncate yourself if you want to be safe)
    std::string input((std::istreambuf_iterator<char>(std::cin)), std::istreambuf_iterator<char>());
    if (input.empty()) {
        return 0; // no caller (double-click): exit silently
    }
    json req;
    try {
        req = json::parse(input);
    } catch (...) {
        return fail("invalid request");
    }
    std::string kit = req.value("kit", "");
    if (kit == "backup") {
        // do the business
        std::cout << R"({"ok":true})" << std::endl;
        return 0;
    }
    return fail("unknown kit: " + kit);
}
```


### C# Example (System.Text.Json)

JSON parsing is built into .NET (Framework / Core / 5+), no third-party packages needed.

```csharp
// Plugin.cs — .NET Framework: csc /out:plugin.exe Plugin.cs    .NET Core: dotnet build
using System;
using System.Text;
using System.Text.Json;

class Plugin
{
    static int Fail(string msg)
    {
        Console.Error.WriteLine("osmium-kits error: " + msg);           // stderr: for humans
        Console.WriteLine("{\"ok\":false,\"error\":\"" + msg + "\"}"); // stdout: protocol response
        return 1;
    }

    static int Main()
    {
        // cap input at 1MB
        var buf = new byte[1024 * 1024];
        int n = Console.OpenStandardInput().Read(buf, 0, buf.Length);
        string input = Encoding.UTF8.GetString(buf, 0, Math.Max(n, 0)).Trim();
        if (input.Length == 0) return 0;   // no caller (double-click): exit silently

        string kit;
        try
        {
            kit = JsonDocument.Parse(input).RootElement.GetProperty("kit").GetString() ?? "";
        }
        catch { return Fail("invalid request"); }

        if (kit == "backup") { Console.WriteLine("{\"ok\":true}"); return 0; }  // do the business
        return Fail("unknown kit: " + kit);
    }
}
```

### Go Example (stdlib encoding/json)

JSON parsing is built into Go's standard library, no third-party packages needed.

```go
// plugin.go — go build -o plugin.exe plugin.go
package main

import (
    "encoding/json"
    "fmt"
    "io"
    "os"
)

// failure response: stderr for humans, stdout for the protocol (json.Marshal escapes special chars)
func fail(msg string) {
    fmt.Fprintf(os.Stderr, "osmium-kits error: %s\n", msg)
    out, _ := json.Marshal(map[string]any{"ok": false, "error": msg})
    fmt.Println(string(out))
    os.Exit(1)
}

func main() {
    // cap input size: read only the first 1MB
    data, err := io.ReadAll(io.LimitReader(os.Stdin, 1024*1024))
    if err != nil {
        fail("read error: " + err.Error())
    }
    if len(data) == 0 {
        return // no caller (double-click): exit silently
    }
    var req map[string]any
    if err := json.Unmarshal(data, &req); err != nil {
        fail("invalid request: " + err.Error())
    }
    kit, _ := req["kit"].(string)
    if kit == "backup" {
        fmt.Println(`{"ok":true}`) // do the business
        return
    }
    fail("unknown kit: " + kit)
}
```


### Java Example (no third-party dependencies)

The JDK has no built-in JSON parser, so here's a dependency-free minimal `kit` extraction; for production, switch to Jackson / Gson.

```java
import java.io.IOException;
import java.nio.charset.StandardCharsets;

public class Plugin {

    public static void main(String[] args) throws IOException {
        // cap input size: read only the first 1MB
        byte[] buf = new byte[1024 * 1024];
        int n = System.in.read(buf);
        String input = new String(buf, 0, Math.max(n, 0), StandardCharsets.UTF_8).trim();
        if (input.isEmpty()) {
            return; // no caller (double-click): exit silently
        }
        String kit = extractKit(input);
        if ("backup".equals(kit)) {
            // do the business
            System.out.println("{\"ok\":true}");
        } else {
            fail("unknown kit: " + kit);
        }
    }

    // minimal extraction of "kit":"xxx" (no full JSON parse; field order doesn't matter)
    private static String extractKit(String json) {
        int i = json.indexOf("\"kit\"");
        if (i < 0) return "";
        int c = json.indexOf(':', i);
        if (c < 0) return "";
        int q1 = json.indexOf('"', c + 1);
        if (q1 < 0) return "";
        int q2 = json.indexOf('"', q1 + 1);
        return q2 < 0 ? "" : json.substring(q1 + 1, q2);
    }

    private static void fail(String msg) {
        System.err.println("osmium-kits error: " + msg);                 // stderr: for humans
        System.out.println("{\"ok\":false,\"error\":\"" + msg + "\"}"); // stdout: protocol response
        System.exit(1);
    }
}
```

### Python Example

The standard library is enough — no third-party packages.

```python
import json
import sys


def fail(msg):
    print(f"osmium-kits error: {msg}", file=sys.stderr)      # stderr: for humans
    print(json.dumps({"ok": False, "error": msg}))          # stdout: protocol response
    sys.exit(1)


def main():
    # cap input size: read only the first 1MB
    data = sys.stdin.buffer.read(1024 * 1024)
    if not data.strip():
        sys.exit(0)  # no caller (double-click): exit silently
    try:
        req = json.loads(data)
    except ValueError as e:
        fail(f"invalid request: {e}")
    kit = req.get("kit", "")
    if kit == "backup":
        # do the business
        print(json.dumps({"ok": True}))
    else:
        fail(f"unknown kit: {kit}")


if __name__ == "__main__":
    main()
```


### Node.js Example

The standard library is enough — `JSON.parse` is built in.

```js
// cap input size: read only the first 1MB
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
    input += chunk;
    if (input.length > 1024 * 1024) {
        process.exit(1); // fail fast when over the limit
    }
});
process.stdin.on('end', () => {
    if (!input.trim()) {
        process.exit(0); // no caller (double-click): exit silently
    }
    let req;
    try {
        req = JSON.parse(input);
    } catch (e) {
        return fail('invalid request: ' + e.message);
    }
    const kit = req.kit || '';
    if (kit === 'backup') {
        // do the business
        console.log(JSON.stringify({ ok: true }));
    } else {
        fail('unknown kit: ' + kit);
    }
});

function fail(msg) {
    console.error('osmium-kits error: ' + msg);               // stderr: for humans
    console.log(JSON.stringify({ ok: false, error: msg }));  // stdout: protocol response
    process.exit(1);
}
```

### Ruby Example

The standard library is enough — `JSON` is built in.

```ruby
#!/usr/bin/env ruby
require 'json'

# Failure response: stderr for humans, stdout for the protocol (JSON.generate escapes special chars)
def fail(msg)
  warn "osmium-kits error: #{msg}"
  puts JSON.generate({ ok: false, error: msg })
  exit 1
end

# Cap the input: read at most 1 MB
input = STDIN.read(1024 * 1024) || ''
if input.strip.empty?
  exit 0 # no caller (double-click): exit silently
end

begin
  req = JSON.parse(input)
rescue JSON::ParserError => e
  fail("invalid request: #{e.message}")
end

kit = req['kit']
if kit == 'backup'
  # do the business work
  puts JSON.generate({ ok: true })
else
  fail("unknown kit: #{kit}")
end
```

### Lua Example

The standard library has no JSON parsing, so here is a minimal `kit` extractor with zero dependencies (no full JSON parse, field order irrelevant); for production, consider lua-cjson or similar.

```lua
-- plugin.lua — run with lua.exe (same backup capability as the Ruby/Node examples)
-- Minimal "kit":"xxx" extraction (no full JSON parse, field order irrelevant)
local function extract_kit(json)
  local p = string.find(json, '"kit"', 1, true)
  if not p then return "" end
  p = string.find(json, ':', p)
  if not p then return "" end
  p = string.find(json, '"', p)
  if not p then return "" end
  p = p + 1
  local q = string.find(json, '"', p)
  if not q then return "" end
  return string.sub(json, p, q - 1)
end

local function fail(msg)
  io.stderr:write("osmium-kits error: " .. msg .. "\n") -- stderr: for humans
  io.stdout:write('{"ok":false,"error":"' .. msg .. '"}') -- stdout: protocol response
  os.exit(1)
end

-- Cap the input: read at most 1 MB
local input = io.read(1024 * 1024) or ""
input = input:gsub("^%s+", ""):gsub("%s+$", "")
if input == "" then
  os.exit(0) -- no caller (double-click): exit silently
end

local kit = extract_kit(input)
if kit == "backup" then
  -- do the business work
  print('{"ok":true}')
else
  fail("unknown kit: " .. kit)
end
```

### Getting Started

1. Rename the compiled program to `xxx.osx`
2. Drop it anywhere under the host exe's directory (standalone: next to the exe; platform install: `%ProgramFiles%\Osmium\exts\`)
3. The directory and plugin file must satisfy the trust requirement (see "Things to Keep in Mind" below)
4. Declare the call in the service config:

```toml
[[plugins]]
kit = "your kit"          # placeholder: your capability name (must match the kit dispatched inside the plugin)
phase = "start_after"
payload = { mode = "full" }
```

5. Run `os --extend` to confirm the green dot, then restart the service

### Multiple Plugins & Execution Order

- Same phase runs in declaration order of the config array
- Each call launches a separate plugin process — no interference, no shared state
- A single plugin failure does not affect the others (`fail_on_error` can only block in the start phase)
- The same kit can be declared by multiple plugins; the host takes the first success in discovery order

## Things to Keep in Mind

- **ACL trust check**: the trust anchor is the host exe's own location — when the exe lives in a protected location (e.g. `%ProgramFiles%\Osmium\`), the plugin directory and file must sit somewhere only SYSTEM / Administrators can write (so nobody can swap a `.osx` to run with SYSTEM privileges); untrusted plugins are refused and marked red. **Inplace embedded deployment** (exe inside your own project directory) is trusted automatically: the plugin sits next to the exe, sharing the same risk surface — an unauthorized user who could replace the plugin could equally replace the exe, so no extra risk is added
- **Execution isolation**: plugins are separate processes, terminated after a 5-second timeout; a crash does not affect the host
- **Input limits**: stdin capped at 1MB; the official unzip plugin also has a total 8GiB extraction cap (protection against abnormal archives)
- **Credential safety**: passwords in plugin requests are decrypted by the host from the config before being passed in; logs only record redacted URLs

## FAQ

**Plugin shows a red dot / log says "writable by unprivileged users"**: the `exts\` directory or the plugin file is writable by a non-admin account (e.g. you extracted it to a user directory). Put the plugin into the admin-installed `%ProgramFiles%\Osmium\exts\` and you're done.

**Log says "plugin 'xxx' not found (no .osx plugin next to the executable)"**: there's no `.osx` under the executable's directory, or the plugin extension isn't `.osx`.

**Does renaming the plugin break my config?** No. The config only knows the `kit` capability name, not the file name; as long as the extension stays `.osx` and it sits under the executable's directory, it works.

**Want a resident plugin?** The plugin protocol is one-shot (launch → handle → exit). If you need a resident service, use the Osmium host to manage the target process — don't write it as a plugin.

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
│       └── service_tests.rs   # Unit tests (200, incl. process-tree integration)
├── Extension/                 # Official kits (external plugin executables, shipped as .osx)
│   └── osmium-official-kits/  # Single bin (64-bit builds as osmium64-official-kits.osx; .release.ps1 cross-builds the 32-bit osmium32-official-kits.osx)
│       ├── Cargo.toml         # Kit config (same format as Project)
│       ├── build.rs           # EXE version info / icon (Extension.ico)
│       └── src/
│           ├── main.rs        # Protocol entry: stdin JSON dispatch (kit field) → stdout JSON
│           ├── kits_core.rs   # Shared implementations (same as Project service_core.rs):
│           │                  # SSPI download / share mapping / unzip / reboot / notify / smtp / syslog / probe
│           └── kits_tests.rs  # Unit + integration tests (38 + 2 ignored)
├── Misc/                      # Icon resources (referenced by build.rs / installer)
│   ├── Osmium.ico             # Installer / distribution icon (SetupIconFile)
│   ├── Osmium.png             # Program icon source
│   ├── Osmium.bmp             # Installer wizard small image (WizardSmallImageFile)
│   ├── Background.bmp         # Installer wizard background (WizardImageFile)
│   ├── Setup.ico              # .osiml config icon (installed as icons\osiml.ico)
│   ├── Setup.png              # .osiml icon source
│   ├── Extension.ico          # .osx plugin icon (installed as icons\osx.ico)
│   └── Extension.png          # .osx icon source
├── Publish/                   # Build artifacts (exe + installer, not committed)
├── .release.ps1               # One-click build script (Rust build & tests + installer)
├── .github/                   # GitHub community templates (issues / PR)
├── CLAUDE.md                  # AI assistant rules
├── CHANGELOG.md               # Development log / version history
├── CODE_OF_CONDUCT.md         # Code of conduct
├── CONTRIBUTING.md            # Contributing guidelines
├── SECURITY.md                # Security policy
├── LICENSE                    # License (Apache-2.0)
├── NOTICE                     # Attribution notice (copyright + third-party)
├── README_CN.md               # Chinese documentation
└── README.md                  # English documentation
```

## Testing

Rust automated tests cover input validation, startup-mode parsing, log cleanup, process-tree collection, ACL permission checks, downloading, and other core logic:

```powershell
# Rust (200 tests + 38 plugin tests + 2 ignored, incl. a real process-tree integration test)
Set-Location Project
cargo test
```

- Tests are consolidated in `Project\service_tests.rs`; the test build never ships in the release binary;
- Security boundaries such as path traversal, control-character injection, and SDDL permission checks are covered.

## Build

The one-click build script produces 3 artifacts (executable + official plugin + installer):

```powershell
.\.release.ps1
```

**Pipeline**: build 64-bit → build 32-bit (i686 cross) → unit tests → compile the installer with ISCC (Inno Setup 7, 64-bit only). Plugins (compiled with opt-level=z, size-first) are UPX-compressed (`--ultra-brute --lzma`) right at build time as the release artifact (~0.9 MB / ~0.7 MB).

After the installer is built, the script asks whether to also produce an optional UPX-compressed build of the **main executable**. Answering `y` compresses the already-built artifacts directly with UPX (`--lzma`; no opt-level=z rebuild — switching optimization levels triggers a full dependency-tree recompile, which is very slow, and the plain build compresses to ~1.4/1.2 MB, barely different from the z variant), outputting `Publish\osmium64-upx.exe` (~1.4 MB) and `Publish\osmium32-upx.exe` (~1.2 MB) — the normal exe and installer are left untouched.

The script reads the version from `Project\Cargo.toml` and automatically syncs it (plus the copyright year) into `installer.iss`. A failing test aborts the pipeline; use `.\.release.ps1 -SkipTests` to skip testing.

**Code signing**: all artifacts (`osmium64.exe` / `osmium32.exe`, both plugins, the installer, `osmium64-upx.exe` / `osmium32-upx.exe`) are Authenticode-signed (SHA256 + RFC 3161 timestamp) when a certificate is available. Certificate sources, in priority order: the `OSMIUM_CERT_PFX` environment variable (plus optional `OSMIUM_CERT_PASSWORD`), or the repo-local dev certificate `Misc\codesign.pfx` (self-signed, `Misc\codesign.pfx` is gitignored and never committed). Without a certificate the pipeline proceeds unsigned with a warning; use `.\.release.ps1 -SkipSign` to skip signing explicitly. The self-signed dev certificate produces valid signatures but is not trusted by other machines — for public releases that must clear SmartScreen, sign with a commercial certificate via `OSMIUM_CERT_PFX`.

### Build Individually

```powershell
Set-Location Project
cargo build --release                     # → <repo root>\target\release\osmium64.exe (workspace-wide target dir)
Copy-Item ..\target\release\osmium64.exe ..\Publish\osmium64.exe
# build the kits → Extension\osmium64-official-kits.osx (see Extension\osmium-official-kits)
ISCC installer.iss                        # → Publish\osmium-win-x64-setup-v<VERSION>.exe

# 32-bit cross build (needs the i686-pc-windows-msvc target + x86 toolchain, see Save-X86Env in .release.ps1)
rustup target add i686-pc-windows-msvc
cargo build --release --target i686-pc-windows-msvc
Copy-Item ..\target\i686-pc-windows-msvc\release\osmium64.exe ..\Publish\osmium32.exe
```

## Installer Deployment

Pre-built installers are available on the [Releases](https://github.com/NXRKYMANE/Osmium/releases) page.

### Installer

| Installer                                   | Notes                                                                            |
| ------------------------------------------- | -------------------------------------------------------------------------------- |
| `osmium-win-x64-setup-v<VERSION>.exe`       | Standard installer (64-bit only; for 32-bit, deploy the exe + plugin standalone) |

The installer places `os.exe` (64-bit) in `%ProgramFiles%\Osmium\` and registers the Control Panel uninstall entry and the boot-time Service Refresher.

### Installer Features

- Installs `os.exe` (64-bit) to `%ProgramFiles%\Osmium\` and adds it to the system PATH
- Component selection page: core (`os.exe`) is fixed; the official extension kit (`osmium64-official-kits.osx` → `Extension\`) is **unchecked by default** — tick it if you need the plugin features (sspi download / unzip / share mapping / reboot / crash alerts), usage: [Extension Guide](#plugin-system)
- Automatically registers the boot-time Service Refresher (`--install-refresher`)
- Registers an uninstall entry in Windows Control Panel
- Auto-detects old versions: silently upgrades on newer, prompts to reinstall on identical, warns on downgrade
- Stops services that use `os.exe` **via Osmium's own management interface** before replacing it (`--stop-all` stops all platform services + `--uninstall-refresher` removes the refresher), then `--start-all` restarts them automatically after install — no reboot prompt. It never stops services directly via WMI/SCM (the host's graceful-stop semantics must be preserved, avoiding stop-ordering/residue issues); on update the old uninstaller runs in silent cleanup mode with the `/UPDATE` flag (skips the hosted-services confirmation dialog that would otherwise hang a silent update)

### Inno Setup Integration Tips

When embedding Osmium in your own Inno Setup installer, watch out for these pitfalls:

1. **TOML path backslashes** — use **single-quoted literal strings** (`'C:\Program Files\ASMMS'`) for install-directory paths, so `\P` is not treated as an escape.
2. **PATH staleness** — the installer process may not find `os.exe` even after installation; read the full path from registry: `HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\os.exe`.
3. **Elevated child process** — Inno's `Exec` returns `ERROR_ACCESS_DENIED` when directly starting a requireAdministrator child; route through `cmd.exe`.
4. **Silent-install language dialog** — `/VERYSILENT` silent installs must pass `/LANG=` explicitly (highest precedence); otherwise the language dialog still pops up and hangs.
5. **Stopping Osmium-hosted services** — stop hosted services with `os.exe --stop-all` (and restore with `--start-all`), never via WMI/SCM directly — hosted services must go through the host's graceful-stop semantics. If your update flow calls the Osmium uninstaller for cleanup, pass `/UPDATE` to skip the hosted-services confirmation dialog (in silent update scenarios that dialog sits unclicked and hangs the installer).

## Requirements

> [!IMPORTANT]
> Installing and managing services requires **Administrator privileges**; the Service Refresher and the shared host run as SYSTEM (platform deployment only).

- Windows 10+ (64-bit builds run on x64/x86 systems; 32-bit builds target x86 systems or embedding scenarios — match the host bitness)
- Administrator privileges
  - Rust stable (edition 2024) + MSVC linker (Visual Studio C++ Build Tools) — to build the Rust binary; the 32-bit build needs `rustup target add i686-pc-windows-msvc` (with the x86 cross linker)
  - Inno Setup 7 — to build the installer (default path `C:\Program Files\Inno Setup 7\ISCC.exe`)

## Development History

> [!NOTE]
> Osmium is named after osmium — the densest non-radioactive metal on Earth — in the hope that this project would be just as hard, steady and powerful as the element: it is not merely a simple service wrapper, the powerful lifecycle management and log-stream system make service management easy and free.
>
> Also, the abbreviation of Osmium — OS — echoes its close relationship with the operating system (Operating System).


> Back in 2024, I had basically finished learning Python and wanted to build my own project, but my laptop was so weak — only 8GB of RAM — that I was constantly anxious about memory.
>
> Later I got into Minecraft Java Edition and came across the PCL2 launcher. Its memory-cleaning feature worked great, but I had to click it manually every time — until I found out I could launch PCL2 silently with the `--memory` parameter to run a single cleanup. That got me interested, so I wrote my first automation service in Python. But Python's Win32 service support was rough, PyInstaller kept failing after packaging, and the high school entrance exam was approaching, so I shelved the memory-cleanup project for a while.
>
> After the exams, I learned about a magic tool called WinSW that can wrap anything into a Windows service, so I built my first project on top of it. But just when I thought everything was going smoothly and packaged my first installer, it turned out it only installed successfully on my own computer — on other machines it failed with weird errors I couldn't make sense of.
>
> Realizing the problem, I decided to write an automated service management platform named WSF (Windows Service Framework). It was pure Python too, still calling WinSW underneath. As development went on, the framework turned out to be extremely bloated, and security issues were hard to handle — basically usable but crippled. And as a purely interpreted language, Python's cold start was painfully slow, and the packaged size was shocking.
>
> To fix this once and for all, during the summer of 2026 I went out of my way to learn Rust, and with the help of the mysterious fat blue fish that eats free meals plus the WinSW source code, I directly built the first truly usable framework. As a chemistry fan, I also picked a name rarely used in the open-source community — Silanes, the silicon hydrides — for the first generation. But after deep development to cover all of WinSW's features (details are all in CHANGELOG.md), I felt Silanes didn't fit the project anymore, so I officially renamed it to Osmium (osmium, the element).
>
> Meanwhile, that half-rotten memory-management project has evolved into the Rust-based Scandium project — a fully self-developed architecture decoupled from the PCL2 launcher kernel, with a much more efficient memory-cleaning approach.

## Sponsor

If this project helps you, consider [sponsoring](https://ifdian.net/a/NXRKYMANE).

## License

Licensed under the Apache License, Version 2.0 — see [LICENSE](LICENSE) and [NOTICE](NOTICE) for details.

Copyright © 2026 NXRKYMANE SOFTWARE
