# 项目规则

## 注释
- 注释块不超过两行；单行注释过长时折叠为两行
- 代码部分始终使用中文注释，避免英文注释

## 代码质量（每次编辑后检查）
- 优化冗余代码，消除死代码
- 优先合并可合并的代码，合并后复查是否还能进一步删除
- 清理未使用的 use 导入（Rust）

## 会话整理
- 每次对话完整理本文件
- 项目代码部分有重要变动时在本文件中记录
- 每次推送代码到仓库时消息总结一个并且使用英文
- 建立新版本 release 资产时要和往期版本的格式一致，并且也使用英文
- 每次提交不允许出现版本号
- 2个readme必须保持同步
- 重大修改和调优前记得备份一个,已经多次发生翻车事故造成项目从头来的情况
# 项目记录

## v26.8.1（2026-08-20）· EcoQoS 实现简化重构（单一 class 4）+ 双模式复验
- 重构 set_eco_qos：删除 ProcessEcoQoS（class 11/12/19）与 PowerSetEffectiveOverlayMode 的多次尝试链（本机全返回 error 87/入口缺失），改为**单一标准调用 ProcessPowerThrottling（class=4，EXECUTION_SPEED）**；逻辑简化后行为不变（任务管理器"效率模式"同底层）
- 真机双模式全链路复验（简化版）：
  - 独立模式（inplace）：auto 空闲进入（child + host）→ 繁忙联动退出 ✓
  - 平台模式（svcs 共享宿主 osP，自定义阈值 15/40、8/25）：child entered + Host entered（CPU 0.0%）→ 10 秒后 worker 忙循环 → child exited（CPU 71.7%）+ Host exited 联动 ✓
- 147 全过、clippy 零警告；测试环境清理完毕（osP 已删、临时目录已删、hydride_svc64 恢复 Running）

## v26.8.1（2026-08-19）· 效率模式（EcoQoS）自动化切换：子进程 + 宿主
- 配置 6 字段：`eco_qos`（none|always|auto）+ `eco_qos_idle_cpu_pct`（默认 10）/`eco_qos_busy_cpu_pct`（默认 30）；`host_eco_qos`（none|always|auto）+ `host_eco_qos_idle_cpu_pct`（默认 5）/`host_eco_qos_busy_cpu_pct`（默认 20）
- 实现：`set_eco_qos(pid, enabled)` 用 **ProcessPowerThrottling（class=4，Win10 1709+）**——ProcessEcoQoS（class 11/12/19）与 PowerSetEffectiveOverlayMode 在本机均返回 error 87/入口缺失，class 4 实测可用（任务管理器"效率模式"底层，PROCESS_POWER_THROTTLING_STATE: Version=1 + EXECUTION_SPEED）
- 子进程 auto：独立采样（`child_eco_sample`，不依赖 runaway 配置），连续 2 次 CPU < idle 进入、> busy 退出；子进程重启时重置状态（防旧状态残留）；always 在 start_child_process 直接设置
- 宿主 auto：自身 CPU 采样 + **子进程繁忙联动退出**（子进程 CPU > busy 时宿主也退出，密集工作期间宿主全速调度）；always 在 on_start_from 末尾设置；stop_host 开头显式退出（防停止/清理被低调度拖慢）
- 真机验证全链路：空闲 worker → `child entered` + `Host entered`（CPU 0.0%）→ worker 变忙（10 秒后忙循环）→ `child exited (CPU 59.2%)` + `Host exited (host 0.0%, child 59.1%)`（联动生效）；单测 +1（自身开/关 + 无效 PID 静默）
- 2 README 新增"效率模式"配置表；147 全过、clippy 零警告、release 构建通过

## v26.8.0（2026-08-19）· 刷新程序日志目录 refresh → refresher
- 用户要求目录名与刷新程序一致：`ProgramData\Osmium\refresh` → `ProgramData\Osmium\refresher`（refresher_log_dir join/fallback + 2 README 同步）
- 真机验证：注册 → 运行 → `ProgramData\Osmium\refresher\*.log`（[refresher] Scanning）→ 卸载移除；旧 refresh 残留目录已清理；146 全过、clippy 零警告

## v26.8.0（2026-08-19）· 术语重命名 updater/更新程序 → refresher/刷新程序
- 全项目术语统一：服务名 `Osmium Service Checker` → `Osmium Service Refresher`；内部命令 `-internal --install-updater/--uninstall-updater/--updater` → `--install-refresher/--uninstall-refresher/--refresher`；日志目录 `ProgramData\Osmium\updater` → `refresh`（日志通道 `[updater]` → `[refresher]`）；函数/常量/注释（updater→refresher、更新程序→刷新程序）
- 范围：service_core（SVC_REFRESHER_*/refresher_log_dir/write_refresher_log/refresh_outdated_hosts/SCM_REFRESHER_MODE/is_refresher_reserved_name 保留名）、service_cli（-internal 路由）、installer.iss（RefresherRegisterFail/RemoveFail 消息 + 命令）、2 README、测试（保留名断言换新名）
- 保留不动：install 覆盖更新的 "Service updated successfully"（SCM 更新语义，非 updater 机制）；AppUpdatesURL（Inno 内置属性）
- 真机验证：注册 → 运行 → `ProgramData\Osmium\refresh\*.log`（[refresher] Scanning）→ 卸载移除；旧 updater/refresher 目录残留已清理；146 全过、clippy 零警告

## v26.7.2（2026-08-19）· 平台模式（svcs 共享宿主）全矩阵真机回归
- 平台部署差异点全验证（共享宿主 Program Files\Osmium\os.exe + svcs 部署）：install → svcs\<name>\<name>.osiml 生成 + 目录 ACL 收紧（仅 SYSTEM/Admin）→ ImagePath 共享宿主 -internal --run 格式 → start（.osiml 加载）→ 日志落 svcs\<name>\logs → 优雅停止（Ctrl+C）→ uninstall 无残留
- 运行字段抽查（平台路径，宿主逻辑与 inplace 同源）：env 注入（1 变量）、优先级 High、prestart 钩子（引号空格路径 + /s 修复后）、runaway_memory_limit_mb 触发（70MB>1MB 强杀）、runaway_pid_file 回写/停止删除、plugins ping 调用（Program Files 插件发现执行）、--list 列出平台服务、--extend 绿点
- 更新路径：同源覆盖成功 + **日志保留**（更新前后 logs 文件数不变，backup/restore 生效）、异源拒绝（"already registered by a different service" 防劫持）
- 更新程序：-internal --install-updater 注册（delayed auto + ImagePath）→ 手动 sc start 扫描（Scanning 1 service）→ 自动停止 → **失效服务清理**（osiml 缺失 → "Config file missing, removing stale service" → SCM 移除 + svcs 目录清除）；-internal --uninstall-updater 移除
- 已知行为复现：restart delay 睡眠期间 --stop 不响应（SCM 超时强杀兜底，等 35s 后 SCM 已强制停止）
- 146 全过、clippy 零警告；测试环境清理完毕（osP/osQ/更新程序已卸、临时目录已删）；hydride_svc64 保持 Running

