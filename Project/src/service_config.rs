use serde::{Deserialize, Serialize};

fn default_true() -> bool {
    true
}
fn default_five() -> i32 {
    5
}
/// download_threads 缺省值（分块下载线程数上限，默认 16）
pub(crate) const DEFAULT_DOWNLOAD_THREADS: i32 = 16;
/// serde 缺省: 配置未写 download_threads 时按 16 处理（显式 0/1 仍为禁用多线程）
fn default_sixteen() -> i32 {
    DEFAULT_DOWNLOAD_THREADS
}

/// TOML 服务配置模型 — 定义将任意可执行程序注册为 Windows 服务的所有参数；
/// serde default 仅字段缺失时生效（区分"缺失"与"显式默认值"）
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ServiceConfig {
    // ==================== 必填字段 ====================

    /// 服务名称 — SCM 内部标识符，不可重复
    #[serde(rename = "service_name")]
    pub service_name: String,

    /// 服务显示名称 — 在 services.msc 中显示的人类可读名称
    #[serde(rename = "service_display_name")]
    pub service_display_name: String,

    /// 服务描述 — 在服务属性对话框中显示
    #[serde(rename = "service_description")]
    pub service_description: String,

    /// 目标可执行程序的完整路径
    #[serde(rename = "service_executable_path")]
    pub service_executable_path: String,

    // ==================== 可选字段 ====================

    /// 目标程序的命令行参数
    #[serde(rename = "service_executable_args")]
    pub service_executable_args: Option<String>,

    /// 启动专用参数 — 配置后覆盖 service_executable_args（对应 WinSW startarguments）
    #[serde(rename = "start_arguments", default)]
    pub start_arguments: Option<String>,

    /// 启动类型: automatic | delayed_auto | manual | disabled
    #[serde(rename = "service_start_mode")]
    pub service_start_mode: Option<String>,

    /// 依赖的服务名列表，分号分隔（如 "EventLog;WinRM"）
    #[serde(rename = "service_dependencies")]
    pub service_dependencies: Option<String>,

    /// 运行服务的 Windows 账户（如 "NT AUTHORITY\NetworkService"）
    #[serde(rename = "service_account")]
    pub service_account: Option<String>,

    /// 服务账户密码（仅自定义账户需要）
    #[serde(rename = "service_password")]
    pub service_password: Option<String>,

    /// 注入目标进程的环境变量
    #[serde(default)]
    pub env: Option<std::collections::HashMap<String, String>>,

    /// 失败计数重置周期（秒），默认 86400（24 小时）
    #[serde(rename = "failure_reset_sec", default)]
    pub failure_reset_sec: i32,

    /// 崩溃后自动重启延迟（毫秒），默认 60000（60 秒）
    #[serde(rename = "restart_delay_ms", default)]
    pub restart_delay_ms: i32,

    /// 停止时是否强制终止整棵进程树，默认 true；设为 false 时仅终止主进程（对应 WinSW #990）
    #[serde(rename = "kill_process_tree", default = "default_true")]
    pub kill_process_tree: bool,

    /// 原地注册模式（默认 false）: 不复制宿主到 ProgramData，直接用当前 os.exe 注册
    /// （toml 须与 exe 同名同目录），此类服务不纳入平台开机更新/清理
    #[serde(rename = "deploy_inplace", default)]
    pub deploy_inplace: bool,

    // ==================== 生命周期钩子（可选） ====================

    /// 启动前钩子命令 — 在拉起目标进程前执行（cmd.exe 语义，失败不阻断）
    #[serde(rename = "prestart_command")]
    pub prestart_command: Option<String>,

    /// 停止后钩子命令 — 在目标进程停止后执行（cmd.exe 语义，失败不阻断）
    #[serde(rename = "poststop_command")]
    pub poststop_command: Option<String>,

    /// 配置热刷新（对应 WinSW autoRefresh）— 宿主运行中检测配置文件变化，
    /// 变化时重新加载配置并优雅重启目标子进程，默认 false
    #[serde(rename = "auto_refresh", default)]
    pub auto_refresh: bool,

    // ==================== 启动前下载（可选） ====================

    /// 启动前下载 URL — 配置后宿主在启动前确保该可执行文件已就位
    #[serde(rename = "download_url")]
    pub download_url: Option<String>,

    /// 下载目标路径 — 相对路径基于服务部署目录；省略时取 service_executable_path 的文件名
    #[serde(rename = "download_to")]
    pub download_to: Option<String>,

    /// 下载文件 SHA-256 校验值（小写十六进制）— 缺失或匹配失败时重新下载
    #[serde(rename = "download_sha256")]
    pub download_sha256: Option<String>,

    /// 下载失败是否导致服务启动失败，默认 true
    #[serde(rename = "download_fail_on_error", default = "default_true")]
    pub download_fail_on_error: bool,

    // ==================== 日志（可选） ====================

    /// 是否写入服务日志，默认 true；设为 false 可彻底关闭宿主日志（含钩子/下载输出）
    #[serde(rename = "log_enabled", default = "default_true")]
    pub log_enabled: bool,

    /// 日志目录 — 相对路径基于服务部署目录；省略时默认 logs 子目录
    #[serde(rename = "log_dir")]
    pub log_dir: Option<String>,

    /// 单日日志大小上限（MB），超过后滚动备份；0 表示不限（默认）
    #[serde(rename = "log_max_size_mb", default)]
    pub log_max_size_mb: i64,

    /// 大小滚动保留的备份份数，默认 5
    #[serde(rename = "log_max_backup_count", default = "default_five")]
    pub log_max_backup_count: i32,

    /// 是否把子进程 stderr 单独写入 yyyy-MM-dd.err.log，默认 false（合并写入主日志）
    #[serde(rename = "log_split_out_err", default)]
    pub log_split_out_err: bool,

    /// 大小滚动出的旧备份是否 zip 压缩归档（默认 false）
    #[serde(rename = "log_zip", default)]
    pub log_zip: bool,

    /// 服务每次启动时清空当前日志文件（对应 WinSW log mode=reset），默认 false
    #[serde(rename = "log_reset", default)]
    pub log_reset: bool,

    /// 每天定点滚动时刻（"HH:mm:ss"），到达后把当日日志改名归档并重开新文件
    #[serde(rename = "log_auto_roll_at")]
    pub log_auto_roll_at: Option<String>,

    /// 是否记录子进程 stdout，默认 true；false 时不写日志也不消费（直接丢弃）
    #[serde(rename = "log_out_enabled", default = "default_true")]
    pub log_out_enabled: bool,

    /// 是否记录子进程 stderr，默认 true；false 时丢弃
    #[serde(rename = "log_err_enabled", default = "default_true")]
    pub log_err_enabled: bool,

    /// 日志文件名日期模式（chrono 格式），默认 "yyyy-MM-dd"
    #[serde(rename = "log_pattern")]
    pub log_pattern: Option<String>,

    /// 自定义主日志文件名（覆盖默认 {pattern}.log；不含日期滚动，文件名须安全）
    #[serde(rename = "log_out_filename")]
    pub log_out_filename: Option<String>,

    /// 自定义 stderr 分离日志文件名（覆盖默认 {pattern}.err.log；须 log_split_out_err=true）
    #[serde(rename = "log_err_filename")]
    pub log_err_filename: Option<String>,

    /// 日志模式（对应 WinSW log mode）: append（默认）| reset | none | roll |
    /// roll-by-size | roll-by-time | roll-by-size-time；指定后覆盖等价配置项
    #[serde(rename = "log_mode")]
    pub log_mode: Option<String>,

    /// roll-by-time 周期（天），默认 1；日志文件按日期周期滚动
    #[serde(rename = "log_roll_period_days", default)]
    pub log_roll_period_days: i64,

    /// zip 归档文件名日期格式（chrono），空 = 保持 {file}.zip（默认）；
    /// 配置后归档生成 {file}.{格式日期}.zip
    #[serde(rename = "log_zip_date_format")]
    pub log_zip_date_format: Option<String>,

    /// 日志脱敏正则列表（每条匹配的文本替换为 ***，应用于宿主/钩子/子进程日志写入前）
    #[serde(rename = "log_redact", default)]
    pub log_redact: Option<Vec<String>>,

    /// 指标导出文件路径（相对部署目录）: 周期性追加 JSON 行（时间/子进程 CPU%/内存/重启次数/运行时长）
    #[serde(rename = "metrics_file")]
    pub metrics_file: Option<String>,

    // ==================== 进程环境（可选） ====================

    /// 目标进程工作目录 — 省略时取目标 exe 所在目录；相对路径基于服务部署目录
    #[serde(rename = "working_directory")]
    pub working_directory: Option<String>,

    /// 目标进程优先级: idle | belownormal | normal | abovenormal | high | realtime（默认 normal）
    #[serde(rename = "process_priority")]
    pub process_priority: Option<String>,

    /// 目标进程 CPU 亲和性（默认全部核心）: 核心编号列表 "0,1,2"（按系统核心数钳制）
    #[serde(rename = "process_affinity")]
    pub process_affinity: Option<String>,

    /// 目标进程 IO 优先级: idle | low | normal | high（默认 normal，对应 ThreadIoPriority）
    #[serde(rename = "io_priority")]
    pub io_priority: Option<String>,

    /// 将子进程放入 Job Object（KILL_ON_JOB_CLOSE）: 宿主异常退出时系统级保证整棵进程树被终止（防孤儿），默认 true
    #[serde(rename = "job_object", default = "default_true")]
    pub job_object: bool,

    // ==================== 自定义停止（可选） ====================

    /// 停止服务时先运行的程序（用于优雅排空等）；运行后等待子进程退出
    #[serde(rename = "stop_executable")]
    pub stop_executable: Option<String>,

    /// stop_executable 的命令行参数（原样拼接，保留引号语义）
    #[serde(rename = "stop_arguments")]
    pub stop_arguments: Option<String>,

    // ==================== SCM 服务标志（可选） ====================

    /// 注册为可交互桌面的服务（SERVICE_INTERACTIVE_PROCESS），默认 false
    #[serde(rename = "interactive", default)]
    pub interactive: bool,

    /// 崩溃恢复动作: restart（默认）| reboot | none
    #[serde(rename = "failure_action")]
    pub failure_action: Option<String>,

    /// 故障恢复动作序列（每项 action+delay_secs）; 失败次数按序取动作，超出后重复最后一个。
    /// 未配置时用 failure_action + restart_delay_ms 构造单动作（兼容旧配置）
    #[serde(rename = "failure_actions", default)]
    pub failure_actions: Option<Vec<FailureActionConfig>>,

    /// 注册自定义服务账户时自动授予其"作为服务登录"权限（默认 false）
    #[serde(rename = "allow_service_logon", default)]
    pub allow_service_logon: bool,

    /// 是否同时写入 Windows 事件日志（默认 false，仅写文件日志）
    #[serde(rename = "event_log", default)]
    pub event_log: bool,

    /// 服务安全描述符（SDDL）— 安装时应用到服务 DACL，控制谁能管理该服务（对应 WinSW securityDescriptor）
    #[serde(rename = "security_descriptor")]
    pub security_descriptor: Option<String>,

    /// 支持 SCM preshutdown 通知（SERVICE_ACCEPT_PRESHUTDOWN，系统关停时获得更长的优雅时间）
    #[serde(rename = "preshutdown", default)]
    pub preshutdown: bool,

    // ==================== 下载增强（可选） ====================

    /// 下载认证方式: basic（用户名/密码）| sspi（Windows 集成认证，经官方 osmium-kit-sspi 插件完成）
    #[serde(rename = "download_auth", default)]
    pub download_auth: Option<String>,

    /// basic 认证用户名
    #[serde(rename = "download_username")]
    pub download_username: Option<String>,

    /// basic 认证密码
    #[serde(rename = "download_password")]
    pub download_password: Option<String>,

    /// 下载使用的代理（http/https 均可）
    #[serde(rename = "download_proxy")]
    pub download_proxy: Option<String>,

    /// 下载文件为 zip 时自动解压到目标目录（默认 false）
    #[serde(rename = "download_unzip", default)]
    pub download_unzip: bool,

    /// 下载执行阶段: before_start（默认，启动前确保目标可执行文件就绪）| after_start | after_stop
    #[serde(rename = "download_stage")]
    pub download_stage: Option<String>,

    /// 多下载条目数组（对应 WinSW download 列表）；配置后优先使用数组，
    /// 未配置数组时仍用旧单条 download_* 字段（向后兼容）
    #[serde(rename = "downloads", default)]
    pub downloads: Option<Vec<DownloadConfig>>,

    /// basic 认证走明文 HTTP 时是否显式放行（对应 WinSW unsecureAuth），默认 false（拒绝）
    #[serde(rename = "download_unsecure_auth", default)]
    pub download_unsecure_auth: bool,

    /// 分块下载线程数上限，默认 16；0/1 禁用多线程改单线程
    #[serde(rename = "download_threads", default = "default_sixteen")]
    pub download_threads: i32,

    /// 下载失败重试次数（默认 2，指数退避后仍失败才报错）；0 不重试
    #[serde(rename = "download_retries", default)]
    pub download_retries: i64,

    /// 下载重试指数退避基数（毫秒，默认 2000: 2s/4s/8s...），仅 download_retries > 0 时生效
    #[serde(rename = "download_retry_backoff_ms", default)]
    pub download_retry_backoff_ms: i64,

    // ==================== 进程与停止（可选） ====================

    /// 隐藏目标进程窗口（CreateNoWindow），默认 true；false 时子进程可创建控制台窗口
    #[serde(rename = "hide_window", default = "default_true")]
    pub hide_window: bool,

    /// 强杀时先终止父进程再杀子树（对应 WinSW stopparentprocessfirst），默认 false
    #[serde(rename = "stop_parent_process_first", default)]
    pub stop_parent_process_first: bool,

    /// 优雅停止超时（秒），默认 10（对应 WinSW stoptimeout）
    #[serde(rename = "stop_timeout_secs", default)]
    pub stop_timeout_secs: i64,

    // ==================== 生命周期扩展（可选） ====================

    /// 额外生命周期扩展命令（多条），phase: start（启动前，默认）| start_after | stop_before | stop，
    /// 与 prestart/poststop 钩子互补；支持 stdout_path/stderr_path 独立重定向
    #[serde(default)]
    pub extensions: Option<Vec<ExtensionConfig>>,

    // ==================== 生命周期插件调用（可选） ====================

    /// 生命周期插件调用（多条），phase 与 extensions 相同四阶段 + crash（崩溃恢复前）；
    /// 按 kit 分发到 exe 同级 .osx 插件（stdin/stdout JSON 协议），第三方插件无需改宿主代码
    #[serde(rename = "plugins", default)]
    pub plugins: Option<Vec<PluginCallConfig>>,

    /// 仅执行带有效 Authenticode 签名的插件（默认 false 仅校验 ACL 信任）；
    /// true 时未签名/签名无效的 .osx 拒绝执行（WinVerifyTrust 校验）
    #[serde(rename = "require_signed_plugins", default)]
    pub require_signed_plugins: bool,

    // ==================== 资源监控 / 健康检查（可选） ====================

    /// HTTP 健康检查: 子进程运行期间轮询该 URL，连续 health_check_failures 次非 200 视为崩溃重启
    #[serde(rename = "health_check_url")]
    pub health_check_url: Option<String>,

    /// 健康检查轮询间隔（秒），默认 30
    #[serde(rename = "health_check_interval_secs", default)]
    pub health_check_interval_secs: i64,

    /// 健康检查请求超时（秒），默认 5
    #[serde(rename = "health_check_timeout_secs", default)]
    pub health_check_timeout_secs: i64,

    /// 连续失败多少次视为崩溃（默认 3）
    #[serde(rename = "health_check_failures", default)]
    pub health_check_failures: i64,

    /// 期望的 HTTP 状态码（默认 200）
    #[serde(rename = "health_check_expected_status", default)]
    pub health_check_expected_status: i64,

    // ==================== 定时调度（可选） ====================

    /// 定时调度: 固定间隔或每日定点触发动作（restart 重启子进程 / reload 热刷新 / hook 执行命令）
    #[serde(rename = "schedules", default)]
    pub schedules: Option<Vec<ScheduleConfig>>,

    // ==================== 资源监控 / 网络映射（可选） ====================

    /// RunawayProcessKiller: 子进程 CPU 占用上限（百分比，全核累计），超限自动终止并触发重启逻辑
    #[serde(rename = "runaway_cpu_limit")]
    pub runaway_cpu_limit: Option<f64>,

    /// RunawayProcessKiller: 子进程工作集内存上限（MB），超限自动终止
    #[serde(rename = "runaway_memory_limit_mb")]
    pub runaway_memory_limit_mb: Option<u64>,

    /// RunawayProcessKiller 检查间隔（秒），默认 30
    #[serde(rename = "runaway_check_interval_secs", default)]
    pub runaway_check_interval_secs: i64,

    /// RunawayProcessKiller 启动清理: pid 文件路径（相对基于部署目录），宿主启动时按该 PID
    /// 终止上次宿主残留的进程树，启动子进程后回写 PID、停止后删除（对应 WinSW RunawayProcessKiller）
    #[serde(rename = "runaway_pid_file")]
    pub runaway_pid_file: Option<String>,

    /// 启动清理时残留进程的优雅停止超时（毫秒），默认 5000；超时后强制终止
    #[serde(rename = "runaway_stop_timeout_ms", default)]
    pub runaway_stop_timeout_ms: i64,

    /// 启动清理时先终止父进程再杀子树（默认 false，对应 WinSW stopParentFirst）
    #[serde(rename = "runaway_stop_parent_first", default)]
    pub runaway_stop_parent_first: bool,

    /// SharedDirectoryMapper: 服务启动时映射的网络共享目录列表，服务停止时自动断开
    #[serde(rename = "shared_directory_mappers", default)]
    pub shared_directory_mappers: Option<Vec<SharedMapperConfig>>,

    // ==================== 效率模式（EcoQoS，可选） ====================

    /// 子进程效率模式: none（默认）| always（常开）| auto（空闲进/繁忙退）
    #[serde(rename = "eco_qos", default)]
    pub eco_qos: Option<String>,

    /// auto 模式: 空闲进入阈值（CPU %），默认 10
    #[serde(rename = "eco_qos_idle_cpu_pct", default)]
    pub eco_qos_idle_cpu_pct: Option<f64>,

    /// auto 模式: 繁忙退出阈值（CPU %），默认 30
    #[serde(rename = "eco_qos_busy_cpu_pct", default)]
    pub eco_qos_busy_cpu_pct: Option<f64>,

    /// 宿主自身效率模式: none（默认）| always | auto（自身 CPU 低进/高退 + 子进程繁忙联动退出）
    #[serde(rename = "host_eco_qos", default)]
    pub host_eco_qos: Option<String>,

    /// 宿主 auto: 空闲进入阈值（CPU %），默认 5
    #[serde(rename = "host_eco_qos_idle_cpu_pct", default)]
    pub host_eco_qos_idle_cpu_pct: Option<f64>,

    /// 宿主 auto: 繁忙退出阈值（CPU %），默认 20
    #[serde(rename = "host_eco_qos_busy_cpu_pct", default)]
    pub host_eco_qos_busy_cpu_pct: Option<f64>,

    // ==================== SCM 上报（可选） ====================

    /// SCM 状态上报 dwWaitHint（毫秒），默认 3600000（1 小时）；
    /// 启动/停止 PENDING 阶段向 SCM 声明的最大等待时间（对应 WinSW waitHint）
    #[serde(rename = "scm_wait_hint_ms", default)]
    pub scm_wait_hint_ms: i64,

    /// 宿主主循环 SCM 信号轮询间隔（毫秒），默认 500（对应 WinSW sleepTime）
    #[serde(rename = "scm_sleep_time_ms", default)]
    pub scm_sleep_time_ms: i64,
}

