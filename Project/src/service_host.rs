use std::io::{BufRead, BufReader, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::service_core::f;
use windows::core::{BOOL, PCWSTR};

use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, WPARAM};
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
    TH32CS_SNAPPROCESS,
};
use windows::Win32::System::EventLog::{
    DeregisterEventSource, EVENTLOG_INFORMATION_TYPE, RegisterEventSourceW, ReportEventW,
};
use windows::Win32::System::Threading::{
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, GetProcessTimes, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS,
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_SET_INFORMATION, PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS, SetPriorityClass, TerminateProcess,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowThreadProcessId, PostMessageW, WM_CLOSE,
};

// ==================== 常量 ====================

/// 优雅关闭超时（秒）
const GRACEFUL_TIMEOUT_SECS: u64 = 10;
/// prestart 钩子超时（毫秒），防止钩子卡死触发 SCM 30 秒启动超时
const HOOK_PRESTART_TIMEOUT_MS: u64 = 60_000;
/// poststop 钩子超时（毫秒）
const HOOK_POSTSTOP_TIMEOUT_MS: u64 = 30_000;
/// 下载超时（秒），覆盖整个下载过程
const DOWNLOAD_TIMEOUT_SECS: u64 = 300;

/// 效率模式（EcoQoS）档位: None 不干预 | Always 常开 | Auto 空闲进/繁忙退
#[derive(PartialEq, Clone, Copy)]
pub(crate) enum EcoQosMode {
    None,
    Always,
    Auto,
}

impl EcoQosMode {
    /// 解析配置值（大小写不敏感）; 未知/缺失 → None
    fn parse(value: Option<&str>) -> Self {
        match value.map(|s| s.to_lowercase()).as_deref() {
            Some("always") => EcoQosMode::Always,
            Some("auto") => EcoQosMode::Auto,
            _ => EcoQosMode::None,
        }
    }
}

// ==================== 宿主配置路径 ====================

/// 宿主配置路径: 平台部署为 .osiml（服务配置文件），inplace 兼容同目录 .toml
pub(crate) fn config_path_next_to(exe: &Path) -> PathBuf {
    let osiml = Path::new(exe).with_extension("osiml");
    if osiml.exists() { osiml } else { Path::new(exe).with_extension("toml") }
}

// ==================== 服务宿主结构 & 日志参数 ====================

/// 服务宿主 — 由 SCM 启动，读取 TOML 配置并启动目标进程
pub struct ServiceHost {
    pub child: Option<Child>,
    pub log_dir: String,
    /// log_enabled 开关: false 时 write_log 变 no-op
    log_enabled: bool,
    /// 日志写入参数（供日志读取线程克隆）
    log_opts: LogOptions,
    /// kill_process_tree: false 时强杀只终止主进程不杀子树（对应 WinSW #990）
    kill_process_tree: bool,
    /// 停止后钩子命令（on_start 时从配置读取）
    poststop_command: Option<String>,
    /// 自定义停止程序（exe, args），停止时先运行（对应 WinSW stopExecutable）
    stop_cmd: Option<(String, String)>,
    /// 目标进程优先级（可选）
    process_priority: Option<String>,
    /// 是否同时写 Windows 事件日志
    event_log: bool,
    /// 生命周期扩展命令列表（phase=start/start_after/stop_before/stop）
    extensions: Option<Vec<crate::service_config::ExtensionConfig>>,
    /// 隐藏目标进程窗口（false 时子进程可创建控制台窗口，对应 WinSW hidewindow）
    hide_window: bool,
    /// 强杀时先终止父进程再杀子树（对应 WinSW stopparentprocessfirst）
    stop_parent_process_first: bool,
    /// 优雅停止超时（秒），默认 10（对应 WinSW stoptimeout）
    stop_timeout_secs: u64,
    /// 分块下载线程数上限（0/1 禁用多线程；默认 16）
    download_threads: i32,
    /// onfailure 动作序列（宿主级: 按失败次数取动作，超出重复最后一个）
    failure_actions: Vec<crate::service_config::FailureActionConfig>,
    /// RunawayProcessKiller: 子进程 CPU 占用上限（百分比），None 不监控
    runaway_cpu_limit: Option<f64>,
    /// RunawayProcessKiller: 子进程工作集内存上限（MB），None 不监控
    runaway_memory_limit_mb: Option<u64>,
    /// RunawayProcessKiller 检查间隔（秒），默认 30
    runaway_check_interval_secs: i64,
    /// RunawayProcessKiller 启动清理 pid 文件路径（绝对，None 不启用）
    runaway_pid_file: Option<String>,
    /// 启动清理残留进程的优雅停止超时（毫秒）
    runaway_stop_timeout_ms: u64,
    /// 启动清理时先终止父进程再杀子树
    runaway_stop_parent_first: bool,
    /// 上次 RunawayProcessKiller 采样（PID, 内核+用户 CPU 时间, 采样时刻）
    runaway_last_sample: Option<(u32, u64, Instant)>,
    /// 上次 RunawayProcessKiller 检查时刻
    runaway_last_check: Option<Instant>,
    /// 子进程效率模式（EcoQoS）: None | Always | Auto
    eco_qos_mode: EcoQosMode,
    /// 子进程 auto: 空闲进入阈值（CPU %）与繁忙退出阈值（CPU %）
    eco_qos_idle_pct: f64,
    eco_qos_busy_pct: f64,
    /// 子进程当前是否处于效率模式 + 连续低占用采样计数
    eco_qos_active: bool,
    eco_qos_idle_streak: u32,
    /// 子进程效率模式采样（独立于 runaway，auto 模式无条件采样）
    child_eco_sample: Option<(u32, u64, Instant)>,
    /// 宿主自身效率模式（EcoQoS）: None | Always | Auto
    host_eco_qos_mode: EcoQosMode,
    /// 宿主 auto: 空闲进入阈值（CPU %）与繁忙退出阈值（CPU %）
    host_eco_qos_idle_pct: f64,
    host_eco_qos_busy_pct: f64,
    /// 宿主当前是否处于效率模式 + 连续低占用采样计数 + 上次宿主采样
    host_eco_qos_active: bool,
    host_eco_qos_idle_streak: u32,
    host_eco_qos_sample: Option<(u64, Instant)>,
    /// 子进程 CPU 采样（宿主 auto 联动判定: 子进程繁忙时宿主退出效率模式）
    host_child_sample: Option<(u32, u64, Instant)>,
    /// 最后一次子进程 PID（供 poststop 钩子注入环境变量）
    last_child_pid: u32,
    /// 最后一次子进程退出码
    last_child_exit_code: i32,
    /// 连续非零退出次数（限制异常重启）
    consecutive_failures: i32,
    /// 0=运行中, 1=停止流程中（防 Exited 重入）
    stopping: AtomicBool,
    /// 部署目录（日志/下载相对路径基准）
    pub(crate) deploy_dir: String,
    /// 配置热刷新开关（对应 WinSW autoRefresh）: 配置文件变化时自动重启子进程
    auto_refresh: bool,
    /// 当前配置文件路径（热刷新检测用）
    config_path: Option<PathBuf>,
    /// 上次加载配置时的文件 mtime（热刷新变化检测）
    config_mtime: Option<SystemTime>,
}

/// 日志写入参数（分流出/错、大小滚动、备份份数、zip 归档、reset、定点滚动、out/err 开关、文件名模式）
#[derive(Clone, Default)]
pub(crate) struct LogOptions {
    pub(crate) split_out_err: bool,
    pub(crate) max_size_mb: i64,
    pub(crate) backup_count: i32,
    pub(crate) zip_backup: bool,
    /// 文件名日期模式（chrono 格式），空串表示默认 "yyyy-MM-dd"
    pub(crate) pattern: String,
    /// 每天定点滚动时刻（"HH:mm:ss"），None 不启用
    pub(crate) auto_roll_at: Option<String>,
    /// 是否记录子进程 stdout（false 时直接丢弃不写日志）
    pub(crate) out_enabled: bool,
    /// 是否记录子进程 stderr
    pub(crate) err_enabled: bool,
    /// 服务启动时清空当前日志文件（对应 WinSW log mode=reset）
    pub(crate) reset: bool,
    /// 自定义主日志文件名（空 = 默认 {pattern}.log）
    pub(crate) out_filename: String,
    /// 自定义 stderr 分离日志文件名（空 = 默认 {pattern}.err.log）
    pub(crate) err_filename: String,
    /// 启动时把当前日志改名 .old（对应 WinSW log mode=roll）
    pub(crate) roll_at_start: bool,
    /// 按天周期滚动（天），0=不启用（对应 WinSW roll-by-time period）
    pub(crate) roll_period_days: i64,
    /// zip 归档文件名日期格式（chrono），空 = 保持 {file}.zip（对应 WinSW zipDateFormat）
    pub(crate) zip_date_format: String,
}

impl ServiceHost {
    // ==================== 构造 & 入口 ====================

