# ✨ Osmium — Windows Service Generator Framework

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

将任意可执行文件或脚本注册为 Win32 系统服务。 [SEE ENGLISH DOCS](README.md)

> 项目开发参考自 [WinSW 2](https://github.com/winsw/winsw)。
> Osmium 基本沿用了 WinSW 的大多数功能，使用 **Rust** 语言编写，一些高级功能采用 OSX 插件化，以便需要的时候可以扩展。

> 项目已基本趋于稳定，不过仍可能有一些小问题，望请各位开发者大佬谅解。

## Rust 实现

Osmium 使用现代 Rust 2024 语言开发，编译为一个独立的 `osmium64.exe`（安装后为 `os.exe`）和一个官方（我本人）提供的高级插件 `osmium64-official-kits.osx`：

| 项         | 说明                                                                          |
| --- | --- |
| 语言       | Rust 2024                                                                     |
| 产物       | `Publish\osmium64.exe` `Publish\osmium64-official-kits.osx`                                   |
| 大小       | `osmium64.exe` 约 3.6 MB，`osmium64-official-kits.osx` 约 1.9 MB（体积优先编译，opt-level=z） |
| UPX 压缩   | `Publish\osmium64-upx.exe`（约 1.1 MB）                                             |
| 分发安装包 | `osmium-win-x64-setup-v<版本>.exe`（使用非 UPX 版本）                         |
| 工具链     | Rust stable + MSVC                                                            |

> 不想用平台框架？想集成到自己的项目？我推荐优先使用 UPX 压缩版（`osmium64-upx.exe`）——体积非常小、可扩展，非常轻量，而且冷启动与原版差异不大。
> 没有你想要的功能？项目支持万物皆插件，用任意语言写出属于你自己的插件放入 exe 目录（平台安装放 `exts\`）即可接入——完整插件开发与使用指南见 [插件指南](EXTENSION_CN.md)，os.exe 运行亮绿灯即为可用插件。

> 说明：平台部署需要使用安装包安装框架，所有的生命周期、日志记录和服务管理完全由框架核心程序 os.exe 完成，一旦缺失服务将无法启动，当然安装回来会恢复作业。
> 依赖框架可以使您自己的项目和配置更加简单，如果不放心或者重要项目建议使用集成方案，集成方案可以随意替换插件，所有的操作和日志记录全部在项目内落实。
> osiml 文件其实还是原封不动的 toml 文件，只不过是为了方便区分。

## 快速开始

```powershell
# 安装服务（需管理员权限）
os --install <svc.toml>
# 快速安装（--pth 或 --path 均可）：直接以名称 + 可执行路径注册一个简单服务（自动生成配置并部署 .osiml 到 svc 目录）
os --install <my-service> --pth C:\app\myapp.exe

# 管理服务
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

## 支持的命令

| 命令                                      | 说明                                                                                                         |
| --- | --- |
| `--install <toml>`                        | 安装 / 更新服务                                                                                              |
| `--install <名称> --pth/--path <exe路径>` | 快速安装：自动生成配置并部署为 `.osiml`（集成项目不需要用这个）                                              |
| `--uninstall <名称>`                      | 停止并卸载服务                                                                                               |
| `--start <名称>`                          | 启动服务                                                                                                     |
| `--stop <名称>`                           | 停止服务                                                                                                     |
| `--restart <名称>`                        | 重启服务                                                                                                     |
| `--refresh <名称>`                        | 从已部署配置刷新 SCM 服务注册属性（显示名/描述/启动类型/账户/故障恢复等），无需重装          |
| `--kill <名称>`                           | 管理员/开发工具：强制终止服务的目标进程树（按 `WINSGF_SERVICE_ID` 定位；简写 `--kil`） |
| `--status <名称>`                         | 查询服务状态                                                                                                 |
| `--delete <名称>`                         | 强制删除（停止 + 卸载）                                                                                      |
| `--list`                                  | 列出平台部署的所有服务（不含 inplace 集成服务）                                                              |
| `--extend`                                | 列出已安装插件并检查可用性（可用绿点 / 不可用红点；可简写 `--ext`；插件开发见 [插件指南](EXTENSION_CN.md)）  |
| `--test <配置>`                           | 前台控制台直接运行目标进程（不安装服务，仅调试用；部署目录=配置目录，`%BASE%` 指向配置目录；可简写 `--tst`） |
| `help` / `-h` / `--help`                  | 显示帮助信息                                                                                                 |

> 管理命令均等价于旧写法 `-m --xxx`（前缀可省略）；框架安装后可直接用 `os` 快捷别名代替 `os.exe`。

> 所有命令均支持简化别名：`--ins` / `--uin` / `--str` / `--stp` / `--rst` / `--rfs` / `--kil` / `--sts` / `--del` / `--lst` / `--tst` / `--ext`（分别对应安装 / 卸载 / 启动 / 停止 / 重启 / 刷新 / 强杀 / 状态 / 删除 / 列表 / 测试 / 扩展）。

> 服务名 `Osmium Service Refresher` 为保留名；服务名需合法：拒绝空名、`.` / `..`（防路径穿越）、路径分隔符与控制字符，长度 ≤ 256。

## 配置参考

配置文件为 **TOML** 格式。平台注册服务时，配置会以 `<服务名>.osiml` 部署到 `C:\ProgramData\Osmium\svcs\<服务名>\`（资源管理器中显示为 Osmium 服务配置文件图标）；inplace 集成模式使用与 exe 同名的 `.toml`。

### 必填字段

```toml
service_name = "My-Service"
service_display_name = "My Service"
service_description = "服务描述"
service_executable_path = 'C:\app\myapp.exe'
```

> TOML 注意：路径含反斜杠时用**单引号字面字符串**（如上），避免基本字符串的 `\` 转义（`"C:\app\..."` 中的 `\a` 是非法转义会解析失败）。

### 基础功能

| 字段                      | 类型   | 默认值        | 说明                                                                                                                                                                                                                 |
| --- | --- | --- | --- |
| `service_executable_args` | string | `""`          | 目标程序命令行参数（原样拼接，保留引号语义）                                                                                                                                                                         |
| `start_arguments`         | string | 无            | 启动专用参数，配置后覆盖 `service_executable_args`（对应 WinSW startarguments）                                                                                                                                      |
| `service_start_mode`      | string | `"automatic"` | 启动类型：`automatic`、`delayed_auto`、`manual`、`disabled`                                                                                                                                                          |
| `service_dependencies`    | string | 无            | 依赖服务名列表，分号分隔（如 `"EventLog;WinRM"`）                                                                                                                                                                    |
| `service_account`         | string | `LocalSystem` | 服务运行账户（如 `"NT AUTHORITY\\NetworkService"`）                                                                                                                                                                  |
| `service_password`        | string | `""`          | 服务账户密码（仅自定义账户需要）                                                                                                                                                                                     |
| `env`                     | object | 无            | 注入目标进程的环境变量（值支持 `%VAR%` 展开，`%BASE%` 指部署目录）。宿主还会自动注入 `BASE`（部署目录）与 `WINSGF_SERVICE_ID`（服务名，供 RunawayProcessKiller 防误杀校验）——用户 `env` 显式配置 `BASE` 时以用户为准 |
| `working_directory`       | string | exe 所在目录  | 目标进程工作目录；相对路径基于服务部署目录                                                                                                                                                                           |
| `process_priority`        | string | `normal`      | 目标进程优先级：`idle` / `belownormal` / `normal` / `abovenormal` / `high` / `realtime`                                                                                                                              |

**配置全局展开**：整个配置都支持 `%VAR%` 环境变量与特殊变量 `%BASE%`（服务部署/配置目录）展开——`service_executable_path`、`service_executable_args`、`start_arguments`、`working_directory`、`download_url`、`download_to`、`stop_executable`、`stop_arguments`、`log_dir`、`runaway_pid_file`、共享映射路径以及 `env` 值（与 WinSW 对齐）。钩子命令是 shell 语义，不展开。`%PID%` 为保留占位符：配置全局展开时原样保留，仅在运行停止命令时替换为目标进程 PID（对应 WinSW #217）。

### 高级功能 — 生命周期与钩子

| 字段                        | 类型   | 默认值    | 说明                                                                                                                                                                                                                                                                  |
| --- | --- | --- | --- |
| `failure_reset_sec`         | int    | `86400`   | 失败计数重置周期（秒）                                                                                                                                                                                                                                                |
| `restart_delay_ms`          | int    | `60000`   | 崩溃后自动重启延迟（毫秒）                                                                                                                                                                                                                                            |
| `kill_process_tree`         | bool   | `true`    | 停止时是否强制终止整棵进程树                                                                                                                                                                                                                                          |
| `prestart_command`          | string | 无        | 启动前钩子（`cmd /c` 语义，失败不阻断；超时 60s 强杀）                                                                                                                                                                                                                |
| `poststop_command`          | string | 无        | 停止后钩子（注入 `WINSGF_CHILD_PID` / `WINSGF_CHILD_EXIT_CODE`）                                                                                                                                                                                                      |
| `auto_refresh`              | bool   | `false`   | 配置热刷新（对应 WinSW `autoRefresh`）：宿主运行中监听配置文件 mtime，变化时优雅重启目标子进程；重载失败保持旧配置运行                                                                                                                                                |
| `stop_executable`           | string | 无        | 停止服务时先运行的优雅排空程序（运行后等待目标进程退出）                                                                                                                                                                                                              |
| `stop_arguments`            | string | `""`      | `stop_executable` 的命令行参数（原样拼接，保留引号语义）；`%PID%` 占位符替换为目标进程 PID，并注入 `WINSGF_CHILD_PID` 环境变量（对应 WinSW #217）                                                                                                                     |
| `interactive`               | bool   | `false`   | 注册为可交互桌面的服务（`SERVICE_INTERACTIVE_PROCESS`）                                                                                                                                                                                                               |
| `failure_action`            | string | `restart` | 崩溃恢复动作：`restart` / `reboot` / `none`                                                                                                                                                                                                                           |
| `failure_actions`           | array  | 无        | 崩溃恢复动作序列：`[{ action = "restart", delay_secs = 10 }, { action = "reboot" }]`——每次失败依次取动作，超出后重复最后一个；`restart` / `reboot` / `none`（非法条目自动过滤）。未配置时用 `failure_action` + `restart_delay_ms` 构造（重启 3 次后停止，保持旧行为） |
| `stop_timeout_secs`         | int    | `10`      | 优雅停止超时（秒），对应 WinSW `stoptimeout`                                                                                                                                                                                                                          |
| `hide_window`               | bool   | `true`    | 以 `CreateNoWindow` 启动目标进程；`false` 时允许其创建控制台窗口（对应 WinSW `hidewindow`）                                                                                                                                                                           |
| `stop_parent_process_first` | bool   | `false`   | 强杀时先终止父进程再杀子树（对应 WinSW `stopparentprocessfirst`）                                                                                                                                                                                                     |
| `allow_service_logon`       | bool   | `false`   | 使用自定义服务账户时，自动授予其"作为服务登录"权限                                                                                                                                                                                                                    |
| `event_log`                 | bool   | `false`   | 同时写入 Windows 事件日志（信息级别，来源名 `Osmium`）                                                                                                                                                                                                                |
| `security_descriptor`       | string | 无        | 服务安全描述符（SDDL），安装时应用到服务 DACL，控制谁能管理该服务（对应 WinSW securityDescriptor）                                                                                                                                                                    |
| `preshutdown`               | bool   | `false`   | 上报 `SERVICE_ACCEPT_PRESHUTDOWN`，系统关停时获得更长的优雅时间                                                                                                                                                                                                       |
| `extensions`                | array  | 无        | 生命周期扩展命令列表：`[{ phase = "start", command = "...", stdout_path?, stderr_path? }]`——`start` 启动前、`start_after` 启动后、`stop_before` 停止前、`stop` 停止后执行，失败不阻断；`stdout_path` / `stderr_path` 把钩子输出重定向到独立文件                       |
| `plugins`                   | array  | 无        | 生命周期插件调用（exe 目录下的 `.osx` 插件）：`[{ kit, phase, payload?, fail_on_error? }]`——详见 [插件指南](EXTENSION_CN.md)                                                                                                                                                      |

### 高级功能 — 资源监控与网络映射

| 字段                          | 类型   | 默认值  | 说明                                                                                                                                                                                               |
| --- | --- | --- | --- |
| `runaway_cpu_limit`           | float  | 无      | RunawayProcessKiller：子进程 CPU 占用（内核+用户时间差/墙钟差，全核累计百分比）超过该值自动终止                                                                                                    |
| `runaway_memory_limit_mb`     | int    | 无      | RunawayProcessKiller：子进程工作集超过该 MB 数自动终止                                                                                                                                             |
| `runaway_check_interval_secs` | int    | `30`    | RunawayProcessKiller 采样间隔（秒）                                                                                                                                                                |
| `runaway_pid_file`            | string | 无      | 启动清理 pid 文件：服务启动时按该 PID 终止上次宿主残留进程树，启动子进程后回写 PID、停止后删除。只清理带本服务 `WINSGF_SERVICE_ID` 标识的进程（对齐 WinSW #237：PID 被系统复用时防止误杀无关进程） |
| `runaway_stop_timeout_ms`     | int    | `5000`  | 启动清理时残留进程的优雅停止超时（毫秒），超时后强杀                                                                                                                                               |
| `runaway_stop_parent_first`   | bool   | `false` | 启动清理时先终止父进程再杀子树                                                                                                                                                                     |
| `shared_directory_mappers`    | array  | 无      | SharedDirectoryMapper：服务启动时映射网络共享、停止时断开：`[{ local_path = "Z:", remote_path = "\\\\server\\share", username?, password? }]`                                                      |

### 高级功能 — 效率模式（EcoQoS）

| 字段                        | 类型   | 默认值 | 说明                                                                                                                                                                                                     |
| --- | --- | --- | --- |
| `eco_qos`                   | string | `none` | 子进程效率模式（任务管理器"效率模式"，ProcessPowerThrottling）：`none`（不干预）/ `always`（启动即开）/ `auto`（空闲进、繁忙退）                                                                          |
| `eco_qos_idle_cpu_pct`      | float  | `10`   | `auto`：连续 2 次采样 CPU 低于该百分比时进入效率模式                                                                                                                                                      |
| `eco_qos_busy_cpu_pct`      | float  | `30`   | `auto`：CPU 超过该百分比时退出效率模式                                                                                                                                                                    |
| `host_eco_qos`              | string | `none` | 宿主自身效率模式：`none` / `always` / `auto`（自身 CPU 低进入；自身或子进程繁忙时退出）                                                                                                                    |
| `host_eco_qos_idle_cpu_pct` | float  | `5`    | `auto`：宿主连续 2 次采样 CPU 低于该百分比时进入效率模式                                                                                                                                                  |
| `host_eco_qos_busy_cpu_pct` | float  | `20`   | `auto`：宿主自身 CPU 超过该百分比、或子进程超过 `eco_qos_busy_cpu_pct` 时退出（密集工作期间联动恢复全速）                                                                                                 |

### 高级功能 — 启动前下载

| 字段                     | 类型   | 默认值         | 说明                                                                                                                                                                                                                                                                          |
| --- | --- | --- | --- |
| `download_url`           | string | 无             | 启动前下载目标可执行文件的 URL（目标已存在且未配置 `download_sha256` 时发送 `If-Modified-Since`，服务器回 304 则跳过重新下载）                                                                                                                                                |
| `download_to`            | string | 无             | 下载目标路径；相对路径基于服务部署目录                                                                                                                                                                                                                                        |
| `download_sha256`        | string | 无             | 下载文件 SHA-256（小写十六进制）                                                                                                                                                                                                                                              |
| `download_fail_on_error` | bool   | `true`         | 下载失败是否导致服务启动失败                                                                                                                                                                                                                                                  |
| `download_auth`          | string | 无             | 下载认证方式：`basic`（用户名/密码），或 `sspi`（Windows 集成认证 Negotiate/NTLM/Kerberos）——`sspi` 由官方 `osmium-kit-sspi` 插件处理（随 `osmium64-official-kits.osx` 提供）；未装插件时下载会明确报错                                                                                 |
| `download_username`      | string | 无             | `basic` 认证用户名                                                                                                                                                                                                                                                            |
| `download_password`      | string | 无             | `basic` 认证密码                                                                                                                                                                                                                                                              |
| `download_proxy`         | string | 无             | 下载使用的代理（http/https 均可）                                                                                                                                                                                                                                             |
| `download_unzip`         | bool   | `false`        | 下载文件为 zip 时自动解压到目标位置（防 zip-slip 穿越）                                                                                                                                                                                                                       |
| `download_stage`         | string | `before_start` | 下载执行阶段：`before_start`（启动前确保目标可执行文件就绪）、`after_start`（目标启动后下载额外资源）、`after_stop`（停止后下载额外资源）；仅 `before_start` 参与启动可执行性检查                                                                                             |
| `download_threads`       | int    | `16`           | 分块下载线程数上限；`0`/`1` 禁用多线程（单线程回退）                                                                                                                                                                                                                          |
| `downloads`              | array  | 无             | 多下载条目（对应 WinSW download 列表）：`[{ from, to, sha256?, fail_on_error?, auth?, username?, password?, unsecure_auth?, proxy?, unzip?, stage? }]`——省略字段回退到配置级 `download_*` 值；配置后数组优先于单条 `download_url`，且可执行路径保持 `service_executable_path` |
| `download_unsecure_auth` | bool   | `false`        | 显式放行 `basic` 认证走明文 `http://`（对应 WinSW unsecureAuth）；默认拒绝（凭据明文泄漏）                                                                                                                                                                                    |

> 安全提示：`http://` 且未提供 `download_sha256` 时，`fail_on_error=true` 直接拒绝启动（防明文传输被篡改）；`basic` 认证走明文 `http://` 时默认拒绝，需 `download_unsecure_auth = true` 显式放行。
> 密钥保护：`service_password`、`download_password`、共享映射 `password` 在部署写入 `.osiml` 时自动 DPAPI 加密（机器级，密文以 `enc:OSMIUM1:` 前缀版本化标记），明文不落盘；旧版明文配置继续兼容。

### 高级功能 — 日志

| 字段                   | 类型   | 默认值     | 说明                                                                                                                                                                                                                                      |
| --- | --- | --- | --- |
| `log_enabled`          | bool   | `true`     | 是否写入服务日志                                                                                                                                                                                                                          |
| `log_dir`              | string | 无         | 日志目录；相对路径基于服务部署目录                                                                                                                                                                                                        |
| `log_max_size_mb`      | int    | `0`        | 单日志大小上限（MB），超过滚动备份；`0` 不限                                                                                                                                                                                              |
| `log_max_backup_count` | int    | `5`        | 滚动保留的备份份数                                                                                                                                                                                                                        |
| `log_split_out_err`    | bool   | `false`    | 子进程 stderr 单独写入 `yyyy-MM-dd.err.log`                                                                                                                                                                                               |
| `log_zip`              | bool   | `false`    | 滚动淘汰的最旧备份、以及开机清理时过期的日志，都会先压缩为 `.zip` 归档再删除                                                                                                                                                              |
| `log_reset`            | bool   | `false`    | 服务每次启动时清空当日日志文件（对应 WinSW log `reset` 模式）                                                                                                                                                                             |
| `log_auto_roll_at`     | string | 无         | 每天定点滚动时刻（`"HH:mm:ss"`），到达后把当日日志改名为 `{pattern}.{HHmmss}.log` 并重开新文件                                                                                                                                            |
| `log_out_enabled`      | bool   | `true`     | 是否记录子进程 stdout；`false` 时直接丢弃（不建管道不写文件）                                                                                                                                                                             |
| `log_err_enabled`      | bool   | `true`     | 是否记录子进程 stderr；`false` 时丢弃                                                                                                                                                                                                     |
| `log_pattern`          | string | `%Y-%m-%d` | 日志文件名使用的 chrono 日期格式（如 `%Y%m%d`），仅允许安全字符（`%`、字母数字、`-_.`），非法模式回退默认                                                                                                                                 |
| `log_out_filename`     | string | 无         | 自定义主日志文件名，覆盖默认 `{pattern}.log`（无日期滚动；仅允许安全字符）                                                                                                                                                                |
| `log_err_filename`     | string | 无         | 自定义 stderr 分离日志文件名，覆盖默认 `{pattern}.err.log`（需 `log_split_out_err = true`）                                                                                                                                               |
| `log_mode`             | string | 无         | WinSW log 模式：`append`（默认）/ `reset`（启动清空）/ `none`（关闭日志）/ `roll`（启动时把当前日志改名为 `.old`）/ `roll-by-size`（大小滚动，缺省阈值 10MB）/ `roll-by-time`（按天滚动，缺省周期 1 天）/ `roll-by-size-time`（两者同时） |
| `log_roll_period_days` | int    | `0`        | 按天滚动周期（天）；日志最后修改日期距今 ≥ N 天时滚动                                                                                                                                                                                     |
| `log_zip_date_format`  | string | 无         | `.zip` 归档文件名的 chrono 日期格式（如 `%Y%m%d`）；空保持 `{file}.zip`                                                                                                                                                                   |

### 高级功能 — SCM 上报

| 字段                | 类型 | 默认值    | 说明                                                                                                            |
| --- | --- | --- | --- |
| `scm_wait_hint_ms`  | int  | `3600000` | 启动/停止 PENDING 阶段向 SCM 上报的 `dwWaitHint`（毫秒，对应 WinSW waitHint）——SCM 等待多长时间后判定服务无响应 |
| `scm_sleep_time_ms` | int  | `500`     | 宿主主循环 SCM 信号轮询间隔（毫秒，对应 WinSW sleepTime）                                                       |

### 高级功能 — 集成模式

| 字段             | 类型 | 默认值  | 说明                                                                                                                                                                                                                   |
| --- | --- | --- | --- |
| `deploy_inplace` | bool | `false` | 原地注册：不复制宿主到 ProgramData，直接用当前 `os.exe` 注册；TOML 必须与 exe 同名同目录（以实际 exe 文件名为准）。适合嵌入自有项目独立使用；不参与开机宿主升级与清理，框架升级需自行到官网 Releases 下载新版 `os.exe` |

### 完整示例

```toml
# 基础配置
service_name = "My-Service"
service_display_name = "My Service"
service_description = "我的应用程序服务"
service_executable_path = 'C:\app\myapp.exe'
service_executable_args = "--mode production"
service_start_mode = "delayed_auto"
service_dependencies = "EventLog;WinRM"
service_account = 'NT AUTHORITY\NetworkService'

# 生命周期与日志
failure_reset_sec = 86400
restart_delay_ms = 60000
kill_process_tree = true
prestart_command = 'echo pre-start >> C:\app\hook.log'
poststop_command = 'echo child=%WINSGF_CHILD_PID% >> C:\app\hook.log'
stop_executable = 'C:\app\graceful-drain.exe'
stop_arguments = '--drain 5000'
stop_timeout_secs = 20
failure_action = "restart"
# 或使用动作序列: 重启 3 次后重启系统
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

# 进程环境与行为
working_directory = 'C:\app'
process_priority = "abovenormal"
event_log = true
hide_window = true
stop_parent_process_first = false

# 启动前下载（可带认证与代理）
download_url = "https://example.com/app.exe"
download_to = 'C:\app\myapp.exe'
download_sha256 = "<sha256>"
download_fail_on_error = true
download_unzip = true
download_username = "user"
download_password = "pass"
download_proxy = "http://127.0.0.1:8080"
download_threads = 16

# 环境变量（值支持 %VAR% 展开，%BASE% 指部署目录）
[env]
MY_VAR = "%BASE%"
LOG_LEVEL = "info"

# 生命周期扩展（start 启动前、start_after 启动后、stop_before 停止前、stop 停止后；
# stdout_path/stderr_path 可把钩子输出重定向到独立文件）
[[extensions]]
phase = "start"
command = 'echo start >> C:\app\hook.log'

# 生命周期插件调用（kit/phase/payload/fail_on_error 完整说明见 Docs\EXTENSION_CN.md）
# [[plugins]]
# kit = "backup"
# phase = "start_after"
# payload = { mode = "full" }
# fail_on_error = false

# 资源看门狗: 子进程内存超过 512 MB 自动终止（RunawayProcessKiller）
runaway_memory_limit_mb = 512
runaway_check_interval_secs = 30

# 启动时映射网络共享、停止时断开（SharedDirectoryMapper）
# [[shared_directory_mappers]]
# local_path = "Z:"
# remote_path = '\\server\share'
```

## 脚本作为服务（解释器 + 脚本路径）

Osmium 的服务目标是「可执行程序」。要让 .py / .ps1 / .bat / .cmd 脚本作为服务，只需把**解释器**填进 `service_executable_path`，脚本路径与参数填进 `service_executable_args`——宿主按普通进程管理，退出码、自动重启、日志、优雅关闭全部照常生效。

> 服务进程默认工作目录是 `C:\Windows\System32`，脚本内请用绝对路径（或自行 `cd`）。

### Python 脚本

```toml
service_name = "py-worker"
service_display_name = "Python Worker"
service_description = "Python 脚本服务"
service_executable_path = 'C:\Python312\python.exe'
service_executable_args = '"C:\app\worker.py --interval 30"'
service_start_mode = "automatic"

[env]
PYTHONUNBUFFERED = "1"    # 关闭输出缓冲，日志实时落盘
```

绑定虚拟环境只需换解释器路径：`service_executable_path = 'C:\app\.venv\Scripts\python.exe'`。

### PowerShell 脚本

```toml
service_name = "ps-worker"
service_display_name = "PS Worker"
service_description = "PowerShell 脚本服务"
service_executable_path = 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe'
service_executable_args = '-NoProfile -ExecutionPolicy Bypass -File C:\app\worker.ps1'
```

PowerShell 7 请用 `C:\Program Files\PowerShell\7\pwsh.exe`，参数不变。脚本需写成纯后台逻辑，避免 `Read-Host` 等交互调用（服务环境无交互界面）。

### 批处理脚本

```toml
service_name = "bat-worker"
service_display_name = "Bat Worker"
service_description = "批处理脚本服务"
service_executable_path = 'C:\Windows\System32\cmd.exe'
service_executable_args = '/c cd /d C:\app && worker.bat'
```

批处理请以 `exit /b <code>` 结尾返回真实退出码，否则宿主拿到的是最后一条命令的退出码。

### Java 应用

```toml
service_name = "java-worker"
service_display_name = "Java Worker"
service_description = "Java 应用服务"
service_executable_path = 'C:\Program Files\Java\jdk-17\bin\java.exe'
service_executable_args = '-jar C:\app\myapp.jar --server.port=8080'
service_start_mode = "automatic"
working_directory = 'C:\app'    # jar 内相对路径读写基于此目录

[env]
JAVA_HOME = 'C:\Program Files\Java\jdk-26'
```

Java 应用经 `java.exe` 启动，与其他可执行程序一样享受崩溃自愈、优雅停止（`Ctrl+C` 触发 JVM shutdown hook）、日志、环境变量注入等全部能力。建议配 `working_directory`，保证 `new File(".")` 这类相对路径解析到应用目录。带 `-jar` 参数的应用请用完整 TOML 注册——快速安装（`--pth`）无法传参数。

### 行为与注意

- **退出码重启**：脚本以非零退出码退出时，宿主自动重启（最多 3 次），超限停止服务；SCM 层按 `restart_delay_ms` 兜底。
- **优雅关闭**：停止服务时解释器进程接收 `Ctrl+C`（cmd / python 会透传），10 秒超时强杀；`kill_process_tree=true`（默认）连进程树一起终止。
- **引号嵌套**：args 原样拼接进命令行，路径含空格时保留内层引号，如 `service_executable_args = '"C:\Program Files\App\worker.py"'`。
- **权限**：改用 `service_account`（如 `NT AUTHORITY\NetworkService`）时，注意该账户对脚本目录的读写权限。

## 工作原理

1. **安装**：Osmium 将配置以 `<服务名>.osiml` 保存到 `C:\ProgramData\Osmium\svcs\<名称>\`（目录 ACL 收紧，仅 SYSTEM / Administrators 可写），经 SCM 注册服务，ImagePath 指向共享宿主：`"…\os.exe" -internal --run <名称>`。所有平台服务共用一份宿主二进制（不再每服务复制副本），无论注册多少服务磁盘占用保持不变。重复安装同名服务时比对来源（可执行路径 + 参数），来源不同则拒绝覆盖。
2. **运行时**：SCM 启动服务时读取 `<服务名>.osiml` 并拉起目标进程；若配置 `download_url`，启动前先确保目标文件就绪（含 SHA-256 校验）。
3. **日志**：子进程 stdout/stderr 与宿主生命周期事件写入 `logs\yyyy-MM-dd.log`（互斥串行化；支持大小滚动与 stderr 分流）。

### 服务恢复

- SCM 层：目标进程崩溃后按 `restart_delay_ms` 延迟自动重启（最多 2 次），失败计数在 `failure_reset_sec` 周期后重置；
- 宿主层：子进程**非零退出码**异常退出时自动重启（最多 3 次），超限则停止服务。

### 钩子（Hooks）

- **prestart**（`prestart_command`）：拉起目标前执行，`cmd /c` 语义支持管道 / 重定向；失败不阻断，超时 60 秒强杀（防止钩子卡死触发 SCM 30 秒启动超时）。
- **poststop**（`poststop_command`）：目标停止后执行，注入 `WINSGF_CHILD_PID` / `WINSGF_CHILD_EXIT_CODE`；失败仅告警。

### 优雅关闭

停止服务或关机时：GUI 程序接收 `WM_CLOSE`（枚举全部顶层窗口）→ 控制台程序接收 `Ctrl+C`（广播到共享控制台，宿主注册忽略处理器防误杀）→ 10 秒超时后强杀；`kill_process_tree=true`（默认）连整棵进程树一并终止。

### 集成模式（inplace）

`deploy_inplace: true` 时 `--install` 把当前 exe **原地注册**为服务：

- 不复制宿主到 ProgramData，ImagePath 直接指向当前 exe；
- `service_name` 必须等于实际 exe 文件名（如 `os`，exe 改名则以其实际文件名为准），否则 SCM 无法分派；
- 适合嵌入自有项目独立使用；不参与开机宿主升级与清理，需开发者自行到[官网 Releases](https://github.com/NXRKYMANE/Osmium/releases) 下载新版 `os.exe` 手动升级。

### 服务刷新程序

安装包会自动注册 **服务刷新程序**（`Osmium Service Refresher`），开机后执行维护并清理残留：

1. **注册（安装时）** — Inno Setup 安装程序调用 `os.exe -internal --install-refresher`，以 `-internal --refresher` 参数注册为「自动（延迟启动）」服务，确保宿主服务先于维护扫描启动。
2. **开机执行** — 系统启动约 2 分钟后扫描 `C:\ProgramData\Osmium\svcs\`，清理失效服务与孤儿目录。所有平台服务共用安装目录中的同一份宿主二进制，宿主升级由重装安装包覆盖完成，刷新程序不再逐服务替换宿主副本。
3. **清理失效服务** — 移除 osiml 缺失 / 目标不存在 / 配置解析失败的服务及其宿主目录，并清理 SCM 无记录但 `svcs` 仍存在的孤儿目录。
4. **日志清理** — 删除各服务日志及刷新程序自身日志（`%ProgramData%\Osmium\refresher\`）中超过 30 天的文件（含 `.err.log` 分流与 `.N` 滚动备份）。
5. **自动停止** — 一轮扫描后自动停止，不常驻后台。
6. **移除（卸载时）** — Inno Setup 卸载程序调用 `os.exe -internal --uninstall-refresher` 停止并移除该服务。

> 刷新程序在下次开机时运行；安装器会在安装完成后立即重启之前停止的服务。

## 构建

一键构建产出全部 2 个产物（exe + 安装包）：

```powershell
.\BUILD.ps1
```

**流水线**：构建 → 单元测试 → ISCC 编译安装包（Inno Setup 7）。

安装包编译完成后，脚本会询问是否生成可选的 UPX 压缩版。选择 `y` 后以 `opt-level = "z"`（体积优先）重建并用 UPX（`--ultra-brute --lzma`）压缩，输出 `Publish\osmium64-upx.exe`（约 1.1 MB，普通版约 3.6 MB）——不影响普通 exe 与安装包。

脚本从 `Project\Cargo.toml` 读取版本号，自动同步到 `installer.iss`（含版权年份）。测试失败会终止流水线；跳过测试用 `.\BUILD.ps1 -SkipTests`。

**代码签名**：找到证书时，全部产物（`osmium64.exe`、`osmium64-official-kits.osx`、安装包、`osmium64-upx.exe`）都会做 Authenticode 签名（SHA256 + RFC 3161 时间戳）。证书来源按优先级：环境变量 `OSMIUM_CERT_PFX`（可配 `OSMIUM_CERT_PASSWORD`），或仓库内开发证书 `Misc\codesign.pfx`（自签名，已被 gitignore 不会提交）。没有证书时流水线照常运行仅告警；显式跳过签名用 `.\BUILD.ps1 -SkipSign`。自签名开发证书签名有效但不被其他机器信任——公开发行要消除 SmartScreen 警告，请用商业证书经 `OSMIUM_CERT_PFX` 签名。

### 单独构建

```powershell
Set-Location Project
cargo build --release                     # → Project\target\release\osmium64.exe
Copy-Item target\release\osmium64.exe ..\Publish\osmium64.exe
# 构建插件 → Extension\osmium64-official-kits.osx（见 Extension\osmium-official-kits）
ISCC installer.iss                        # → Publish\osmium-win-x64-setup-v<版本>.exe
```

## 安装包部署

预构建的安装包可在 [Releases](https://github.com/NXRKYMANE/Osmium/releases) 页面获取。

### 安装包

| 安装包                             | 说明       |
| --- | --- |
| `osmium-win-x64-setup-v<版本>.exe` | 标准安装包 |

安装包将 `os.exe` 安装到 `%ProgramFiles%\Osmium\`，注册控制面板卸载条目与开机服务刷新程序。

### 安装器特性

- 将 `os.exe` 安装到 `%ProgramFiles%\Osmium\` 并加入系统 PATH
- 选择组件页：core（`os.exe`）固定必选；官方扩展包（`osmium64-official-kits.osx` → `Extension\`）**默认不勾选**，需要插件功能（sspi 下载 / 解压 / 共享映射 / 重启）时勾上，用法见 [插件指南](EXTENSION_CN.md)
- 自动注册开机服务刷新程序（`--install-refresher`）
- 注册控制面板卸载条目
- 自动检测旧版本：高版本静默升级、同版本询问重装、低版本警告降级
- 替换 os.exe 前自动停止使用它的服务，安装完成后自动重启，无重启提示

### Inno Setup 集成注意事项

在自己的 Inno Setup 安装包中嵌入 Osmium 时，注意以下几个坑：

1. **TOML 路径反斜杠** — 安装目录路径请用**单引号字面字符串**（`'C:\Program Files\ASMMS'`），避免基本字符串把 `\P` 当作转义。
2. **PATH 时效** — 安装后当前进程可能仍找不到 `os.exe`，应从注册表读取：`HKLM\Software\Microsoft\Windows\CurrentVersion\App Paths\os.exe`。
3. **提权子进程** — Inno 的 `Exec` 直接启动 requireAdministrator 子进程会返回 `ERROR_ACCESS_DENIED`，需经 `cmd.exe` 中转。
4. **静默安装语言** — `/VERYSILENT` 静默安装必须显式传 `/LANG=`（优先级最高），否则语言选择框仍会弹出卡住。

## 项目结构

```
Osmium/
├── Cargo.toml                 # workspace 配置（成员: Project / Extension/osmium-official-kits）
├── Cargo.lock                 # 依赖锁定文件（workspace 统一）
├── Project/                   # Rust 实现
│   ├── build.rs               # EXE 版本信息 / 图标 / 语言元数据（winresource）
│   ├── Cargo.toml             # 项目配置（release 速度优化）
│   ├── installer.iss          # Inno Setup 安装脚本
│   └── src/                   # Rust 源码
│       ├── main.rs            # 入口：模块装配
│       ├── service_cli.rs     # CLI：终端命令接收 / 路由 / 帮助
│       ├── service_core.rs    # 核心：SCM API、部署、服务刷新程序、下载引擎
│       ├── service_host.rs    # 服务宿主：拉起目标进程 + 插件调用
│       ├── service_config.rs  # TOML 配置模型（serde）
│       └── service_tests.rs   # 单元测试（139 个，含进程树集成测试）
├── Extension/                 # 官方工具包（外部插件可执行程序，发布为 .osx）
│   └── osmium-official-kits/  # 单一 bin（构建为 osmium64-official-kits.osx）
│       ├── Cargo.toml         # 工具包配置（格式与 Project 一致）
│       ├── build.rs           # EXE 版本信息 / 图标（Extension.ico）
│       └── src/
│           ├── main.rs        # 协议入口：stdin JSON 按 kit 字段分发 → stdout JSON
│           ├── kits_core.rs   # 共享实现集中文件（同 Project 的 service_core.rs）：
│           │                  # SSPI 下载 / 共享映射 / 解压 / 重启
│           └── kits_tests.rs  # 单元 + 集成测试（24 个 + 1 ignored）
├── Misc/                      # 图标资源（build.rs / installer 引用）
│   ├── Osmium.ico             # 安装器 / 分发图标（SetupIconFile）
│   ├── Osmium.png             # 程序图标源图
│   ├── Osmium.bmp             # 安装向导小图（WizardSmallImageFile）
│   ├── Background.bmp         # 安装向导背景大图（WizardImageFile）
│   ├── Setup.ico              # .osiml 配置文件图标（安装为 icons\osiml.ico）
│   ├── Setup.png              # .osiml 图标源图
│   ├── Extension.ico          # .osx 插件图标（安装为 icons\osx.ico）
│   └── Extension.png          # .osx 图标源图
├── Docs/                      # 文档
│   ├── EXTENSION_CN.md        # 插件开发与使用指南（中文）
│   └── EXTENSION_EN.md        # 插件开发与使用指南（英文）
├── Publish/                   # 构建产物（exe + 安装包，不提交）
├── BUILD.ps1                  # 一键构建脚本（Rust 构建与测试 + 安装包）
├── .github/                   # GitHub 社区模板（Issue / PR）
├── CLAUDE.md                  # AI 协作规则
├── CODE_OF_CONDUCT.md         # 行为准则
├── CONTRIBUTING.md            # 贡献指南
├── SECURITY.md                # 安全政策
├── LICENSE                    # 许可证
├── README_CN.md               # 中文文档
└── README.md                  # 英文文档
```

## 测试

Rust 自动化测试覆盖输入校验、启动模式解析、日志清理、进程树收集、ACL 权限判定、下载等核心逻辑：

```powershell
# Rust（139 个测试 + 插件 24 个测试 + 1 ignored，含真实进程树集成测试）
Set-Location Project
cargo test
```

- 测试集中在 `Project\service_tests.rs`，测试构建不进入正式产物；
- 覆盖路径穿越、控制字符注入、SDDL 权限判定等安全边界。

## 环境要求

- Windows 10+ x64
- 管理员权限
- 构建工具（仅构建时需要）：
  - Rust stable（edition 2024）+ MSVC 链接器（Visual Studio C++ 生成工具）— 编译 Rust 版
  - Inno Setup 7 — 编译安装包（默认路径 `C:\Program Files\Inno Setup 7\ISCC.exe`）

## 开发历史

> 2024 年的时候，我基本上学完了 Python 语言，本想尝试开发属于自己的项目，但当时笔记本电脑性能太拉跨，内存只有 8GB，导致很多时候我对内存感到焦虑。
>
> 后来我第一次接触 Minecraft Java 版，也了解到 PCL2 启动器，偶然发现 PCL2 启动器的内存清理非常好用，但是每次都要手动点击，不过意外发现可以通过 `--memory` 参数静默启动 PCL2，使其只运行一次内存清理。这让我来了兴趣，于是用 Python 写了第一个自动化服务，可是 Python 对 Win32 服务支持不是很友好，PyInstaller 打包后总是报错，加上临近中考，我暂时放弃了这个内存清理服务项目。
>
> 中考完后，我了解到一个叫 WinSW 的神奇工具，可以把任何想封装为系统服务的项目封装起来，于是当时我借用了 WinSW 开发出了第一个项目。可是当我以为项目顺利推进并包装为第一个安装包时，发现只有在我自己的电脑上能够成功安装，在其它电脑上总是诡异的报错，这让我摸不清头脑。
>
> 意识到问题的我打算着手写一个自动化服务管理平台，命名为 WSF（Windows Service Framework），这个项目也是纯 Python 写的，其实还是调用的 WinSW 内核。开发到后期发现这个框架非常地臃肿，而且安全问题也很难处理，基本上就是一个能用但是残废的状态，而且 Python 作为一个纯解释语言，冷启动也是慢到令人发指的地步，打包后大小也非常地惊人。
>
> 为了彻底解决这个问题，到了 2026 年暑假的时候，我特地去学了 Rust 语言，并且借助吃白饭的神秘蓝色大肥鱼和 WinSW 的源码，直接开发出第一代真正能用的框架。身为一个化学爱好者，我也是参考了开源社区用得比较少的名字，给第一代取名为 Silanes，也就是硅烷；后来项目为了适应 WinSW 全功能（具体操作都放到 CLAUDE.md 里面了）进行深度开发后，觉得 Silanes 这个名字不符合项目，正式重命名为 Osmium（锇），同时早期的那个快烂完的内存管理项目也演变为 Rust 编写的 Hydride 项目。

## 赞助

如果这个项目对你有帮助，欢迎 [赞助支持](https://ifdian.net/a/NXRKYMANE)。

## 许可证

Copyright © 2026 NXRKYMANE SOFTWARE
