use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::Ordering;
use std::sync::{Mutex, atomic::{AtomicBool, AtomicU32}};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE};
use windows::Win32::Security::{
    GetTokenInformation, LookupAccountNameW, PSECURITY_DESCRIPTOR, PSID, SID_NAME_USE,
    TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WOW64_32KEY, KEY_WOW64_64KEY, REG_SAM_FLAGS,
    RRF_RT_REG_SZ, RegCloseKey, RegGetValueW, RegOpenKeyExW,
};
use windows::Win32::System::Services::{
    ChangeServiceConfig2W, CloseServiceHandle, ControlService, CreateServiceW,
    DeleteService, ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW, QueryServiceStatus,
    SC_HANDLE, SC_MANAGER_ALL_ACCESS, SERVICE_AUTO_START,
    SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO,
    SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_ERROR_NORMAL,
    SERVICE_FAILURE_ACTIONSW, SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_START_TYPE,
    SERVICE_STATUS, SERVICE_STOP, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
// SERVICE_INTERACTIVE_PROCESS 位于 SystemServices（u32 位标志，非 ENUM_SERVICE_TYPE）
use windows::Win32::System::SystemServices::SERVICE_INTERACTIVE_PROCESS;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

use crate::service_config::ServiceConfig;

// ==================== 常量 ====================

/// 模板格式化: 将 {0} {1}... 依次替换为 args
pub(crate) fn f(template: &str, args: &[&str]) -> String {
    let mut s = template.to_string();
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{}}}", i), a);
    }
    s
}

/// 更新程序的启动类型 — 自动启动
const SVC_UPDATER_START_MODE: SERVICE_START_TYPE = SERVICE_AUTO_START;

/// 更新程序为一次性任务，无需故障恢复
const SVC_UPDATER_FAILURE_RESET_SEC: u32 = 0;

/// 更新程序为一次性任务，无需重启延迟
const SVC_UPDATER_RESTART_DELAY_MS: u32 = 0;

/// 超过此天数的服务日志将在启动时被清理
const LOG_RETENTION_DAYS: i64 = 30;

/// 超过此天数的 zip 归档将在启动时被清理（约半年；归档压缩后更省磁盘，保留期更长）
const LOG_ZIP_RETENTION_DAYS: i64 = 180;

/// SCM 启停/重启操作超时（秒）
pub(crate) const SCM_OP_TIMEOUT_SECS: u64 = 30;

/// 服务名校验失败的错误消息模板（多处共用，避免文案漂移）
const INVALID_NAME_MSG: &str = "Invalid service name: '{0}'. Service names must be 1-256 chars, must not be '.' or '..', and must not contain '\\', '/' or control characters.";

/// 内部服务更新程序保留名冲突的错误消息模板（多处共用）
const RESERVED_NAME_MSG: &str = "Service name '{0}' is reserved for the internal Osmium Service Checker. Use a different service_name.";

/// 服务名已被其他服务注册的错误消息模板（多处共用）
const ALREADY_REGISTERED_MSG: &str = "Service name '{0}' is already registered by a different service. Use a different service_name or uninstall it first.";

/// 服务名是否为更新程序保留名
pub(crate) fn is_updater_reserved_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Osmium Service Checker")
}

/// CLI 输出统一前缀（对齐 WinSW 的 "WinSW Service Management Interface" 输出风格）
pub(crate) const CLI_PREFIX: &str = "Osmium Service Management Interface";

const SERVICE_DELETE_ACCESS: u32 = 0x00010000;

// ==================== SCM 宿主入口 & 服务安装部署 ====================

/// SCM 宿主入口（无参数、非交互时由 CLI 路由调用）
pub(crate) fn run_service_host() {
    scm_entry(false, None);
}

/// 共享宿主部署入口: SCM 以 `-internal --run <name>` 启动，显式指定服务名
pub(crate) fn run_service_host_with_name(name: &str) {
    scm_entry(false, Some(name.to_string()));
}

/// 服务更新程序服务入口（-internal --updater）
pub(crate) fn run_svc_updater_service() {
    scm_entry(true, None);
}

/// 快速安装: 校验服务名/可执行路径合规，生成临时 TOML 配置，返回其路径。
/// 名称校验与 install_from_config_path 一致（含保留名）；路径须为已存在的绝对路径。
pub(crate) fn write_quick_config(name: &str, exe_path: &str) -> String {
    if !is_valid_service_name(name) {
        error(&f(INVALID_NAME_MSG, &[name]));
    }
    if is_updater_reserved_name(name) {
        error(&f(RESERVED_NAME_MSG, &[name]));
    }
    let rooted = Path::new(exe_path).is_absolute() || exe_path.starts_with('\\');
    if !rooted {
        error(&f("Quick install requires an absolute executable path (got: '{0}'). Use a full path like 'C:\\app\\service.exe'.", &[exe_path]));
    }
    let exe = std::fs::canonicalize(exe_path).unwrap_or_else(|_| PathBuf::from(exe_path));
    if !exe.exists() {
        error(&f("Invalid file path in service config: '{0}' does not exist or is not accessible. Check the executable path and try again.", &[exe_path]));
    }
    let config = ServiceConfig {
        service_name: name.to_string(),
        service_display_name: name.to_string(),
        service_description: name.to_string(),
        service_executable_path: exe.to_string_lossy().to_string(),
        // 显式给出与 serde 默认一致的取值，避免派生 Default 序列化出错误默认（false/0）
        failure_reset_sec: 86400,
        restart_delay_ms: 60000,
        kill_process_tree: true,
        log_enabled: true,
        log_max_backup_count: 5,
        download_threads: crate::service_config::DEFAULT_DOWNLOAD_THREADS,
        hide_window: true,
        log_out_enabled: true,
        log_err_enabled: true,
        ..Default::default()
    };
    let content = toml::to_string_pretty(&config)
        .unwrap_or_else(|e| panic!("{}", f("Failed to serialize config: {0}", &[&e.to_string()])));
    // tmp 原子创建: temp 目录所有用户可写，攻击者可预创建同名文件诱导加载恶意配置（TOCTOU），
    // 用 create_new 拒绝替换；文件名带 PID 降低冲突面
    let tmp = std::env::temp_dir().join(format!("osmium-quick-{}-{}.toml", process::id(), name));
    let write = || std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp)
        .and_then(|mut f| { use std::io::Write; f.write_all(content.as_bytes()) });
    write().unwrap_or_else(|e| panic!("{}", f("Failed to write temp config: {0}", &[&e.to_string()])));
    tmp.to_string_lossy().to_string()
}

pub(crate) fn install_from_config_path(config_path_str: &str) {
    let config_path = std::fs::canonicalize(config_path_str)
        .unwrap_or_else(|_| PathBuf::from(config_path_str));

    if !config_path.exists() {
        error(&f("Config file not found: '{0}'. Check the path and try again.", &[config_path_str]));
        return;
    }

    let config = load_config(&config_path);
    let svc_name = config.service_name.clone();

    // 服务名合法性: 防止 "." / ".." 之类名称把部署/删除路径带出 svcs 目录（路径穿越），
    // 或携带路径分隔符导致部署到意外位置
    if !is_valid_service_name(&svc_name) {
        error(&f(INVALID_NAME_MSG, &[&svc_name]));
        return;
    }

    // 保留名冲突: "Osmium Service Checker" 是内部开机更新程序的服务名，
    // 若允许用户服务同名，install-updater 会误停/误卸用户的服务
    if is_updater_reserved_name(&svc_name) {
        error(&f(RESERVED_NAME_MSG, &[&svc_name]));
        return;
    }

    let svc_display_name = config.service_display_name.clone();
    let svc_description = config.service_description.clone();
    let svc_exe_path = std::fs::canonicalize(&config.service_executable_path)
        .unwrap_or_else(|_| PathBuf::from(&config.service_executable_path));

    println!("{CLI_PREFIX}: Verifying service registration info");
    // 仅校验"安装时即应存在"的普通绝对路径: download_url 目标启动时才下载、
    // 相对路径按部署目录解析，安装时不存在属正常
    let has_download = has_download(&config);
    let exe_path_str = &config.service_executable_path;
    let rooted = Path::new(exe_path_str).is_absolute() || exe_path_str.starts_with('\\');
    if !has_download && rooted && !svc_exe_path.exists() {
        error(&f("Invalid file path in service config: service_executable_path '{0}' does not exist. Check the path in the config and try again.", &[exe_path_str]));
        return;
    }
    // P0-3: 平台部署同样校验目标 exe 及其目录不被非管理员可写（对齐 inplace 的 P0-1）。
    // 若 exe 位于 Downloads/Public/工作区等可写位置，任意用户可替换它，宿主以 LocalSystem
    // 启动时即提权；工作目录同理（可放恶意 DLL 侧加载）。
    let mut unsafe_paths: Vec<String> = Vec::new();
    if let Some(exe) = svc_exe_path.to_str() {
        let exe_dir = Path::new(exe).parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_user_writable(&exe_dir) || is_user_writable(exe) {
            unsafe_paths.push(exe.to_string());
        }
    }
    if let Some(workdir) = config.working_directory.as_deref()
        && !workdir.trim().is_empty()
        && is_user_writable(workdir)
    {
        unsafe_paths.push(format!("working_directory '{workdir}'"));
    }
    // 下载目标（download_to / downloads 条目 to）: 绝对路径指向可写位置时同样可被
    // 预放恶意文件替换（若 sha 匹配或未配置则跳过下载直接执行），纳入可写性校验
    if let Some(to) = config.download_to.as_deref()
        && (Path::new(to).is_absolute() || to.starts_with('\\'))
        && is_user_writable(to)
    {
        unsafe_paths.push(format!("download_to '{to}'"));
    }
    if let Some(list) = config.downloads.as_deref() {
        for d in list {
            let to = d.to.trim();
            if (Path::new(to).is_absolute() || to.starts_with('\\'))
                && is_user_writable(to)
            {
                unsafe_paths.push(format!("downloads[].to '{to}'"));
            }
        }
    }
    if !unsafe_paths.is_empty() {
        error(&f("Application error: {0}",
            &[&format!("service_executable_path (or working_directory) is writable by unprivileged users: {}. Move the executable to a SYSTEM/Administrators-only location (e.g. Program Files).",
                unsafe_paths.join(", "))]));
        return;
    }

    // 原地模式（deploy_inplace）: 不复制宿主到 ProgramData，直接用当前 exe 注册。
    // 宿主启动时按"同目录同名 toml"读取配置，因此配置必须与 exe 同名同目录
    let inplace = config.deploy_inplace;
    let own_exe = get_own_path();
    if inplace {
        // 配置名: 与 exe 同名，后缀 .toml（宿主读取时按同名 toml）
        let exe_stem = Path::new(&own_exe)
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let expected_names = [format!("{}.toml", exe_stem)];
        // canonicalize 会产生 \\?\ 前缀，与 own_exe 的普通路径前缀不一致，先去除再比较
        let config_file = strip_verbatim_prefix(&config_path)
            .file_name()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        if !expected_names.iter().any(|n| n.to_lowercase() == config_file) {
            error(&f("deploy_inplace: config file must be named '{0}' next to the executable (host reads its own .toml by name).",
                &[&format!("{}.toml", exe_stem)]));
            return;
        }
        // 原地注册宿主以 LocalSystem 运行，若 EXE 目录允许低权限用户写入（Downloads/Public/工作区等），
        // 任何用户可替换 EXE 获得 SYSTEM 执行；目录/DACL 与 EXE/TOML 的 ACL 须仅允许管理员改写（P0-1）
        let exe_dir = Path::new(&own_exe).parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        if is_user_writable(&exe_dir)
            || is_user_writable(&own_exe)
            || is_user_writable(config_path_str)
        {
            error(&f("Application error: {0}",
                &["deploy_inplace: directory (or its exe/toml) is writable by unprivileged users. Move the executable to a SYSTEM/Administrators-only location (e.g. Program Files)."]));
            return;
        }
        // 宿主 scm_svc_name 固定取 exe 文件名（os），SCM 要求注册名与 dispatcher 服务名一致，
        // inplace 不重命名 exe，故服务名必须等于 exe 文件名，否则注册成功却无法启动
        if !svc_name.eq_ignore_ascii_case(&exe_stem) {
            error(&f("Application error: {0}",
                &[&format!("deploy_inplace: service_name must equal the executable file name '{}', otherwise SCM cannot dispatch the service.", exe_stem)]));
            return;
        }
    }

    // 已注册判定以 SCM 为准。不能用 is_registered:
    // 同名外部服务会被其绕过冲突检测，失败回滚还会误删外部服务
    let is_update = if service_exists(&svc_name) {
        // 来源冲突检测: 防止同名但来源不同的服务被误覆盖
        if inplace {
            // 原地模式: 已注册服务的 ImagePath 必须与当前 exe 一致；
            // 未注册/ImagePath 读不到时跳过冲突检测
            if let Some(current_image) = get_service_image_path(&svc_name)
                && !current_image.trim_matches('"').eq_ignore_ascii_case(&own_exe)
            {
                error(&f(ALREADY_REGISTERED_MSG, &[&svc_name]));
            }
        } else {
            // 平台部署: 已部署 .osiml 可对比时要求可执行路径/参数一致才允许覆盖更新；
            // .osiml 缺失/损坏时退回 ImagePath 归属判定，仅 Osmium 部署可覆盖修复
            let config_dest = deployed_config_path(&svc_name);
            if !can_overwrite_source(config_dest.to_str().unwrap_or(""), config_path_str, &svc_name) {
                error(&f(ALREADY_REGISTERED_MSG, &[&svc_name]));
            }
        }
        force_remove_service(&svc_name, true);
        true
    } else {
        false
    };

    println!();
    println!("{CLI_PREFIX}: Registering service with system");

    // 部署文件（inplace 不复制宿主到 ProgramData，ImagePath 直接指向当前 exe）
    let base_dir = base_dir(&svc_name);
    let bin_path = if inplace {
        // 原地注册: ImagePath 直接指向当前 exe（路径含空格时需引号）
        format!("\"{}\"", own_exe)
    } else {
        // 平台化部署: 先收紧 Osmium/svcs/服务叶目录 ACL（所有者 Administrators + 仅 SYSTEM/Admin 可写），
        // 防普通用户预建目录/junction 诱导 SYSTEM 更新器误删服务；加固失败必须中止安装（防 P0-2）
        let osmium_dir = registry_dir().parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| registry_dir().to_string_lossy().to_string());
        let _ = std::fs::create_dir_all(&osmium_dir);
        let _ = std::fs::create_dir_all(registry_dir());
        let _ = std::fs::create_dir_all(&base_dir);
        if !secure_directory(&osmium_dir)
            || !secure_directory(&registry_dir().to_string_lossy())
            || !secure_directory(&base_dir.to_string_lossy())
        {
            let _ = uninstall_service_scm(&svc_name);
            safe_delete_dir(&base_dir);
            error(&f("Service registration failed: {0}", &["Failed to deploy service files"]));
        }
        let config_dest = deployed_config_path(&svc_name);
        if !write_deployed_config(config_path_str, &config_dest)
        {
            let _ = uninstall_service_scm(&svc_name);
            safe_delete_dir(&base_dir);
            error(&f("Service registration failed: {0}", &["Failed to deploy service files"]));
        }
        // 共享宿主: 所有服务复用框架安装目录的同一份 exe（不再每服务复制副本）；
        // 框架未安装（源码直跑）时回退当前 exe；服务名允许空格，ImagePath 须引号包裹
        let shared_host = if install_path().exists() { install_path() } else { PathBuf::from(&own_exe) };
        // ImagePath 必须加引号: 服务名允许空格，未加引号的路径会被 SCM 按首空格截断解析，
        // 攻击者可投放较短前缀路径对应的恶意 EXE 由 LocalSystem 启动
        format!("\"{}\" -internal --run \"{}\"", shared_host.display(), svc_name)
    };

    let (start_mode, delayed_auto) = parse_start_mode(config.service_start_mode.as_deref());
    let failure_reset = if config.failure_reset_sec > 0 { config.failure_reset_sec } else { 86400 };
    let restart_delay = if config.restart_delay_ms > 0 { config.restart_delay_ms } else { 60000 };

    match install_service_scm(&InstallServiceParams {
        service_name: &svc_name,
        display_name: &svc_display_name,
        description: &svc_description,
        executable_path: &bin_path,
        start_mode,
        failure_reset_sec: failure_reset as u32,
        restart_delay_ms: restart_delay as u32,
        dependencies: config.service_dependencies.as_deref(),
        service_account: config.service_account.as_deref(),
        password: config.service_password.as_deref(),
        delayed_auto_start: delayed_auto,
        interactive: config.interactive,
        failure_action: config.failure_action.as_deref(),
        allow_service_logon: config.allow_service_logon,
        security_descriptor: config.security_descriptor.as_deref(),
    }) {
        Ok(()) => println!("{CLI_PREFIX}: {}",
            if is_update { "Service updated successfully" } else { "Service registered successfully" }),
        Err(e) => {
            let _ = uninstall_service_scm(&svc_name);
            safe_delete_dir(&base_dir); // inplace 模式无部署目录，删除为空操作
            error(&f("Service registration failed: {0}", &[&e]));
        }
    }
}