    pub fn new() -> Self {
        Self {
            child: None,
            log_dir: String::new(),
            log_enabled: true,
            log_opts: LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 5, zip_backup: false, pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() },
            kill_process_tree: true,
            poststop_command: None,
            stop_cmd: None,
            process_priority: None,
            event_log: false,
            extensions: None,
            hide_window: true,
            stop_parent_process_first: false,
            stop_timeout_secs: GRACEFUL_TIMEOUT_SECS,
            download_threads: crate::service_config::DEFAULT_DOWNLOAD_THREADS,
            failure_actions: Vec::new(),
            runaway_cpu_limit: None,
            runaway_memory_limit_mb: None,
            runaway_check_interval_secs: 30,
            runaway_pid_file: None,
            runaway_stop_timeout_ms: 5000,
            runaway_stop_parent_first: false,
            runaway_last_sample: None,
            runaway_last_check: None,
            eco_qos_mode: EcoQosMode::None,
            eco_qos_idle_pct: 10.0,
            eco_qos_busy_pct: 30.0,
            eco_qos_active: false,
            eco_qos_idle_streak: 0,
            child_eco_sample: None,
            host_eco_qos_mode: EcoQosMode::None,
            host_eco_qos_idle_pct: 5.0,
            host_eco_qos_busy_pct: 20.0,
            host_eco_qos_active: false,
            host_eco_qos_idle_streak: 0,
            host_eco_qos_sample: None,
            host_child_sample: None,
            last_child_pid: 0,
            last_child_exit_code: -1,
            consecutive_failures: 0,
            stopping: AtomicBool::new(false),
            deploy_dir: String::new(),
            auto_refresh: false,
            config_path: None,
            config_mtime: None,
        }
    }

    /// 宿主 scm_svc_name 使用: 普通部署取 exe 文件名（os），共享宿主按显式名注册
    pub fn svc_name() -> String {
        let path = crate::service_core::get_own_path();
        Path::new(&path)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "os".to_string())
    }

    /// 返回 false 表示启动失败（等价于 OnStart 抛异常 → SCM 报告启动失败）
    pub fn on_start(&mut self) -> bool {
        let process_path = crate::service_core::get_own_path();
        // 与 Path.ChangeExtension 等价（对非 ASCII 路径也安全，不依赖手工切片）
        self.on_start_from(&config_path_next_to(Path::new(&process_path)))
    }

    /// 共享宿主部署入口: 按服务名加载 svcs\<name>\<name>.osiml（部署目录 = 配置所在目录）。
    /// 服务名来自 SCM ImagePath，先校验合法性防路径穿越读取 svcs 外任意 .osiml
    pub fn on_start_with_name(&mut self, name: &str) -> bool {
        if !crate::service_core::is_valid_service_name(name) {
            self.write_log("host", &f("Invalid service name from SCM: '{0}'", &[name]));
            return false;
        }
        let config_path = crate::service_core::deployed_config_path(name);
        self.on_start_from(&config_path)
    }

    /// 以显式配置路径启动宿主（SCM 模式用 exe 旁配置；-m --test 用命令行指定配置，部署目录=配置目录）
    pub(crate) fn on_start_from(&mut self, config_path: &Path) -> bool {
        if !config_path.exists() {
            self.write_log("host", &f("Service config file not found: {0}", &[&config_path.display().to_string()]));
            return false;
        }

        // 解析失败用 catch_unwind 兜底，避免 panic 穿越 extern "system" SCM 入口导致 abort
        // （与 try_restart_child / cleanup_invalid_service 一致）
        let config = match std::panic::catch_unwind(|| crate::service_core::load_config(config_path)) {
            Ok(c) => c,
            Err(p) => {
                self.write_log("host", &crate::service_core::panic_msg(&*p, "Unknown error"));
                return false;
            }
        };
        // 部署目录: 配置所在目录（平台模式 .osiml 与 exe 同目录，inplace/test 模式同样成立）
        self.deploy_dir = config_path
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        // 配置全局 %VAR%/%BASE% 展开（对应 WinSW 配置内环境变量展开），后续字段读取均用展开后值
        let config = self.expand_config(&config);
        // 记录配置路径与 mtime（供 auto_refresh 热刷新检测变化）
        self.config_path = Some(config_path.to_path_buf());
        self.config_mtime = std::fs::metadata(config_path).ok().and_then(|m| m.modified().ok());
        self.auto_refresh = config.auto_refresh;

        self.apply_log_settings(&config);
        // 启动滚动（mode=roll）: 改名当前日志为 .old（与 reset 互斥，roll 保留旧内容）
        if self.log_enabled && self.log_opts.roll_at_start {
            roll_logs_to_old(&self.log_dir, &self.log_opts);
        }
        self.apply_runtime_fields(&config);
        // RunawayProcessKiller 启动清理: 终止上次宿主残留的进程树（失败仅告警）；
        // 传入服务名作防误杀校验（只清理带 WINSGF_SERVICE_ID 的本服务残留进程）
        self.cleanup_runaway_pid(Some(&config.service_name));
        if self.log_enabled {
            let _ = std::fs::create_dir_all(&self.log_dir);
        }
        // log reset: 每次启动清空当日日志文件（含 err 分离文件），对应 WinSW log mode=reset
        if self.log_enabled && self.log_opts.reset {
            reset_current_logs(&self.log_dir, &self.log_opts);
        }
        // SharedDirectoryMapper: 服务启动时映射网络共享目录（失败仅告警，不阻断启动）
        self.netmap_via_plugin(&config, "map");
        // 宿主自身效率模式 always: 启动完成后立即进入（auto 由 tick 采样驱动）
        if self.host_eco_qos_mode == EcoQosMode::Always {
            let _ = set_eco_qos(std::process::id(), true);
        }

        self.write_log("host", &f("Service starting, config: {0}", &[&config_path.display().to_string()]));

        // 启动前钩子（可选，失败不阻断）；日志禁用时传入空目录使其静默
        let hook_log_dir = self.hook_log_dir();
        run_hook(config.prestart_command.as_deref(), "prestart", HOOK_PRESTART_TIMEOUT_MS, hook_log_dir, None, &self.log_opts, None, None);

        match self.start_child_process(&config) {
            Ok(()) => true,
            Err(e) => {
                self.write_log("host", &e);
                false
            }
        }
    }

    /// 共享目录映射: shared_directory_mappers 配置时经 osmium-kit-netmap 插件执行；
    /// action 区分 map（启动时）/ unmap（停止时），插件缺失或失败仅告警不阻断
    fn netmap_via_plugin(&self, config: &crate::service_config::ServiceConfig, action: &str) {
        let Some(mappers) = config.shared_directory_mappers.as_deref() else { return };
        if mappers.is_empty() {
            return;
        }
        match run_plugin("netmap", &serde_json::json!({ "action": action, "mappers": mappers })) {
            Ok(()) => {}
            Err(e) => self.write_log("host", &f("Shared directory {0} failed (non-fatal): {1}", &[action, &e])),
        }
    }

    /// 应用日志目录与日志参数（log_dir + LogOptions + log mode 映射；不含 roll/reset 等启动一次性动作）
    fn apply_log_settings(&mut self, config: &crate::service_config::ServiceConfig) {
        // 日志目录: 默认部署目录下 logs 子目录；可用 log_dir 覆盖（相对路径基于部署目录）
        self.log_dir = match config.log_dir.as_deref() {
            None | Some("") => format!("{}\\logs", self.deploy_dir),
            Some(dir) => resolve_deploy_path(dir, &self.deploy_dir),
        };
        self.log_enabled = config.log_enabled;
        self.log_opts = LogOptions {
            split_out_err: config.log_split_out_err,
            max_size_mb: config.log_max_size_mb,
            backup_count: config.log_max_backup_count,
            zip_backup: config.log_zip,
            pattern: config.log_pattern.as_deref()
                .map(|p| p.trim().to_string())
                .unwrap_or_default(),
            auto_roll_at: config.log_auto_roll_at.as_deref()
                .map(|t| t.trim().to_string())
                .filter(|t| !t.is_empty()),
            out_enabled: config.log_out_enabled,
            err_enabled: config.log_err_enabled,
            reset: config.log_reset,
            // 自定义文件名: 仅允许安全字符（log_pattern_safe 同款校验），非法回退默认
            out_filename: safe_log_name(config.log_out_filename.as_deref()),
            err_filename: safe_log_name(config.log_err_filename.as_deref()),
            roll_at_start: false,
            roll_period_days: config.log_roll_period_days.max(0),
            // zip 日期格式仅接受安全字符（防路径穿越），非法回退空（保持 {file}.zip）
            zip_date_format: {
                let raw = config.log_zip_date_format.as_deref().unwrap_or_default();
                if log_pattern_safe(raw) { raw.trim().to_string() } else { String::new() }
            },
        };
        // 文件名模式非法（含路径分隔符等）时回退默认日期格式，防路径穿越
        if !log_pattern_safe(&self.log_opts.pattern) {
            self.log_opts.pattern.clear();
        }
        // log mode（对应 WinSW log mode）: append|reset|none|roll|roll-by-size|roll-by-time|roll-by-size-time
        apply_log_mode(config.log_mode.as_deref(), &mut self.log_enabled, &mut self.log_opts);
    }

    /// 应用子进程运行相关宿主字段（on_start 与热刷新共用；不含启动一次性逻辑）
    fn apply_runtime_fields(&mut self, config: &crate::service_config::ServiceConfig) {
        self.kill_process_tree = config.kill_process_tree;
        self.poststop_command = config.poststop_command.clone();
        self.stop_cmd = match config.stop_executable.as_deref() {
            Some(e) if !e.trim().is_empty() => Some((e.to_string(), config.stop_arguments.clone().unwrap_or_default())),
            _ => None,
        };
        self.process_priority = config.process_priority.clone();
        self.event_log = config.event_log;
        self.extensions = config.extensions.clone();
        self.hide_window = config.hide_window;
        self.stop_parent_process_first = config.stop_parent_process_first;
        self.stop_timeout_secs = if config.stop_timeout_secs > 0 { config.stop_timeout_secs as u64 } else { GRACEFUL_TIMEOUT_SECS };
        self.download_threads = config.download_threads;
        // onfailure 动作序列: 未配置 failure_actions 时用 failure_action + restart_delay_ms 构造单动作
        self.failure_actions = failure_action_chain(config);
        self.runaway_cpu_limit = config.runaway_cpu_limit;
        self.runaway_memory_limit_mb = config.runaway_memory_limit_mb;
        self.runaway_check_interval_secs = if config.runaway_check_interval_secs > 0 { config.runaway_check_interval_secs } else { 30 };
        // 效率模式（EcoQoS）: 子进程与宿主各自独立配置
        self.eco_qos_mode = EcoQosMode::parse(config.eco_qos.as_deref());
        self.eco_qos_idle_pct = config.eco_qos_idle_cpu_pct.unwrap_or(10.0);
        self.eco_qos_busy_pct = config.eco_qos_busy_cpu_pct.unwrap_or(30.0);
        self.host_eco_qos_mode = EcoQosMode::parse(config.host_eco_qos.as_deref());
        self.host_eco_qos_idle_pct = config.host_eco_qos_idle_cpu_pct.unwrap_or(5.0);
        self.host_eco_qos_busy_pct = config.host_eco_qos_busy_cpu_pct.unwrap_or(20.0);
        self.runaway_pid_file = config.runaway_pid_file.as_deref()
            .map(|p| if p.trim().is_empty() { String::new() } else { resolve_deploy_path(p, &self.deploy_dir) })
            .filter(|p| !p.is_empty());
        self.runaway_stop_timeout_ms = if config.runaway_stop_timeout_ms > 0 { config.runaway_stop_timeout_ms as u64 } else { 5000 };
        self.runaway_stop_parent_first = config.runaway_stop_parent_first;
        // SCM preshutdown 支持开关（scm_status_params 读取该标志决定是否上报 SERVICE_ACCEPT_PRESHUTDOWN）
        crate::service_core::set_preshutdown_enabled(config.preshutdown);
        // SCM 状态上报 waitHint 与主循环轮询间隔（毫秒），可配置化（对应 WinSW waitHint/sleepTime）
        crate::service_core::set_scm_wait_hint_ms(if config.scm_wait_hint_ms > 0 { config.scm_wait_hint_ms as u32 } else { 3_600_000 });
        crate::service_core::set_scm_sleep_time_ms(if config.scm_sleep_time_ms > 0 { config.scm_sleep_time_ms as u32 } else { 500 });
    }

    /// 配置全局环境变量展开: %BASE% 指部署目录，%NAME% 取系统环境变量（对应 WinSW 配置内展开）；
    /// 应用于路径/参数/下载/停止命令等路径类字段；钩子命令是 shell 语义，不展开
    pub(crate) fn expand_config(&self, config: &crate::service_config::ServiceConfig) -> crate::service_config::ServiceConfig {
        let mut c = config.clone();
        c.service_executable_path = expand_env_value(&config.service_executable_path, &self.deploy_dir);
        c.service_executable_args = config.service_executable_args.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.start_arguments = config.start_arguments.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.working_directory = config.working_directory.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.download_url = config.download_url.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.download_to = config.download_to.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.downloads = config.downloads.as_ref().map(|list| list.iter().map(|d| {
            let mut d2 = d.clone();
            d2.from = expand_env_value(&d.from, &self.deploy_dir);
            d2.to = expand_env_value(&d.to, &self.deploy_dir);
            d2
        }).collect());
        c.stop_executable = config.stop_executable.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.stop_arguments = config.stop_arguments.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.log_dir = config.log_dir.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        c.runaway_pid_file = config.runaway_pid_file.as_deref().map(|s| expand_env_value(s, &self.deploy_dir));
        if let Some(mappers) = &mut c.shared_directory_mappers {
            for m in mappers {
                m.local_path = expand_env_value(&m.local_path, &self.deploy_dir);
                m.remote_path = expand_env_value(&m.remote_path, &self.deploy_dir);
            }
        }
        c
    }

    // ==================== 运行监控 & 停止流程 ====================

    pub fn on_stop(&mut self) {
        self.stop_host("SCM stop signal received", "Service stopping", Some("Service stopped"));
    }

    pub fn on_shutdown(&mut self) {
        self.stop_host("SCM shutdown signal received", "System shutting down", None);
    }

    /// 子进程监控（主循环每次轮询调用）: 等价 OnChildExited，
    /// 非零退出按 onfailure 动作序列处理（restart → reboot → none），否则停止宿主；返回 false 表示服务应停止
    pub fn tick(&mut self) -> bool {
        if self.stopping.load(Ordering::SeqCst) {
            return false;
        }
        let code = match self.child.as_mut() {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => status.code().unwrap_or(-1),
                Ok(None) => {
                    self.check_runaway();
                    self.check_child_eco_qos();
                    self.check_host_eco_qos();
                    // 配置热刷新（autoRefresh）: 配置文件变化时重载并重启子进程
                    if self.auto_refresh {
                        self.check_config_refresh();
                    }
                    return true; // 仍在运行
                }
                Err(_) => return false,
            },
            None => return false,
        };
        self.last_child_exit_code = code;
        self.write_log("host", &f("Child process exited with code {0}", &[&code.to_string()]));

        // 防重入: 停止流程中子进程被终止也会触发本路径（对应 Interlocked.CompareExchange）
        if self.stopping.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst).is_err() {
            self.write_log("host", "Child exit detected while already stopping — ignoring");
            return false;
        }

        if code == 0 {
            self.consecutive_failures = 0;
            // 正常退出 → 停止宿主（等价 Stop() → OnStop → StopHost）
            self.stop_host("Child exited normally, stopping service", "Service stopping", Some("Service stopped"));
            return false;
        }

        // 异常退出: 按 onfailure 动作序列取动作（失败次数超出序列长度时重复最后一个）
        let idx = (self.consecutive_failures as usize).min(self.failure_actions.len().saturating_sub(1));
        self.consecutive_failures += 1;
        let action = self.failure_actions.get(idx).cloned();
        let Some(action) = action else {
            // 动作序列为空（异常状态）→ 停止宿主
            self.stop_host("Failure policy: no recovery action", "Service stopping", Some("Service stopped"));
            return false;
        };
        self.write_log("host", &f("Child process exited abnormally (code {0}), failure #{1}, applying action: {2} (delay {3}s)",
            &[&code.to_string(), &self.consecutive_failures.to_string(), &action.action, &action.delay_secs.to_string()]));
        if action.delay_secs > 0 {
            thread::sleep(Duration::from_secs(action.delay_secs));
        }
        match action.action.to_lowercase().as_str() {
            "restart" => match self.try_restart_child() {
                Ok(()) => {
                    self.stopping.store(false, Ordering::SeqCst); // 允许重启后的子进程再次触发
                    self.write_log("host", &f("Child process restarted (failure #{0})", &[&self.consecutive_failures.to_string()]));
                    return true;
                }
                Err(e) => self.write_log("host", &f("Child restart failed: {0}", &[&e])),
            },
            "reboot" => {
                // 系统重启经 osmium-kit-reboot 插件执行；插件缺失时仅告警
                self.write_log("host", "Failure policy: rebooting system");
                match run_plugin("reboot", &serde_json::json!({})) {
                    Ok(()) => {}
                    Err(e) => self.write_log("host", &f("Reboot plugin failed: {0}", &[&e])),
                }
            }
            _ => {
                // none → 停止宿主（保持停止）
                self.write_log("host", "Failure policy: no further restart (action=none), stopping service");
            }
        }
        self.stop_host("Failure policy applied, stopping service", "Service stopping", Some("Service stopped"));
        false
    }

    /// 停止流程公共路径: 置停止标志 → prestop 钩子 → 停止子进程 → 停止后钩子 → 生命周期扩展 phase=stop
    fn stop_host(&mut self, signal_msg: &str, stopping_msg: &str, done_msg: Option<&str>) {
        self.stopping.store(true, Ordering::SeqCst);
        // 停止流程前退出宿主效率模式（保证停止/清理不被低调度拖慢）
        if self.host_eco_qos_active {
            let _ = set_eco_qos(std::process::id(), false);
            self.host_eco_qos_active = false;
        }
        self.write_log("host", signal_msg);
        self.write_log("host", stopping_msg);
        // 停止阶段资源操作需要配置，宿主不常驻整份配置 → 停止流程开始前重读一次
        let config = self.current_config();
        // prestop 钩子 + 生命周期插件 phase=stop_before（主进程停止前，失败不阻断）
        self.run_extensions("stop_before");
        if let Some(cfg) = &config
            && let Err(e) = self.run_plugin_calls(cfg.plugins.as_deref(), "stop_before")
        {
            self.write_log("host", &e);
        }
        self.stop_child_process();
        // RunawayProcessKiller: 子进程已停止，删除 pid 文件防残留
        self.remove_runaway_pid();
        self.run_poststop();
        self.run_extensions("stop_after");
        // 停止阶段: 生命周期插件 phase=stop_after + after_stop 下载 + 断开共享目录映射（失败仅告警）
        if let Some(cfg) = &config {
            if let Err(e) = self.run_plugin_calls(cfg.plugins.as_deref(), "stop_after") {
                self.write_log("host", &e);
            }
            // after_stop 阶段下载（逐条按条目级 stage 过滤，失败仅告警）
            run_aux_download(cfg, &self.deploy_dir, &self.hook_log_dir(), &self.log_opts, "after_stop");
            self.netmap_via_plugin(cfg, "unmap");
        }
        if let Some(done) = done_msg {
            self.write_log("host", done);
        }
    }

    /// 重读部署目录配置（停止阶段资源操作需要配置，宿主不常驻整份配置）
    fn current_config(&self) -> Option<crate::service_config::ServiceConfig> {
        self.load_deployed_config().ok().map(|c| self.expand_config(&c))
    }

    /// 读取部署配置（异常重启/停止阶段资源操作需要配置，宿主不常驻整份配置）:
    /// 优先用启动时记录的配置路径（共享宿主 = svcs\<name>\<name>.osiml），
    /// 普通宿主回退 exe 旁配置；失败返回 panic 详情
    fn load_deployed_config(&self) -> Result<crate::service_config::ServiceConfig, String> {
        let config_path = self.config_path.clone()
            .unwrap_or_else(|| config_path_next_to(Path::new(&crate::service_core::get_own_path())));
        std::panic::catch_unwind(|| crate::service_core::load_config(&config_path))
            .map_err(|p| crate::service_core::panic_msg(&*p, "Unknown error"))
    }

    /// RunawayProcessKiller: 周期检查子进程内存/CPU 占用，超限自动终止（触发 onfailure 流程）
    fn check_runaway(&mut self) {
        if self.runaway_cpu_limit.is_none() && self.runaway_memory_limit_mb.is_none() {
            return;
        }
        let Some(pid) = self.child.as_ref().map(|c| c.id()) else { return };
        let interval = Duration::from_secs(self.runaway_check_interval_secs.max(1) as u64);
        if self.runaway_last_check.is_some_and(|t| t.elapsed() < interval) {
            return;
        }
        self.runaway_last_check = Some(Instant::now());

        // 内存超限（工作集 MB）
        let ws = process_working_set_mb(pid);
        if runaway_exceeded(ws, self.runaway_memory_limit_mb, None, None) {
            self.write_log("host", &f("RunawayProcessKiller: memory {0} MB exceeds limit {1} MB, killing child",
                &[&ws.unwrap_or(0).to_string(), &self.runaway_memory_limit_mb.unwrap_or(0).to_string()]));
            self.force_kill();
            return;
        }
        // CPU 超限（内核+用户时间差 / 墙钟差，全核累计百分比）
        if let Some(limit) = self.runaway_cpu_limit {
            let now = Instant::now();
            if let Some(cpu) = process_cpu_100ns(pid) {
                if let Some((last_pid, last_cpu, last_at)) = self.runaway_last_sample
                    && last_pid == pid
                {
                    let wall = now.duration_since(last_at).as_secs_f64();
                    let delta = cpu.saturating_sub(last_cpu) as f64 / 10_000_000.0; // 100ns → 秒
                    if wall > 0.5 && runaway_exceeded(None, None, Some(delta / wall * 100.0), self.runaway_cpu_limit) {
                        // f() 模板不支持格式说明符（{0:.1} 不会被替换），百分比先格式化再插入
                        let pct = format!("{:.1}", delta / wall * 100.0);
                        let limit_str = format!("{:.1}", limit);
                        self.write_log("host", &f("RunawayProcessKiller: CPU {0}% exceeds limit {1}%, killing child",
                            &[&pct, &limit_str]));
                        self.force_kill();
                        self.runaway_last_sample = None;
                        return;
                    }
                }
                self.runaway_last_sample = Some((pid, cpu, now));
            }
        }
    }

    /// 子进程 auto 效率模式切换（独立采样，不受 runaway 配置影响）
    fn check_child_eco_qos(&mut self) {
        if self.eco_qos_mode != EcoQosMode::Auto { return; }
        let Some(pid) = self.child.as_ref().map(|c| c.id()) else { return };
        let now = Instant::now();
        let Some(cpu) = process_cpu_100ns(pid) else { return };
        let Some((last_pid, last_cpu, last_at)) = self.child_eco_sample else {
            self.child_eco_sample = Some((pid, cpu, now));
            return;
        };
        self.child_eco_sample = Some((pid, cpu, now));
        if last_pid != pid { return; }
        let wall = now.duration_since(last_at).as_secs_f64();
        if wall <= 0.5 { return; }
        let cpu_pct = cpu.saturating_sub(last_cpu) as f64 / 10_000_000.0 / wall * 100.0;
        if self.eco_qos_active {
            if cpu_pct > self.eco_qos_busy_pct {
                if set_eco_qos(pid, false) {
                    self.write_log("host", &format!("EcoQoS: child exited efficiency mode (CPU {:.1}%)", cpu_pct));
                }
                self.eco_qos_active = false;
            }
        } else if cpu_pct < self.eco_qos_idle_pct {
            self.eco_qos_idle_streak += 1;
            if self.eco_qos_idle_streak >= 2 {
                if set_eco_qos(pid, true) {
                    self.write_log("host", &format!("EcoQoS: child entered efficiency mode (CPU {:.1}%)", cpu_pct));
                }
                self.eco_qos_active = true;
                self.eco_qos_idle_streak = 0;
            }
        } else {
            self.eco_qos_idle_streak = 0;
        }
    }

    /// 宿主自身 auto 效率模式: 自身 CPU 低（连续 2 次低于 idle）进入、
    /// 自身或子进程 CPU 高于 busy 退出（子进程繁忙联动，保证密集工作期间宿主全速调度）
    fn check_host_eco_qos(&mut self) {
        if self.host_eco_qos_mode == EcoQosMode::None { return; }
        let now = Instant::now();
        let host_pid = std::process::id();
        let Some(cpu) = process_cpu_100ns(host_pid) else { return };
        let Some((last_cpu, last_at)) = self.host_eco_qos_sample else {
            self.host_eco_qos_sample = Some((cpu, now));
            return;
        };
        self.host_eco_qos_sample = Some((cpu, now));
        let wall = now.duration_since(last_at).as_secs_f64();
        if wall <= 0.5 { return; }
        let host_pct = cpu.saturating_sub(last_cpu) as f64 / 10_000_000.0 / wall * 100.0;
        // 子进程 CPU（联动）: 与宿主同间隔采样
        let mut child_pct = 0.0_f64;
        if let Some(cid) = self.child.as_ref().map(|c| c.id())
            && let Some(ccpu) = process_cpu_100ns(cid)
        {
            if let Some((lp, lc, la)) = self.host_child_sample
                && lp == cid
            {
                let w = now.duration_since(la).as_secs_f64();
                if w > 0.5 {
                    child_pct = ccpu.saturating_sub(lc) as f64 / 10_000_000.0 / w * 100.0;
                }
            }
            self.host_child_sample = Some((cid, ccpu, now));
        } else {
            self.host_child_sample = None;
        }
        let busy = host_pct > self.host_eco_qos_busy_pct || child_pct > self.eco_qos_busy_pct;
        if self.host_eco_qos_active {
            if busy {
                if set_eco_qos(host_pid, false) {
                    self.write_log("host", &format!("Host EcoQoS: exited efficiency mode (host CPU {:.1}%, child {:.1}%)", host_pct, child_pct));
                }
                self.host_eco_qos_active = false;
            }
        } else if host_pct < self.host_eco_qos_idle_pct && child_pct <= self.eco_qos_busy_pct {
            self.host_eco_qos_idle_streak += 1;
            if self.host_eco_qos_idle_streak >= 2 {
                if set_eco_qos(host_pid, true) {
                    self.write_log("host", &format!("Host EcoQoS: entered efficiency mode (host CPU {:.1}%)", host_pct));
                }
                self.host_eco_qos_active = true;
                self.host_eco_qos_idle_streak = 0;
            }
        } else {
            self.host_eco_qos_idle_streak = 0;
        }
    }

    /// RunawayProcessKiller 启动清理: 按 pid 文件终止上次宿主残留的进程树（失败仅告警）；
    /// service_id 用于防误杀校验（对齐 WinSW #237: 只清理带本服务标识的残留进程）
    fn cleanup_runaway_pid(&mut self, service_id: Option<&str>) {
        let Some(path) = self.runaway_pid_file.clone() else { return };
        match runaway_cleanup_pid_file(&path, self.runaway_stop_timeout_ms, self.runaway_stop_parent_first, service_id) {
            Ok(Some(pid)) => self.write_log("host", &f("RunawayProcessKiller: terminated leftover process {0} from pid file", &[&pid.to_string()])),
            Ok(None) => {}
            Err(e) => self.write_log("host", &f("RunawayProcessKiller: {0}", &[&e])),
        }
    }

    /// 配置热刷新（对应 WinSW autoRefresh）: 配置文件 mtime 变化时重新加载配置并重启子进程。
    /// 解析失败保持旧配置运行（避免配置损坏期间反复重启）；仅 auto_refresh=true 时由 tick 调用
    fn check_config_refresh(&mut self) {
        let Some(path) = self.config_path.clone() else { return };
        let mtime = std::fs::metadata(&path).ok().and_then(|m| m.modified().ok());
        if mtime.is_none() || mtime == self.config_mtime {
            return;
        }
        let config = match std::panic::catch_unwind(|| crate::service_core::load_config(&path)) {
            Ok(c) => c,
            Err(p) => {
                let detail = crate::service_core::panic_msg(&*p, "unknown error");
                self.write_log("host", &f("Configuration reload failed: {0}. Keeping previous configuration.", &[&detail]));
                return;
            }
        };
        let config = self.expand_config(&config);
        self.config_mtime = mtime;
        self.write_log("host", "Configuration file changed, restarting child process");
        // 应用新配置（日志参数 + 子进程运行字段），再优雅停止旧子进程并按新配置重启
        self.apply_log_settings(&config);
        self.apply_runtime_fields(&config);
        self.stop_child_process();
        self.consecutive_failures = 0;
        if let Err(e) = self.start_child_process(&config) {
            self.write_log("host", &f("Child restart after config change failed: {0}", &[&e]));
        }
    }

    /// 启动成功后回写子进程 PID 到 pid 文件（下次启动据此清理残留）
    fn write_runaway_pid(&self, pid: u32) {
        let Some(path) = &self.runaway_pid_file else { return };
        let _ = std::fs::write(path, pid.to_string());
    }

    /// 停止后删除 pid 文件（避免残留 PID 指向已退出进程）
    fn remove_runaway_pid(&self) {
        if let Some(path) = &self.runaway_pid_file {
            let _ = std::fs::remove_file(path);
        }
    }

    // ==================== 子进程启动 & 控制 ====================

    /// 启动目标子进程（on_start 与异常退出重启共用）；返回 Err 表示启动失败
    fn start_child_process(&mut self, config: &crate::service_config::ServiceConfig) -> Result<(), String> {
        // 启动前下载（可选）: 仅 before_start 阶段在启动前确保目标可执行文件就绪；
        // 其他阶段（after_start/after_stop）不参与可执行性检查；日志禁用时传空目录使其静默
        let hook_log_dir = self.hook_log_dir();
        let exe_path = if download_stage_is(config, "before_start") {
            prepare_download(config, &self.deploy_dir, &hook_log_dir, &self.log_opts)?
        } else {
            config.service_executable_path.clone()
        };

        if !Path::new(&exe_path).exists() {
            return Err(f("Executable not found: '{0}'. Check service_executable_path or download settings.", &[&exe_path]));
        }
        // 工作目录: working_directory 优先（相对基于部署目录），缺省取目标 exe 所在目录
        let working_dir = match config.working_directory.as_deref() {
            Some(dir) if !dir.trim().is_empty() => resolve_deploy_path(dir, &self.deploy_dir),
            _ => Path::new(&exe_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|| self.deploy_dir.clone()),
        };
        if !Path::new(&working_dir).exists() {
            return Err(f("Working directory does not exist: '{0}'. Check service_executable_path / download_to / working_directory.", &[&working_dir]));
        }

        // 生命周期扩展 phase=start（在目标进程启动前执行）
        self.run_extensions("start_before");
        // 生命周期插件 phase=start（fail_on_error=true 时阻断启动）
        self.run_plugin_calls(config.plugins.as_deref(), "start_before")?;

        // 启动专用参数（start_arguments）覆盖普通参数（对应 WinSW startarguments）
        let args_str = config.start_arguments.as_deref()
            .filter(|s| !s.trim().is_empty())
            .or(config.service_executable_args.as_deref())
            .unwrap_or("");
        // 参数为空时后缀为空串，否则带前导空格
        let args_prefix = if args_str.is_empty() { String::new() } else { format!(" {}", args_str) };
        self.write_log("host", &f("Target: {0}{1}", &[&exe_path, &args_prefix]));

        // 构造目标进程 Command（工作目录/env/参数/窗口隐藏/输出管道；注入 BASE 与 WINSGF_SERVICE_ID）
        let mut cmd = build_child_command(&exe_path, Some(args_str), &working_dir,
            config.env.as_ref(), &self.deploy_dir, self.hide_window,
            self.log_opts.out_enabled, self.log_opts.err_enabled,
            Some(&config.service_name));
        if let Some(ref env) = config.env {
            self.write_log("host", &f("Injected {0} environment variable(s)", &[&env.len().to_string()]));
        }

        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => return Err(e.to_string()),
        };
        let pid = child.id();
        // 设置目标进程优先级（可选）
        set_process_priority(pid, self.process_priority.as_deref());
        // RunawayProcessKiller 启动清理: 回写当前子进程 PID，供下次启动清理残留
        self.write_runaway_pid(pid);
        // 消费子进程 stdout/stderr，避免管道缓冲区写满阻塞子进程；日志禁用时传空目录使其静默
        let reader_log_dir = if self.log_enabled { self.log_dir.clone() } else { String::new() };
        let reader_opts = self.log_opts.clone();
        if let Some(out) = child.stdout.take() {
            let _ = spawn_log_reader(out, reader_log_dir.clone(), "out", reader_opts.clone());
        }
        if let Some(err) = child.stderr.take() {
            let _ = spawn_log_reader(err, reader_log_dir, "err", reader_opts);
        }
        self.write_log("host", &f("Child process started, PID: {0}", &[&pid.to_string()]));
        self.child = Some(child);
        self.last_child_pid = pid;
        // 效率模式 always: 子进程启动后立即进入（auto 由 check_runaway 采样驱动）
        if self.eco_qos_mode == EcoQosMode::Always {
            let _ = set_eco_qos(pid, true);
        } else {
            // auto 模式: 新子进程重置状态（旧进程的效率模式状态/采样不适用）
            self.eco_qos_active = false;
            self.eco_qos_idle_streak = 0;
            self.child_eco_sample = None;
        }
        // poststart 钩子（主进程启动后，失败不阻断）
        self.run_extensions("start_after");
        // 生命周期插件 phase=start_after（进程已启动不可回滚，失败仅告警）
        if let Err(e) = self.run_plugin_calls(config.plugins.as_deref(), "start_after") {
            self.write_log("host", &e);
        }
        // after_start 阶段下载（可选资源，逐条按条目级 stage 过滤，失败仅告警）
        run_aux_download(config, &self.deploy_dir, &self.hook_log_dir(), &self.log_opts, "after_start");
        Ok(())
    }

    /// 异常重启: 重新读取部署目录下的 toml 配置后再次启动（等价 ReloadConfig）
    fn try_restart_child(&mut self) -> Result<(), String> {
        let config = self.load_deployed_config()?;
        self.start_child_process(&self.expand_config(&config))
    }

    // ==================== 停止策略 & 钩子 ====================

    /// 停止子进程: GUI → WM_CLOSE, 控制台 → Ctrl+C, 超时 → 强制终止
    fn stop_child_process(&mut self) {
        let child = match &mut self.child {
            Some(c) => c,
            None => {
                self.write_log("host", "Child process already exited, nothing to stop");
                return;
            }
        };

        // 检查是否已退出
        match child.try_wait() {
            Ok(Some(_)) => {
                self.write_log("host", "Child process already exited, nothing to stop");
                return;
            }
            Ok(None) => {}
            Err(e) => {
                self.write_log("host", &f("Failed to check child process state: {0}", &[&e.to_string()]));
                return;
            }
        }

        let pid = child.id();
        self.write_log("host", &f("Stopping child process (PID: {0})", &[&pid.to_string()]));

        // 自定义停止程序（对应 WinSW stopExecutable）: 先运行停止命令，若子进程随之退出则完成；
        // 传入子进程 PID（%PID% 占位符 + WINSGF_CHILD_PID 环境变量，对应 WinSW #217）
        if let Some((exe, args)) = self.stop_cmd.clone() {
            self.write_log("host", &f("Running stop executable: {0}", &[&exe]));
            run_stop_command(&exe, &args, pid, self.stop_timeout_secs, self.hook_log_dir(), &self.log_opts);
            if let Some(Ok(Some(_))) = self.child.as_mut().map(|c| c.try_wait()) {
                self.write_log("host", "Child exited after stop executable");
                return;
            }
        }

        if self.try_close_main_window(pid) {
            self.write_log("host", "Child exited via WM_CLOSE");
            return;
        }
        if self.try_send_ctrl_c(pid) {
            self.write_log("host", "Child exited via Ctrl+C");
            return;
        }

        self.write_log("host", "Graceful shutdown failed, force killing");
        self.force_kill();
        self.write_log("host", "Child force killed");
    }

    /// 仅向该进程的顶层窗口发送 WM_CLOSE（等价于 Process.CloseMainWindow），
    /// 未找到该进程的窗口时快速失败，不等待
    fn try_close_main_window(&mut self, pid: u32) -> bool {
        WM_CLOSE_SENT.store(false, Ordering::SeqCst);
        unsafe {
            if let Err(e) = EnumWindows(Some(send_wm_close), LPARAM(pid as isize)) {
                self.write_log("host", &f("WM_CLOSE failed: {0}", &[&e.to_string()]));
                return false;
            }
        }

        // 没有找到该进程的窗口 → 无主窗口可关闭，快速失败
        if !WM_CLOSE_SENT.load(Ordering::SeqCst) {
            return false;
        }

        // 等待进程退出
        wait_child_exit(&mut self.child, self.stop_timeout_secs)
    }

    /// Ctrl+C 已发送且进程在超时前退出则返回 true；附加子进程控制台广播 (0,0)，
    /// 保持 Ctrl+C 忽略处理器注册到子进程退出，防止宿主自身被广播误杀。
    fn try_send_ctrl_c(&mut self, pid: u32) -> bool {
        unsafe {
            let _ = FreeConsole();
            if AttachConsole(pid).is_ok() {
                // 附加到控制台后再注册忽略 Ctrl+C，防止宿主自身被终止
                // （GenerateConsoleCtrlEvent(0,0) 会发给共享控制台的所有进程）
                let _ = SetConsoleCtrlHandler(Some(ignore_ctrl_c), true);
                if let Err(e) = GenerateConsoleCtrlEvent(0, 0) {
                    self.write_log("host", &f("Ctrl+C failed: {0}", &[&e.to_string()]));
                }
                // 关键: 先等待子进程退出再移除 handler/分离控制台。
                // Ctrl+C 事件异步派发，若先移除 handler，事件到达时走默认处理（终止宿主）
                let exited = wait_child_exit(&mut self.child, self.stop_timeout_secs);
                let _ = SetConsoleCtrlHandler(Some(ignore_ctrl_c), false);
                let _ = FreeConsole();
                exited
            } else {
                self.write_log("host", "Ctrl+C skipped: cannot attach to child console");
                false
            }
        }
    }

    /// 终止子进程（等价于 Process.Kill(entireProcessTree: kill_process_tree)）
    fn force_kill(&mut self) {
        let child = match &mut self.child {
            Some(c) => c,
            None => return,
        };
        if let Ok(Some(_)) = child.try_wait() {
            return; // 已经退出
        }

        let pid = child.id();
        // kill_process_tree=false 时仅终止主进程，保留其派生的独立子进程（对应 WinSW #990）；
        // stop_parent_process_first=true 时先终止父进程（主进程）再杀子树（对应 WinSW stopparentprocessfirst）
        if self.kill_process_tree {
            if self.stop_parent_process_first {
                let _ = child.kill();
                let _ = child.wait();
                terminate_pid_tree(pid);
                return;
            }
            terminate_pid_tree(pid);
        }

        let kill_result = child.kill();
        let _ = child.wait();
        if let Err(e) = kill_result {
            self.write_log("host", &f("Force kill failed: {0}", &[&e.to_string()]));
        }
    }

    // ==================== 生命周期钩子 ====================
    /// 钩子/下载的日志目录: log_enabled=false 时传空字符串使其静默（空串表示禁用）
    fn hook_log_dir(&self) -> String {
        if self.log_enabled {
            self.log_dir.clone()
        } else {
            String::new()
        }
    }

    /// 运行 poststop 钩子（目标进程停止后；失败仅告警），
    /// 注入 WINSGF_CHILD_PID/EXIT_CODE 环境变量便于精确处理子进程（对应 WinSW #217）
    fn run_poststop(&self) {
        let log_dir = self.hook_log_dir();
        let env: Option<Vec<(String, String)>> = if self.last_child_pid > 0 {
            Some(vec![
                ("WINSGF_CHILD_PID".to_string(), self.last_child_pid.to_string()),
                ("WINSGF_CHILD_EXIT_CODE".to_string(), self.last_child_exit_code.to_string()),
            ])
        } else {
            None
        };
        run_hook(self.poststop_command.as_deref(), "poststop", HOOK_POSTSTOP_TIMEOUT_MS,
            log_dir, env.as_deref(), &self.log_opts, None, None);
    }

    /// 执行指定阶段的全部生命周期扩展命令（失败仅告警，不阻断）;
    /// phase 兼容: start_before/start（启动前）、start_after（启动后）、stop_before（停止前）、stop_after/stop（停止后）
    fn run_extensions(&self, phase: &str) {
        let Some(exts) = &self.extensions else { return };
        let log_dir = self.hook_log_dir();
        for ext in exts.iter().filter(|e| ext_phase_matches(&e.phase, phase)) {
            // 重定向文件相对路径基于部署目录解析（与日志目录一致，防写到进程当前目录）
            let out_path = ext.stdout_path.as_deref()
                .map(|p| resolve_deploy_path(p, &self.deploy_dir));
            let err_path = ext.stderr_path.as_deref()
                .map(|p| resolve_deploy_path(p, &self.deploy_dir));
            run_hook(Some(&ext.command), &f("extension[{0}]", &[phase]),
                HOOK_PRESTART_TIMEOUT_MS, log_dir.clone(), None, &self.log_opts,
                out_path.as_deref(), err_path.as_deref());
        }
    }

    /// 执行指定阶段的全部生命周期插件调用（exe 同级 .osx，kit 分发 + payload 透传）;
    /// phase 兼容规则同 extensions；fail_on_error=true 时返回 Err（start 阶段阻断启动），
    /// 否则失败仅告警不阻断
    fn run_plugin_calls(&self, plugins: Option<&[crate::service_config::PluginCallConfig]>, phase: &str) -> Result<(), String> {
        let Some(plugins) = plugins else { return Ok(()) };
        for p in plugins.iter().filter(|p| ext_phase_matches(&p.phase, phase)) {
            self.write_log("host", &f("Running plugin [{0}]: {1}", &[phase, &p.kit]));
            // 非对象 payload（Null/标量）规范化为空对象，保证 kit 字段可注入请求 JSON
            let payload = if p.payload.is_object() { p.payload.clone() } else { serde_json::json!({}) };
            match run_plugin(&p.kit, &payload) {
                Ok(()) => self.write_log("host", &f("Plugin completed: {0}", &[&p.kit])),
                Err(e) => {
                    if p.fail_on_error {
                        return Err(f("Plugin {0} failed: {1}", &[&p.kit, &e]));
                    }
                    self.write_log("host", &f("Plugin {0} failed (non-fatal): {1}", &[&p.kit, &e]));
                }
            }
        }
        Ok(())
    }

    // ==================== 日志输出 ====================

    /// 写入宿主日志条目: 受 log_enabled 控制；stderr 分流与大小滚动由 log_opts 决定；
    /// event_log 开启时 host 通道同时写入 Windows 事件日志（独立于 log_enabled 开关）
    pub fn write_log(&self, channel: &str, message: &str) {
        // 事件日志先写：event_log=true 时即使 log_enabled=false 也记录事件（两个开关语义独立）
        if self.event_log && channel == "host" {
            report_event_log(message);
        }
        if !self.log_enabled {
            return;
        }
        write_log_entry(&self.log_dir, channel, message, &self.log_opts);
    }
}