## v26.7.2（2026-08-19）· 最后 12 项字段补测 + 修复第 6 个 bug（allow_service_logon）
- 修复 ⑥`allow_service_logon` 对 `.\user` 账户静默失败：grant_service_logon_right 用 LookupAccountNameW(".\osmtest") 解析 SID 返回 0（**LookupAccountNameW 不支持 `.\` 前缀**，cmd/net 语法），授权被跳过 → 服务启动 1069 登录失败；修复为剥离 `.\` 前缀再解析；真机验证：修复前 1069 → 修复后 START_PENDING（授权生效，跨过登录失败）
- 补测（除 IIS/reboot 外全部独立部署真机）✅：log_auto_roll_at 定点滚动（未来时刻触发，{pattern}.{HHmmss}.log 归档 + 新文件）、log_roll_period_days 按天滚动（改 mtime 为 2 天前 → 下条日志触发归档）、log_mode roll-by-size 缺省 10MB（10.2MB .1 备份）、scm_sleep_time_ms=8000 生效（sc stop 后 10 秒才 Stopped，默认 <1 秒）、stop_parent_process_first 强杀路径（python SIG_IGN 忽略 Ctrl+C → 3.7 秒优雅超时 → force_kill 整树清除）、runaway_stop_parent_first=true 启动清理（带 WINSGF_SERVICE_ID 残留 cmd 树被清）、download_unzip 单条模式（下载 zip + 解压落地）、hide_window（GUI 程序窗口可见；CREATE_NO_WINDOW 仅影响控制台程序，代码分支明确）
- 测试侧记录：notepad 在 Win11 是 AppX 别名（System32 无 notepad.exe）；secedit 不导出本地账户 SeServiceLogonRight（改用 sc start 错误码区分）；Python314 安装目录 Users 可写被 P0-1 正确拦截（复制到受保护目录绕过）；控制台窗口可见性受 job 会话限制不可靠自动化
- 146 全过、clippy 零警告、release 构建通过；Program Files\Osmium\os.exe 更新为最新 release；hydride_svc64 恢复 Running

## v26.7.2（2026-08-19）· 未验证字段补测 + 第 4/5 个 bug 修复 + interactive 提示
- 修复 ④`auto_refresh` 热刷新重启失败：Ctrl+C 广播后宿主 stdin 句柄失效 → 子进程 spawn 报 os error 6（句柄无效）；build_child_command 显式 `stdin(Stdio::null())`（与 run_hook 同款修复）；真机验证：运行中改配置 → "Configuration file changed" → 优雅停止旧子进程 → 新配置子进程重启成功
- 修复 ⑤runaway CPU 日志格式化：`f()` 模板不支持格式说明符（`{0:.1}` 不被替换，CPU 值原样漏出）；改为百分比/限制值先 `format!("{:.1}")` 再插值；真机验证 "CPU 125.7% exceeds limit 50.0%"
- 可改进项：`interactive=true + 非 LocalSystem 账户` 时 CreateServiceW 报 0x80070057（参数错误），install 前主动校验并提示 "interactive=true requires the LocalSystem account"
- 补测（全部独立部署真机）✅：start_arguments 覆盖 args、stop_executable+%PID%+WINSGF_CHILD_PID（stop.log 记录子进程 PID）、auto_refresh（修复后）、download_stage after_start/after_stop、up-to-date 跳过、sha 不匹配（重下→校验失败→丢弃→fail_on_error 阻断）、download_proxy（不可达 → Connection refused → 阻断）、runaway_cpu_limit（忙循环触发）、runaway_pid_file 启动清理（残留进程按 PID 终止）、日志（log_mode reset 清空/roll 生成 .old/none 关闭 + log_reset + log_out_filename/log_err_filename 自定义名 + log_out/err_enabled 丢弃）、平台部署 DPAPI 密码加密落盘（enc:OSMIUM1:，明文不落盘）、netmap 失败告警（非致命，服务照常启动）
- 测试侧记录（非 bug）：304（If-Modified-Since）与 P1-4 设计矛盾——http+无 sha 被安全策略拦截，304 仅 https 可用（单测覆盖，真机跳过）；非 SYSTEM 账户（NetworkService）服务无法写收紧 ACL 的部署/日志目录 → 启动失败（权限设计，README 已提示）；TOML `[[数组表]]` 后顶层键失效再次踩坑（download_proxy 追加在 [[downloads]] 后无效）
- 146 全过、clippy 零警告、release 构建通过；Program Files\Osmium\os.exe 更新为最新 release；hydride_svc64 恢复 Running

## v26.7.2（2026-08-19）· 独立部署全字段真机回归 + 修复 3 个真 bug
- 真机回归（5 个 inplace 服务 + 本地 HTTP 服务器 + 官方插件），覆盖全部配置字段/插件场景，发现并修复 3 个真 bug：
  - ①`is_inplace_service` 硬编码 exe 文件名 `os.exe`——改名 exe（如 osCore.exe）的 inplace 服务注册后无法被管理命令识别（--start/--stop 报 Service not found）；改为按"ImagePath 文件名去扩展名 == 服务名"判定
  - ②`run_hook` 的 cmd 构造 `cmd /d /c "<command>"` 缺 `/s`——引号包裹的命令内重定向被 cmd 吞掉（echo x >> file 静默失败，钩子输出丢失）；补 `/s` 强制剥引号规则
  - ③`is_user_writable` 对不存在路径 fail-closed 误判"用户可写"——download_to 指向尚未下载的文件时安装被拒；改为不存在时按父目录 ACL 判定（新建文件继承父目录权限）
- 验证结论（全部通过）：delayed_auto/依赖/账户/interactive(0x110)/SDDL/failure_actions 序列/runaway 内存触发强杀/kill 进程树/event_log/preshutdown/优先级/env 注入(2)/working_directory/钩子(PID 注入)/extensions 四阶段/pid 文件/优雅停止(WM_CLOSE)；日志全字段（split/pattern/大小滚动/zip 归档带日期格式/自定义目录）+ 日志完整性；插件四阶段 + fail_on_error 阻断 + 缺失/改名/挪位/删除发现；下载全字段（分块 3MB/sha 校验/up-to-date 跳过/basic 认证/unzip 解压/数组模式保持 exe/P1-4 http 无 sha 拒绝）
- 测试侧发现（非 bug）：TOML `[[数组表]]` 之后的顶层键属于数组元素（osCore log 字段在 [[extensions]] 后、osRun runaway 在 [[failure_actions]] 后都因此失效）；`--stop` 在 restart delay 睡眠期间不响应 SCM 信号（SCM 超时强杀兜底，已知行为）
- 环境残留注意：本机 Program Files\Osmium\os.exe 已更新为最新 release；hydride_svc64 恢复 Running

## v26.7.1（2026-08-18）· 新增 --kill 命令 + 修复共享宿主异常重启读错配置
- `os --kill <name>`（简写 `--kil`，对应 WinSW dev kill）：管理员/开发者工具——按宿主注入的 `WINSGF_SERVICE_ID` 环境变量枚举全部进程定位某服务的子进程，强杀整棵进程树（先子树后自身）；预先启用 SeDebugPrivilege（SE_DEBUG_NAME 常量 + AdjustTokenPrivileges，管理员默认持有但禁用，否则无法终止 SYSTEM 级子进程）
- 实现：`service_host::kill_service_processes`（Toolhelp 枚举 + process_env_var 匹配 + collect_descendants 子树）+ `all_process_ids`/`enable_debug_privilege` 工具；CLI 帮助/路由/别名/别名测试补 4 项
- 顺带修复真 bug：共享宿主（-internal --run）异常重启路径 `load_deployed_config` 原来读**宿主 exe 旁配置**（Program Files\Osmium\os.toml 不存在 → 重启必失败），改为优先用启动时记录的 config_path（svcs\<name>\<name>.osiml）；真机验证 kill 后 5s 延迟重启成功拉起新子进程
- 真机验证全链路（ping -t localhost 常驻 → kill 进程消失 → 宿主按 restart 动作恢复）；144 全过、clippy 零警告；2 README 命令表/别名行同步；边缘测试 +2（两层进程树匹配杀整树、未知服务 Ok(0)）

## v26.7.2（2026-08-18）· 修复 --install 更新删除服务日志
- 事故：`--install` 更新已注册服务时 `force_remove_service(&svc_name, true)` 删除整个 svcs\<name> 目录（含 logs），重装/升级后历史日志全部丢失（Hydride 安装器已改 --stop 仍复现，根因在此）
- 修复：更新分支先 `backup_service_logs`（logs 挪到系统临时目录 `osmium-logs-backup-<name>`）→ force_remove_service 重建 → `restore_service_logs` 还原回新目录；新增底层可测函数 `backup_logs_dir`/`restore_logs_dir`（pub(crate)，tag 保证备份路径唯一）；无 logs（首次安装）不产生备份，挪出失败保持旧行为
- 测试：+2（backup_restore_logs_preserves_log_dir 完整还原、backup_logs_returns_none_without_logs_dir），146 全过、clippy 零警告

## v26.7.1（2026-08-18）· 新增 --refresh 命令（对应 WinSW refresh）
- `os --refresh <name>`（简写 `--rfs`）：从已部署配置重新同步 SCM 服务注册属性，不重建服务、不触碰 ImagePath/部署文件——显示名/描述/启动类型/依赖/账户密码/故障恢复/延迟启动/交互标志/SDDL 全部按 .osiml（inplace 为 exe 旁同名 toml）重写；allow_service_logon 同步授权
- 实现：`service_core::refresh_service` 用 `ChangeServiceConfigW`（含 lpDisplayName 显示名，windows crate 0.62 签名带该参数）+ 闭包内 ChangeServiceConfig2W（描述/故障恢复/延迟启动显式 true/false/SDDL）统一关句柄；OpenServiceW 须 `SERVICE_ALL_ACCESS`（SERVICE_CHANGE_CONFIG 设 failure actions 会拒绝访问 0x80070005）
- CLI：帮助文本/路由/is_cli_command 补 `--refresh | --rfs`；别名测试补 2 项；service_host 的 config_path_next_to 转 pub(crate) 供 refresh 定位 inplace 配置
- 2 README 命令表/别名行同步；真机验证（临时服务：改显示名/描述 → refresh → SCM 属性更新成功 → 卸载无残留）；144 全过、clippy 零警告；边缘测试 +3（非法名/系统服务/未知服务拒绝，只读 SCM）

## v26.7.1（2026-08-18）· 资产重命名 + 插件发现放宽 + 安装器无重启升级
- 资产重命名（与元素锇全称统一）：`os64.exe` → `osmium64.exe`、`os-upx.exe` → `osmium64-upx.exe`、`osmium-okits.osx` → `exts\osmium64-official-kits.osx`（BUILD.ps1 输出/签名/提示 + installer.iss Source/组件描述同步；安装后仍改名 `os.exe`）
- 插件发现放宽：`plugin_dir()` 从 exe 同级 `exts` 改为 **exe 所在目录本身**，递归扫描全部 `.osx`（仅跳过 `.` 开头隐藏目录）——独立部署不再强制 exts 子目录；平台安装仍装 `{app}\exts`；run_plugin 缺失错误消息同步
- 安装器修复 os.exe 占用：PrepareToInstall 新增"停止所有 ImagePath 含 os.exe 的 SCM 服务"（PowerShell 枚举），等待退出从仅静默模式改为全部模式
- 取消安装后重启提示（删 RebootPrompt 消息 + ssDone shutdown）：停止服务时服务名写入 `{tmp}\osmium-svc-list.txt`，ssPostInstall 自动重启全部——WMI `StopService()` 并行停止 + 轮询等 Stopped（总超时 3 分钟，防 N 服务串行叠加），`StartService()` 异步触发不等待（防慢启动服务阻塞安装器）
- 版本升至 26.7.1；2 README 安装器特性/重启说明同步；139 + 24/1 全过、ISCC 编译通过、真机验证停止/重启链路（hydride_svc64 停止 4.6s → 重启 2s 返回恢复 Running）

## v26.7.0（2026-08-18）· 代码签名（Authenticode）集成
- 生成自签名代码签名证书（CN=Osmium Dev Signing，2026-08 起 5 年，RSA2048/SHA256，CodeSigning EKU），导出 `Misc\codesign.pfx`（固定密码，`.gitignore` 排除，绝不提交）；signtool 用 `F:\DevTools\Windows11 SDK\bin\10.0.28000.0\x64\signtool.exe`
- BUILD.ps1 集成签名：`Get-SignCert` 证书来源优先级——环境变量 `OSMIUM_CERT_PFX`（+可选 `OSMIUM_CERT_PASSWORD`）→ 仓库 `Misc\codesign.pfx`；`Sign-File` 用 `/fd SHA256 + RFC 3161 时间戳`（DigiCert → Sectigo → Comodoca 依次回退，全不可达时无时间戳签名并告警）；签名对象：os64.exe、exts\osmium-okits.osx、安装包（ISCC 编译后）、os-upx.exe
- 新参数 `-SkipSign`（跳过签名）；无证书/signtool 缺失时自动跳过仅告警
- 实测通过：DigiCert 时间戳成功（Done Adding Additional Store），签名链完整；自签名证书 Status=UnknownError 属预期（根不受信任），真机验证签名元数据/时间戳齐全；消除 SmartScreen 需商业证书走 OSMIUM_CERT_PFX
- 注意：PowerShell 5.1 的 Export-PfxCertificate 产物 signtool 可读（需传 /p 密码）；New-SelfSignedCertificate 证书存储内私钥不可再导出，备份以 pfx 为准

## v26.7.0（2026-08-15）· 品牌重命名 Silanes → Osmium（按新软件对待，新旧共存）
- 全项目品牌重命名：exe `osmium64.exe`（Cargo bin）、安装目录 `Program Files\Osmium`、部署目录 `ProgramData\Osmium\svcs`、更新程序服务名 `Osmium Service Updater`、CLI 前缀/帮助/事件日志来源/DPAPI 前缀 `enc:OSMIUM1:`、安装包 `osmium-win-x64-setup`、注册表 App Paths / Uninstall / ProgID、文档 4 README 全部同步
- 配置扩展名 `.silml` → `.osiml`（部署路径 svcs\<name>\<name>.osiml、宿主 with_extension、文件关联、图标 osiml.ico）；快捷别名 `sil` → `os`（Misc\os.cmd）
- 旧版迁移兼容全部移除（新旧软件共存，互不清理）：删除 DPAPI 旧前缀解密、旧版更新程序服务清理、installer.iss 的 NSIS 旧卸载键与旧目录清理；AppId 更换为新 GUID 使旧版 Inno 安装不被识别为同产品
- 保留不改名的功能性标识：`WINSGF_SERVICE_ID` 环境变量
- 版本升至 26.7.0；依赖升级（toml 1 / base64 0.23 / sha2 0.11 / reqwest 0.13 / zip 8 / windows 0.62，消 IDE 新版本警告）；Publish 旧产物已清空
- HTTP 客户端 reqwest → ureq 3.4（轻量化）：零 tokio/hyper/aws-lc 依赖，改用 url 2.5 解析 URL；`http_status_as_error(false)` 由调用方按状态码处理（304/401/206/404），Basic 认证头改为请求级附加，超时判定 Error::Timeout；exe 5.11 → 3.59 MB（-30%），依赖树 295 → 201 行；需注意 ring 汇编 + LTO 偶发链接 0xc0000005（重试即过）
- 测试：保留名测试改为仅校验新名，共 125 个 + 1 ignored 全过
- 图标：Proj.ico/Proj.png/Rust.bmp/Rust.png 删除，换 Osmium.ico（Osmium.png 裁剪透明边距后 16/32/48/256 多尺寸生成）；Setup.ico 重做（Setup.png 实际 80x88 无透明边距，等比居中）；新增 Extension.ico（Extension.png 裁剪 80x84，作 .osx 插件图标，installer 注册 .osx 关联 osx.ico）；Docs 目录删除不再随安装包分发，installer.iss 移除 Docs/Rust.bmp/GetDocPath 引用，README 规则改为 2 个同步
- 插件化起步：新建 Extension\osmium-official-kits（格式与 Project 一致，多 bin 工具包：lib 共享实现 + src/bin/kit_*.rs 每功能一个插件），SSPI 认证下载从 service_core.rs 搬迁为 osmium-kit-sspi；协议为 stdin 单行 JSON（url/to/username/password/proxy/timeout_secs）→ stdout 单行 JSON（ok/error），退出码 0/非0；tmp 原子写入 + 改名；6 个测试全过
- 主程序瘦身：删除内置 SSPI（SspiGuard/sspi_spn/sspi_download/split_credential/DownloadAuth::Sspi），配置 download_auth=sspi 报迁移提示（"moved to the osmium-kit-sspi plugin"）；Cargo.toml 移除 Win32_Security_Credentials feature；Project 源码迁入 src\ 子目录；宿主 run_plugin 调用机制留待下一步；测试 125 → 121 全过
- 主程序二次瘦身：共享目录映射/下载解压/系统重启迁入 kits（osmium-kit-netmap/unzip/reboot，stdin JSON 协议同 sspi）；宿主对应配置（shared_directory_mappers / download_unzip / failure reboot 动作）报迁移提示并安全降级；Cargo.toml 移除 WNet/Shutdown features，exe 3.55 → 3.48 MB；kits 共 4 个插件 8 测试全过
- CLI 拆分：Project\src 新增 service_cli.rs（main_entry/路由/帮助/9 个命令壳），service_core.rs 转为纯后端逻辑（SCM/部署/下载/更新器，被调函数 pub(crate) 化）；main.rs 只做模块装配；测试 121 → 119（cli 相关测试 import 改路径）；clippy 无新增告警
- kits 结构对齐 Project：共享实现 5 文件（sspi/netmap/unzip/reboot/lib_tests）合并为单一 kits_core.rs（lib.rs 只做模块装配，测试内联 #[cfg(test)] mod tests），bin 仅协议入口；kits 8 测试全过、4 bin release 构建通过
- kits 二次合并：4 个 bin 协议入口（kit_sspi/netmap/unzip/reboot）合并为单一 src\main.rs（stdin JSON 按 kit 字段分发 sspi/netmap/unzip/reboot，构建产物 osmium-kit.exe → .osx），删除 src\bin 目录；kits 最终结构 main.rs（协议入口）+ lib.rs（装配）+ kits_core.rs（共享实现），与 Project 的 main.rs/service_cli.rs/service_core.rs 对应；8 测试全过、clippy 零警告
- kits 测试独立：测试从 kits_core.rs 内联 mod tests 移出为 src\kits_tests.rs（对齐 Project 的 service_tests.rs），lib.rs 声明 #[cfg(test)] mod kits_tests；8 测试全过
- kits 去 lib.rs：删除 src\lib.rs 改为纯 bin 项目，main.rs 直接 mod kits_core + #[cfg(test)] mod kits_tests（对应 Project main.rs 模式）；8 测试全过、osmium-kit.exe release 构建通过
- 代码整理：service_cli.rs 帮助文本移至文件顶部（入口→帮助→辅助→路由→命令→底层辅助排序）；service_core.rs 模板工具 f() 移入底部工具区、块标题修正；service_host.rs 顶部补块标题、build_child_command 从日志块移入"子进程 Command 构造 & 输出消费"块；全项目注释按规则修正（折叠超两行注释块、更新过时 bin 结构注释、测试文件头去历史描述）；main.rs 补模块装配注释；119+8 测试全过、clippy 无新增
- 依赖瘦身：主项目移除未使用的 Win32_System_Memory feature（grep 全项目无引用）；zip/base64 注释更新（解压已迁插件、base64 兼用于 DPAPI）；kits 全依赖逐一核验无冗余；构建 + 119/8 测试全过
- 产物与命名重构：kits opt-level 改 "z"（2.43 → 1.93 MB）；主项目保持 opt-level 3；BUILD.ps1 输出 Publish\os64.exe + Extension\osmium-okits.osx（osmium-kit.exe 改名）+ 安装包，UPX 版 os-upx.exe；Misc\os.cmd 删除、Misc\images 图片平铺到 Misc 根（build.rs/installer 图标路径同步）；主程序命名 osmium64 → os（install_path/get_own_path/is_inplace/svc_name/帮助文本），安装时 os64.exe 改名为 os.exe；installer 组件页（core 固定 + osx 扩展默认不勾选，osmium-okits.osx → {app}\Extension）；readme 查看逻辑确认彻底移除；$ErrorActionPreference 改 Continue（cargo stderr 触发 NativeCommandError 中断问题）
- 插件调用落地：service_host 新增 run_plugin（exts\*.osx 递归发现 + stdin/stdout JSON 协议），四功能接入——sspi 下载（插件完成下载+宿主 sha 校验）/ unzip 解压 / netmap 启停映射（失败仅告警）/ reboot 动作；新增 run_plugin_missing_extension_reports_not_found 测试，主项目 120 + kits 8 全过
- 插件目录重构：安装输出目录 Extension → exts（Publish\exts + {app}\exts）；宿主导入 discover_plugins 递归扫描 exts 下所有 .osx（跳过名称以 . 开头的目录），run_plugin 遍历全部插件首个 ok 即成功；新增 ensure_osx_association（reg.exe 幂等写入 HKCR，服务启动兜底强制 .osx 关联，不依赖安装器）；installer.iss 加 [UninstallDelete] 卸载清空 {app}\*（旧版遗留图标/别名一并删除）、图标改装 {app}\imgs\、删除 app.ico 安装条目；120/8 测试全过、含 UPX 全量构建通过
- CLI 扩展与 inplace 数据落地：inplace/独立部署 panic.log 落 exe 同目录（平台安装才写 ProgramData）；新增 --extend/--ext 命令列出已安装插件并检查可用性（plugin_usable 启动探测，可用绿点/不可用红点，stdout VT 渲染）；帮助文本 --list 下新增 --extend 行、deploy_inplace 提示下显示已安装插件（无则 None）；120/8 测试全过
- 插件协议完备：可用性改为 ping 协议探测（喂 {"kit":"ping"} 验证 ok=true，5s 超时）；插件新增 ping kit、空输入静默退出（双击无显示）、失败 stderr 抛 "osmium-kit error" 详情；插件移除内嵌图标；新增 tests/protocol.rs 集成测试（真实调用 bin，冒烟 ping/unzip、暴力坏 JSON/未知 kit/缺字段、边缘空输入静默/zip-slip/netmap 坏共享，7 个）；宿主侧补 plugin_usable 对非协议可执行（cmd.exe）与 discover_plugins 空目录测试；主项目 123 + kits 8 单元 + 7 集成全过
- 插件功能可用性验证：集成测试补 sspi 协议层端到端（本地 TcpListener HTTP 服务器）——200 直连真实下载落地并校验内容与 tmp 清理、401 无挑战快速失败（ok:false + stderr 详情）；kits 8 单元 + 9 集成全过；netmap 成功映射（需真实共享）与 reboot（真重启系统）不可自动化，留人工验证
- SSPI 真机 IIS 回归（本机）：修复本机 IIS（WAS 0x80070003 因 C:\inetpub 标准目录缺失，重建 wwwroot/logs/temp 后 W3SVC 恢复）；sspitest 站点（Windows 认证 + Negotiate,NTLM、匿名关、8808 端口，避开 Steam 占用 80/8080）；新增 #[ignore] 测试 sspi_download_authenticates_against_real_iis（真实 Negotiate/NTLM 挑战循环 → 200 下载落地校验）
- SSPI 握手连接复用修复：curl 2 轮成功而插件 3 轮失败（Type3 被 IIS 拒）根因——401 响应体未读导致 ureq 连接不归还池，每轮新连接使 NTLM 状态（绑定 TCP 连接）丢失；修复为 401 分支读完 body 丢弃后再进下一轮；同时补 InitializeSecurityContextW 请求标志（CONFIDENTIALITY/MUTUAL_AUTH/INTEGRITY）；kits 8 单元 + 9 集成 + 1 ignored 全过
- 插件测试补全与 IIS 清理：新增 4 个测试——显式凭据路径（DOMAIN\User 构造 SEC_WINNT_AUTH_IDENTITY_EXW + 匿名 200 成功）、proxy 不可达报错（DISCARD 端口验证代理分支生效）、协议层缺 url 快速失败、netmap unmap 空条目成功 + 非法 action 失败；剩 reboot（真重启）与 netmap 成功映射（需真实共享）不可自动化留人工验证；真机 IIS 验证完成后清理测试环境（删除 sspitest 站点与 C:\inetpub\sspi-test、Default Web Site 端口还原 80、删除 iisstart.htm），IIS 真机回归测试改为 #[ignore] 协议层端到端保留（注释注明站点重建要求）；kits 10 单元 + 12 集成 + 1 ignored 全过、clippy 零警告
- kits 测试合并单文件：tests\protocol.rs 集成测试并入 src\kits_tests.rs（删除 tests 目录），invoke 改用运行时路径推导（option_env CARGO_BIN_EXE 优先，单元测试场景回退 current_exe 的 deps 上一级取 osmium-kit.exe，缺失时经 CARGO_MANIFEST_DIR 上溯 workspace 根自动 cargo build -p）；kits 22 单元/集成 + 1 ignored 全过、clippy 零警告
- 仓库根 workspace 化（RustRover 识别修复）：根 Cargo.toml 建 [workspace] members=[Project, Extension/osmium-official-kits]（resolver=3），解决 IDE 提示"插件项目不属于已知 Cargo 项目"；产物统一根 target、根 Cargo.lock（132 包，删除成员级 Cargo.lock 与旧 target 目录）；profile.release 提升到 workspace 根（成员级 profile 会被忽略），BUILD.ps1 适配——插件构建加 --config 'profile.release.opt-level="z"'、产物复制与 UPX opt-level 切换改指根路径与根 Cargo.toml；.gitignore 改 target/；kits 测试二进制路径推导修正 + invoke 缺失时自动构建；clippy 新规则（Rust 1.97 too_many_arguments）在 build_child_command/run_hook 加 allow；123 + 22/1 全过、clippy 零警告；注意 Extension 目录尚未被 git 跟踪（含此前所有插件改动）
- 事故与重建：git checkout 误将 service_core/service_host/service_tests 恢复为 index 旧版（Silanes/reqwest 时代），v26.7.0 未暂存改动丢失且 LocalHistory 无内容可恢复；按 v26.7.0 目标完整重建三个文件——品牌 Osmium（CLI_PREFIX/保留名/ProgramData\Osmium\svcs/enc:OSMIUM1:/os.exe）、CLI 拆分对齐 service_cli.rs（main_entry 等已迁出）、共享宿主（-internal --run + scm_entry 显式名 + deployed_config_path + parse_run_service_name + is_osmium_deployed 新旧格式）、ureq 化下载（删除 reqwest/内置 SSPI/SspiGuard/sspi_spn/split_credential，DownloadAuth 仅 None/Basic）、插件调用落地（run_plugin/discover_plugins/plugin_usable，sspi/netmap/unzip/reboot 四功能迁移提示 + 宿主降级）、更新器简化为只清理（get_file_version/compare_versions 转 #[cfg(test)]）、panic_log_path（inplace 落 exe 旁）、windows-result 0.4.1 适配（Error::from_hresult + HRESULT::from_win32、LocalFree 收 Option、LookupAccountNameW Option 参数）；测试 122 → 123（去 sspi/netmap/unzip 内置测试、补 parse_run_service_name 2 个 + 插件迁移报错 3 个 + userinfo 去敏 1 个）；123 + 22/1 全过、clippy 零警告、release 构建 + --extend 冒烟通过；注意 git 状态仍是旧 index（需用户重新 git add）
- 代码整理（排序/注释/冗余）：service_host.rs 补块标题（常量/宿主配置路径/服务宿主结构&日志参数/构造&入口/运行监控&停止流程/子进程启动&控制/停止策略&钩子），build_child_command 从日志块移入"子进程 Command 构造 & 输出消费"块（对齐 v26.7.0 记录）；service_core.rs 块标题细化（"SCM 宿主入口 & 服务安装部署"）、合并 write_deployed_config 冗余中间变量、折叠 2 处超长注释为两行（apply_service_sddl/delete_old_logs 的 3 行注释压缩）；注释规范全查（3 行连续注释清零、超长行折叠）；全项目 clippy 零警告 + 123/22/1 全过
- 测试补全与真机验证：主项目补 3 个测试（panic_msg 提取 &str/String/兜底、panic_log_path 分支、write_log_line 写入日期条目）126 全过；真机验证——netmap 插件真实映射（\\localhost\Users 共享 → Z: 映射成功 + 文件访问 + unmap 断开）；SCM 全生命周期（install→start→status Running→子进程日志→stop 优雅 Ctrl+C→uninstall 无残留，ImagePath 共享宿主 -internal --run 格式确认）；stop_child_process 集成（winver GUI 子进程 → SCM stop → 日志 "Child exited via WM_CLOSE" 优雅停止）；剩余不可自动化：kits reboot（真重启）、netmap 成功映射已真机覆盖、SCM 真实操作已真机覆盖；CLAUDE.md 记录
- 安全审查与修复：发现 P0 提权漏洞——exts 插件目录/文件无 ACL 加固（Authenticated Users 可写），任意登录用户可替换 .osx 插件，宿主以 LocalSystem 执行即提权；修复三处——installer.iss 加 SecureExtsDir（ssPostInstall 阶段 takeown + icacls 重建 DACL 仅 SYSTEM/Admin 完全控制）、宿主新增 plugin_path_trusted（run_plugin/plugin_usable 执行前校验插件目录与文件 ACL，不可信拒绝执行）、invoke_plugin 加 5 秒超时（子线程读 stdout + 超时强杀，防恶意插件挂死宿主）；验证——未加固 temp 目录插件被拒绝执行（rejected 日志）、加固目录正常、installer 编译通过；126 + 22/1 全过、clippy 零警告
- 安全审查第二轮：发现 P0-3——平台部署（svcs 模式）未校验目标 service_executable_path/working_directory 可写性（inplace 有 P0-1 而平台缺失），管理员可注册指向 Public/Downloads 可写目录的服务，攻击者替换 exe 或放恶意 DLL 侧加载获 SYSTEM 提权；修复为安装时校验 exe/目录/工作目录 ACL（is_user_writable），可写则拒绝注册；同时修 P1 两处——on_start_with_name 未校验 SCM ImagePath 传入的服务名（防 deployed_config_path 路径穿越读 svcs 外 .osiml，补 is_valid_service_name 校验）、write_quick_config 的 tmp 文件用 std::fs::write 可被预创建替换（改 create_new 原子创建 + PID 后缀文件名）；新增测试 shared_host_rejects_invalid_service_name_from_scm；127 + 22/1 全过、clippy 零警告
- 用户体验审查（错误提示/日志清晰度）：消除模糊错误——Config file not found/Invalid file path 带具体路径与原因、Service not found 带服务名并提示 --list、uninstall 失败带服务名与 --status 指引、do_stop 失败带服务名与原因；宿主日志增强——配置热刷新失败带解析错误详情、failure action 日志含退出码/动作序号/延迟/重启结果、none 动作明确"stopping service"；下载失败日志含目标路径与排查建议；kits 的 AcquireCredentialsHandleW 失败带 Win32 错误码与身份提示；127 + 22/1 全过、clippy 零警告
- kits 错误提示补全：网络/IO 错误带 URL/目标路径上下文（request failed for {url}/failed to write '{to}'/server returned HTTP {} for {url}）、unzip 全部错误带 zip 路径与条目名/目标目录（zip-slip 报条目名）、tmp 创建失败带 tmp 路径、401 明确提示"check download_username/download_password"（single_download 与 download_chunk 统一）、sspi 3 轮超限提示凭据/协商问题、netmap 空 mappers 明确报错（不再静默成功，防宿主误以为映射成功）；SCM 宿主 RegisterServiceCtrlHandlerExW 失败不再静默（报服务名+错误码）；主项目单测 127 + kits 22/1 全过、clippy 零警告
- 安全审查第三轮 + 重构：修复 P0-3 延伸——下载目标（download_to/downloads[].to 绝对路径）指向可写位置时同样可被预放恶意文件替换提权，纳入安装可写性校验；修复 bug——run_extensions 的 stdout_path/stderr_path 相对路径未按部署目录解析（原写到进程当前目录）；kits 补 stdin 输入 1MB 上限与 unzip 总解压 8GiB 上限（zip bomb 兜底）；重构——deployed_config_path 统一 4 处重复 .osiml 拼接、map/unmap_shared_via_plugin 合并为 netmap_via_plugin(action)、current_config 与 try_restart_child 提取 load_deployed_config 共用；tick 防重入与 stop_child_process 检查失败补日志；127 + 22/1 全过、clippy 零警告
- 测试补全（上轮安全修复回归）：主项目 +7——deployed_config_path 布局、is_updater_reserved_name 大小写、has_download 空白裁剪、green_dot/red_dot 无 VT 无色、decrypt_sensitive 三字段解密（SharedMapperConfig 手写字段，无 Default）、恢复丢失的 discover_plugins（结构不变量 + 隐藏目录过滤）与 plugin_usable（cmd.exe 5s 超时判定不可用）；kits +2——stdin 恰好 1MB 边界完整解析、超 1MB 截断快速失败（invoke_large 分块写入容忍 broken pipe，防管道阻塞死锁）；clippy 修复 repeat().take()→repeat_n 与一处多余 to_string；主项目 134 + kits 24/1 全过、clippy 零警告
- 插件化兼容层清除：主项目删除全部"已迁移"痕迹——迁移提示日志 3 处（netmap "moved to the" / unzip "moved to the" / reboot "via plugin"）改正常功能日志、sspi 降级分支与 sspi_download_via_plugin 函数整体删除、download_auth_from_entry 移除 sspi 映射（未知值落 None）、配置/枚举/函数注释清理（"已迁移至"字样清零）；download_auth=sspi 由"静默降级+提示"改为 run_download_entry 直接拒绝（明确报错指向 osmium-kit-sspi 插件，防无认证静默下载）；测试同步——4 个 migration 测试重命名 missing_plugin_reports_error、download_auth_from_entry_maps_modes 删 sspi 断言（kerberos 已覆盖未知值）、load_config 解析测试 auth 改 basic、warn_if_insecure_download 删 sspi 两段（P1-4 已有独立测试）、新增 sspi_auth_rejected_in_download_config 集成测试（on_start_from 启动失败 + 日志断言含 "not supported"）；2 README 的 download_auth 行同步（sspi 由官方插件提供，配置会直接报错）；主项目 135 + kits 24/1 全过、clippy 零警告
- 第三方插件接入（配置驱动，无需改宿主）：ServiceConfig 新增 plugins 数组（PluginCallConfig: kit/phase/payload/fail_on_error，phase 与 extensions 同四阶段，payload 为 JSON 对象合并进请求）；宿主新增 run_plugin_calls 接入 4 个生命周期点——start_before（fail_on_error=true 阻断启动）、start_after/stop_before/stop_after（仅告警，进程已起/停止流程不可回滚）；非对象 payload 规范化为空对象保证 kit 字段可注入；stop_host 重构——current_config 提前到停止流程开始处供 stop 两阶段插件与下载/netmap 复用；测试 +4——plugins 全字段 TOML 解析（payload 对象字段断言）、缺插件 fail_on_error=false 启动不阻断（日志 non-fatal 告警）、=true 阻断启动、stop 阶段失败插件不影响启动；2 README 补 plugins 配置表行与示例段；主项目 139 + kits 24/1 全过、clippy 零警告
- sspi 官方插件支持回归（内建配置字段）：download_auth=sspi 由"直接拒绝"改回正式支持——try_download_entry 提前分流 sspi_download_via_plugin（osmium-kit-sspi 插件完成 401 挑战-响应循环并原子落盘，宿主补 sha 校验，凭据/proxy 进 payload）；run_download_entry 的 sspi 拒绝校验删除；download_auth_from_entry 注释说明 sspi 不经映射；配置注释恢复（download_auth/auth 支持 sspi）；测试——sspi_auth_rejected_in_download_config 改名为 sspi_download_missing_plugin_fails_clearly（插件缺失 → 启动失败 + 日志含 sspi 失败详情）；真机端到端冒烟（本机）——宿主 --test + download_auth=sspi + 加固 exts（takeown /a 所有者归 Administrators + icacls 仅 SYSTEM/Admin）→ 日志链路完整（Downloading → SSPI download error: plugin exited with code 1 → Download failed），验证 ACL 信任校验/插件调起/协议交换/失败传播全链路；2 README 的 download_auth 行同步（sspi 由官方插件处理，未装插件明确报错）；主项目 139 + kits 24/1 全过、clippy 零警告
- 服务更新程序重命名 Osmium Service Updater → Osmium Service Checker：service_core.rs 12 处（保留名校验/注册/移除/更新程序自识别）+ 测试 + 2 README 同步；内部命令名 -internal --install-updater 保留（接口不变）；installer.iss 无服务名字符串无需改；139 + 24/1 全过、clippy 零警告
- 插件信任模型修正（集成部署支持）：plugin_path_trusted 信任锚点改为宿主 exe 自身位置——exe 位于用户可写目录（inplace 集成部署/开发者目录）时插件与 exe 同级自动放行（攻击面与宿主一致，能替换插件的攻击者同样能替换 exe，不额外增加风险）；exe 位于受保护位置（Program Files 等）时保持严格校验（exts 与插件文件须仅 SYSTEM/Admin 可写，防 P0 提权）；真机双分支验证——可写目录 exts 插件真实执行（Plugin completed: ping）、受保护 exe 目录 + 可写 exts 拒绝（refusing to execute）；2 插件指南安全节同步；139 + 24/1 全过、clippy 零警告
- 真机测试发现并修复 2 个 bug + CodeQL 修复：①sddl_sid_is_administrative 不识别 TrustedInstaller（S-1-5-80-*）→ System32 下 cmd.exe 被误判"用户可写"导致安装误拒，加前缀识别 + 测试；②installer.iss SecureExtsDir 的 takeown 漏 /A → exts 所有者归当前登录用户而非 Administrators → 官方插件装完必红，补 /A（注释说明）；③CodeQL rust/access-invalid-pointer——get_file_version 的 VerQueryValueW 输出指针解引用缺非空校验，加 is_null 检查；DPAPI 加密/解密两处 from_raw_parts 同样预防性补 pbData 判空；kits SSPI 已有判空无需改；真机全链路验证通过（安装→启动 Running→Ctrl+C 优雅停止→卸载、插件 ping 调用、保留名/路径穿越/插件缺失阻断/快速安装/不存在配置等边缘）；139 + 24/1 全过、clippy 零警告
- 注意：GitHub 远程与文档 URL 已改为 NXRKYMANE/Osmium，若远端仓库尚未改名需同步重命名

## v26.6.0（2026-08-12）· 共享宿主部署（去重每服务 exe 副本）
- 背景：平台部署原先每服务复制一份宿主 exe（svcs\<name>\<name>.exe），批量安装磁盘体积 N×4.3MB；改为共享宿主
- install 不再复制 exe；ImagePath 改为 `"<共享宿主>" -internal --run "<name>"`（服务名引号包裹，兼容空格）；共享宿主优先 `%ProgramFiles%\Osmium\osmium64.exe`，框架未安装（源码直跑）回退当前 exe
- 新增 `-internal --run` 入口 + `SCM_EXPLICIT_NAME` 全局 + `scm_entry`/`scm_svc_name` 显式服务名 + `ServiceHost::on_start_with_name` + `service_core::deployed_config_path`
- `is_osmium_deployed` 兼容新旧格式（新 `--run` 解析 + 旧 svcs 前缀）；新增 `parse_run_service_name`（pub(crate)，定位 --run 后内容并去引号）
- 升级器 `upgrade_outdated_hosts` 简化为只清理（删除逐服务 exe 版本对比替换；宿主升级由重装安装包覆盖共享 exe 完成）；`get_file_version`/`compare_versions` 转 `#[cfg(test)]`
- 测试：新增 parse_run_service_name 2 个，共 124 个 + 1 ignored；clippy 无新增告警
- 文档：4 个 README 部署模型同步（共享宿主 ImagePath / 更新程序只清理）、测试数 122→124；wiki 同步