// ==================== CLI 动作辅助 ====================

/// 校验服务名并确认已注册；任一失败即报错退出（6 个服务操作命令共用）
pub(crate) fn require_registered(svc_name: &str) {
    if !is_valid_service_name(svc_name) {
        error(&f(INVALID_NAME_MSG, &[svc_name]));
    }
    if !is_registered(svc_name) {
        error(&f("Service not found in registry: '{0}'. Use --list to see registered services.", &[svc_name]));
    }
}

pub(crate) fn do_uninstall(svc_name: &str, force_delete: bool) {
    if !do_stop(svc_name) {
        // 停止失败未完成卸载必须以非零码退出（P2-3）
        error(&f("Cannot uninstall service '{0}' — failed to stop it. Check service state with --status '{0}' and try again.", &[svc_name]));
    }
    match uninstall_service_scm(svc_name) {
        Ok(()) => {
            // 与 install 的更新路径一致: 等待 SCM 完全移除，避免立即重装同名服务
            // 触发延迟删除竞态（服务注册成功但稍后从 SCM 消失）
            wait_service_deleted(svc_name);
            safe_delete_dir(&base_dir(svc_name));
            println!("{CLI_PREFIX}: {}",
                if force_delete { "Service force-deleted" } else { "Service unregistered successfully" });
        }
        Err(e) => {
            if force_delete {
                error(&f("Force delete failed: {0}", &[&e]));
            }
            error(&f("Service unregistration failed: {0}", &[&e]));
        }
    }
}

pub(crate) fn do_start(svc_name: &str) {
    match start_service(svc_name, Duration::from_secs(SCM_OP_TIMEOUT_SECS)) {
        Ok(()) => println!("{CLI_PREFIX}: Service started successfully"),
        Err(e) => error(&f("Service start failed: {0}", &[&e])),
    }
}

pub(crate) fn do_stop(svc_name: &str) -> bool {
    match stop_service(svc_name, Duration::from_secs(SCM_OP_TIMEOUT_SECS)) {
        Ok(()) => {
            println!("{CLI_PREFIX}: Service stopped successfully");
            true
        }
        Err(e) => {
            cli_error_out(&f("Failed to stop service '{0}': {1}. Check service state with --status '{0}'.", &[svc_name, &e]));
            false
        }
    }
}

// ==================== 输出 / 错误 ====================

/// stderr 是否已启用 ANSI 虚拟终端处理（决定错误消息能否红色渲染）
static STDERR_VT_ENABLED: AtomicBool = AtomicBool::new(false);

/// 启用 stderr 的 ANSI 渲染: SetConsoleMode 加 ENABLE_VIRTUAL_TERMINAL_PROCESSING；
/// stderr 被重定向到文件/管道（无控制台）时失败，自动退化为无色
pub(crate) fn enable_stderr_vt() {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_ERROR_HANDLE, SetConsoleMode,
    };
    unsafe {
        let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) else { return };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_ok()
            && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
        {
            STDERR_VT_ENABLED.store(true, Ordering::Relaxed);
        }
    }
}

/// stdout 是否已启用 ANSI 虚拟终端处理（决定绿点/红点能否彩色渲染）
static STDOUT_VT_ENABLED: AtomicBool = AtomicBool::new(false);

/// 启用 stdout 的 ANSI 渲染（与 enable_stderr_vt 对称；stdout 被重定向时静默失败）
pub(crate) fn enable_stdout_vt() {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    unsafe {
        let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else { return };
        let mut mode = CONSOLE_MODE(0);
        if GetConsoleMode(h, &mut mode).is_ok()
            && SetConsoleMode(h, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
        {
            STDOUT_VT_ENABLED.store(true, Ordering::Relaxed);
        }
    }
}

/// 可用状态绿点（●）；VT 未启用时无色
pub(crate) fn green_dot() -> String {
    if STDOUT_VT_ENABLED.load(Ordering::Relaxed) {
        "\x1b[32m●\x1b[0m".to_string()
    } else {
        "●".to_string()
    }
}

/// 不可用状态红点（●）；VT 未启用时无色
pub(crate) fn red_dot() -> String {
    if STDOUT_VT_ENABLED.load(Ordering::Relaxed) {
        "\x1b[31m●\x1b[0m".to_string()
    } else {
        "●".to_string()
    }
}

/// 包装错误消息为 ANSI 红色；VT 未启用时原样返回（重定向场景不产生转义乱码）
pub(crate) fn red(message: &str) -> String {
    if STDERR_VT_ENABLED.load(Ordering::Relaxed) {
        format!("\x1b[31m{}\x1b[0m", message)
    } else {
        message.to_string()
    }
}

/// CLI 错误输出（红色统一前缀，不退出；与 error() 的区别是不终止进程）
fn cli_error_out(message: &str) {
    eprintln!("{}", red(&format!("{CLI_PREFIX} Error: {message}")));
}

pub(crate) fn error(message: &str) {
    cli_error_out(message);
    process::exit(1);
}

/// 校验服务名合法性: 服务名拼入 svcs 路径，"." / ".." 会路径穿越，分隔符/控制字符致部署或注册失败；
/// 长度限 256（SCM 上限），并拒绝 DOS 设备名（CON/NUL/COM1…）与结尾空格/点
pub(crate) fn is_valid_service_name(name: &str) -> bool {
    !name.trim().is_empty()
        // 用 UTF-16 码元计数，避免多字节字符（中文等）被字节计数错误拒绝
        && name.encode_utf16().count() <= 256
        && name != "."
        && name != ".."
        && !name.contains('\\')
        && !name.contains('/')
        && name.chars().all(|c| !c.is_control())
        // Windows 文件名保留字符: 服务名兼作 svcs 目录名，含这些字符会创建失败/路径歧义/ADS 语义（P2-2）
        && !name.chars().any(|c| matches!(c, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        && name.trim_end_matches([' ', '.']) == name
        && !is_dos_device_name(name)
}

/// Windows 保留设备名: 即使带扩展名（如 CON.txt）也会被解析为设备，不能作为文件名/目录名
fn is_dos_device_name(name: &str) -> bool {
    const DEVICES: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL",
        "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9",
        "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = name.split('.').next().unwrap_or("");
    DEVICES.iter().any(|d| stem.eq_ignore_ascii_case(d))
}

/// 提取 panic payload 的字符串消息（支持 &str 与 String），失败时返回兜底文案
pub(crate) fn panic_msg(payload: &(dyn std::any::Any + Send), fallback: &str) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        fallback.to_string()
    }
}

// ==================== 权限 & 路径 ====================

pub(crate) fn is_administrator() -> bool {
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size: u32 = size_of::<TOKEN_ELEVATION>() as u32;
        if GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut _),
            size,
            &mut size,
        )
        .is_err()
        {
            let _ = CloseHandle(token);
            return false;
        }
        let _ = CloseHandle(token);
        elevation.TokenIsElevated != 0
    }
}

pub fn get_own_path() -> String {
    std::env::current_exe()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| "os.exe".to_string())
}