// ==================== 子进程 Command 构造 & 输出消费 ====================

/// 构造目标进程 Command（工作目录/env/参数/窗口隐藏/输出管道）: 自动注入 BASE 与 WINSGF_SERVICE_ID；
/// 注意不能加 CREATE_NEW_PROCESS_GROUP: 否则子进程忽略 Ctrl+C，优雅停止退化为强制终止
#[allow(clippy::too_many_arguments)] // 全部为命令构造所需配置项，参数打包反增调用点负担
pub(crate) fn build_child_command(
    exe_path: &str,
    args: Option<&str>,
    working_dir: &str,
    env: Option<&std::collections::HashMap<String, String>>,
    deploy_dir: &str,
    hide_window: bool,
    out_enabled: bool,
    err_enabled: bool,
    service_id: Option<&str>,
) -> Command {
    let mut cmd = Command::new(exe_path);
    // raw_arg 原样拼接参数字符串，保留引号语义（不经拆分，避免带引号参数被切碎）
    if let Some(args) = args
        && !args.trim().is_empty()
    {
        cmd.raw_arg(args);
    }
    // out/err 被禁用时直接丢弃（null），避免管道积压阻塞子进程
    let out_mode = if out_enabled { Stdio::piped() } else { Stdio::null() };
    let err_mode = if err_enabled { Stdio::piped() } else { Stdio::null() };
    cmd.current_dir(working_dir)
        .stdin(Stdio::null())
        .stdout(out_mode)
        .stderr(err_mode)
        .creation_flags(if hide_window { 0x08000000 } else { 0 });
    // 注入自定义环境变量（值支持 %VAR% 展开，%BASE% 指部署目录）
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, expand_env_value(v, deploy_dir));
        }
    }
    // 自动注入 BASE 环境变量（对应 WinSW: wrapper 设置 BASE 且子进程可读）；
    // 用户 env 显式配置 BASE 时以用户为准
    if !env.map(|e| e.keys().any(|k| k.eq_ignore_ascii_case("BASE"))).unwrap_or(false) {
        cmd.env("BASE", deploy_dir);
    }
    // 注入服务标识（对应 WinSW WINSW_SERVICE_ID，RunawayProcessKiller 防 PID 复用误杀）
    if let Some(id) = service_id
        && !id.is_empty()
    {
        cmd.env("WINSGF_SERVICE_ID", id);
    }
    cmd
}