## v26.6.0（2026-08-12）· redact_url 去敏补 userinfo（防内嵌凭据进日志）
- 安全审查发现：redact_url 仅去 query/fragment，未去 URL 内嵌凭据（http://user:pass@host），明文凭据会随下载日志落盘，绕过 DPAPI"明文不落盘"意图
- 修复：redact_url 补 u.set_username("") + u.set_password(None)；测试新增 redact_url_strips_userinfo_credentials，并更新 redact_url_edge_cases 旧断言；全量 122 个 + 1 ignored，clippy 无新增告警

## v26.6.0（2026-08-12）· 文档补充：独立部署推荐 UPX 版
- 4 个 README（EN/CN + 2 HTML）与 wiki（Features/Build-Guide）在 Rust 实现表格后补充引用：独立部署优先使用 UPX 压缩版（约 1/4 体积、便于分发；仅启动一次性多几十毫秒，运行性能无差异）
- 实测冷启动：原版 ~16 ms，UPX 版 ~79 ms（LZMA 解压开销，运行期无性能差距）

## v26.6.0（2026-08-12）· 版本升级 + 完整构建（含 UPX）
- 版本升至 26.6.0（Cargo.toml 唯一来源；Cargo.lock 随 cargo 自动更新；installer.iss 的 MyAppVersion 由 BUILD.ps1 自动同步）
- BUILD.ps1 完整构建通过：release 构建 + 121 测试 + 1 ignored 全过 + 安装包 osmium-win-x64-setup-v26.6.0.exe（3.61 MB）
- UPX 压缩：opt-level="z" 重建（3.35 MB）+ upx --ultra-brute --lzma → Publish\osmium64-upx.exe（1.19 MB）；注意 BUILD.ps1 的 UPX 交互询问在非交互终端下 Read-Host 阻塞（管道输入不生效），本次以手动等价步骤完成
- 文档：4 个 README 测试数补 120→121、UPX 大小 ~1.5→~1.2 MB（实测 1.19 MB）；wiki 同步版本 v26.6.0 / 测试数 / UPX 大小（本地已改未推送）