/// 生命周期扩展配置: phase=start 在目标进程启动前执行，phase=start_after 在启动后执行，
/// phase=stop_before 在停止前执行，phase=stop 在停止后执行
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionConfig {
    /// 执行阶段: start | start_after | stop_before | stop
    pub phase: String,
    /// 扩展命令（cmd /c 语义，失败不阻断）
    pub command: String,
    /// 可选: 钩子 stdout 重定向文件（省略时写入宿主日志）
    pub stdout_path: Option<String>,
    /// 可选: 钩子 stderr 重定向文件（省略时写入宿主日志）
    pub stderr_path: Option<String>,
}

/// 生命周期插件调用配置: 按 kit 分发到 exe 同级 .osx 插件（stdin 单行 JSON，
/// 响应 stdout 单行 JSON ok:true/false）；payload 合并进请求 JSON 根对象透传
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginCallConfig {
    /// 插件标识（对应插件请求 JSON 的 kit 字段）
    pub kit: String,
    /// 执行阶段: start（启动前）| start_after | stop_before | stop（停止后）
    pub phase: String,
    /// 可选: 透传给插件的参数（JSON 对象，与 kit 字段合并）
    #[serde(default)]
    pub payload: serde_json::Value,
    /// 可选: 插件失败是否阻断流程（start 阶段阻断启动；其他阶段仅告警），默认 false
    #[serde(default)]
    pub fail_on_error: bool,
}