/// 等待子进程退出（最多 timeout_secs 秒），返回是否已退出。
/// 优雅停止（WM_CLOSE / Ctrl+C）后复用，保证信号异步派发期间处理器保持注册
fn wait_child_exit(child: &mut Option<Child>, timeout_secs: u64) -> bool {
    if let Some(c) = child {
        for _ in 0..(timeout_secs * 10) {
            match c.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => thread::sleep(Duration::from_millis(100)),
                Err(_) => return false,
            }
        }
    }
    false
}

/// 读取子进程输出流并逐行写入日志，直到 EOF；返回线程句柄供等待
fn spawn_log_reader<R: Read + Send + 'static>(
    stream: R,
    log_dir: String,
    channel: &'static str,
    opts: LogOptions,
) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = line.trim_end();
                    if !text.is_empty() {
                        write_log_entry(&log_dir, channel, text, &opts);
                    }
                }
            }
        }
    })
}

/// 把流逐行原样追加写入指定文件（钩子 stdout/stderr 独立重定向用）
fn spawn_raw_reader<R: Read + Send + 'static>(stream: R, file_path: String) -> thread::JoinHandle<()> {
    thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let mut out = std::fs::OpenOptions::new().create(true).append(true).open(&file_path);
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Ok(f) = out.as_mut() {
                        let _ = f.write_all(line.as_bytes());
                    }
                }
            }
        }
    })
}

// ==================== 生命周期钩子执行 ====================

/// phase 匹配: 兼容旧值 start→start_before、stop→stop_after
pub(crate) fn ext_phase_matches(configured: &str, current: &str) -> bool {
    match current {
        "start_before" => configured.eq_ignore_ascii_case("start_before") || configured.eq_ignore_ascii_case("start"),
        "stop_after" => configured.eq_ignore_ascii_case("stop_after") || configured.eq_ignore_ascii_case("stop"),
        _ => configured.eq_ignore_ascii_case(current),
    }
}

/// 下载执行阶段判定: 未配置 download_stage 时默认 before_start
pub(crate) fn download_stage_is(config: &crate::service_config::ServiceConfig, stage: &str) -> bool {
    match config.download_stage.as_deref().map(|s| s.to_lowercase()) {
        Some(s) => s == stage,
        None => stage == "before_start",
    }
}

/// 构造 onfailure 动作序列: 优先 failure_actions（过滤非法动作）；
/// 未配置时用 failure_action + restart_delay_ms 构造单动作并补齐"重启 3 次后停止"的旧行为
pub(crate) fn failure_action_chain(config: &crate::service_config::ServiceConfig) -> Vec<crate::service_config::FailureActionConfig> {
    if let Some(actions) = config.failure_actions.as_ref()
        && !actions.is_empty()
    {
        return actions.iter()
            .filter(|a| matches!(a.action.to_lowercase().as_str(), "restart" | "reboot" | "none"))
            .cloned()
            .collect();
    }
    let delay = if config.restart_delay_ms > 0 { config.restart_delay_ms as u64 / 1000 } else { 60 };
    // 兼容旧配置: 每次异常退出执行 failure_action（默认 restart），重复 3 次后停止（与历史 MAX_RESTART_ATTEMPTS=3 行为一致）
    let action = config.failure_action.clone().unwrap_or_else(|| "restart".into());
    vec![
        crate::service_config::FailureActionConfig { action: action.clone(), delay_secs: delay },
        crate::service_config::FailureActionConfig { action: action.clone(), delay_secs: delay },
        crate::service_config::FailureActionConfig { action, delay_secs: delay },
        crate::service_config::FailureActionConfig { action: "none".into(), delay_secs: 0 },
    ]
}