## v26.5.1（2026-08-12）· --test 简写 --tst + Wiki 同步
- CLI：--test 支持简化别名 --tst（run_cli 原有 "test"|"tst" 路由）；is_cli_command 补充 "--test"|"--tst"，使省略 -m 前缀直接可用（与帮助/README 中 `sil --test` 用法对齐），函数转 pub(crate)
- CLI 帮助标注 --tst：Short aliases 行补 --tst；--test 行描述精简为 "Run in foreground"（删括号说明、对齐描述列）
- 文档：4 个 README 命令表 --test 行补"可简写 --tst"，简化别名行补 --tst（对应测试）；GitHub Wiki 8 页同步 v26.5.1（Configuration 全 79 字段、Features/Home/Build-Guide 数值与功能、User-Guide --test/--tst、FAQ/Updater 日志清理细节；本地已改未推送）
- 测试：新增 cli_short_aliases_cover_test（全命令+别名含 --tst 识别、非命令不误判），共 121 个 + 1 ignored；clippy 无新增告警

## v26.5.1（2026-08-11）· 修复最后 2 个 WinSW 细节缺口
- BASE 环境变量注入子进程（对应 WinSW wrapper 自动设置 BASE）：build_child_command 在用户 env 之外自动注入 BASE=部署目录（用户 env 显式配置 BASE 时以用户为准，大小写不敏感检测）；子进程可直接读取 %BASE% 对应的 BASE 变量
- RunawayProcessKiller 防误杀（对齐 WinSW #237）：子进程自动注入 WINSGF_SERVICE_ID=服务名（对应 WinSW WINSW_SERVICE_ID）；runaway_cleanup_pid_file 新增 expected_service_id 参数，清理前经 process_env_var 读取残留进程该变量，不匹配则跳过并告警（防 PID 被系统复用时误杀无关进程）；cleanup_runaway_pid 传入服务名
- 新增 process_env_var（pub(crate)）：NtQueryInformationProcess(ProcessBasicInformation, Wdk_System_Threading) → PEB+0x20 ProcessParameters → +0x80 Environment（Windows 10+ x64 布局 PVOID）→ 逐块 ReadProcessMemory 至双 null 结尾（上限 256KB）→ UTF-16 条目大小写不敏感匹配；windows features 新增 Win32_System_Diagnostics_Debug / Wdk_System_Threading
- 测试：新增 3 个（process_env_var 读子进程注入变量与 PATH、BASE/WINSGF_SERVICE_ID 注入与用户 BASE 覆盖、pid 标识不匹配跳过/匹配清理），共 116 个 + 1 ignored；clippy 无新增告警、release 构建通过
- 文档：4 个 README 同步（env 行补 BASE/WINSGF_SERVICE_ID 自动注入，runaway_pid_file 行补防误杀说明）；CLAUDE.md 记录