/// 是否配置了启动前下载（download_url 非空）
pub(crate) fn has_download(config: &ServiceConfig) -> bool {
    config.download_url.as_deref().map(|s| !s.trim().is_empty()).unwrap_or(false)
}

pub fn load_config(path: impl AsRef<Path>) -> ServiceConfig {
    let path = path.as_ref();
    let content = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("{}", f("Failed to parse config '{0}': {1}", &[&path.display().to_string(), &e.to_string()])));
    let mut config: ServiceConfig = toml::from_str(&content)
        .unwrap_or_else(|e| panic!("{}", f("Failed to parse config '{0}': {1}", &[&path.display().to_string(), &e.to_string()])));
    // 部署配置的敏感字段为 DPAPI 密文，解析后统一解密
    decrypt_sensitive(&mut config);
    config
}

/// 平台部署覆盖判定: toml 可解析时对比可执行路径/参数同源；toml 缺失/损坏时退回 ImagePath 归属判定，
/// 仅 Osmium 部署才允许覆盖修复
pub(crate) fn can_overwrite_source(deployed_config: &str, config_path: &str, svc_name: &str) -> bool {
    if !Path::new(deployed_config).exists() {
        return is_osmium_deployed(svc_name);
    }
    std::panic::catch_unwind(|| {
        let existing = load_config(deployed_config);
        let current = load_config(config_path);
        // 路径与参数均忽略大小写，未填写的参数视为空串
        existing.service_executable_path.eq_ignore_ascii_case(current.service_executable_path.as_str())
            && existing.service_executable_args.as_deref().unwrap_or("")
                .eq_ignore_ascii_case(current.service_executable_args.as_deref().unwrap_or(""))
    })
    .unwrap_or_else(|_| is_osmium_deployed(svc_name))
}

/// 写部署配置: 敏感字段（service_password / download_password / 共享映射密码）DPAPI 加密后落盘，
/// 避免明文密码在 .osiml 中（P1-2）；配置无法解析（非标准 TOML）时退回按行剥离 service_password 的旧逻辑
pub(crate) fn write_deployed_config(source: &str, dest: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(source) else { return false };
    let encrypted = std::panic::catch_unwind(|| {
        let mut config: ServiceConfig = toml::from_str(&content).ok()?;
        if let Some(p) = &mut config.service_password {
            *p = dpapi_encrypt(p).unwrap_or_default();
        }
        if let Some(p) = &mut config.download_password {
            *p = dpapi_encrypt(p).unwrap_or_default();
        }
        if let Some(mappers) = &mut config.shared_directory_mappers {
            for m in mappers {
                if let Some(p) = &mut m.password {
                    *p = dpapi_encrypt(p).unwrap_or_default();
                }
            }
        }
        toml::to_string_pretty(&config).ok()
    });
    match encrypted {
        Ok(Some(text)) => std::fs::write(dest, text).is_ok(),
        _ => {
            let filtered: Vec<&str> = content
                .lines()
                .filter(|l| !l.trim_start().to_ascii_lowercase().starts_with("service_password"))
                .collect();
            std::fs::write(dest, filtered.join("\r\n")).is_ok()
        }
    }
}

/// 返回 (启动类型, 是否延迟自动启动)
pub(crate) fn parse_start_mode(mode: Option<&str>) -> (SERVICE_START_TYPE, bool) {
    match mode.map(|s| s.to_lowercase()).as_deref() {
        Some("delayed_auto") | Some("delayed-auto") | Some("delayedauto") => (SERVICE_AUTO_START, true),
        Some("automatic") => (SERVICE_AUTO_START, false),
        Some("manual") => (SERVICE_DEMAND_START, false),
        Some("disabled") => (SERVICE_DISABLED, false),
        _ => (SERVICE_AUTO_START, false),
    }
}

// ==================== 服务注册目录 & 安全加固 ====================

fn registry_dir() -> PathBuf {
    // SystemDrive 形如 "C:"（无尾部分隔符），需补 "\\" 才是根目录绝对路径
    let root = std::env::var("SystemDrive")
        .map(|d| if d.ends_with('\\') { d } else { format!("{}\\", d) })
        .unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(root).join("ProgramData").join("Osmium").join("svcs")
}

/// 平台部署服务的配置文件路径（共享宿主按名加载）: svcs\<name>\<name>.osiml
pub(crate) fn deployed_config_path(name: &str) -> PathBuf {
    registry_dir().join(name).join(format!("{}.osiml", name))
}

/// 服务更新程序日志目录 — 与 svcs 并列（ProgramData/Osmium/updater），
/// 避免占用 svcs/updater 目录，防止与真实名为 updater 的服务冲突
fn updater_log_dir() -> PathBuf {
    registry_dir()
        .parent()
        .map(|p| p.join("updater"))
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData\\Osmium\\updater"))
}

/// 是否 Osmium 管理的服务: 平台部署按 SCM ImagePath 是否位于 svcs 判定（而非仅目录存在，
/// 防对同名非 Osmium 部署服务误删/启停）；inplace 按 ImagePath 指向 os.exe 判定
fn is_registered(svc_name: &str) -> bool {
    service_exists(svc_name) && (is_osmium_deployed(svc_name) || is_inplace_service(svc_name))
}

/// 判定已注册服务是否为 inplace 原地注册: ImagePath 是 os.exe 且不在 svcs 平台部署目录内
fn is_inplace_service(svc_name: &str) -> bool {
    let Some(image) = get_service_image_path(svc_name) else { return false };
    let image = image.trim_matches('"');
    if !Path::new(image).file_name().map(|n| n.eq_ignore_ascii_case("os.exe")).unwrap_or(false) {
        return false;
    }
    // inplace 服务指向用户自己位置的 os.exe；svcs 目录内的是平台部署副本（名为 {svcName}.exe）
    let canonical = std::path::absolute(image).unwrap_or_else(|_| PathBuf::from(image));
    let canonical_str = canonical.to_string_lossy().to_lowercase();
    let prefix = format!("{}\\", registry_dir().to_string_lossy()).to_lowercase();
    !canonical_str.starts_with(&prefix)
}

/// 去除 std::fs::canonicalize 在 Windows 上产生的 \\?\ 前缀
pub(crate) fn strip_verbatim_prefix(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(rest) = s.strip_prefix("\\\\?\\") {
        PathBuf::from(rest)
    } else {
        path.to_path_buf()
    }
}

/// 查询已注册服务的 ImagePath（未注册/查询失败返回 null），用于 inplace 来源冲突检测与身份判定。
/// 直接读 SCM 服务注册表键并双视图查询（64/32 位），避免 QueryServiceConfig 的结构/缓冲问题
fn get_service_image_path(service_name: &str) -> Option<String> {
    let subkey = format!("SYSTEM\\CurrentControlSet\\Services\\{}", service_name);
    for flags in [
        REG_SAM_FLAGS(KEY_READ.0 | KEY_WOW64_64KEY.0),
        REG_SAM_FLAGS(KEY_READ.0 | KEY_WOW64_32KEY.0),
    ] {
        if let Some(p) = read_reg_string(HKEY_LOCAL_MACHINE, &subkey, "ImagePath", flags)
            && !p.is_empty()
        {
            return Some(p);
        }
    }
    None
}

/// 判定 SCM 服务是否 Osmium 平台部署（新格式 `-internal --run <name>` 或旧格式 ImagePath 位于 svcs 目录内）；
/// 供更新器/--list 按目录名操作前校验，防止误操作外部服务或被同名目录诱导
fn is_osmium_deployed(service_name: &str) -> bool {
    let Some(image) = get_service_image_path(service_name) else { return false };
    // 新格式: 共享宿主 + -internal --run <name>（按名判定，最准确）
    if parse_run_service_name(&image)
        .map(|n| n.eq_ignore_ascii_case(service_name))
        .unwrap_or(false)
    {
        return true;
    }
    // 旧格式（每服务一份宿主副本）: ImagePath 位于 svcs 部署目录内
    let path = image.trim_matches('"');
    let prefix = format!("{}\\", registry_dir().to_string_lossy()).to_lowercase();
    path.to_lowercase().starts_with(&prefix)
}

/// 从 ImagePath 解析 `-internal --run <name>` 中的服务名（共享宿主部署格式）。
/// 定位 "--run" 后的剩余内容并去掉外层引号，兼容服务名含空格（install 时引号包裹）
pub(crate) fn parse_run_service_name(image: &str) -> Option<String> {
    let s = image.trim();
    let idx = s.to_lowercase().find("--run")?;
    let after = s[idx + "--run".len()..].trim();
    if after.is_empty() {
        return None;
    }
    Some(after.trim_matches('"').to_string())
}

/// 读取注册表字符串值（REG_SZ），键不存在、值非字符串或为空时返回 None
fn read_reg_string(root: HKEY, subkey: &str, value: &str, flags: REG_SAM_FLAGS) -> Option<String> {
    unsafe {
        let subkey_wide = to_wide(subkey);
        let mut key = HKEY::default();
        let status = RegOpenKeyExW(root, PCWSTR::from_raw(subkey_wide.as_ptr()), Some(0), flags, &mut key);
        if status != ERROR_SUCCESS {
            return None;
        }
        let value_wide = to_wide(value);
        // 两段式: 先查所需大小，再读数据（RegGetValueW 按 RRF_RT_REG_SZ 过滤类型）
        let mut size: u32 = 0;
        let mut status = RegGetValueW(
            key,
            PCWSTR::null(),
            PCWSTR::from_raw(value_wide.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        );
        if status != ERROR_SUCCESS {
            let _ = RegCloseKey(key);
            return None;
        }
        let mut buf: Vec<u16> = vec![0; (size as usize / 2) + 1];
        let mut buf_size = (buf.len() * 2) as u32;
        status = RegGetValueW(
            key,
            PCWSTR::null(),
            PCWSTR::from_raw(value_wide.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut buf_size),
        );
        let _ = RegCloseKey(key);
        if status != ERROR_SUCCESS {
            return None;
        }
        let s = String::from_utf16_lossy(&buf);
        let s = s.split('\0').next().unwrap_or("").to_string();
        if s.is_empty() { None } else { Some(s) }
    }
}

/// 收紧部署目录 ACL: 所有者设 Administrators（takeown /A），DACL 仅 SYSTEM/Administrators 完全控制
/// （SID 形式不受语言影响），防低权限用户篡改 toml/exe 执行任意代码（WinSW #439）；失败返回 false 中止（防 P0-2）
pub(crate) fn secure_directory(path: &str) -> bool {
    let own = process::Command::new("takeown.exe")
        .args(["/F", path, "/A"])
        .creation_flags(0x08000000)
        .output();
    // 重建 DACL: 关闭继承 + 移除全部显式 ACE（含攻击者预创建目录的自带 ACE）+
    // 仅授 SYSTEM/Administrators 完全控制；用 .NET API 避免依赖 PowerShell Security 模块
    let escaped = path.replace('\'', "''");
    let script = [
        format!("$d=[IO.Directory]::GetAccessControl('{escaped}');"),
        String::from("$d.SetAccessRuleProtection($true,$false);"),
        String::from("$d.Access | ForEach-Object { $d.RemoveAccessRuleSpecific($_) };"),
        String::from("$d.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier('S-1-5-18')),'FullControl','ContainerInherit,ObjectInherit','None','Allow')));"),
        String::from("$d.AddAccessRule((New-Object Security.AccessControl.FileSystemAccessRule((New-Object Security.Principal.SecurityIdentifier('S-1-5-32-544')),'FullControl','ContainerInherit,ObjectInherit','None','Allow')));"),
        format!("[IO.Directory]::SetAccessControl('{escaped}',$d)"),
    ]
    .join("");
    let acl = process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000)
        .output();
    let ok = matches!(&own, Ok(o) if o.status.success())
        && matches!(&acl, Ok(a) if a.status.success());
    if !ok {
        let err = match &acl {
            Ok(a) if !a.status.success() => String::from_utf8_lossy(&a.stderr).trim().to_string(),
            _ => "ACL hardening failed".to_string(),
        };
        eprintln!("{}", red(&f("Warning: failed to secure deployment directory '{0}': {1}", &[path, &err])));
    }
    ok
}