/// 非启动阶段下载（after_start/after_stop）: 逐条按条目级 stage 过滤执行，失败仅告警，不影响服务生命周期
fn run_aux_download(
    config: &crate::service_config::ServiceConfig,
    deploy_dir: &str,
    log_dir: &str,
    opts: &LogOptions,
    stage: &str,
) {
    let is_array = config.downloads.as_deref().is_some_and(|l| !l.is_empty());
    for entry in download_entries(config) {
        // 无下载配置时生成的空 legacy 条目直接跳过（同 prepare_download）
        if !is_array && entry.from.trim().is_empty() {
            continue;
        }
        if !download_entry_stage(&entry, config).eq_ignore_ascii_case(stage) {
            continue;
        }
        if let Err(e) = run_download_entry(config, &entry, deploy_dir, log_dir, opts) {
            write_log_entry(log_dir, "host", &f("Aux download failed (non-fatal): {0}", &[&e]), opts);
        }
    }
}

/// 执行钩子命令: cmd.exe /d /c 运行，输出记入日志，超时强杀整棵进程树，失败仅告警（对应 RunHook）；
/// 信任模型: 命令来自管理员部署的 toml，目录 ACL 已收紧仅 SYSTEM/Administrators 可写（WinSW #922/#439）
#[allow(clippy::too_many_arguments)] // 全部为钩子执行所需配置项，参数打包反增调用点负担
pub(crate) fn run_hook(
    command: Option<&str>,
    phase: &str,
    timeout_ms: u64,
    log_dir: String,
    env: Option<&[(String, String)]>,
    opts: &LogOptions,
    out_path: Option<&str>,
    err_path: Option<&str>,
) {
    let Some(command) = command else { return };
    if command.trim().is_empty() {
        return;
    }
    write_log_entry(&log_dir, "host", &f("Hook [{0}] executing: {1}", &[phase, command]), opts);

    let mut cmd = Command::new("cmd.exe");
    // /s 强制"剥离首尾引号"规则: 不加 /s 时引号包裹的命令内重定向/管道会被吞掉
    //（如 `echo x >> file` 静默失败），加了 /s 后重定向照常生效（内层引号语义保留）
    cmd.raw_arg("/d").raw_arg("/s").raw_arg("/c").raw_arg(format!("\"{}\"", command));
    // stdin 显式置 null: 服务进程在 Ctrl+C 广播后标准句柄可能变为无效句柄，
    // 继承该句柄会让 CreateProcessW 报 ERROR_INVALID_HANDLE（poststop 钩子必现）
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(0x08000000);
    if let Some(env) = env {
        for (k, v) in env {
            cmd.env(k, v);
        }
    }

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            write_log_entry(&log_dir, "host", &f("Hook [{0}] failed to run: {1}", &[phase, &e.to_string()]), opts);
            return;
        }
    };
    let pid = child.id();

    // 消费钩子输出（channel=hook）: 配置了独立重定向文件则原样追加写入，否则进宿主日志
    let mut handles = Vec::new();
    if let Some(out) = child.stdout.take() {
        handles.push(match out_path {
            Some(p) => spawn_raw_reader(out, p.to_string()),
            None => spawn_log_reader(out, log_dir.clone(), "hook", opts.clone()),
        });
    }
    if let Some(err) = child.stderr.take() {
        handles.push(match err_path {
            Some(p) => spawn_raw_reader(err, p.to_string()),
            None => spawn_log_reader(err, log_dir.clone(), "hook", opts.clone()),
        });
    }

    // 轮询等待，超时强杀整棵进程树
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status.code().unwrap_or(-1)),
            Ok(None) => {
                if Instant::now() >= deadline {
                    break None;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break Some(-1),
        }
    };

    match exit_code {
        None => {
            write_log_entry(&log_dir, "host",
                &f("Hook [{0}] timed out after {1}s, killing", &[phase, &(timeout_ms / 1000).to_string()]), opts);
            terminate_pid_tree(pid);
            let _ = child.kill();
            let _ = child.wait();
        }
        Some(code) => {
            if code == 0 {
                write_log_entry(&log_dir, "host", &f("Hook [{0}] completed (code 0)", &[phase]), opts);
            } else {
                write_log_entry(&log_dir, "host", &f("Hook [{0}] exited with code {1} (non-fatal)", &[phase, &code.to_string()]), opts);
            }
        }
    }

    // 等待日志读取线程排空输出后再返回
    for h in handles {
        let _ = h.join();
    }
}

// ==================== 启动前下载 ====================

/// 确保下载文件就绪并返回应启动的本地路径（等价 PrepareDownload）:
/// 数组模式逐个执行 before_start 条目（可执行路径不变），单条模式下载目标即可执行文件
fn prepare_download(
    config: &crate::service_config::ServiceConfig,
    deploy_dir: &str,
    log_dir: &str,
    opts: &LogOptions,
) -> Result<String, String> {
    let entries = download_entries(config);
    let is_array = config.downloads.as_deref().is_some_and(|l| !l.is_empty());
    for entry in &entries {
        // 无下载配置时 download_entries 生成一条空 legacy 条目，须跳过（否则报 missing 'from' 阻断启动）
        if !is_array && entry.from.trim().is_empty() {
            continue;
        }
        if download_entry_stage(entry, config).eq_ignore_ascii_case("before_start") {
            run_download_entry(config, entry, deploy_dir, log_dir, opts)?;
        }
    }
    // 旧单条模式: 下载目标替换可执行路径；数组模式: 可执行路径保持 service_executable_path。
    // 未配置下载（download_url 空）时不替换，保持原可执行路径
    if !is_array
        && crate::service_core::has_download(config)
        && let Some(entry) = entries.first()
    {
        let target = resolve_entry_target(entry, config, deploy_dir);
        if !target.is_empty() {
            return Ok(target);
        }
    }
    Ok(config.service_executable_path.clone())
}

/// 明文 HTTP 下载防护（对应 WinSW #1352 + unsecureAuth）: http 无 sha256 拒绝（P1-4）；
/// basic + http 拒绝，除非显式 unsecure_auth=true（凭据明文泄漏，对应 WinSW unsecureAuth）
#[cfg(test)]
pub(crate) fn warn_if_insecure_download(config: &crate::service_config::ServiceConfig) -> Result<(), String> {
    for entry in download_entries(config) {
        warn_if_insecure_entry(&entry)?;
    }
    Ok(())
}

fn warn_if_insecure_entry(entry: &crate::service_config::DownloadConfig) -> Result<(), String> {
    let url = entry.from.trim();
    if url.is_empty() {
        return Ok(());
    }
    let Ok(uri) = url::Url::parse(url) else { return Ok(()) };
    if uri.scheme() != "http" {
        return Ok(());
    }
    // 去敏 URL（去 query/fragment/userinfo）再进错误与日志，防带认证参数的地址泄漏（P1-2）
    let redacted = redact_url(url);
    let basic_over_http = entry.auth.as_deref().map(|a| a.eq_ignore_ascii_case("basic")).unwrap_or(false);
    if basic_over_http && !entry.unsecure_auth.unwrap_or(false) {
        return Err(f("Insecure download: '{0}' sends basic auth credentials over plain HTTP. Set unsecure_auth (or download_unsecure_auth) = true to allow, or use an https:// URL.", &[&redacted]));
    }
    let sha_empty = entry.sha256.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true);
    if !sha_empty {
        return Ok(());
    }
    Err(f("Insecure download: '{0}' uses plain HTTP without sha256. The payload may be tampered with in transit and is executed as the service account. Use an https:// URL or provide sha256.", &[&redacted]))
}

/// 归一化下载条目: downloads 数组优先（条目缺省回退配置级 download_* 值），
/// 未配置数组时用旧单条 download_* 字段构造一条（向后兼容）
pub(crate) fn download_entries(config: &crate::service_config::ServiceConfig) -> Vec<crate::service_config::DownloadConfig> {
    if let Some(list) = config.downloads.as_deref()
        && !list.is_empty()
    {
        return list.iter().map(|d| merge_download_defaults(d, config)).collect();
    }
    vec![crate::service_config::DownloadConfig {
        from: config.download_url.clone().unwrap_or_default(),
        to: config.download_to.clone().unwrap_or_default(),
        sha256: config.download_sha256.clone(),
        fail_on_error: Some(config.download_fail_on_error),
        auth: config.download_auth.clone(),
        username: config.download_username.clone(),
        password: config.download_password.clone(),
        unsecure_auth: Some(config.download_unsecure_auth),
        proxy: config.download_proxy.clone(),
        unzip: Some(config.download_unzip),
        stage: config.download_stage.clone(),
    }]
}

/// 条目缺省回退配置级 download_* 值
fn merge_download_defaults(d: &crate::service_config::DownloadConfig, config: &crate::service_config::ServiceConfig) -> crate::service_config::DownloadConfig {
    crate::service_config::DownloadConfig {
        from: d.from.clone(),
        to: d.to.clone(),
        sha256: d.sha256.clone().or_else(|| config.download_sha256.clone()),
        fail_on_error: d.fail_on_error.or(Some(config.download_fail_on_error)),
        auth: d.auth.clone().or_else(|| config.download_auth.clone()),
        username: d.username.clone().or_else(|| config.download_username.clone()),
        password: d.password.clone().or_else(|| config.download_password.clone()),
        unsecure_auth: d.unsecure_auth.or(Some(config.download_unsecure_auth)),
        proxy: d.proxy.clone().or_else(|| config.download_proxy.clone()),
        unzip: d.unzip.or(Some(config.download_unzip)),
        stage: d.stage.clone().or_else(|| config.download_stage.clone()),
    }
}

/// 条目有效阶段: 条目级 stage 优先，回退配置级 download_stage，再回退 before_start
pub(crate) fn download_entry_stage<'a>(entry: &'a crate::service_config::DownloadConfig, config: &'a crate::service_config::ServiceConfig) -> &'a str {
    if let Some(s) = entry.stage.as_deref()
        && !s.trim().is_empty()
    {
        return s;
    }
    if let Some(s) = config.download_stage.as_deref()
        && !s.trim().is_empty()
    {
        return s;
    }
    "before_start"
}

/// 条目目标路径解析: to 必填（相对基于部署目录）；单条模式缺 to 时沿用旧语义（exe 文件名）
fn resolve_entry_target(entry: &crate::service_config::DownloadConfig, config: &crate::service_config::ServiceConfig, deploy_dir: &str) -> String {
    let to = entry.to.trim();
    if !to.is_empty() {
        let p = Path::new(to);
        return if p.is_absolute() || to.starts_with('\\') {
            p.to_string_lossy().to_string()
        } else {
            Path::new(deploy_dir).join(to).to_string_lossy().to_string()
        };
    }
    if config.downloads.as_deref().is_some_and(|l| !l.is_empty()) {
        return String::new(); // 数组模式缺 to → 由调用方报错
    }
    resolve_download_target(config, deploy_dir)
}

/// 条目 sha 校验: 未配置 sha256 视为匹配
fn entry_sha_ok(entry: &crate::service_config::DownloadConfig, target: &str) -> bool {
    match entry.sha256.as_deref() {
        None => true,
        Some(s) if s.trim().is_empty() => true,
        Some(s) => crate::service_core::sha256_matches(target, Some(s)),
    }
}

/// 执行单条下载条目（等价 PrepareDownload 核心）:
/// insecure 检查 → 已就绪跳过 → 下载（含 304/If-Modified-Since）→ sha 校验 → zip 解压（插件）
fn run_download_entry(
    config: &crate::service_config::ServiceConfig,
    entry: &crate::service_config::DownloadConfig,
    deploy_dir: &str,
    log_dir: &str,
    opts: &LogOptions,
) -> Result<(), String> {
    let url = entry.from.trim();
    if url.is_empty() {
        return Err("Download entry missing 'from'".into());
    }
    warn_if_insecure_entry(entry)?;
    let target = resolve_entry_target(entry, config, deploy_dir);
    if target.is_empty() {
        return Err("Download entry missing 'to'".into());
    }
    let sha_ok = entry_sha_ok(entry, &target);
    // 已存在且（无 sha 或 sha 匹配）→ 跳过下载
    if Path::new(&target).exists() && sha_ok {
        write_log_entry(log_dir, "host", &f("Download target already up to date: {0}", &[&target]), opts);
        return Ok(());
    }
    // 缓存存在但校验失败 → 删除不可信缓存，防止 fail_on_error=false 时校验失败的文件被继续执行
    if !sha_ok && Path::new(&target).exists() {
        write_log_entry(log_dir, "host", "Download target SHA-256 mismatch, re-downloading", opts);
        let _ = std::fs::remove_file(&target);
    }
    if !try_download_entry(config, entry, url, &target, log_dir, opts) {
        let fail_on_error = entry.fail_on_error.unwrap_or(config.download_fail_on_error);
        if fail_on_error {
            return Err(f("Download failed: {0} (target: {1}). Check the URL, network connectivity, and authentication settings.", &[url, &target]));
        }
        write_log_entry(log_dir, "host", &f("Download failed but fail_on_error=false: continuing (target may be missing): {0} (target: {1})", &[url, &target]), opts);
        // fail_on_error=false 允许"目标缺失时继续（由启动阶段的文件存在性检查报错）"，
        // 但绝不允许执行校验失败/不可信的目标
        let target_ok = !Path::new(&target).exists() || entry_sha_ok(entry, &target);
        if !target_ok {
            return Err(f("Download failed: {0} (target: {1})", &[url, &target]));
        }
    }
    // zip 解压（可选，经 osmium-kit-unzip 插件）: 下载文件为 .zip 且 unzip=true 时解压到目标目录
    if entry.unzip.unwrap_or(config.download_unzip)
        && target.to_lowercase().ends_with(".zip")
    {
        write_log_entry(log_dir, "host", &f("Extracting zip: {0}", &[&target]), opts);
        let dest = Path::new(&target)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| deploy_dir.to_string());
        match run_plugin("unzip", &serde_json::json!({ "src": target, "dest": dest })) {
            Ok(()) => {}
            Err(e) => {
                if entry.fail_on_error.unwrap_or(config.download_fail_on_error) {
                    return Err(f("Unzip failed: {0}", &[&e]));
                }
                write_log_entry(log_dir, "host", &f("Unzip failed but fail_on_error=false: {0}", &[&e]), opts);
            }
        }
    }
    Ok(())
}

/// 部署相对路径解析: 绝对路径/根化路径（含 "\x" 这类仅根化的相对路径）原样返回，否则基于部署目录
fn resolve_deploy_path(raw: &str, deploy_dir: &str) -> String {
    if Path::new(raw).is_absolute() || raw.starts_with('\\') {
        raw.to_string()
    } else {
        format!("{}\\{}", deploy_dir, raw)
    }
}

/// 解析下载目标路径（等价 ResolveDownloadTarget）:
/// download_to 优先（相对基于部署目录），否则取 service_executable_path 的文件名
pub(crate) fn resolve_download_target(config: &crate::service_config::ServiceConfig, deploy_dir: &str) -> String {
    if let Some(to) = config.download_to.as_deref()
        && !to.trim().is_empty()
    {
        let p = Path::new(to);
        return if p.is_absolute() || to.starts_with('\\') {
            p.to_string_lossy().to_string()
        } else {
            Path::new(deploy_dir).join(to).to_string_lossy().to_string()
        };
    }
    let name = Path::new(&config.service_executable_path)
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "app.exe".to_string());
    Path::new(deploy_dir).join(name).to_string_lossy().to_string()
}