## v26.5.1（2026-08-11）· 补齐最后 5 个 WinSW 缺口
- 多下载条目：ServiceConfig 新增 downloads 数组（DownloadConfig 条目 from/to/sha256/fail_on_error/auth/username/password/unsecure_auth/proxy/unzip/stage，缺省回退配置级 download_*）；新增 download_entries 归一化（数组优先，旧单条字段兼容）、download_entry_stage（条目级→配置级→before_start）；prepare_download/run_aux_download 改为逐条按 stage 过滤执行；数组模式可执行路径保持 service_executable_path；expand_config 展开各条目 from/to
- If-Modified-Since/304：download_core 新增 if_modified_since 参数（目标已存在且无 sha 时发送，强制单线程，服务器回 304 删 tmp 保留原目标，对应 WinSW v2.7+）；single_download 返回 SingleOutcome（Downloaded/NotModified）；host 层 http_date_from_mtime（RFC 1123 GMT）
- unsecureAuth：新增 download_unsecure_auth（配置级）+ 条目级 unsecure_auth；basic 认证 + http:// 默认拒绝（凭据明文），显式放行才允许（对应 WinSW unsecureAuth）；P1-4（http+无 sha 拒绝）保持不变；warn_if_insecure_download 改为遍历全部条目，与 download_auth_from_config 标记 #[cfg(test)]
- 日志模式：新增 log_mode（append/reset/none/roll/roll-by-size/roll-by-time/roll-by-size-time，apply_log_mode 映射，size 缺省 10MB、period 缺省 1 天）、log_roll_period_days（roll_by_time_if_due 按 mtime 距今天数滚动）、log_zip_date_format（zip_backup_file 归档名日期格式，安全字符校验，空保持 {file}.zip）；roll_logs_to_old（mode=roll 启动改名 .old 覆盖）；LogOptions 新增 roll_at_start/roll_period_days/zip_date_format
- SCM 配置化：新增 scm_wait_hint_ms（默认 3600000，PENDING dwWaitHint 统一读取）与 scm_sleep_time_ms（默认 500，主循环轮询间隔，对应 WinSW waitHint/sleepTime）；service_core 全局原子 + setter（下限钳制），host on_start 写入
- 测试：新增 10 个（download_entries 归一化、downloads 数组+log/scm 字段 TOML 解析、304 删 tmp、If-Modified-Since 头发送、unsecure_auth 拒绝/放行/sspi 豁免、log_mode 映射、roll 到 .old 覆盖、按天滚动到期、zip 日期格式、SCM 参数存储与钳制），共 113 个 + 1 ignored；全量 clippy 无新增告警、release 构建通过
- 文档：4 个 README 同步（下载表加 downloads/download_unsecure_auth 与 304 说明，日志表加 log_mode/log_roll_period_days/log_zip_date_format，新增 SCM 上报表）；CLAUDE.md 记录