/// 定时调度配置: every_secs（固定间隔）与 daily_at（每日定点 "HH:mm:ss"）二选一；
/// action = restart（重启子进程）| reload（热刷新重载配置）| hook（执行 command）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduleConfig {
    /// 固定间隔触发（秒），与 daily_at 互斥
    #[serde(rename = "every_secs")]
    pub every_secs: Option<i64>,
    /// 每日定点触发时刻（"HH:mm:ss"）
    #[serde(rename = "daily_at")]
    pub daily_at: Option<String>,
    /// 触发动作: restart | reload | hook（默认 restart）
    #[serde(rename = "action", default)]
    pub action: String,
    /// action=hook 时执行的命令（cmd /c 语义）
    #[serde(rename = "command")]
    pub command: Option<String>,
}

/// 故障恢复动作配置: action = restart | reboot | none，delay_secs 为动作前等待秒数
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureActionConfig {
    /// 恢复动作: restart | reboot | none
    pub action: String,
    /// 动作前等待秒数，默认 0（立即执行）
    #[serde(default)]
    pub delay_secs: u64,
}

/// 网络共享映射配置: 本地挂载点 + 远程 UNC 路径（可选认证账户）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SharedMapperConfig {
    /// 本地挂载点，如 "Z:" 或 "C:\\share"
    pub local_path: String,
    /// 远程共享路径，如 "\\\\server\\share"
    pub remote_path: String,
    /// 可选认证账户（Domain\\User），省略用当前上下文
    pub username: Option<String>,
    /// 可选密码（部署时自动加密存储）
    pub password: Option<String>,
}