/// 对象（目录/文件）是否允许低权限主体改写: 用 PowerShell 输出 SDDL 解析所有者与 DACL；
/// 解析失败/无法判定一律视为可写（fail-closed），拒绝在不可信位置注册 SYSTEM 服务（防 P0-1）
pub(crate) fn is_user_writable(path: &str) -> bool {
    let escaped = path.replace('\'', "''");
    let script = format!(
        "([IO.Directory]::GetAccessControl('{}')).GetSecurityDescriptorSddlForm(6)", // 6 = Access|Owner
        escaped
    );
    let out = process::Command::new("powershell.exe")
        .args(["-NoProfile", "-NonInteractive", "-Command", &script])
        .creation_flags(0x08000000)
        .output();
    let Ok(out) = out else { return true };
    if !out.status.success() { return true; }
    let sddl = String::from_utf8_lossy(&out.stdout);
    let sddl = sddl.trim();
    let Some(dacl_at) = sddl.find("D:") else { return true };
    let owner_ok = sddl_owner_is_administrative(&sddl[..dacl_at]);
    if !owner_ok { return true; }
    sddl_dacl_grants_non_admin_write(&sddl[dacl_at..])
}

/// SDDL 所有者段（"O:xxx"）是否管理员级主体（SYSTEM / Administrators / 域管理员 / 内建管理员 RID）
pub(crate) fn sddl_owner_is_administrative(owner_segment: &str) -> bool {
    let Some(o) = owner_segment.find("O:") else { return false };
    let sid = owner_segment[o + 2..].trim();
    sddl_sid_is_administrative(sid)
}

/// SDDL DACL 段是否授予非管理员级主体写能力
pub(crate) fn sddl_dacl_grants_non_admin_write(dacl: &str) -> bool {
    let mut rest = dacl;
    while let Some(start) = rest.find('(') {
        let Some(end) = rest[start..].find(')') else { break };
        let ace = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        // 格式: A|D;<flags>;<rights>;<objectGUID>;<inheritObjectGUID>;<sid>
        let parts: Vec<&str> = ace.split(';').collect();
        if parts.len() < 6 { continue; }
        let ace_type = parts[0];
        // 仅传播给子对象的 InheritOnly ACE（如 Program Files 标准 ACL 中 CREATOR OWNER 的
        // 继承 FullControl）不影响当前对象本身的可写性，须跳过，否则会被误判为"非管理员可写"
        if parts[1].contains("IO") { continue; }
        let rights = parts[2];
        let sid = parts[5].trim();
        let write = sddl_rights_include_write(rights);
        if !write { continue; }
        let admin = sddl_sid_is_administrative(sid.trim());
        if ace_type == "A" && !admin { return true; }
        if ace_type == "D" && admin { return true; }
    }
    false
}

/// SDDL 权限令牌是否含写能力（文件/目录写、删子项、改 DACL/所有者、删除等）
fn sddl_rights_include_write(rights: &str) -> bool {
    matches!(
        rights,
        "FA" | "FW" | "M" | "WD" | "WO" | "GA" | "GW" | "DC" | "AD" | "DT" | "DE" | "WDAC" | "WOWN"
    ) || rights.strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .map(|m| m & (0x2 | 0x4 | 0x40 | 0x10 | 0x100 | 0x10000 | 0x40000 | 0x80000) != 0)
        .unwrap_or(false)
}

/// SDDL SID 是否管理员级主体
fn sddl_sid_is_administrative(sid: &str) -> bool {
    match sid {
        "SY" | "BA" | "DA" | "EA" | "SA" => true,
        "S-1-5-18" | "S-1-5-32-544" => true,
        // TrustedInstaller 等服务安装器 SID（S-1-5-80-*）: 系统组件受保护主体，
        // System32 下的系统文件（如 cmd.exe）由它拥有，视为管理员级
        _ if sid.starts_with("S-1-5-80-") => true,
        _ if sid.starts_with("S-1-5-21-") => {
            // 域/本地账户: 末尾 RID 500（内建管理员）/ 512（域管理员）视为管理员级
            sid.rsplit('-').next().map(|r| r == "500" || r == "512").unwrap_or(false)
        }
        _ => false,
    }
}

fn base_dir(svc_name: &str) -> PathBuf {
    registry_dir().join(svc_name)
}

fn get_service_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(registry_dir()) else { return vec![] };
    entries
        .flatten() // 跳过不可读目录项
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .filter_map(|e| e.file_name().into_string().ok())
        .collect()
}

/// 列出当前确为 Osmium 管理的服务（SCM 存在且 ImagePath 位于 svcs 部署目录），
/// 排除卸载残留的孤儿目录与攻击者伪造的同名目录（供 CLI --list 使用）
pub(crate) fn list_osmium_services() -> Vec<String> {
    get_service_names()
        .into_iter()
        .filter(|s| is_osmium_deployed(s))
        .collect()
}

/// panic 日志路径: 平台安装（Program Files\Osmium\os.exe）→ ProgramData\Osmium\svcs；
/// inplace/独立部署 → exe 同目录（所有文件落 exe 旁）
pub(crate) fn panic_log_path() -> PathBuf {
    let own = get_own_path();
    if own.eq_ignore_ascii_case(&install_path().to_string_lossy()) {
        registry_dir().join("panic.log")
    } else {
        Path::new(&own)
            .parent()
            .map(|p| p.join("panic.log"))
            .unwrap_or_else(|| registry_dir().join("panic.log"))
    }
}

/// 安全删除目录（有界重试的递归删除）。
/// 避免 std::fs::remove_dir_all 在文件被其他进程短暂锁定时阻塞挂起。
pub(crate) fn safe_delete_dir(path: &Path) {
    for _ in 0..5 {
        if delete_dir_tree(path) {
            return;
        }
        thread::sleep(Duration::from_millis(200));
    }
}

/// 递归删除目录树；安全要点: 用 DirEntry::file_type 判断（不跟随符号链接），Path::is_dir 会跟随
/// junction/symlink，攻击者可放置指向任意目录的 junction 诱导 SYSTEM 更新器递归删除其目标（#4）
pub(crate) fn delete_dir_tree(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    let mut ok = true;
    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            match entry.file_type() {
                // delete_dir_tree 递归末尾已移除该子目录，此处不再二次 remove_dir
                Ok(ft) if ft.is_dir() => {
                    if !delete_dir_tree(&p) {
                        ok = false;
                    }
                }
                // 符号链接/junction/reparse point: 仅移除链接本身，绝不递归进入其目标
                _ => {
                    if std::fs::remove_file(&p).is_err()
                        && std::fs::remove_dir(&p).is_err()
                    {
                        ok = false;
                    }
                }
            }
        }
    }
    if std::fs::remove_dir(path).is_err() {
        ok = false;
    }
    ok
}

// ==================== 服务更新程序 — 元数据 & 命令 ====================

/// 返回 os.exe 的安装路径
fn install_path() -> PathBuf {
    let prog_files = std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    PathBuf::from(prog_files).join("Osmium").join("os.exe")
}

/// 校验当前进程是否运行在安装路径，防止恶意副本执行敏感命令
fn require_install_path() {
    let own = get_own_path();
    let canonical = install_path();
    if !own.eq_ignore_ascii_case(canonical.to_str().unwrap_or("")) {
        eprintln!("{}", red("Error: This command must be run from the installed location:"));
        eprintln!("{}", red(&f("  {0}", &[&canonical.display().to_string()])));
        eprintln!("{}", red(&f("Current: {0}", &[&own])));
        process::exit(1);
    }
}

/// -internal --install-updater: 将 Osmium 自身注册为开机服务更新程序
pub(crate) fn install_svc_updater_command() {
    require_install_path();

    if service_exists("Osmium Service Checker") {
        force_remove_service("Osmium Service Checker", false);
    }

    let own_exe = get_own_path();
    let bin_path = format!("\"{}\" -internal --updater", own_exe);

    match install_service_scm(&InstallServiceParams {
        service_name: "Osmium Service Checker",
        display_name: "Osmium Service Checker",
        description: "Boot-time maintenance service: removes stale Osmium services and orphaned directories, cleans up expired logs, and stops after running once.",
        executable_path: &bin_path,
        start_mode: SVC_UPDATER_START_MODE,
        failure_reset_sec: SVC_UPDATER_FAILURE_RESET_SEC,
        restart_delay_ms: SVC_UPDATER_RESTART_DELAY_MS,
        dependencies: None,
        service_account: None,
        password: None,
        delayed_auto_start: true,
        interactive: false,
        failure_action: None,
        allow_service_logon: false,
        security_descriptor: None,
    }) {
        Ok(()) => println!("{CLI_PREFIX}: Service updater registered (runs on boot)"),
        Err(e) => error(&f("Service updater registration failed: {0}", &[&e])),
    }
}

/// -internal --uninstall-updater: 移除服务更新程序
pub(crate) fn uninstall_svc_updater_command() {
    require_install_path();

    if !service_exists("Osmium Service Checker") {
        println!("{CLI_PREFIX}: Service updater not found");
        return;
    }
    // 尽力停止后卸载（停止失败也继续卸载）
    let _ = stop_service("Osmium Service Checker", Duration::from_secs(SCM_OP_TIMEOUT_SECS));
    match uninstall_service_scm("Osmium Service Checker") {
        Ok(()) => println!("{CLI_PREFIX}: Service updater removed"),
        Err(e) => error(&f("Service updater removal failed: {0}", &[&e])),
    }
}

// ==================== 服务更新程序 — 升级 & 清理 ====================

/// 删除各服务日志目录以及服务更新程序日志目录中超过 LOG_RETENTION_DAYS 天的日志文件；
/// 服务开启 log_zip 时先归档再删除（过期日志都有归档机会），未开启则直接删
fn cleanup_old_logs() {
    let cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(LOG_RETENTION_DAYS);
    let mut deleted = 0;

    for svc_name in get_service_names() {
        let log_dir = registry_dir().join(&svc_name).join("logs");
        if log_dir.exists() {
            deleted += delete_old_logs(&log_dir, cutoff, service_log_zip(&svc_name));
        }
    }

    // 清理服务更新程序日志（自身无 log_zip 配置，不归档直接删）
    let updater_log_dir = updater_log_dir();
    if updater_log_dir.exists() {
        deleted += delete_old_logs(&updater_log_dir, cutoff, false);
    }

    // panic.log 位于 svcs 根目录（独立于各服务 logs 子目录，无日期前缀），按 mtime 纳入清理
    let panic_log = panic_log_path();
    if panic_log.exists() {
        let stale = std::fs::metadata(&panic_log).ok()
            .and_then(|m| m.modified().ok())
            .map(|t| { let dt: chrono::DateTime<chrono::Local> = t.into(); dt.date_naive() < cutoff })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&panic_log);
            deleted += 1;
        }
    }

    if deleted > 0 {
        println!("{}", f("  Log cleanup: removed {0} expired log file(s) (>{1}d)", &[&deleted.to_string(), &LOG_RETENTION_DAYS.to_string()]));
    }
}

/// 读取服务配置的 log_zip 开关；配置缺失/损坏时保守按 false 处理（不归档）
fn service_log_zip(svc_name: &str) -> bool {
    let cfg = deployed_config_path(svc_name);
    std::panic::catch_unwind(|| load_config(&cfg).log_zip).unwrap_or(false)
}