/// 去掉 URL 的 query/fragment/userinfo 部分（仅保留 scheme://host/path），
/// 防止带认证参数/内嵌凭据的地址进入日志
pub(crate) fn redact_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            u.set_query(None);
            u.set_fragment(None);
            u.set_username("").ok();
            u.set_password(None).ok();
            u.to_string()
        }
        Err(_) => url.to_string(),
    }
}

/// 下载失败类型（对应 TryDownload 中不同日志分支）
enum DownloadFail {
    /// 超时（对应 dl_timeout）
    Timeout,
    /// 已下载但 SHA-256 不匹配（对应 dl_sha_mismatch_downloaded，不再记 dl_error）
    ShaMismatch,
    /// 其他错误
    Other(String),
}

/// 文件最后修改时间 → HTTP 日期（RFC 1123, GMT），用于 If-Modified-Since 头
pub(crate) fn http_date_from_mtime(path: &str) -> Option<String> {
    let mtime = std::fs::metadata(path).ok()?.modified().ok()?;
    let dt: chrono::DateTime<chrono::Utc> = mtime.into();
    Some(dt.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
}

/// 从下载条目映射认证方式: basic（手动拼头）| 其他/未知方式 → 无认证；
/// sspi 不经此映射（try_download_entry 提前分流给 osmium-kit-sspi 插件）
pub(crate) fn download_auth_from_entry(entry: &crate::service_config::DownloadConfig) -> crate::service_core::DownloadAuth<'_> {
    if entry.auth.as_deref().map(|a| a.eq_ignore_ascii_case("basic")).unwrap_or(false) {
        crate::service_core::DownloadAuth::Basic(
            entry.username.as_deref().unwrap_or(""),
            entry.password.as_deref().unwrap_or(""))
    } else {
        crate::service_core::DownloadAuth::None
    }
}

/// 下载单条文件到目标路径并校验 SHA-256（等价 TryDownload）: 支持认证/代理/分块并行，
/// 目标已存在且未配置 sha 时发 If-Modified-Since（304 → 跳过）
fn try_download_entry(
    config: &crate::service_config::ServiceConfig,
    entry: &crate::service_config::DownloadConfig,
    url: &str,
    target: &str,
    log_dir: &str,
    opts: &LogOptions,
) -> bool {
    // 记录去敏 URL（去掉 query 参数），避免带认证 token 的下载地址进入日志
    write_log_entry(log_dir, "host", &f("Downloading {0} -> {1}", &[&redact_url(url), target]), opts);
    let tmp = format!("{}.download.tmp", target);
    let parent = Path::new(target)
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."));
    let _ = std::fs::create_dir_all(&parent);

    // sspi 认证下载经 osmium-kit-sspi 插件完成: 插件完成 401 挑战-响应循环并原子落盘，
    // 宿主侧随后做 sha 校验（插件缺失/失败按 fail_on_error 由调用方处理）
    if entry.auth.as_deref().map(|a| a.eq_ignore_ascii_case("sspi")).unwrap_or(false) {
        return sspi_download_via_plugin(entry, url, target, log_dir, opts);
    }

    // 认证与代理（条目级，缺省已回退配置级）: basic 手动拼头
    let auth = download_auth_from_entry(entry);
    let proxy = entry.proxy.as_deref();
    // If-Modified-Since/304: 目标已存在且未配置 sha 时发送（服务器回 304 → 视为已最新，保留原文件）
    let sha_empty = entry.sha256.as_deref().map(|s| s.trim().is_empty()).unwrap_or(true);
    let if_modified_since = if sha_empty { http_date_from_mtime(target) } else { None };

    let result: Result<(), DownloadFail> = (|| {
        // 分块并行下载（CreateNew 原子创建，TOCTOU 防护）；download_threads 0/1 禁用多线程
        crate::service_core::download_core(url, &tmp, DOWNLOAD_TIMEOUT_SECS,
            auth, proxy, config.download_threads, if_modified_since)
            .map_err(|(timeout, e)| if timeout { DownloadFail::Timeout } else { DownloadFail::Other(e) })?;
        // 304 未修改: download_core 已删除 tmp，目标文件保持原样
        if !Path::new(&tmp).exists() {
            write_log_entry(log_dir, "host", &f("Download not modified (304), keeping: {0}", &[target]), opts);
            return Ok(());
        }
        if let Some(sha) = entry.sha256.as_deref()
            && !sha.trim().is_empty()
            && !crate::service_core::sha256_matches(&tmp, Some(sha))
        {
            write_log_entry(log_dir, "host", "Downloaded file SHA-256 mismatch, discarding", opts);
            let _ = std::fs::remove_file(&tmp);
            return Err(DownloadFail::ShaMismatch);
        }

        let _ = std::fs::remove_file(target); // File.Move 覆盖语义
        std::fs::rename(&tmp, target).map_err(|e| DownloadFail::Other(e.to_string()))?;
        write_log_entry(log_dir, "host", &f("Download complete: {0}", &[target]), opts);
        Ok(())
    })();

    match result {
        Ok(()) => true,
        Err(DownloadFail::Timeout) => {
            let _ = std::fs::remove_file(&tmp);
            write_log_entry(log_dir, "host",
                &f("Download timed out after {0}s", &[&DOWNLOAD_TIMEOUT_SECS.to_string()]), opts);
            false
        }
        Err(DownloadFail::ShaMismatch) => false, // 已记录，不重复记 dl_error
        Err(DownloadFail::Other(e)) => {
            let _ = std::fs::remove_file(&tmp);
            write_log_entry(log_dir, "host", &f("Download error: {0}", &[&e]), opts);
            false
        }
    }
}

/// sspi 认证下载（经 osmium-kit-sspi 插件）: 插件完成 401 挑战-响应循环并原子落盘，
/// 宿主侧随后做 sha 校验（失败仅告警，由调用方决定是否阻断）
fn sspi_download_via_plugin(
    entry: &crate::service_config::DownloadConfig,
    url: &str,
    target: &str,
    log_dir: &str,
    opts: &LogOptions,
) -> bool {
    let mut payload = serde_json::json!({
        "url": url,
        "to": target,
        "timeout_secs": DOWNLOAD_TIMEOUT_SECS,
    });
    if let Some(obj) = payload.as_object_mut() {
        if let Some(u) = entry.username.as_deref()
            && !u.trim().is_empty()
        {
            obj.insert("username".into(), serde_json::Value::String(u.to_string()));
            obj.insert("password".into(), serde_json::Value::String(entry.password.clone().unwrap_or_default()));
        }
        if let Some(p) = entry.proxy.as_deref() {
            obj.insert("proxy".into(), serde_json::Value::String(p.to_string()));
        }
    }
    match run_plugin("sspi", &payload) {
        Ok(()) => {
            // 插件原子落盘后，宿主侧补 sha 校验（防插件行为异常/被替换）
            if let Some(sha) = entry.sha256.as_deref()
                && !sha.trim().is_empty()
                && !crate::service_core::sha256_matches(target, Some(sha))
            {
                write_log_entry(log_dir, "host", "Downloaded file SHA-256 mismatch (via plugin), discarding", opts);
                let _ = std::fs::remove_file(target);
                return false;
            }
            write_log_entry(log_dir, "host", &f("Download complete (via sspi plugin): {0}", &[target]), opts);
            true
        }
        Err(e) => {
            write_log_entry(log_dir, "host", &f("SSPI download error: {0}", &[&e]), opts);
            false
        }
    }
}

// ==================== 插件调用（exe 同级 .osx） ====================

/// 插件根目录: exe 所在目录（独立部署/平台安装通用——不强制 exts 子目录）
fn plugin_dir() -> PathBuf {
    Path::new(&crate::service_core::get_own_path())
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 插件文件是否可信: 信任锚点 = 宿主 exe 自身位置——
/// 宿主位于用户可写目录（inplace 集成部署 / 开发者目录）时，插件与 exe 同级或在其子目录，
/// 攻击面与宿主一致（能替换插件的攻击者同样能替换 exe），不额外增加风险，直接放行；
/// 宿主位于受保护位置（如 Program Files）时，严格要求插件所在目录及插件文件
/// 仅 SYSTEM/Administrators 可写（对齐 secure_directory 语义，防任意登录用户
/// 替换插件获得 SYSTEM 提权，P0）。校验失败返回错误原因，调用方拒绝执行该插件
fn plugin_path_trusted(path: &Path) -> Result<(), String> {
    let host_dir = Path::new(&crate::service_core::get_own_path())
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    if crate::service_core::is_user_writable(&host_dir) {
        return Ok(());
    }
    // 受保护位置: 插件目录/文件 ACL 仅 SYSTEM/Admin 可写（Authenticated Users 有 M 即不可信）
    let dir = path.parent().unwrap_or(Path::new("."));
    if crate::service_core::is_user_writable(&dir.to_string_lossy()) {
        return Err(f("plugin directory '{0}' is writable by unprivileged users, refusing to execute", &[&dir.to_string_lossy()]));
    }
    if crate::service_core::is_user_writable(&path.to_string_lossy()) {
        return Err(f("plugin file '{0}' is writable by unprivileged users, refusing to execute", &[&path.to_string_lossy()]));
    }
    Ok(())
}

/// 递归扫描 exe 同级目录下所有 .osx 插件（跳过名称以 . 开头的隐藏目录，防混入）
pub(crate) fn discover_plugins() -> Vec<PathBuf> {
    let mut out = Vec::new();
    scan_plugin_dir(&plugin_dir(), &mut out);
    out
}

fn scan_plugin_dir(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let ft = match entry.file_type() {
            Ok(ft) => ft,
            Err(_) => continue,
        };
        if ft.is_dir() {
            scan_plugin_dir(&path, out);
        } else if ft.is_file()
            && path.extension().map(|e| e.eq_ignore_ascii_case("osx")).unwrap_or(false)
        {
            out.push(path);
        }
    }
}

/// 插件可用性: 协议探测——喂 {"kit":"ping"} 并验证响应 ok=true；
/// ACL 不可信（任意用户可写）或协议不响应均视为不可用（P0 防提权）
pub(crate) fn plugin_usable(path: &Path) -> bool {
    // 信任校验: 插件目录/文件被非管理员可写 → 不可用（防恶意插件替换提权）
    if plugin_path_trusted(path).is_err() {
        return false;
    }
    invoke_plugin(path, "ping", &serde_json::json!({})).is_ok()
}

/// 运行插件: 遍历 exe 同级发现的全部 .osx，喂 stdin JSON（kit 字段分发），首个 ok=true 即成功；
/// 全部失败返回最后一个错误；ACL 不可信的插件（任意用户可写）跳过执行并告警（P0 防提权）
pub(crate) fn run_plugin(kit: &str, payload: &serde_json::Value) -> Result<(), String> {
    let plugins = discover_plugins();
    if plugins.is_empty() {
        return Err(f("plugin '{0}' not found (no .osx plugin next to the executable)", &[kit]));
    }
    let mut last_err = String::from("no plugin responded ok");
    for plugin in &plugins {
        // 信任校验: 插件目录/文件被非管理员可写 → 拒绝执行（防恶意插件替换提权）
        if let Err(reason) = plugin_path_trusted(plugin) {
            last_err = f("plugin '{0}' skipped: {1}", &[&plugin.display().to_string(), &reason]);
            continue;
        }
        match invoke_plugin(plugin, kit, payload) {
            Ok(()) => return Ok(()),
            Err(e) => last_err = e,
        }
    }
    Err(last_err)
}