/// 单条下载配置（downloads 数组元素，对应 WinSW download 条目）；
/// from/to 必填，其余可选字段缺省时回退到配置级 download_* 值
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DownloadConfig {
    /// 下载源 URL
    #[serde(rename = "from")]
    pub from: String,
    /// 本地目标路径（相对路径基于服务部署目录）
    #[serde(rename = "to")]
    pub to: String,
    /// 可选: SHA-256 校验值（缺省回退 download_sha256）
    #[serde(rename = "sha256", default)]
    pub sha256: Option<String>,
    /// 可选: 下载失败是否导致服务启动失败（缺省回退 download_fail_on_error）
    #[serde(rename = "fail_on_error", default)]
    pub fail_on_error: Option<bool>,
    /// 可选: 认证方式 basic | sspi（缺省回退 download_auth；sspi 经插件完成）
    #[serde(rename = "auth", default)]
    pub auth: Option<String>,
    /// 可选: basic 认证用户名（缺省回退 download_username）
    #[serde(rename = "username", default)]
    pub username: Option<String>,
    /// 可选: basic 认证密码（缺省回退 download_password）
    #[serde(rename = "password", default)]
    pub password: Option<String>,
    /// 可选: basic 认证明文 HTTP 显式放行（缺省回退 download_unsecure_auth）
    #[serde(rename = "unsecure_auth", default)]
    pub unsecure_auth: Option<bool>,
    /// 可选: 下载代理（缺省回退 download_proxy）
    #[serde(rename = "proxy", default)]
    pub proxy: Option<String>,
    /// 可选: zip 下载后自动解压（缺省回退 download_unzip）
    #[serde(rename = "unzip", default)]
    pub unzip: Option<bool>,
    /// 可选: 下载阶段 before_start | after_start | after_stop（缺省回退 download_stage）
    #[serde(rename = "stage", default)]
    pub stage: Option<String>,
}