/// 删除过期日志；zip_archives=true 时删除普通日志前先压缩为 .zip 归档（先归档再删除，
/// 保证每个过期日志都有归档机会，与 WinSW zipOlderThanNumDays 语义对齐）；失败保留原文件
pub(crate) fn delete_old_logs(log_dir: &Path, cutoff: chrono::NaiveDate, zip_archives: bool) -> i32 {
    // zip 归档独立保留期（约半年），普通日志沿用传入 cutoff（30 天）
    let zip_cutoff = chrono::Local::now().date_naive() - chrono::Duration::days(LOG_ZIP_RETENTION_DAYS);
    let mut deleted = 0;
    let Ok(entries) = std::fs::read_dir(log_dir) else { return 0 };
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // 主日志/err 分流（.log）、roll 模式 .old、滚动备份（.N）、zip 归档（.zip）都纳入清理
        let is_log = ext == "log" || ext == "old" || ext.parse::<u32>().is_ok() || ext == "zip";
        if !is_log { continue; }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
        // 优先取文件名开头日期段判定（兼容滚动备份 .log.1 与 err 分流 .err.log）；
        // 自定义文件名/非 %Y-%m-%d 前缀解析失败时回退按 mtime 判定（否则永不被清理，G4 修复）
        let date = name.get(..10)
            .and_then(|d| chrono::NaiveDate::parse_from_str(d, "%Y-%m-%d").ok())
            .or_else(|| {
                std::fs::metadata(&path).ok()?.modified().ok().map(|t| {
                    let dt: chrono::DateTime<chrono::Local> = t.into();
                    dt.date_naive()
                })
            });
        let Some(date) = date else { continue };
        let effective = if ext == "zip" { zip_cutoff } else { cutoff };
        if date < effective {
            // 普通日志过期且开启归档 → 先压缩再删；归档失败保留原文件（下次再试）
            //（开机清理无服务级 zip 日期格式，保持 {file}.zip 命名）
            if zip_archives && ext != "zip"
                && !crate::service_host::zip_backup_file(&path, "")
            {
                continue;
            }
            let _ = std::fs::remove_file(&path);
            deleted += 1;
        }
    }
    deleted
}

/// 串行化日志文件写入，避免多线程 append 同一文件时 IO 冲突
static LOG_WRITE_LOCK: Mutex<()> = Mutex::new(());

/// 写入日志条目: <log_dir>/yyyy-MM-dd.log（服务宿主与更新程序共用）
pub(crate) fn write_log_line(log_dir: &Path, channel: &str, message: &str) {
    let _ = std::fs::create_dir_all(log_dir);
    let today = chrono::Local::now().format("%Y-%m-%d");
    let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
    let log_file = log_dir.join(format!("{}.log", today));
    let entry = format!("[{}] [{}] {}\r\n", now, channel, message);
    let _guard = LOG_WRITE_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _ = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_file)
        .map(|mut f| {
            use std::io::Write;
            let _ = f.write_all(entry.as_bytes());
        });
}

/// 写入服务更新程序日志: ProgramData/Osmium/updater/yyyy-MM-dd.log
fn write_updater_log(channel: &str, message: &str) {
    write_log_line(&updater_log_dir(), channel, message);
}

/// 开机维护: 校验并清理失效服务 / 孤儿目录 / 过期日志；仅扫描 svcs 平台部署目录。
/// 共享宿主复用安装目录同一份 exe，宿主升级由重装安装包覆盖；inplace 服务平台不兜底
fn upgrade_outdated_hosts() {
    let services = get_service_names();
    if services.is_empty() {
        write_updater_log("updater", "No registered services found, skipping cleanup");
        cleanup_old_logs();
        return;
    }

    // 校验并清理失效服务 / 孤儿目录（共享宿主部署: 所有服务复用框架安装目录的同一份 exe，
    // 宿主升级由重装安装包覆盖共享 exe 完成，更新器不再逐服务替换宿主副本）
    for svc_name in &services {
        // 更新程序自身不部署 svcs 目录，跳过保留名目录
        if !svc_name.eq_ignore_ascii_case("Osmium Service Checker") {
            cleanup_invalid_service(svc_name);
        }
    }

    let services = get_service_names();
    if services.is_empty() {
        write_updater_log("updater", "All services were stale, nothing to clean");
        cleanup_old_logs();
        return;
    }
    write_updater_log("updater", &f("Scanning {0} registered service(s)",
        &[&services.len().to_string()]));

    cleanup_old_logs();
}

/// 校验服务配置有效性: toml 缺失/可执行路径不存在/解析失败则从 SCM 移除并删宿主目录，
/// 并清理 SCM 无记录但目录仍在的孤儿；仅扫描 svcs 部署目录，inplace 服务不兜底清理
fn cleanup_invalid_service(svc_name: &str) {
    let base = registry_dir().join(svc_name);
    // 卸载残留: 卸载流程中断可能只删了 SCM 记录而遗留目录
    if !service_exists(svc_name) {
        write_updater_log("warn", &f("[{0}] Service not in SCM, removing orphaned directory", &[svc_name]));
        safe_delete_dir(&base);
        return;
    }
    // 安全边界: 仅当目录对应 Osmium 部署的服务才可操作；普通用户可伪造与系统服务同名的空目录，
    // 直接按目录名停止/卸载会诱导 SYSTEM 更新器删除无关服务
    if !is_osmium_deployed(svc_name) {
        write_updater_log("warn", &f("[{0}] Invalid config ({1}), removing stale service", &[svc_name, "not an Osmium-managed service"]));
        return;
    }
    let config_path = deployed_config_path(svc_name);

    if !config_path.exists() {
        write_updater_log("warn", &f("[{0}] Config file missing, removing stale service", &[svc_name]));
        remove_stale_service(svc_name);
        return;
    }

    // 解析失败用 catch_unwind 兜底；配置 download_url 的服务启动时才下载，
    // 开机扫描时跳过存在性校验避免误删
    let invalid_exe = std::panic::catch_unwind(|| {
        let config = load_config(&config_path);
        let has_download = has_download(&config);
        if !has_download && !Path::new(&config.service_executable_path).exists() {
            Some(config.service_executable_path)
        } else {
            None
        }
    });
    match invalid_exe {
        Ok(Some(exe_path)) => {
            write_updater_log("warn", &f("[{0}] Invalid executable path '{1}', removing stale service", &[svc_name, &exe_path]));
            remove_stale_service(svc_name);
        }
        Ok(None) => {}
        Err(payload) => {
            let detail = panic_msg(&*payload, "unknown error");
            write_updater_log("warn", &f("[{0}] Invalid config ({1}), removing stale service", &[svc_name, &detail]));
            remove_stale_service(svc_name);
        }
    }
}

/// 尽力停止并卸载服务，等待 SCM 完全移除，可选删除宿主目录。
/// 失败不抛出（供"卸载后重建/清理残留"这类尽力而为的场景使用）。
fn force_remove_service(svc_name: &str, delete_host_dir: bool) {
    let _ = stop_service(svc_name, Duration::from_secs(SCM_OP_TIMEOUT_SECS));
    let _ = uninstall_service_scm(svc_name);
    wait_service_deleted(svc_name);
    if delete_host_dir {
        safe_delete_dir(&base_dir(svc_name));
    }
}

/// 移除失效服务: 停止 → 卸载 SCM 服务 → 等待删除 → 删除宿主目录
fn remove_stale_service(svc_name: &str) {
    force_remove_service(svc_name, true);
    write_updater_log("updater", &f("[{0}] Stale service removed", &[svc_name]));
}

// ==================== 下载 & 文件校验 ====================

/// 分块下载单块大小（字节）: 大于此值的文件启用多线程分块并行下载（aria2 风格）
const CHUNK_SIZE: u64 = 1024 * 1024;

/// 单块下载失败重试次数（含首次共尝试 3 次，容忍网络抖动）
const CHUNK_MAX_RETRIES: u32 = 2;

/// 下载认证方式（对应配置 download_auth）: None 无认证 / Basic(user, pass)
#[derive(Clone, Copy)]
pub(crate) enum DownloadAuth<'a> {
    None,
    Basic(&'a str, &'a str),
}

/// 单线程下载结果: Downloaded=已下载, NotModified=服务器回 304（目标未变化，跳过）
enum SingleOutcome {
    Downloaded,
    NotModified,
}

/// 构造下载 Agent（全局超时覆盖整个下载；4xx/5xx 不转错误，调用方按状态码处理）
fn build_agent(timeout_secs: u64, proxy: Option<&str>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(timeout_secs)));
    if let Some(proxy_url) = proxy
        && let Ok(p) = ureq::Proxy::new(proxy_url)
    {
        builder = builder.proxy(Some(p));
    }
    ureq::Agent::new_with_config(builder.build())
}

/// 多线程分块下载核心: HEAD 探测 Range，支持且 >1MiB 时分块并发（threads 0/1 禁用），失败回退单线程；
/// tmp 以 CreateNew 创建（TOCTOU 防护）；304 视为完成删 tmp 保留原目标
pub(crate) fn download_core(url: &str, tmp: &str, timeout_secs: u64,
    auth: DownloadAuth<'_>, proxy: Option<&str>, threads: i32,
    if_modified_since: Option<String>) -> Result<(), (bool, String)> {
    let client = build_agent(timeout_secs, proxy);

    // CreateNew 原子创建，拒绝预创建文件替换；残留同名文件清理后重试一次
    let create = || std::fs::OpenOptions::new().write(true).create_new(true).open(tmp);
    let file = match create() {
        Ok(f) => f,
        Err(_) => {
            let _ = std::fs::remove_file(tmp);
            create().map_err(|e| (false, e.to_string()))?
        }
    };

    // 304 优化: 目标已存在且无 sha 校验时发送 If-Modified-Since，并强制单线程
    //（Range+If-Modified-Since 组合无意义，未变化时服务器直接回 304）
    if let Some(date) = if_modified_since {
        return match single_download(&client, url, &file, auth, Some(&date)) {
            Ok(SingleOutcome::NotModified) => {
                // 目标未变化: 保留原文件，删除空 tmp，视为下载完成
                let _ = std::fs::remove_file(tmp);
                Ok(())
            }
            Ok(SingleOutcome::Downloaded) => Ok(()),
            Err(e) => Err(e),
        }
    }

    // 探测: HEAD 取 Content-Length 与 Accept-Ranges；HEAD 异常视为不支持 Range，直接单线程
    let probe = client.head(url).call();
    if let Ok(resp) = probe
        && resp.status().is_success()
    {
        let ranges_ok = resp.headers().get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes")).unwrap_or(false);
        if ranges_ok
            && let Some(size) = resp.headers().get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        {
            if size > CHUNK_SIZE && threads >= 2
                && chunked_download(&client, url, &file, auth, size, threads as u64).is_ok()
            {
                return Ok(());
            }
            // 分块失败（服务器实际不支持 Range/网络异常）→ 清零后回退单线程
            let _ = file.set_len(0);
        }
    }
    single_download(&client, url, &file, auth, None).map(|_| ())
}

/// 单线程完整下载（不支持 Range / 小文件 / 分块回退路径；可选 If-Modified-Since 头）;
/// 服务器回 304 时返回 NotModified 且不写内容
fn single_download(client: &ureq::Agent, url: &str, file: &std::fs::File,
    auth: DownloadAuth<'_>, if_modified_since: Option<&str>) -> Result<SingleOutcome, (bool, String)> {
    let mut req = client.get(url);
    if let DownloadAuth::Basic(user, pass) = auth {
        use base64::Engine as _;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req = req.header("authorization", format!("Basic {token}"));
    }
    if let Some(date) = if_modified_since {
        req = req.header("if-modified-since", date);
    }
    let resp = req.call().map_err(|e| (matches!(e, ureq::Error::Timeout(_)), e.to_string()))?;
    if resp.status().as_u16() == 304 {
        return Ok(SingleOutcome::NotModified);
    }
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        // 401: 明确提示认证配置问题（Basic 凭据错误/未配置）
        let hint = if status == 401 {
            "server returned HTTP 401 Unauthorized — check download_username/download_password or server authentication requirements"
        } else {
            &format!("server returned HTTP {}", status)
        };
        return Err((false, hint.to_string()));
    }
    let mut reader = resp.into_body().into_reader();
    let mut out = file.try_clone().map_err(|e| (false, e.to_string()))?;
    std::io::copy(&mut reader, &mut out).map_err(|e| (false, e.to_string()))?;
    Ok(SingleOutcome::Downloaded)
}