## v26.5.1（2026-08-11）· WinSW #217（PID 注入停止命令）
- WinSW #217（pass PID to stopExecutable，官方未实现，社区用 %PID% 占位符）：stop_arguments/stop_executable 支持 %PID% 占位符，运行停止命令时替换为目标子进程 PID（新增 pub(crate) expand_stop_pid，按字符迭代大小写不敏感兼容中文）；同时向停止命令注入 WINSGF_CHILD_PID 环境变量（与 poststop 钩子一致）
- 配置全局展开（expand_env_value）把 %PID% 列为保留占位符原样保留，仅停止命令执行时替换，避免被 env 查找吞掉
- run_stop_command 新增 pid: u32 参数，stop_child_process 调用点传入 child.id()
- 测试：新增 run_stop_command_injects_child_pid（echo 断言 %PID% 与 WINSGF_CHILD_PID 同时注入）、expand_stop_pid_placeholder_cases（大小写/未闭合/中文/与 expand_env_value 串行）；expand_env_value_edge_cases 补 %PID% 保留断言；共 103 个 + 1 ignored
- 文档：4 个 README 同步（stop_arguments 说明 + 配置全局展开段补充 %PID% 保留语义）；CLAUDE.md 记录

## v26.5.1（2026-08-11）· WinSW 对齐第四轮（配置全局展开 + test 模式）
- 配置全局 %VAR%/%BASE% 展开（对应 WinSW 配置内展开）：新增 ServiceHost::expand_config，应用于可执行路径/参数/工作目录/下载/停止命令/日志目录/pid 文件/共享映射路径；钩子命令（shell 语义）不展开；on_start_from（SCM）与 try_restart_child / current_config（异常重启/停止阶段）统一走展开
- on_start 重构：拆出 pub(crate) on_start_from(config_path)，部署目录改为"配置所在目录"（平台 .silml 与 exe 同目录，inplace/test 同样成立），SCM 入口 on_start 委托之
- test 模式：-m --test <配置> 前台控制台运行目标进程（不安装服务，对应 WinSW test）；SetConsoleCtrlHandler 拦截 Ctrl+C 触发优雅停止；部署目录=配置目录
- 测试：新增 expand_config 展开断言（%BASE%/%VAR% 全覆盖路径字段），共 101 个 + 1 ignored；实测 test 模式完整生命周期（ping 子进程启动→退出→优雅停止，exit=0）
- 文档：4 个 README 同步（CLI 表加 --test，配置说明加"配置全局展开"段）；CLAUDE.md 记录