/// 单次插件调用: spawn → stdin JSON → stdout JSON 响应解析（非零退出码视为失败）；
/// 整体超时 5 秒防恶意/损坏插件挂死宿主（SCM 停止/启动流程不能被插件阻塞）
fn invoke_plugin(plugin: &Path, kit: &str, payload: &serde_json::Value) -> Result<(), String> {
    use std::io::{Read, Write};

    let mut req = payload.clone();
    if let Some(obj) = req.as_object_mut() {
        obj.insert("kit".into(), serde_json::Value::String(kit.to_string()));
    }
    let input = req.to_string();

    let mut child = Command::new(plugin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .creation_flags(0x08000000)
        .spawn()
        .map_err(|e| f("plugin '{0}' failed to start: {1}", &[&plugin.display().to_string(), &e.to_string()]))?;
    child.stdin.take().unwrap().write_all(input.as_bytes())
        .map_err(|e| e.to_string())?;
    drop(child.stdin.take());
    let mut stdout = child.stdout.take().unwrap();
    // 子线程读 stdout 到 EOF；主线程限时等待，超时强杀插件（防挂死宿主）
    let reader = thread::spawn(move || {
        let mut out = String::new();
        let _ = stdout.read_to_string(&mut out);
        out
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    let out = loop {
        // 插件已退出且 stdout 已排空 → 读取完成
        if reader.is_finished() {
            break reader.join().unwrap_or_default();
        }
        if Instant::now() >= deadline {
            // 超时: 强杀插件（stdout 未读完时 read_to_string 会随句柄关闭返回）
            let _ = child.kill();
            break reader.join().unwrap_or_default();
        }
        thread::sleep(Duration::from_millis(50));
    };
    let code = child.wait().map_err(|e| e.to_string())?.code().unwrap_or(-1);
    if code != 0 {
        return Err(f("plugin '{0}' exited with code {1}", &[&plugin.display().to_string(), &code.to_string()]));
    }
    if out.trim().is_empty() {
        return Err(f("plugin '{0}' did not respond within 5s", &[&plugin.display().to_string()]));
    }
    let resp: serde_json::Value = serde_json::from_str(out.trim())
        .map_err(|e| f("plugin '{0}' returned invalid JSON: {1}", &[&plugin.display().to_string(), &e.to_string()]))?;
    if resp.get("ok").and_then(|v| v.as_bool()).unwrap_or(false) {
        Ok(())
    } else {
        let msg = resp.get("error").and_then(|v| v.as_str()).unwrap_or("unknown plugin error");
        Err(f("plugin '{0}' failed: {1}", &[&plugin.display().to_string(), msg]))
    }
}

// ==================== 日志底层写入 ====================

/// 串行化日志文件写入（宿主专用，与 service_core 的锁相互独立）
static LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 日志文件名字符安全校验: 仅允许字母数字、% 与 -_.（chrono 格式如 %Y%m%d；
/// 防止日期模式注入路径分隔符穿越日志目录）
pub(crate) fn log_pattern_safe(pattern: &str) -> bool {
    pattern.is_empty()
        || pattern.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '%'))
}

/// 当前日志文件名（主日志 / err 分离文件）；自定义 out/err 文件名优先（对应 WinSW outFileStdout/outFileStderr）
pub(crate) fn current_log_name(opts: &LogOptions, channel: &str, now: &chrono::DateTime<chrono::Local>) -> String {
    // err 文件名仅在分流（split_out_err）时生效，未分流时 err 通道写入主日志
    if channel == "err" && opts.split_out_err {
        if !opts.err_filename.is_empty() {
            return opts.err_filename.clone();
        }
        return format!("{}.err.log", format_log_date(opts, now));
    }
    if !opts.out_filename.is_empty() {
        return opts.out_filename.clone();
    }
    format!("{}.log", format_log_date(opts, now))
}

/// 按 log_pattern 格式化日志日期（空 pattern 回退 %Y-%m-%d）
fn format_log_date(opts: &LogOptions, now: &chrono::DateTime<chrono::Local>) -> String {
    if opts.pattern.is_empty() {
        now.format("%Y-%m-%d").to_string()
    } else {
        now.format(&opts.pattern).to_string()
    }
}

/// 自定义日志文件名安全校验: 仅允许字母数字与 -_.（与 log_pattern_safe 同款，防路径穿越）
fn safe_log_name(name: Option<&str>) -> String {
    name.map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')))
        .unwrap_or_default()
}

/// log reset: 清空当日主日志与 err 分离文件（对应 WinSW log mode=reset）
pub(crate) fn reset_current_logs(log_dir: &str, opts: &LogOptions) {
    let now = chrono::Local::now();
    for channel in ["host", "err"] {
        let name = current_log_name(opts, channel, &now);
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(Path::new(log_dir).join(&name));
    }
}

/// 应用 WinSW log mode 语义（映射到等价配置项）: append 不改动 | reset 清空 | none 关闭 |
/// roll 滚到 .old | roll-by-size（缺省 10MB）| roll-by-time（缺省 1 天）| roll-by-size-time（两者）
pub(crate) fn apply_log_mode(mode: Option<&str>, log_enabled: &mut bool, opts: &mut LogOptions) {
    match mode.map(str::trim).unwrap_or("") {
        "none" => *log_enabled = false,
        "reset" => opts.reset = true,
        "roll" => opts.roll_at_start = true,
        "roll-by-size" | "roll_by_size" => {
            if opts.max_size_mb <= 0 {
                opts.max_size_mb = 10; // 与 WinSW sizeThreshold 缺省一致
            }
        }
        "roll-by-time" | "roll_by_time" => {
            if opts.roll_period_days <= 0 {
                opts.roll_period_days = 1;
            }
        }
        "roll-by-size-time" | "roll_by_size_time" => {
            if opts.max_size_mb <= 0 {
                opts.max_size_mb = 10;
            }
            if opts.roll_period_days <= 0 {
                opts.roll_period_days = 1;
            }
        }
        _ => {}
    }
}

/// 启动滚动（mode=roll）: 把当前日志（含 err 分离文件）改名为 {name}.old（覆盖旧 .old）
pub(crate) fn roll_logs_to_old(log_dir: &str, opts: &LogOptions) {
    let now = chrono::Local::now();
    for channel in ["host", "err"] {
        let name = current_log_name(opts, channel, &now);
        let src = Path::new(log_dir).join(&name);
        if std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0) == 0 {
            continue;
        }
        let dst = Path::new(log_dir).join(format!("{}.old", name));
        let _ = std::fs::remove_file(&dst); // 覆盖语义（WinSW roll 直接滚到 *.old）
        let _ = std::fs::rename(&src, &dst);
    }
}

/// 按天周期滚动（roll-by-time）: 日志文件最后修改日期距今 >= N 天时改名归档（对应 WinSW period）
pub(crate) fn roll_by_time_if_due(log_dir: &str, opts: &LogOptions, now: &chrono::DateTime<chrono::Local>) {
    let days = opts.roll_period_days;
    if days <= 0 {
        return;
    }
    let date = if opts.pattern.is_empty() {
        now.format("%Y-%m-%d").to_string()
    } else {
        now.format(&opts.pattern).to_string()
    };
    let stamp = now.format("%H%M%S").to_string();
    let cutoff = now.date_naive() - chrono::Duration::days(days);
    for suffix in [".log", ".err.log"] {
        let src = Path::new(log_dir).join(format!("{}{}", date, suffix));
        let due = std::fs::metadata(&src)
            .and_then(|m| m.modified())
            .map(|t| {
                let d: chrono::DateTime<chrono::Local> = t.into();
                d.date_naive() <= cutoff
            })
            .unwrap_or(false);
        if due {
            let dst = Path::new(log_dir).join(format!("{}.{}{}", date, stamp, suffix));
            let _ = std::fs::rename(&src, &dst);
        }
    }
}

/// 已定点滚动的日期（防同日重复滚动）
static LAST_AUTO_ROLL: Mutex<Option<String>> = Mutex::new(None);

/// 重置定点滚动状态（仅测试隔离使用，不进入生产二进制）
#[cfg(test)]
pub(crate) fn reset_auto_roll_state() {
    *LAST_AUTO_ROLL.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 每天定点滚动: 到达 auto_roll_at 时刻且当日尚未滚动时，把当日日志改名为 {pattern}.{HHmmss} 归档
pub(crate) fn auto_roll_logs(log_dir: &str, opts: &LogOptions, now: &chrono::DateTime<chrono::Local>) {
    let Some(at) = opts.auto_roll_at.as_deref() else { return };
    let today = now.format("%Y-%m-%d").to_string();
    let now_time = now.format("%H:%M:%S").to_string();
    if now_time.as_str() < at {
        return;
    }
    let mut last = LAST_AUTO_ROLL.lock().unwrap_or_else(|e| e.into_inner());
    if last.as_deref() == Some(today.as_str()) {
        return; // 当日已滚动
    }
    let date = if opts.pattern.is_empty() {
        now.format("%Y-%m-%d").to_string()
    } else {
        now.format(&opts.pattern).to_string()
    };
    let stamp = now.format("%H%M%S").to_string();
    for suffix in [".log", ".err.log"] {
        let src = Path::new(log_dir).join(format!("{}{}", date, suffix));
        if std::fs::metadata(&src).map(|m| m.len()).unwrap_or(0) > 0 {
            let dst = Path::new(log_dir).join(format!("{}.{}{}", date, stamp, suffix));
            let _ = std::fs::rename(&src, &dst);
        }
    }
    *last = Some(today);
}

/// 写入一条日志（含 stderr 分流文件名与滚动参数）；log_dir 为空表示禁用（空串判定）
pub(crate) fn write_log_entry(log_dir: &str, channel: &str, message: &str, opts: &LogOptions) {
    if log_dir.is_empty() {
        return;
    }
    let now = chrono::Local::now();
    let log_file = Path::new(log_dir).join(current_log_name(opts, channel, &now));
    // 子进程 out/err 已按行分隔，无需转义；其余条目（钩子命令/URL/错误等）转义控制字符，
    // 防止伪造日志条目（对应 WinSW #924 日志注入）
    let text = if channel == "out" || channel == "err" {
        message.to_string()
    } else {
        escape_invisible(message)
    };
    let entry = format!("[{}] [{}] {}\r\n", now.format("%Y-%m-%d %H:%M:%S"), channel, text);
    let _guard = LOG_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    // 定点滚动/按天滚动/大小滚动与写入串行化: 避免并发 rename 撞上正在追加的句柄
    //（对应 WinSW #894/#1016/#1088 日志滚动崩溃/静默失败类问题）
    auto_roll_logs(log_dir, opts, &now);
    roll_by_time_if_due(log_dir, opts, &now);
    roll_if_needed(&log_file, opts.max_size_mb, opts.backup_count, opts.zip_backup, &opts.zip_date_format);
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .map(|mut f| {
            let _ = f.write_all(entry.as_bytes());
        });
}

/// 日志大小滚动: 超过 max_size_mb 按 .1/.2 后缀顺延，保留 backup_count 份；
/// zip_backup=true 时被淘汰的最旧备份先压成 .zip 归档再删除，zip_date_format 带格式化日期
pub(crate) fn roll_if_needed(log_file: &Path, max_size_mb: i64, backup_count: i32, zip_backup: bool, zip_date_format: &str) {
    if max_size_mb <= 0 || backup_count <= 0 {
        return;
    }
    let len = std::fs::metadata(log_file).map(|m| m.len()).unwrap_or(0);
    if len < (max_size_mb as u64) * 1024 * 1024 {
        return;
    }

    let oldest = PathBuf::from(format!("{}.{}", log_file.display(), backup_count));
    if zip_backup && oldest.exists() {
        let _ = zip_backup_file(&oldest, zip_date_format);
    }
    let _ = std::fs::remove_file(&oldest);
    for i in (1..backup_count).rev() {
        let src = PathBuf::from(format!("{}.{}", log_file.display(), i));
        if src.exists() {
            let dst = PathBuf::from(format!("{}.{}", log_file.display(), i + 1));
            let _ = std::fs::remove_file(&dst); // File.Move(overwrite:true) 语义
            let _ = std::fs::rename(&src, &dst);
        }
    }
    let first = PathBuf::from(format!("{}.1", log_file.display()));
    let _ = std::fs::remove_file(&first);
    let _ = std::fs::rename(log_file, &first);
}

/// 将日志备份压缩为 .zip 归档（deflate），成功后返回 true；不删除原文件（删除由调用方决定）。
/// zip_date_format 非空时生成 {file}.{格式日期}.zip（对应 WinSW zipDateFormat），空则保持 {file}.zip
pub(crate) fn zip_backup_file(file: &Path, zip_date_format: &str) -> bool {
    use std::io::Write;
    let Ok(data) = std::fs::read(file) else { return false };
    let zip_path = if zip_date_format.is_empty() {
        format!("{}.zip", file.display())
    } else {
        format!("{}.{}.zip", file.display(), chrono::Local::now().format(zip_date_format))
    };
    let Ok(f) = std::fs::File::create(&zip_path) else { return false };
    let mut zw = zip::ZipWriter::new(f);
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated);
    let name = file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "log".into());
    if zw.start_file(name, options).is_err() { return false; }
    if zw.write_all(&data).is_err() { return false; }
    zw.finish().is_ok()
}

/// 转义不可见/控制字符为可见序列（\r \n \t \x..），用于错误信息与日志（对应 WinSW #462/#1337）
pub(crate) fn escape_invisible(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\r' => out.push_str("\\r"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c as u32 == 0x7f => out.push_str(&format!("\\x{:02X}", c as u32)),
            _ => out.push(c),
        }
    }
    out
}

// ==================== Win32 回调 / 工具 ====================

/// 标记 WM_CLOSE 是否已实际发送给目标进程的窗口
static WM_CLOSE_SENT: AtomicBool = AtomicBool::new(false);

/// 吞掉 Ctrl+C，防止宿主在向子进程控制台广播时被误杀（等价 CtrlHandler）
unsafe extern "system" fn ignore_ctrl_c(_ctrl_type: u32) -> BOOL {
    BOOL(1)
}

/// 枚举窗口回调: 向属于目标 PID 的顶层窗口发送 WM_CLOSE
unsafe extern "system" fn send_wm_close(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let target_pid = lparam.0 as u32;
    let mut win_pid: u32 = 0;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut win_pid));
        if win_pid == target_pid {
            let _ = PostMessageW(Some(hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
            WM_CLOSE_SENT.store(true, Ordering::SeqCst);
        }
    }
    BOOL(1)
}

/// 终止 pid 的所有后代进程（基于 Toolhelp 快照），等价于 Process.Kill(entireProcessTree) 的子树部分
fn terminate_pid_tree(root_pid: u32) {
    for desc_pid in collect_descendants(root_pid) {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, desc_pid) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }
}

/// 进程是否存活（OpenProcess + GetExitCodeProcess == STILL_ACTIVE）
pub(crate) fn process_alive(pid: u32) -> bool {
    use windows::Win32::Foundation::STILL_ACTIVE;
    use windows::Win32::System::Threading::{GetExitCodeProcess, PROCESS_QUERY_LIMITED_INFORMATION};
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return false };
        let mut code: u32 = 0;
        let ok = GetExitCodeProcess(h, &mut code).is_ok() && code == STILL_ACTIVE.0 as u32;
        let _ = CloseHandle(h);
        ok
    }
}

/// 读取目标进程的环境变量（PEB → RTL_USER_PROCESS_PARAMETERS → Environment，x64 布局）。
/// 用于 RunawayProcessKiller 防误杀校验（对齐 WinSW #237: 校验 WINSGF_SERVICE_ID 防 PID 复用误杀）
pub(crate) fn process_env_var(pid: u32, name: &str) -> Option<String> {
    // ProcessBasicInformation 结构（与 PROCESS_BASIC_INFORMATION 同布局，x64）
    #[repr(C)]
    struct BasicInfo {
        exit_status: u32,
        peb_base: *mut core::ffi::c_void,
        affinity: usize,
        base_priority: i32,
        unique_pid: usize,
        inherited_pid: usize,
    }
    use windows::Wdk::System::Threading::{NtQueryInformationProcess, PROCESSINFOCLASS};
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut info = BasicInfo { exit_status: 0, peb_base: std::ptr::null_mut(), affinity: 0, base_priority: 0, unique_pid: 0, inherited_pid: 0 };
        let mut ret = 0u32;
        let status = NtQueryInformationProcess(
            handle,
            PROCESSINFOCLASS(0), // ProcessBasicInformation
            &mut info as *mut _ as *mut core::ffi::c_void,
            size_of::<BasicInfo>() as u32,
            &mut ret,
        );
        let _ = CloseHandle(handle);
        if status.0 != 0 || info.peb_base.is_null() {
            return None;
        }
        // x64 PEB: ProcessParameters 位于 0x20；Windows 10+ 的 RTL_USER_PROCESS_PARAMETERS 中
        // CommandLine@0x68 / Environment@0x80（PVOID，环境块以双 null 结尾）
        let peb = info.peb_base as usize;
        let mut params_ptr: usize = 0;
        if !read_process_memory(pid, peb + 0x20, &mut params_ptr as *mut _ as *mut u8, 8) {
            return None;
        }
        let mut env_ptr: usize = 0;
        if !read_process_memory(pid, params_ptr + 0x80, &mut env_ptr as *mut _ as *mut u8, 8)
            || env_ptr == 0
        {
            return None;
        }
        // 逐块读取直到双 null 结尾（上限 256KB 防失控）
        let mut buf = Vec::with_capacity(4096);
        let mut off = 0usize;
        loop {
            let mut chunk = [0u8; 4096];
            let n = read_process_memory_partial(pid, env_ptr + off, chunk.as_mut_ptr(), 4096);
            if n == 0 {
                return None;
            }
            buf.extend_from_slice(&chunk[..n]);
            off += n;
            if buf.len() >= 4 && buf[buf.len() - 4..].iter().all(|&b| b == 0) {
                break;
            }
            if buf.len() > 256 * 1024 {
                return None; // 环境块异常，防无限读取
            }
        }
        // 环境块: NAME=VALUE\0...\0\0；按 \0 拆分 UTF-16 条目，变量名大小写不敏感匹配
        let mut item = Vec::new();
        for chunk in buf.as_chunks::<2>().0 {
            let u = u16::from_le_bytes(*chunk);
            if u == 0 {
                let s = String::from_utf16_lossy(&item);
                if let Some((key, value)) = s.split_once('=')
                    && key.eq_ignore_ascii_case(name)
                {
                    return Some(value.to_string());
                }
                item.clear();
            } else {
                item.push(u);
            }
        }
        None
    }
}