/// 按 CHUNK_SIZE 分块并发下载到预分配文件（各块独立线程，Windows seek_write 按偏移写）
fn chunked_download(client: &ureq::Agent, url: &str, file: &std::fs::File,
    auth: DownloadAuth<'_>, size: u64, max_workers: u64) -> Result<(), (bool, String)> {
    use std::sync::Arc;

    file.set_len(size).map_err(|e| (false, e.to_string()))?; // 预分配，避免零散分配
    let file = Arc::new(file.try_clone().map_err(|e| (false, e.to_string()))?);

    let chunk_count = size.div_ceil(CHUNK_SIZE);
    let workers = chunk_count.min(max_workers);
    // auth 含借用，闭包内转 owned（Basic 凭据拷贝）供分块线程使用
    let auth_owned: Option<(String, String)> = match auth {
        DownloadAuth::None => None,
        DownloadAuth::Basic(u, p) => Some((u.to_string(), p.to_string())),
    };
    let mut handles = Vec::new();
    for w in 0..workers {
        let client = client.clone();
        let file = file.clone();
        let url = url.to_string();
        let auth_owned = auth_owned.clone();
        handles.push(thread::spawn(move || {
            let mut i = w;
            while i < chunk_count {
                let start = i * CHUNK_SIZE;
                let end = (start + CHUNK_SIZE - 1).min(size - 1);
                let mut attempt = 0u32;
                loop {
                    if download_chunk(&client, &url, &file,
                        auth_owned.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
                        start, end).is_ok() { break; }
                    attempt += 1;
                    if attempt > CHUNK_MAX_RETRIES {
                        return Err((false, format!("chunk {}-{} failed after retries", start, end)));
                    }
                }
                i += workers;
            }
            Ok(())
        }));
    }
    for h in handles {
        let inner = h.join().map_err(|_| (false, "chunk thread panic".into()))?;
        inner?;
    }
    Ok(())
}

/// 下载单个分块（Range 请求）并写入文件偏移；服务器必须返回 206
fn download_chunk(client: &ureq::Agent, url: &str, file: &std::fs::File,
    auth: Option<(&str, &str)>, start: u64, end: u64) -> Result<(), (bool, String)> {
    use std::io::Read;
    use std::os::windows::fs::FileExt;

    let mut req = client.get(url)
        .header("range", format!("bytes={}-{}", start, end));
    if let Some((user, pass)) = auth {
        use base64::Engine as _;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req = req.header("authorization", format!("Basic {token}"));
    }
    let resp = req.call().map_err(|e| (matches!(e, ureq::Error::Timeout(_)), e.to_string()))?;
    if !resp.status().is_success() {
        let status = resp.status().as_u16();
        let hint = if status == 401 {
            "server returned HTTP 401 Unauthorized — check download_username/download_password or server authentication requirements"
        } else {
            &format!("server returned HTTP {}", status)
        };
        return Err((false, hint.to_string()));
    }
    // 服务器必须回 206 Partial Content；忽略 Range 返回 200 会导致数据错位，视为失败
    if resp.status().as_u16() != 206 {
        return Err((false, format!("server returned HTTP {} for ranged request", resp.status().as_u16())));
    }
    let mut reader = resp.into_body().into_reader();
    let mut buf = [0u8; 64 * 1024];
    let mut offset = start;
    loop {
        let n = reader.read(&mut buf).map_err(|e| (false, e.to_string()))?;
        if n == 0 { break; }
        file.seek_write(&buf[..n], offset).map_err(|e| (false, e.to_string()))?;
        offset += n as u64;
    }
    Ok(())
}

/// 计算文件 SHA-256（小写十六进制）并比较；未提供校验值视为匹配
pub(crate) fn sha256_matches(path: &str, expected: Option<&str>) -> bool {
    use sha2::{Digest, Sha256};
    let Some(sha) = expected else { return true };
    let sha = sha.trim();
    if sha.is_empty() {
        return true;
    }
    let data = match std::fs::read(path) {
        Ok(d) => d,
        Err(_) => return false,
    };
    let hex: String = Sha256::digest(&data).iter().map(|b| format!("{:02x}", b)).collect();
    hex == sha.to_lowercase()
}

// ==================== SCM API ====================

/// 服务注册参数（收敛 install_service_scm 过多入参）
struct InstallServiceParams<'a> {
    service_name: &'a str,
    display_name: &'a str,
    description: &'a str,
    executable_path: &'a str,
    start_mode: SERVICE_START_TYPE,
    failure_reset_sec: u32,
    restart_delay_ms: u32,
    dependencies: Option<&'a str>,
    service_account: Option<&'a str>,
    password: Option<&'a str>,
    delayed_auto_start: bool,
    interactive: bool,
    failure_action: Option<&'a str>,
    allow_service_logon: bool,
    security_descriptor: Option<&'a str>,
}

fn install_service_scm(p: &InstallServiceParams) -> Result<(), String> {
    unsafe {
        let service_name_wide = to_wide(p.service_name);
        let display_name_wide = to_wide(p.display_name);
        let exe_path_wide = to_wide(p.executable_path);

        let scm = OpenSCManagerW(
            PCWSTR::null(),
            PCWSTR::null(),
            SC_MANAGER_ALL_ACCESS,
        ).map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;

        // 宽字符串必须保持存活直到 CreateServiceW 调用完成
        let dep_str = build_dependency_string(p.dependencies);
        let dep_wide = dep_str.as_deref().map(to_wide);
        let dep_pcwstr = dep_wide.as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let account_wide = p.service_account.map(to_wide);
        let account_pcwstr = account_wide.as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let password_wide = p.password.map(to_wide);
        let password_pcwstr = password_wide.as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());

        // interactive=true 时附加 SERVICE_INTERACTIVE_PROCESS（可交互桌面）
        let mut service_type = SERVICE_WIN32_OWN_PROCESS;
        if p.interactive {
            service_type |= ENUM_SERVICE_TYPE(SERVICE_INTERACTIVE_PROCESS);
        }

        // DeleteService 后 SCM 可能仍处于"已标记删除"（1072）状态，立即以同名重建会失败。
        // wait_service_deleted 已尽量等待，此处再做最后防线: 遇到 1072 时短暂重试
        let mut svc = Err(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(0)));
        for attempt in 0..6 {
            svc = CreateServiceW(
                scm,
                PCWSTR::from_raw(service_name_wide.as_ptr()),
                PCWSTR::from_raw(display_name_wide.as_ptr()),
                windows::Win32::System::Services::SERVICE_ALL_ACCESS,
                service_type,
                p.start_mode,
                SERVICE_ERROR_NORMAL,
                PCWSTR::from_raw(exe_path_wide.as_ptr()),
                PCWSTR::null(),
                None,
                dep_pcwstr,
                account_pcwstr,
                password_pcwstr,
            );
            match &svc {
                Ok(_) => break,
                Err(e) if e.code().0 as u32 & 0xFFFF == 1072 && attempt < 5 => {
                    thread::sleep(Duration::from_millis(500));
                }
                Err(_) => break,
            }
        }
        let svc = svc.map_err(|e| format!("{}: {e}", "Failed to create service"))?;

        // 设置描述（失败必须传播，不能静默缺失，P2-3）
        let desc_wide = to_wide(p.description);
        let desc_info = SERVICE_DESCRIPTIONW {
            lpDescription: PWSTR::from_raw(desc_wide.as_ptr() as *mut _),
        };
        ChangeServiceConfig2W(svc, SERVICE_CONFIG_DESCRIPTION, Some(&desc_info as *const _ as *const _))
            .map_err(|e| format!("{}: {e}", "Failed to set service description"))?;

        // 设置故障恢复（failure_action 决定动作序列）
        if p.failure_reset_sec > 0 {
            set_failure_actions(svc, p.failure_reset_sec, p.restart_delay_ms, p.failure_action)?;
        }

        // 延迟自动启动
        if p.delayed_auto_start {
            let delay_info = SERVICE_DELAYED_AUTO_START_INFO { fDelayedAutostart: true.into() };
            ChangeServiceConfig2W(svc, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, Some(&delay_info as *const _ as *const _))
                .map_err(|e| format!("{}: {e}", "Failed to set delayed auto start"))?;
        }

        // 服务安全描述符（SDDL）: 应用到服务 DACL，控制谁能管理该服务（对应 WinSW securityDescriptor）
        if let Some(sddl) = p.security_descriptor {
            apply_service_sddl(svc, sddl)
                .map_err(|e| format!("{}: {e}", "Failed to set service security descriptor"))?;
        }

        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
    }

    // allow_service_logon: 服务创建后若使用自定义账户，自动授予其"作为服务登录"权限
    if p.allow_service_logon
        && let Some(account) = p.service_account
    {
        grant_service_logon_right(account);
    }
    Ok(())
}

/// 解析 SDDL 为安全描述符缓冲（调用方负责 LocalFree 释放；失败返回错误）
pub(crate) fn security_descriptor_from_sddl(sddl: &str) -> Result<PSECURITY_DESCRIPTOR, windows::core::Error> {
    use windows::Win32::Security::Authorization::{
        ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
    };
    unsafe {
        let sddl_wide = to_wide(sddl);
        let mut sd = PSECURITY_DESCRIPTOR(std::ptr::null_mut());
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR::from_raw(sddl_wide.as_ptr()),
            SDDL_REVISION_1,
            &mut sd,
            None,
        )
        .is_err()
        {
            return Err(windows::core::Error::from_hresult(
                windows::core::HRESULT::from_win32(0)));
        }
        Ok(sd)
    }
}

/// 把 SDDL 安全描述符应用到服务 DACL
/// （ConvertStringSecurityDescriptorToSecurityDescriptorW + SetServiceObjectSecurity）
fn apply_service_sddl(svc: SC_HANDLE, sddl: &str) -> Result<(), windows::core::Error> {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::DACL_SECURITY_INFORMATION;
    use windows::Win32::System::Services::SetServiceObjectSecurity;
    unsafe {
        let sd = security_descriptor_from_sddl(sddl)?;
        // LocalFree 释放 Convert 分配的缓冲；应用失败时同样需要释放
        let result = SetServiceObjectSecurity(svc, DACL_SECURITY_INFORMATION, sd);
        let _ = LocalFree(Some(HLOCAL(sd.0)));
        result
    }
}

/// 授予指定账户"作为服务登录"权限（SeServiceLogonRight）;
/// 失败仅告警——CreateServiceW 本身会因缺少该权限而报错，此处授权失败不影响主流程判断
fn grant_service_logon_right(account: &str) {
    use windows::Win32::Security::Authentication::Identity::{LSA_HANDLE, LSA_OBJECT_ATTRIBUTES,
                                                             LSA_UNICODE_STRING,
                                                             LsaAddAccountRights, LsaClose, LsaOpenPolicy, POLICY_AUDIT_LOG_ADMIN, POLICY_CREATE_ACCOUNT, POLICY_CREATE_PRIVILEGE,
                                                             POLICY_CREATE_SECRET, POLICY_GET_PRIVATE_INFORMATION, POLICY_LOOKUP_NAMES,
                                                             POLICY_NOTIFICATION, POLICY_SERVER_ADMIN, POLICY_SET_AUDIT_REQUIREMENTS,
                                                             POLICY_SET_DEFAULT_QUOTA_LIMITS, POLICY_TRUST_ADMIN, POLICY_VIEW_AUDIT_INFORMATION,
                                                             POLICY_VIEW_LOCAL_INFORMATION, SE_SERVICE_LOGON_NAME,
    };

    // 解析账户名 → SID（".\user" 需先解析为完整名称，LookupAccountNameW 支持 ".\user"）
    unsafe {
        let name_wide = to_wide(account);
        let mut domain_len: u32 = 256;
        let mut sid_len: u32 = 0;
        let mut use_enum = SID_NAME_USE(0);
        // 第一次调用获取 SID 大小
        let _ = LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR::from_raw(name_wide.as_ptr()),
            None,
            &mut sid_len,
            None,
            &mut domain_len,
            &mut use_enum,
        );
        if sid_len == 0 {
            return;
        }
        let mut sid_buf = vec![0u8; sid_len as usize];
        let mut domain_buf = [0u16; 256];
        let mut domain_len_out: u32 = domain_buf.len() as u32;
        let res = LookupAccountNameW(
            PCWSTR::null(),
            PCWSTR::from_raw(name_wide.as_ptr()),
            Some(PSID(sid_buf.as_mut_ptr() as *mut _)),
            &mut sid_len,
            Some(PWSTR(domain_buf.as_mut_ptr())),
            &mut domain_len_out,
            &mut use_enum,
        );
        if res.is_err() {
            return;
        }
        let sid = PSID(sid_buf.as_mut_ptr() as *mut _);

        // POLICY_ALL_ACCESS 在 windows crate 无别名，按各标志位拼合（与 winnt.h 定义一致）
        let policy_access = (POLICY_VIEW_LOCAL_INFORMATION
            | POLICY_VIEW_AUDIT_INFORMATION
            | POLICY_GET_PRIVATE_INFORMATION
            | POLICY_TRUST_ADMIN
            | POLICY_CREATE_ACCOUNT
            | POLICY_CREATE_SECRET
            | POLICY_CREATE_PRIVILEGE
            | POLICY_SET_DEFAULT_QUOTA_LIMITS
            | POLICY_SET_AUDIT_REQUIREMENTS
            | POLICY_AUDIT_LOG_ADMIN
            | POLICY_SERVER_ADMIN
            | POLICY_LOOKUP_NAMES
            | POLICY_NOTIFICATION) as u32;
        let attrs = LSA_OBJECT_ATTRIBUTES::default();
        let mut ph: LSA_HANDLE = LSA_HANDLE(0);
        if LsaOpenPolicy(None, &attrs, policy_access, &mut ph).is_ok() {
            let right = SE_SERVICE_LOGON_NAME;
            let rights = [LSA_UNICODE_STRING {
                Length: (right.len() * 2) as u16,
                MaximumLength: ((right.len() + 1) * 2) as u16,
                Buffer: PWSTR(right.as_ptr() as *mut u16),
            }];
            let _ = LsaAddAccountRights(ph, sid, &rights);
            let _ = LsaClose(ph);
        }
    }
}