## v26.5.1（2026-08-11）· WinSW 对齐第三轮（补缺功能 + 补测 + issue 整改）
- 新功能：start_arguments（启动专用参数覆盖 args）；security_descriptor（SDDL 服务 DACL，ConvertStringSecurityDescriptorToSecurityDescriptorW + SetServiceObjectSecurity）；preshutdown（SERVICE_ACCEPT_PRESHUTDOWN 上报 + 处理，host 经 set_preshutdown_enabled 开关）；runaway_pid_file 启动清理（残留进程树终止/回写 PID/停止删除，runaway_stop_timeout_ms/runaway_stop_parent_first）；log_out_filename/log_err_filename（自定义日志文件名，safe_log_name 校验）；未做 beep 与 GitHubRelease（用户要求排除）
- 可测性重构：build_child_command（env/参数/工作目录构造）、download_auth_from_config（download_auth→DownloadAuth 映射）、runaway_exceeded（超限判定纯函数）、runaway_cleanup_pid_file（pid 清理）、process_alive、security_descriptor_from_sddl 均提取为 pub(crate)；run_stop_command 转 pub(crate)
- 日志滚动竞态修复：auto_roll_logs 移入 LOG_WRITE_LOCK 内串行化（对应 WinSW #894/#1016/#1088 滚动崩溃/静默失败类）；项目逐行 open/append/close 设计本就免疫文件锁问题
- WinSW issue 审计：#894/#1016/#1088 已修；#1136/#872（受限账户停止访问 SCM）宿主用 status handle 天然免疫；#855（workingdirectory ".."）CreateProcess 解析免疫；#482 删除失败容忍
- 测试：新增 10 个（env 注入+参数、download_auth 映射、run_stop_command 完成+超时强杀、runaway_exceeded、runaway pid 清理、process_alive、自定义日志文件名、preshutdown 标志、SDDL 解析、新字段 roundtrip），共 100 个 + 1 ignored；修复测试空参数 cmd.exe 挂起（stdin 置 null）
- 文档：4 个 README 同步新增 8 个字段（start_arguments/security_descriptor/preshutdown/log_out_filename/log_err_filename/runaway_pid_file/runaway_stop_timeout_ms/runaway_stop_parent_first）；修正 download_auth 过时注释；windows features 新增 Win32_Security_Authorization / Win32_System_Memory

## v26.5.1（2026-08-11）· 审查修复（下载线程默认 / SSPI 句柄与 SPN / 真机验证）
- 修复 download_threads 缺省默认：serde 改 `default = "default_sixteen"`（缺失补 16，显式 0/1 仍禁用多线程），DEFAULT_DOWNLOAD_THREADS 常量收拢到 service_config 单一来源，write_quick_config 显式写 16；新增回归测试（缺失→16、显式 0→0）
- SSPI 句柄泄漏修复：新增 SspiGuard（RAII Drop 统一 FreeCredentialsHandle + DeleteSecurityContext，覆盖成功/报错/`?` 提前返回全部退出路径）；循环内 `guard.ctx.replace` 轮换并立即删旧句柄，new_ctx 无论成败交守卫释放（原实现所有退出路径均未删最终 ctx，网络/IO 错误路径还泄漏 cred）
- SSPI SPN 端口：抽出 sspi_spn（默认端口省略 :port、非默认拼入），修复 Kerberos 非默认端口 SPN 不匹配；新增测试
- 真机验证（本机）：IIS 站点（Windows 身份验证、匿名关、8080）跑通 Negotiate/NTLM → 200 下载，保留为 #[ignore] 回归测试；express-ntlm（thunderclient/ntlm-server）Type2 不合 Windows NTLM 校验（.NET 同拒，纯 Python 宽松才过），客户端无问题
- 全量测试 90 通过 + 1 ignored