/// ReadProcessMemory 封装: 读取目标进程指定地址 len 字节，要求读满（权限不足/越界返回 false）
fn read_process_memory(pid: u32, addr: usize, buf: *mut u8, len: usize) -> bool {
    read_process_memory_partial(pid, addr, buf, len) == len
}

/// ReadProcessMemory 封装: 读取目标进程指定地址，返回实际读取字节数（失败返回 0）
fn read_process_memory_partial(pid: u32, addr: usize, buf: *mut u8, len: usize) -> usize {
    use windows::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows::Win32::System::Threading::PROCESS_VM_READ;
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ, false, pid) else { return 0 };
        let mut read = 0usize;
        let _ = ReadProcessMemory(h, addr as *const core::ffi::c_void, buf as *mut core::ffi::c_void, len, Some(&mut read));
        let _ = CloseHandle(h);
        read
    }
}

/// RunawayProcessKiller 启动清理: 读取 pid 文件终止残留进程树；expected_service_id 提供时
/// 先校验残留进程 WINSGF_SERVICE_ID（防 PID 复用误杀，对齐 WinSW #237），不匹配跳过并告警
pub(crate) fn runaway_cleanup_pid_file(path: &str, stop_timeout_ms: u64, parent_first: bool, expected_service_id: Option<&str>) -> Result<Option<u32>, String> {
    let Ok(content) = std::fs::read_to_string(path) else { return Ok(None) };
    let pid = content.trim().parse::<u32>().map_err(|_| format!("invalid pid file '{0}'", path))?;
    if pid == 0 || pid == std::process::id() || !process_alive(pid) {
        return Ok(None);
    }
    // 防误杀（对应 WinSW #237）: PID 可能已被系统复用给无关进程，须校验服务标识环境变量
    if let Some(expected) = expected_service_id {
        let matched = process_env_var(pid, "WINSGF_SERVICE_ID")
            .map(|actual| actual.eq_ignore_ascii_case(expected))
            .unwrap_or(false);
        if !matched {
            return Err(format!("RunawayProcessKiller: PID {0} does not belong to this service (WINSGF_SERVICE_ID mismatch), skipping", pid));
        }
    }
    // 先杀子树，再处理父进程（parent_first 时先父后子）
    let mut to_kill = collect_descendants(pid);
    if parent_first {
        to_kill.push(pid);
    }
    unsafe {
        for p in &to_kill {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, *p) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
        if !parent_first
            && let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, pid) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
    }
    // 等待进程退出（stop_timeout_ms），超时忽略（强杀已完成，仅等句柄回收）
    let deadline = Instant::now() + Duration::from_millis(stop_timeout_ms.max(100));
    while Instant::now() < deadline && process_alive(pid) {
        thread::sleep(Duration::from_millis(50));
    }
    Ok(Some(pid))
}

/// RunawayProcessKiller 判定: 任一超限即 true（内存工作集 MB / CPU 采样百分比）
pub(crate) fn runaway_exceeded(
    ws_mb: Option<u64>, mem_limit_mb: Option<u64>,
    cpu_pct: Option<f64>, cpu_limit: Option<f64>,
) -> bool {
    (mem_limit_mb.is_some_and(|l| ws_mb.is_some_and(|w| w > l)))
        || (cpu_limit.is_some_and(|l| cpu_pct.is_some_and(|c| c > l)))
}

/// 收集 pid 的所有后代进程 ID（BFS，基于 Toolhelp 快照）
pub(crate) fn collect_descendants(root_pid: u32) -> Vec<u32> {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return vec![];
        };

        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);

        let mut result = Vec::new();
        let mut queue = std::collections::VecDeque::from([root_pid]);
        while let Some(pid) = queue.pop_front() {
            for &(child_pid, parent_pid) in &pairs {
                if parent_pid == pid && child_pid != pid {
                    result.push(child_pid);
                    queue.push_back(child_pid);
                }
            }
        }
        result
    }
}

// ==================== 进程采样 / 进程优先级 / 环境展开 / 事件日志 / 自定义停止 ====================

/// 设置进程省电节流（ProcessPowerThrottling，Win10 1709+）: enabled=true 开启
/// 执行速度节流、false 关闭；失败静默返回 false（旧系统/无权限）
pub(crate) fn set_eco_qos(pid: u32, enabled: bool) -> bool {
    use windows::Win32::System::Threading::{
        OpenProcess, SetProcessInformation, PROCESS_INFORMATION_CLASS, PROCESS_SET_INFORMATION,
    };
    // PROCESS_POWER_THROTTLING_STATE: Version=1, ControlMask/StateMask=EXECUTION_SPEED(1)
    #[repr(C)]
    struct PowerThrottling {
        version: u32,
        control_mask: u32,
        state_mask: u32,
    }
    unsafe {
        let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) else { return false };
        let st = PowerThrottling {
            version: 1,
            control_mask: 1,
            state_mask: if enabled { 1 } else { 0 },
        };
        let ok = SetProcessInformation(
            h,
            PROCESS_INFORMATION_CLASS(4),
            &st as *const _ as *const _,
            size_of::<PowerThrottling>() as u32,
        )
        .is_ok();
        let _ = CloseHandle(h);
        ok
    }
}
/// 枚举全部进程 PID（Toolhelp 快照）
fn all_process_ids() -> Vec<u32> {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return vec![];
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut out = Vec::new();
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                out.push(entry.th32ProcessID);
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        out
    }
}

/// 启用 SeDebugPrivilege（管理员默认持有但禁用）: 供 kill 终止 SYSTEM 级服务子进程
fn enable_debug_privilege() {
    use windows::Win32::Foundation::{HANDLE, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_DEBUG_NAME,
        SE_PRIVILEGE_ENABLED, TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY, &mut token).is_err() {
            return;
        }
        let mut luid = LUID::default();
        if LookupPrivilegeValueW(PCWSTR::null(), SE_DEBUG_NAME, &mut luid).is_err() {
            let _ = CloseHandle(token);
            return;
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES { Luid: luid, Attributes: SE_PRIVILEGE_ENABLED }],
        };
        let _ = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        let _ = CloseHandle(token);
    }
}

/// 管理员/开发者工具（对应 WinSW dev kill）: 按 WINSGF_SERVICE_ID 定位并强制终止
/// 某服务的目标子进程（整棵进程树）。返回终止的进程数；服务未运行返回 0。
/// 需管理员权限，必要时启用 SeDebugPrivilege 以终止 SYSTEM 级进程
pub(crate) fn kill_service_processes(service_id: &str) -> Result<u32, String> {
    enable_debug_privilege();
    let mut killed = 0u32;
    let mut errors = Vec::new();
    for pid in all_process_ids() {
        let matched = process_env_var(pid, "WINSGF_SERVICE_ID")
            .map(|v| v.eq_ignore_ascii_case(service_id))
            .unwrap_or(false);
        if !matched {
            continue;
        }
        unsafe {
            // 先杀子树再杀自身（与 runaway_cleanup_pid_file 顺序一致）
            for desc in collect_descendants(pid) {
                if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, desc) {
                    let _ = TerminateProcess(h, 1);
                    let _ = CloseHandle(h);
                }
            }
            match OpenProcess(PROCESS_TERMINATE, false, pid) {
                Ok(h) => {
                    if TerminateProcess(h, 1).is_err() {
                        let _ = CloseHandle(h);
                        errors.push(format!("PID {pid} refused to terminate"));
                    } else {
                        let _ = CloseHandle(h);
                        killed += 1;
                    }
                }
                Err(_) => errors.push(format!("PID {pid} is not accessible (run as administrator)")),
            }
        }
    }
    if killed == 0 && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(killed)
}

/// 进程工作集内存（MB），失败返回 None（RunawayProcessKiller 采样用）
pub(crate) fn process_working_set_mb(pid: u32) -> Option<u64> {
    unsafe {
        use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return None };
        let mut pmc = PROCESS_MEMORY_COUNTERS::default();
        let r = GetProcessMemoryInfo(h, &mut pmc, size_of::<PROCESS_MEMORY_COUNTERS>() as u32);
        let _ = CloseHandle(h);
        r.is_ok().then_some((pmc.WorkingSetSize / 1024 / 1024) as u64)
    }
}

/// 进程内核+用户 CPU 时间（100ns 单位），失败返回 None（RunawayProcessKiller 采样用）
pub(crate) fn process_cpu_100ns(pid: u32) -> Option<u64> {
    unsafe {
        use windows::Win32::Foundation::FILETIME;
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else { return None };
        let (mut ct, mut et, mut kt, mut ut) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
        let r = GetProcessTimes(h, &mut ct, &mut et, &mut kt, &mut ut);
        let _ = CloseHandle(h);
        r.is_ok().then(|| {
            let k = (kt.dwHighDateTime as u64) << 32 | kt.dwLowDateTime as u64;
            let u = (ut.dwHighDateTime as u64) << 32 | ut.dwLowDateTime as u64;
            k + u
        })
    }
}

/// 设置目标进程优先级（对应 WinSW priority）；未知值忽略（保持默认）
pub(crate) fn set_process_priority(pid: u32, priority: Option<&str>) {
    let class = match priority.map(|s| s.to_lowercase()).as_deref() {
        Some("idle") => IDLE_PRIORITY_CLASS,
        Some("belownormal") => BELOW_NORMAL_PRIORITY_CLASS,
        Some("abovenormal") => ABOVE_NORMAL_PRIORITY_CLASS,
        Some("high") => HIGH_PRIORITY_CLASS,
        Some("realtime") => REALTIME_PRIORITY_CLASS,
        _ => return, // normal / 未配置 → 保持默认
    };
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SET_INFORMATION, false, pid) {
            let _ = SetPriorityClass(h, class);
            let _ = CloseHandle(h);
        }
    }
}

/// 展开环境变量引用 %NAME%（未定义展开为空串），%BASE% 特指部署目录（按字符迭代，兼容中文）
pub(crate) fn expand_env_value(value: &str, base: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            let mut end = i + 1;
            while end < chars.len() && chars[end] != '%' {
                end += 1;
            }
            if end < chars.len() {
                let name: String = chars[i + 1..end].iter().collect();
                // %PID% 占位符保留原样，停止命令执行时才替换为子进程 PID（对应 WinSW #217）
                if name.eq_ignore_ascii_case("PID") {
                    out.extend(chars[i..=end].iter());
                    i = end + 1;
                    continue;
                }
                let replacement = if name.eq_ignore_ascii_case("BASE") {
                    base.to_string()
                } else {
                    std::env::var(&name).unwrap_or_default()
                };
                out.push_str(&replacement);
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 写 Windows 事件日志（信息级别，来源名 Osmium）
fn report_event_log(message: &str) {
    unsafe {
        let source = crate::service_core::to_wide("Osmium");
        if let Ok(h) = RegisterEventSourceW(PCWSTR::null(), PCWSTR::from_raw(source.as_ptr())) {
            let wide = crate::service_core::to_wide(message);
            let strings = [PCWSTR::from_raw(wide.as_ptr())];
            let _ = ReportEventW(h, EVENTLOG_INFORMATION_TYPE, 0, 0, None, 0,
                Some(&strings), None);
            let _ = DeregisterEventSource(h);
        }
    }
}

/// 将停止命令中的 %PID% 占位符替换为子进程 PID（大小写不敏感，按字符迭代兼容中文；对应 WinSW #217）
pub(crate) fn expand_stop_pid(value: &str, pid: u32) -> String {
    let chars: Vec<char> = value.chars().collect();
    let mut out = String::with_capacity(value.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '%' {
            let mut end = i + 1;
            while end < chars.len() && chars[end] != '%' {
                end += 1;
            }
            if end < chars.len() && end - i == 4
                && chars[i + 1].eq_ignore_ascii_case(&'p')
                && chars[i + 2].eq_ignore_ascii_case(&'i')
                && chars[i + 3].eq_ignore_ascii_case(&'d')
            {
                out.push_str(&pid.to_string());
                i = end + 1;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// 运行自定义停止命令（直接启动 stop_executable + args，最多等 timeout_secs 秒）:
/// %PID% 替换为子进程 PID 并注入 WINSGF_CHILD_PID（WinSW #217），超时/失败仅告警不阻断停止
pub(crate) fn run_stop_command(exe: &str, args: &str, pid: u32, timeout_secs: u64, log_dir: String, opts: &LogOptions) {
    let exe = expand_stop_pid(exe, pid);
    let args = expand_stop_pid(args, pid);
    write_log_entry(&log_dir, "host", &f("Stop executable: {0} {1}", &[&exe, &args]), opts);
    let mut cmd = Command::new(exe);
    if !args.trim().is_empty() {
        cmd.raw_arg(args);
    }
    // 注入子进程 PID 环境变量，与 poststop 钩子一致（WINSGF_CHILD_PID）
    cmd.env("WINSGF_CHILD_PID", pid.to_string());
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).creation_flags(0x08000000);

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            write_log_entry(&log_dir, "host", &f("Stop executable failed to run: {0}", &[&e.to_string()]), opts);
            return;
        }
    };
    let pid = child.id();
    let mut handles = Vec::new();
    if let Some(out) = child.stdout.take() {
        handles.push(spawn_log_reader(out, log_dir.clone(), "hook", opts.clone()));
    }
    if let Some(err) = child.stderr.take() {
        handles.push(spawn_log_reader(err, log_dir.clone(), "hook", opts.clone()));
    }

    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let timed_out = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                write_log_entry(&log_dir, "host",
                    &f("Stop executable exited with code {0}", &[&status.code().unwrap_or(-1).to_string()]), opts);
                break false;
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    break true;
                }
                thread::sleep(Duration::from_millis(50));
            }
            Err(_) => break false,
        }
    };
    if timed_out {
        write_log_entry(&log_dir, "host", &f("Stop executable timed out after {0}s, killing", &[&timeout_secs.to_string()]), opts);
        terminate_pid_tree(pid);
        let _ = child.kill();
        let _ = child.wait();
    }
    for h in handles {
        let _ = h.join();
    }
}