/// 配置故障恢复: 按 failure_action 选择动作序列（restart 默认/reboot/none）
fn set_failure_actions(svc: SC_HANDLE, reset_sec: u32, delay_ms: u32,
    failure_action: Option<&str>) -> Result<(), String> {
    unsafe {
        use windows::Win32::System::Services::SC_ACTION;
        let action_kind = match failure_action.map(|s| s.to_lowercase()).as_deref() {
            Some("reboot") => windows::Win32::System::Services::SC_ACTION_REBOOT,
            Some("none") => windows::Win32::System::Services::SC_ACTION_NONE,
            _ => windows::Win32::System::Services::SC_ACTION_RESTART,
        };
        let actions = [
            SC_ACTION { Type: action_kind, Delay: delay_ms },
            SC_ACTION { Type: action_kind, Delay: delay_ms },
        ];

        let fa = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: reset_sec,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_ptr() as *mut _,
        };

        // 失败必须传播，不能静默缺失（P2-3）
        ChangeServiceConfig2W(svc, SERVICE_CONFIG_FAILURE_ACTIONS, Some(&fa as *const _ as *const _))
            .map_err(|e| format!("{}: {e}", "Failed to set failure actions"))
    }
}

/// 将分号分隔的依赖字符串转换为 SC multi-sz 格式
pub(crate) fn build_dependency_string(dependencies: Option<&str>) -> Option<String> {
    let deps = dependencies?;
    if deps.trim().is_empty() {
        return None;
    }
    let parts: Vec<&str> = deps
        .split(&[';', ',', ':'][..])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect();
    if parts.is_empty() {
        return None;
    }
    // CreateService 期望格式: "Svc1\0Svc2\0\0"（multi-sz 双 null 结尾，此处显式给出）
    Some(parts.join("\0") + "\0\0")
}

fn uninstall_service_scm(service_name: &str) -> Result<(), String> {
    unsafe {
        let service_name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(service_name_wide.as_ptr()),
            SERVICE_STOP | SERVICE_DELETE_ACCESS,
        ).map_err(|e| format!("{}: {e}", "Failed to open service"))?;
        let result = DeleteService(svc);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        if result.is_err() {
            return Err("Failed to delete service".into());
        }
    }
    Ok(())
}