## v26.5.1（2026-08-11）· 第二轮 WinSW 对齐
- 故障恢复：新增 failure_actions 动作序列（宿主级逐次取动作、超出重复最后一个；restart→reboot→none 过滤非法项）；未配置时用 failure_action + restart_delay_ms 构造（重启 3 次后停止，保持旧行为）；reboot 动作调 InitiateSystemShutdownExW 重启系统（Win32_System_Shutdown）
- 停止增强：stop_timeout_secs 可配置（默认 10，替换固定 GRACEFUL_TIMEOUT_SECS，贯穿 stop 命令/超时等待）；hide_window（默认 true，false 时不加 CreateNoWindow）；stop_parent_process_first（强杀先父后子）
- 日志增强：log_reset（启动清空当日日志）；log_auto_roll_at（每天定点滚动，改名 {pattern}.{HHmmss}.log，LAST_AUTO_ROLL 防同日重复）；log_out_enabled / log_err_enabled（禁用则 null 丢弃不建管道）；log_pattern（chrono 格式文件名，log_pattern_safe 仅允许 % 与字母数字 -_. 防路径穿越，非法回退默认）
- 下载增强：download_stage 三阶段（before_start 参与启动可执行性检查 / after_start 启动后 / after_stop 停止后，run_aux_download 失败仅告警）；download_threads 线程数可配（默认 16，0/1 禁用多线程，download_core 新增参数，删除 MAX_CHUNK_WORKERS 常量）
- SSPI 下载认证：download_auth=sspi 走 401 挑战-响应循环（AcquireCredentialsHandleW Negotiate + InitializeSecurityContextW + FreeContextBuffer/FreeCredentialsHandle，SPN=HTTP/<host>，最多 3 轮，SEC_E_OK/SEC_I_CONTINUE_NEEDED 按 HRESULT 低 32 位 0/0x90312 判定）；凭据缺省用当前进程身份，提供时构造 SEC_WINNT_AUTH_IDENTITY_EXW（Version=0x200、Flags=0x2 Unicode，DOMAIN\User 由 split_credential 拆分，身份缓冲闭包内构造保活）；认证模型重构为 DownloadAuth 枚举（None/Basic/Sspi），SSPI 路径禁用分块；Win32_Security_Credentials feature（SecHandle 位置）
- 扩展框架：RunawayProcessKiller（runaway_cpu_limit 内核+用户时间差/墙钟差百分比 + runaway_memory_limit_mb 工作集 MB，GetProcessTimes/GetProcessMemoryInfo 采样，超限 force_kill 触发 onfailure 流程）；SharedDirectoryMapper（shared_directory_mappers 数组，WNetAddConnection2W 启动映射 / WNetCancelConnection2W 停止断开，Win32_NetworkManagement_WNet 模块，NET_CONNECT_FLAGS/NET_RESOURCE_SCOPE newtype）
- 配置加密：敏感字段（service_password / download_password / 共享映射 password）部署时 DPAPI 加密（CryptProtectData CRYPTPROTECT_LOCAL_MACHINE，Win32_Security_Cryptography 模块 CRYPT_INTEGER_BLOB），值前缀 enc:OSMIUM1: 版本化；load_config 经 decrypt_sensitive 自动解密，明文旧配置原样兼容；write_deployed_config 解析失败退回按行剥离旧逻辑
- 生命周期扩展：extensions phase 扩展为 start/start_after/stop_before/stop（ext_phase_matches 兼容旧 start/stop）；钩子独立 stdout_path/stderr_path 重定向（spawn_raw_reader 原样追加写文件，run_hook 新增两参数）
- windows features：新增 Win32_Security_Cryptography / Win32_Security_Credentials / Win32_System_ProcessStatus / Win32_System_Shutdown / Win32_NetworkManagement_WNet
- 测试：新增 11 个（DPAPI 往返与明文透传、部署加密还原、动作序列默认与过滤、下载阶段默认、pattern 安全与自定义文件名、自定义 pattern 写入与 reset、phase 兼容匹配、进程采样自身/不存在进程、映射空输入、凭据拆域、SSPI 401 无挑战快速报错），共 81 个
- 审查整理（第二轮后置清理）：core 下载&文件校验函数群加独立段落标题（原混在"服务更新程序—升级&清理"区）；host 尾部区标题更新涵盖网络映射/进程采样/关机重启；DownloadAuth 加 Clone/Copy（消除两次模式匹配的移动歧义）；prepare_download/run_aux_download/fail_on_error 回退的 sha 校验三处合并为 download_sha_ok 辅助；reset_auto_roll_state 标记 #[cfg(test)] 不进生产二进制；clippy 修复 then_some / if 折叠 / Some+ok()? 冗余；测试段落标题对齐"第二轮 WinSW 对齐"；新增 7 个测试（定点滚动可控时间+防重复、钩子 stdout 重定向独立文件、threads=1 禁分块仅 2 请求、CPU 采样单调不减、download_stage 大小写、全非法 failure_actions 过滤为空、split_credential 暴力输入），共 88 个
- 文档：4 个 README 同步新增字段与示例（Lifecycle 表扩 failure_actions/stop_timeout_secs/hide_window/stop_parent_process_first，新增"资源监控与网络映射"表，下载/日志表扩字段，密钥保护安全提示）

## v26.5.1（2026-08-11）
- 版本升至 26.5.1；对齐 WinSW 补齐进程与注册能力（配置新增字段全部实现）
- 配置新增：working_directory / process_priority / stop_executable / stop_arguments / interactive / failure_action / allow_service_logon / event_log / log_zip / download_auth / download_username / download_password / download_proxy / download_unzip / extensions（phase=start/stop）
- 注册增强：interactive 附加 SERVICE_INTERACTIVE_PROCESS（SystemServices 模块 u32 常量）；failure_action 映射 SC_ACTION_RESTART/REBOOT/NONE；allow_service_logon 用 LsaOpenPolicy + LsaAddAccountRights 授予 SeServiceLogonRight（POLICY_ALL_ACCESS 无别名，按标志位拼合）
- 宿主增强：working_directory 解析（相对基于部署目录）；SetPriorityClass 设优先级；stop_executable 先于优雅停止运行；生命周期扩展 run_extensions；%VAR%/%BASE% 环境展开（expand_env_value）；event_log 写 Windows 事件日志（Win32_System_EventLog 模块，ReportEventW 8 参）
- 下载增强：download_core 新增 auth/proxy 参数；basic 认证手动拼 Authorization 头（reqwest 的 ClientBuilder 无 basic_auth 方法，引入 base64）；download_unzip 解压（unzip_to_dir 防 zip-slip，词法规范化 . / .. 组件）
- 日志增强：roll_if_needed 新增 zip_backup 参数，最旧备份压缩为 .zip 归档；开机清理按"先归档再删除"处理（delete_old_logs 加 zip_archives 参数，按服务 log_zip 配置决定），归档失败保留原文件待下次再试；zip 独立保留期 180 天（LOG_ZIP_RETENTION_DAYS），普通日志仍 30 天
- core 代码块按表面→底层排序：入口/CLI → 命令 → 辅助 → SCM API（install 底层辅助簇归位）→ SCM 宿主入口 → Win32 底层工具
- CLI 错误输出 ANSI 红色：enable_stderr_vt 对 STD_ERROR_HANDLE 启用 ENABLE_VIRTUAL_TERMINAL_PROCESSING，red() 包装 \x1b[31m（重定向/无控制台自动退化为无色），所有 eprintln 错误消息统一走 red
- windows features 调整：新增 Win32_System_EventLog / Win32_Security_Authentication_Identity / Win32_System_SystemServices，移除 Win32_Security_Authorization
- 新增测试：expand_env_value / 日志 zip 归档 / unzip zip-slip / 清理先归档再删除 / 下载增强（单线程回退、Basic 认证、404、超时、分块回退）/ 日志底层分流与转义 / 钩子超时强杀 / 进程优先级 / 边缘暴力（版本、env、转义、下载目标、URL 去敏、SDDL 畸形、同源大小写、全字段配置、滚动阈值、缺失文件），共 70 个
- 代码清理：服务名校验错误消息提取为 INVALID_NAME_MSG 常量（8 处共用）；消除 let mut 冗余重绑定；YAML 过时注释改为 TOML（service_config.rs / service_tests.rs）；4 处超行注释压缩为两行（secure_directory / delete_old_logs / download_core / roll_if_needed）；移除未使用的 Win32_Globalization feature；3 处"Failed to create service"错误文案修正为对应操作（设置描述/故障恢复/延迟启动）；warn_if_insecure_download 重复的 URL 去敏逻辑复用 redact_url；run_internal 错误文案 -m 修正为 -internal；6 个服务操作命令的重复校验（服务名+已注册）提取为 require_registered 辅助函数

## v26.5.0（2026-08-10）
- 配置从 YAML 迁移为 TOML（toml = "0.9"）；Windows 路径用单引号字面字符串
- 平台部署配置扩展名改为 .silml（svcs 目录）；inplace 模式仍用与 exe 同名的 .toml
- 安装包输出 setup.ico 并注册 .silml 文件关联（图标 silml.ico、类型描述仅英文）
- CLI 新增简化别名：--ins/--uin/--str/--stp/--rst/--sts/--del/--lst
- CLI 新增快速安装：--install <名称> --pth <exe路径>（校验服务名/保留名/绝对路径后自动生成配置并部署）
- 冲突检测读取的部署配置路径由 .toml 修正为 .silml
- CLI 帮助文本改为 TOML 格式
- 安装包图标输出改名 app.ico / silml.ico；Setup.ico 重建为 16/32/48/256 多尺寸并保持 80:88 比例
- README 拆分为 README.md（英文）/ README_CN.md（中文），HTML 文档同步
- 新增测试 quick_config_serializes_sane_defaults，共 39 个