/// 等待服务从 SCM 完全移除，避免立即以同名重建触发延迟删除竞态（注册成功但稍后消失）
fn wait_service_deleted(service_name: &str) {
    for _ in 0..25 {
        // 最长 5 秒
        unsafe {
            let name_wide = to_wide(service_name);
            let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS);
            let scm = match scm {
                Ok(h) => h,
                Err(_) => return,
            };
            let result = OpenServiceW(scm, PCWSTR::from_raw(name_wide.as_ptr()), SERVICE_QUERY_STATUS);
            let _ = CloseServiceHandle(scm);
            if let Err(e) = result {
                // 1060 = ERROR_SERVICE_DOES_NOT_EXIST → 已完全删除
                if e.code().0 as u32 & 0xFFFF == 1060 {
                    return;
                }
            }
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn start_service(service_name: &str, timeout: Duration) -> Result<(), String> {
    let status = get_status_raw(service_name)?;
    if status.dwCurrentState == windows::Win32::System::Services::SERVICE_RUNNING {
        return Ok(());
    }

    unsafe {
        let name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(scm, PCWSTR::from_raw(name_wide.as_ptr()), SERVICE_START)
            .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        let result = StartServiceW(svc, None);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        if result.is_err() {
            return Err("Failed to start service".into());
        }
    }

    // 等待运行状态
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let status = get_status_raw(service_name)?;
        if status.dwCurrentState == windows::Win32::System::Services::SERVICE_RUNNING {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err("Timeout waiting for service to start".into());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn stop_service(service_name: &str, timeout: Duration) -> Result<(), String> {
    let status = get_status_raw(service_name)?;
    if status.dwCurrentState == SERVICE_STOPPED {
        return Ok(());
    }

    unsafe {
        let name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(scm, PCWSTR::from_raw(name_wide.as_ptr()), SERVICE_STOP)
            .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        let mut svc_status = SERVICE_STATUS::default();
        let result = ControlService(svc, SERVICE_CONTROL_STOP, &mut svc_status);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        if result.is_err() {
            return Err("Failed to stop service".into());
        }
    }

    // 等待停止
    let deadline = std::time::Instant::now() + timeout;
    loop {
        let status = get_status_raw(service_name)?;
        if status.dwCurrentState == SERVICE_STOPPED {
            return Ok(());
        }
        if std::time::Instant::now() > deadline {
            return Err("Timeout waiting for service to stop".into());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

pub(crate) fn restart_service(service_name: &str, stop_timeout: Duration, start_timeout: Duration) -> Result<(), String> {
    stop_service(service_name, stop_timeout)?;
    thread::sleep(Duration::from_secs(2));
    start_service(service_name, start_timeout)
}

pub(crate) fn get_status(service_name: &str) -> Result<String, String> {
    let status = get_status_raw(service_name)?;
    match status.dwCurrentState {
        windows::Win32::System::Services::SERVICE_RUNNING => Ok("Running".into()),
        SERVICE_STOPPED => Ok("Stopped".into()),
        windows::Win32::System::Services::SERVICE_START_PENDING => Ok("Start Pending".into()),
        windows::Win32::System::Services::SERVICE_STOP_PENDING => Ok("Stop Pending".into()),
        windows::Win32::System::Services::SERVICE_PAUSED => Ok("Paused".into()),
        windows::Win32::System::Services::SERVICE_PAUSE_PENDING => Ok("Pause Pending".into()),
        windows::Win32::System::Services::SERVICE_CONTINUE_PENDING => Ok("Continue Pending".into()),
        _ => Ok(format!("Unknown ({:?})", status.dwCurrentState)),
    }
}

fn get_status_raw(service_name: &str) -> Result<SERVICE_STATUS, String> {
    unsafe {
        let name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(scm, PCWSTR::from_raw(name_wide.as_ptr()), SERVICE_QUERY_STATUS)
            .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        let mut status = SERVICE_STATUS::default();
        let result = QueryServiceStatus(svc, &mut status);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        if result.is_err() {
            return Err("Failed to query status".into());
        }
        Ok(status)
    }
}

fn service_exists(service_name: &str) -> bool {
    get_status_raw(service_name).is_ok()
}

// ==================== 服务宿主/更新程序入口 (SCM) ====================

/// 当前进程是否为更新程序模式（true=-internal --updater, false=宿主）
static SCM_UPDATER_MODE: Mutex<Option<bool>> = Mutex::new(None);
/// 共享宿主显式服务名（-internal --run <name> 传入；None 时取 exe 文件名）
static SCM_EXPLICIT_NAME: Mutex<Option<String>> = Mutex::new(None);
static STOP_FLAG: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
/// 是否启用 SCM preshutdown 通知（由宿主导入，scm_status_params 读取后决定上报 SERVICE_ACCEPT_PRESHUTDOWN）
static PRESHUTDOWN_ENABLED: AtomicBool = AtomicBool::new(false);
/// SCM 状态上报 dwWaitHint（毫秒），默认 1 小时（覆盖 prestart 钩子 60s 与启动前下载 300s）
static SCM_WAIT_HINT_MS: AtomicU32 = AtomicU32::new(3_600_000);
/// 宿主主循环 SCM 信号轮询间隔（毫秒）
static SCM_SLEEP_TIME_MS: AtomicU32 = AtomicU32::new(500);

/// 开关 SCM preshutdown 通知（host 在 on_start 读取配置后调用）
pub(crate) fn set_preshutdown_enabled(enabled: bool) {
    PRESHUTDOWN_ENABLED.store(enabled, Ordering::SeqCst);
}

/// 设置 SCM 状态上报 dwWaitHint（host 在 on_start 读取配置后调用）
pub(crate) fn set_scm_wait_hint_ms(ms: u32) {
    SCM_WAIT_HINT_MS.store(ms.max(1000), Ordering::SeqCst);
}

/// 设置宿主主循环轮询间隔（host 在 on_start 读取配置后调用）
pub(crate) fn set_scm_sleep_time_ms(ms: u32) {
    SCM_SLEEP_TIME_MS.store(ms.max(50), Ordering::SeqCst);
}

#[cfg(test)]
pub(crate) fn scm_wait_hint_ms() -> u32 {
    SCM_WAIT_HINT_MS.load(Ordering::SeqCst)
}

#[cfg(test)]
pub(crate) fn scm_sleep_time_ms() -> u32 {
    SCM_SLEEP_TIME_MS.load(Ordering::SeqCst)
}

/// 当前 SCM 注册的服务名: 更新程序使用保留名，共享宿主按显式名，普通宿主取自身文件名
fn scm_svc_name(updater: bool) -> String {
    if updater {
        "Osmium Service Checker".to_string()
    } else if let Some(name) = SCM_EXPLICIT_NAME.lock().unwrap().clone() {
        name
    } else {
        crate::service_host::ServiceHost::svc_name()
    }
}

fn scm_entry(updater_mode: bool, explicit_name: Option<String>) {
    use windows::Win32::System::Services::{SERVICE_TABLE_ENTRYW,
                                           StartServiceCtrlDispatcherW,
    };

    *SCM_UPDATER_MODE.lock().unwrap() = Some(updater_mode);
    *SCM_EXPLICIT_NAME.lock().unwrap() = explicit_name;
    let svc_name = scm_svc_name(updater_mode);

    // 重置停止标志
    STOP_FLAG.store(false, Ordering::SeqCst);
    SHUTDOWN_FLAG.store(false, Ordering::SeqCst);

    let name_wide = to_wide(&svc_name);

    unsafe {
        unsafe extern "system" fn service_main_wrapper(_argc: u32, _argv: *mut PWSTR) {
            scm_service_main();
        }

        let entry = SERVICE_TABLE_ENTRYW {
            lpServiceName: PWSTR::from_raw(name_wide.as_ptr() as *mut _),
            lpServiceProc: Some(service_main_wrapper),
        };
        let mut table = [entry, SERVICE_TABLE_ENTRYW::default()];

        if StartServiceCtrlDispatcherW(table.as_mut_ptr()).is_err() {
            eprintln!("{}", red("Error: service control dispatcher failed — must be launched by SCM"));
        }
    }
}

fn scm_service_main() {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SERVICE_RUNNING, SERVICE_START_PENDING, SERVICE_STOPPED,
        SERVICE_STOP_PENDING,
    };

    let updater = SCM_UPDATER_MODE.lock().unwrap().unwrap_or(false);

    let svc_name = scm_svc_name(updater);
    let svc_name_wide = to_wide(&svc_name);

    unsafe {
        unsafe extern "system" fn ctrl_handler(
            ctrl: u32,
            _event_type: u32,
            _data: *mut std::ffi::c_void,
            _ctx: *mut std::ffi::c_void,
        ) -> u32 {
            let ctrl_val = ctrl as i32;
            match ctrl_val {
                x if x == SERVICE_CONTROL_STOP as i32 => {
                    STOP_FLAG.store(true, Ordering::SeqCst);
                    NO_ERROR.0
                }
                x if x == windows::Win32::System::Services::SERVICE_CONTROL_SHUTDOWN as i32
                    || x == windows::Win32::System::Services::SERVICE_CONTROL_PRESHUTDOWN as i32 =>
                {
                    SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
                    NO_ERROR.0
                }
                _ => 1,
            }
        }

        let handler = RegisterServiceCtrlHandlerExW(
            PCWSTR::from_raw(svc_name_wide.as_ptr()),
            Some(ctrl_handler),
            None,
        );

        let status_handle = match handler {
            Ok(h) => h,
            Err(e) => {
                eprintln!("{}", red(&f("Failed to register SCM control handler for '{0}': {1}", &[&svc_name, &e.to_string()])));
                return;
            }
        };

        // SCM 默认只等待 30 秒启动完成，但 prestart 钩子最长 60s、启动前下载最长 300s，
        // 必须先申请额外启动时间（waitHint，可配 scm_wait_hint_ms），否则 SCM 会判定服务无响应并终止
        report_scm_status(status_handle, SERVICE_START_PENDING.0, 0, SCM_WAIT_HINT_MS.load(Ordering::SeqCst));

        if updater {
            report_scm_status(status_handle, SERVICE_RUNNING.0, 0, 0);
            upgrade_outdated_hosts();
            report_scm_status(status_handle, SERVICE_STOPPED.0, 0, 0);
        } else {
            let mut host = crate::service_host::ServiceHost::new();
            // 共享宿主（-internal --run）按显式名加载 svcs\<name>\<name>.osiml；
            // 普通宿主按 exe 旁配置启动
            let started = if let Some(name) = SCM_EXPLICIT_NAME.lock().unwrap().clone() {
                host.on_start_with_name(&name)
            } else {
                host.on_start()
            };
            if !started {
                report_scm_status(status_handle, SERVICE_STOPPED.0, 0, 0);
                return;
            }
            report_scm_status(status_handle, SERVICE_RUNNING.0, 0, 0);

            loop {
                // 检查 SCM 停止/关机信号
                if STOP_FLAG.load(Ordering::SeqCst) {
                    host.write_log("host", "SCM stop signal received");
                    // 优雅停止最长 10s + poststop 钩子最长 30s，超出 SCM 默认 30s 停止时限，
                    // 先报 STOP_PENDING 并申请额外停止时间
                    report_scm_status(status_handle, SERVICE_STOP_PENDING.0, 0, SCM_WAIT_HINT_MS.load(Ordering::SeqCst));
                    host.on_stop();
                    break;
                }
                if SHUTDOWN_FLAG.load(Ordering::SeqCst) {
                    host.write_log("host", "SCM shutdown signal received");
                    report_scm_status(status_handle, SERVICE_STOP_PENDING.0, 0, SCM_WAIT_HINT_MS.load(Ordering::SeqCst));
                    host.on_shutdown();
                    break;
                }
                // 子进程退出监控与异常自动重启由宿主内部处理
                if !host.tick() {
                    break;
                }
                thread::sleep(Duration::from_millis(SCM_SLEEP_TIME_MS.load(Ordering::SeqCst) as u64));
            }

            report_scm_status(status_handle, SERVICE_STOPPED.0, 0, 0);
        }
    }
}

fn report_scm_status(
    handle: windows::Win32::System::Services::SERVICE_STATUS_HANDLE,
    state: u32,
    exit_code: u32,
    wait_hint: u32,
) {
    use windows::Win32::System::Services::{
        SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE, SERVICE_WIN32_OWN_PROCESS, SetServiceStatus,
    };
    let (controls, checkpoint) = scm_status_params(state);
    unsafe {
        let status = SERVICE_STATUS {
            dwServiceType: SERVICE_WIN32_OWN_PROCESS,
            dwCurrentState: SERVICE_STATUS_CURRENT_STATE(state),
            dwControlsAccepted: controls,
            dwWin32ExitCode: exit_code,
            dwServiceSpecificExitCode: 0,
            dwCheckPoint: checkpoint,
            dwWaitHint: wait_hint,
        };
        if let Err(e) = SetServiceStatus(handle, &status) {
            // 上报失败不能静默忽略（服务模式下 stderr 不可见，尽力记录）
            eprintln!("{}", red(&f("[scm] SetServiceStatus failed: {0}", &[&e.to_string()])));
        }
    }
}

/// SCM 状态上报参数: 返回 (dwControlsAccepted, dwCheckPoint)。
/// PENDING/STOPPED 阶段不得接受停止/关机控制码，仅 RUNNING 接受；PENDING checkpoint 非零（P2-1）
pub(crate) fn scm_status_params(state: u32) -> (u32, u32) {
    use windows::Win32::System::Services::{
        SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
        SERVICE_START_PENDING, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    };
    let controls = if state == SERVICE_START_PENDING.0
        || state == SERVICE_STOP_PENDING.0
        || state == SERVICE_STOPPED.0
    {
        0
    } else {
        let base = SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN;
        // preshutdown 配置开启时额外上报 SERVICE_ACCEPT_PRESHUTDOWN（系统关停获更长优雅时间）
        if PRESHUTDOWN_ENABLED.load(Ordering::SeqCst) {
            base | SERVICE_ACCEPT_PRESHUTDOWN
        } else {
            base
        }
    };
    let checkpoint = if state == SERVICE_START_PENDING.0 || state == SERVICE_STOP_PENDING.0 {
        1
    } else {
        0
    };
    (controls, checkpoint)
}

// ==================== 宽字符串工具 ====================

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ==================== 文件版本工具（供测试与更新器使用） ====================

/// 读取文件版本（4 段）; 更新器逐服务替换已移除，现仅供单元测试验证版本读取
#[cfg(test)]
pub(crate) fn get_file_version(path: &str) -> Option<String> {
    use windows::Win32::Storage::FileSystem::{
        GetFileVersionInfoSizeW, GetFileVersionInfoW, VS_FIXEDFILEINFO, VerQueryValueW,
    };

    unsafe {
        let path_wide = to_wide(path);
        let mut handle: u32 = 0;
        let size = GetFileVersionInfoSizeW(PCWSTR::from_raw(path_wide.as_ptr()), Some(&mut handle));
        if size == 0 {
            return None;
        }

        let mut buf: Vec<u8> = vec![0; size as usize];
        if GetFileVersionInfoW(PCWSTR::from_raw(path_wide.as_ptr()), Some(0), size, buf.as_mut_ptr() as *mut _).is_err() {
            return None;
        }

        let mut value_ptr: *mut std::ffi::c_void = std::ptr::null_mut();
        let mut value_len: u32 = 0;

        // 查询 VS_FIXEDFILEINFO（\\ 子块）
        let sub_block_fixed = to_wide("\\");
        if !VerQueryValueW(
            buf.as_ptr() as *const _,
            PCWSTR::from_raw(sub_block_fixed.as_ptr()),
            &mut value_ptr,
            &mut value_len,
        ).as_bool() {
            return None;
        }

        // 指针非空 + 长度足够才解引用（CodeQL: 防止访问无效指针导致未定义行为）
        if !value_ptr.is_null() && value_len as usize >= size_of::<VS_FIXEDFILEINFO>() {
            let info = &*(value_ptr as *const VS_FIXEDFILEINFO);
            let major = (info.dwFileVersionMS >> 16) & 0xFFFF;
            let minor = info.dwFileVersionMS & 0xFFFF;
            let build = (info.dwFileVersionLS >> 16) & 0xFFFF;
            let revision = info.dwFileVersionLS & 0xFFFF;
            // 读取完整 4 段
            // （build.rs 生成为 major.minor.build.revision）
            Some(format!("{}.{}.{}.{}", major, minor, build, revision))
        } else {
            None
        }
    }
}

#[cfg(test)]
pub(crate) fn compare_versions(a: &str, b: &str) -> i32 {
    let parse = |v: &str| -> Vec<u32> {
        v.split('.')
            .filter_map(|s| s.parse::<u32>().ok())
            .collect()
    };
    let va = parse(a);
    let vb = parse(b);
    for i in 0..va.len().max(vb.len()) {
        let na = va.get(i).copied().unwrap_or(0);
        let nb = vb.get(i).copied().unwrap_or(0);
        match na.cmp(&nb) {
            std::cmp::Ordering::Greater => return 1,
            std::cmp::Ordering::Less => return -1,
            std::cmp::Ordering::Equal => {}
        }
    }
    0
}

// ==================== 配置加密（DPAPI） ====================

/// DPAPI 加密值前缀（版本化，便于将来换算法）
const DPAPI_ENC_PREFIX: &str = "enc:OSMIUM1:";

/// DPAPI 加密（机器级，LocalSystem 与本地管理员均可解密）:
/// 用于部署时加密 .osiml 中的敏感字段（密码等），避免明文落盘（P1-2）
pub(crate) fn dpapi_encrypt(plain: &str) -> Option<String> {
    use base64::Engine as _;
    use windows::Win32::Security::Cryptography::{CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
                                                 CryptProtectData,
    };
    let bytes = plain.as_bytes();
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: bytes.len() as u32,
        pbData: bytes.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if CryptProtectData(
            &in_blob,
            PCWSTR::null(),
            None,
            None,
            None,
            CRYPTPROTECT_LOCAL_MACHINE | CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .is_err()
        {
            return None;
        }
        if out_blob.pbData.is_null() {
            return None;
        }
        let cipher = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let b64 = base64::engine::general_purpose::STANDARD.encode(cipher);
        let _ = windows::Win32::Foundation::LocalFree(
            Some(windows::Win32::Foundation::HLOCAL(out_blob.pbData as *mut std::ffi::c_void)));
        Some(format!("{}{}", DPAPI_ENC_PREFIX, b64))
    }
}

/// DPAPI 解密: 仅处理 enc:OSMIUM1: 前缀的值；明文/旧格式/解密失败原样返回（兼容 inplace 手写配置）
pub(crate) fn dpapi_decrypt(value: &str) -> String {
    let Some(rest) = value.strip_prefix(DPAPI_ENC_PREFIX) else {
        return value.to_string();
    };
    use base64::Engine as _;
    use windows::Win32::Security::Cryptography::{CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB,
                                                 CryptUnprotectData,
    };
    let Ok(cipher) = base64::engine::general_purpose::STANDARD.decode(rest) else {
        return value.to_string();
    };
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if CryptUnprotectData(&in_blob, None, None, None, None, CRYPTPROTECT_UI_FORBIDDEN, &mut out_blob).is_err() {
            return value.to_string();
        }
        if out_blob.pbData.is_null() {
            return value.to_string();
        }
        let plain = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let s = String::from_utf8_lossy(plain).into_owned();
        let _ = windows::Win32::Foundation::LocalFree(
            Some(windows::Win32::Foundation::HLOCAL(out_blob.pbData as *mut std::ffi::c_void)));
        s
    }
}

/// 解密配置中的敏感字段（service_password / download_password / 共享映射密码）
pub(crate) fn decrypt_sensitive(config: &mut ServiceConfig) {
    if let Some(p) = &mut config.service_password {
        *p = dpapi_decrypt(p);
    }
    if let Some(p) = &mut config.download_password {
        *p = dpapi_decrypt(p);
    }
    if let Some(mappers) = &mut config.shared_directory_mappers {
        for m in mappers {
            if let Some(p) = &mut m.password {
                *p = dpapi_decrypt(p);
            }
        }
    }
}
