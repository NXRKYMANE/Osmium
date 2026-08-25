use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process;
use std::sync::atomic::Ordering;
use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicU32, AtomicU64},
};
use std::thread;
use std::time::{Duration, Instant};

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
    ChangeServiceConfig2W, ChangeServiceConfigW, CloseServiceHandle, ControlService,
    CreateServiceW, DeleteService, ENUM_SERVICE_TYPE, OpenSCManagerW, OpenServiceW,
    QUERY_SERVICE_CONFIGW, QueryServiceConfig2W, QueryServiceConfigW, QueryServiceStatus,
    SC_ACTION_REBOOT, SC_ACTION_RESTART, SC_HANDLE, SC_MANAGER_ALL_ACCESS, SC_MANAGER_CONNECT,
    SERVICE_AUTO_START, SERVICE_CONFIG_DELAYED_AUTO_START_INFO, SERVICE_CONFIG_DESCRIPTION,
    SERVICE_CONFIG_FAILURE_ACTIONS, SERVICE_CONTROL_STOP, SERVICE_DELAYED_AUTO_START_INFO,
    SERVICE_DEMAND_START, SERVICE_DESCRIPTIONW, SERVICE_DISABLED, SERVICE_ERROR,
    SERVICE_ERROR_NORMAL, SERVICE_FAILURE_ACTIONSW, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_START_TYPE, SERVICE_STATUS, SERVICE_STOP,
    SERVICE_STOP_PENDING, SERVICE_STOPPED, SERVICE_WIN32_OWN_PROCESS, StartServiceW,
};
// SERVICE_INTERACTIVE_PROCESS 位于 SystemServices（u32 位标志，非 ENUM_SERVICE_TYPE）
use windows::Win32::System::SystemServices::SERVICE_INTERACTIVE_PROCESS;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
use windows::core::{PCWSTR, PWSTR};

use crate::service_config::ServiceConfig;

// ==================== 常量 ====================

/// 模板格式化: 将 {0} {1}... 依次替换为 args（单遍扫描，避免逐项 replace 的 O(n²)）
pub(crate) fn f(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        // 尝试解析 {数字} 占位符；非占位符（{} / {abc} / 未闭合）原样保留
        match after.find('}') {
            Some(end_rel)
                if after[..end_rel]
                    .parse::<usize>()
                    .ok()
                    .and_then(|i| args.get(i))
                    .is_some() =>
            {
                let i: usize = after[..end_rel].parse().unwrap();
                out.push_str(args[i]);
                rest = &after[end_rel + 1..];
            }
            _ => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 刷新程序的启动类型 — 自动启动
const SVC_REFRESHER_START_MODE: SERVICE_START_TYPE = SERVICE_AUTO_START;

/// 刷新程序为一次性任务，无需故障恢复
const SVC_REFRESHER_FAILURE_RESET_SEC: u32 = 0;

/// 刷新程序为一次性任务，无需重启延迟
const SVC_REFRESHER_RESTART_DELAY_MS: u32 = 0;

/// 超过此天数的服务日志将在启动时被清理
const LOG_RETENTION_DAYS: i64 = 30;

/// 超过此天数的 zip 归档将在启动时被清理（约半年；归档压缩后更省磁盘，保留期更长）
const LOG_ZIP_RETENTION_DAYS: i64 = 180;

/// SCM 启停/重启操作超时（秒）
pub(crate) const SCM_OP_TIMEOUT_SECS: u64 = 30;

/// 服务名校验失败的错误消息模板（多处共用，避免文案漂移）
const INVALID_NAME_MSG: &str = "Invalid service name: '{0}'. Service names must be 1-256 chars, must not be '.' or '..', and must not contain '\\', '/' or control characters.";

/// 内部服务刷新程序保留名冲突的错误消息模板（多处共用）
const RESERVED_NAME_MSG: &str = "Service name '{0}' is reserved for the internal Osmium Service Refresher. Use a different service_name.";

/// 服务名已被其他服务注册的错误消息模板（多处共用）
const ALREADY_REGISTERED_MSG: &str = "Service name '{0}' is already registered by a different service. Use a different service_name or uninstall it first.";

/// 服务名是否为刷新程序保留名
pub(crate) fn is_refresher_reserved_name(name: &str) -> bool {
    name.eq_ignore_ascii_case("Osmium Service Refresher")
}

/// CLI 输出统一前缀（对齐 WinSW 的 "WinSW Service Management Interface" 输出风格）
pub(crate) const CLI_PREFIX: &str = "Osmium Service Management Interface";

const SERVICE_DELETE_ACCESS: u32 = 0x00010000;

// ==================== 配置签名（RSA-SHA256） ====================

/// 签名/校验密钥文件（exe 旁）: 私钥 osmium-sign.key（PKCS#8 PEM）用于 install 自动签名，
/// 公钥 osmium-public.pem（PKCS#8 PEM）用于宿主校验（require_signed_config=true 时）
const SIGN_KEY_FILE: &str = "osmium-sign.key";
const PUBLIC_KEY_FILE: &str = "osmium-public.pem";

/// exe 旁密钥文件路径（不存在返回 None）
fn key_file_adjacent(name: &str) -> Option<PathBuf> {
    let own = get_own_path();
    let dir = Path::new(&own).parent()?;
    let p = dir.join(name);
    p.exists().then_some(p)
}

/// 私钥是否可能被非管理员篡改/替换（文件或所在目录对低权限用户可写）。
/// 可写即能换成攻击者自己的密钥替任意配置签名——与"可读"同样致命，
/// 且复用既有 is_user_writable 的 ACL 判定基础设施（读判定需另建 Win32 解析链）
fn key_file_tamperable_by_unprivileged(path: &Path) -> bool {
    crate::service_core::is_user_writable(&path.to_string_lossy())
        || crate::service_core::is_user_writable(
            &path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        )
}

/// 对文件内容做 RSA-SHA256 签名，写入 `<path>`.sig（DER 二进制）;
/// 私钥来自 exe 旁 osmium-sign.key（PKCS#8 PEM）。返回是否成功
pub(crate) fn sign_config_file(path: &Path) -> bool {
    use rsa::pkcs1v15::SigningKey;
    use rsa::pkcs8::DecodePrivateKey;
    use rsa::sha2::Sha256;
    use rsa::signature::{RandomizedSigner, SignatureEncoding};
    use std::io::Write;
    let Some(key_path) = key_file_adjacent(SIGN_KEY_FILE) else {
        return false;
    };
    let Ok(key_pem) = std::fs::read_to_string(&key_path) else {
        return false;
    };
    let Ok(key) = rsa::RsaPrivateKey::from_pkcs8_pem(&key_pem) else {
        return false;
    };
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let signing_key = SigningKey::<Sha256>::new(key);
    let mut rng = rsa::rand_core::OsRng;
    let sig = signing_key.sign_with_rng(&mut rng, &data);
    let sig_bytes = sig.to_bytes();
    let sig_path = path.with_extension("sig");
    std::fs::File::create(&sig_path)
        .and_then(|mut f| f.write_all(&sig_bytes))
        .is_ok()
}

/// 校验配置文件签名: `<path>`.sig 存在且用 exe 旁 osmium-public.pem（PKCS#8 PEM 公钥）
/// RSA-SHA256 校验通过。公钥缺失/签名缺失/校验失败均返回 false（fail-closed）
pub(crate) fn verify_config_signature(path: &Path) -> bool {
    use rsa::pkcs1v15::{Signature, VerifyingKey};
    use rsa::pkcs8::DecodePublicKey;
    use rsa::sha2::Sha256;
    use rsa::signature::Verifier;
    let Some(pub_path) = key_file_adjacent(PUBLIC_KEY_FILE) else {
        return false;
    };
    let Ok(pub_pem) = std::fs::read_to_string(&pub_path) else {
        return false;
    };
    let Ok(pub_key) = rsa::RsaPublicKey::from_public_key_pem(&pub_pem) else {
        return false;
    };
    let sig_path = path.with_extension("sig");
    let Ok(sig) = std::fs::read(&sig_path) else {
        return false;
    };
    let Ok(data) = std::fs::read(path) else {
        return false;
    };
    let Ok(sig) = Signature::try_from(sig.as_slice()) else {
        return false;
    };
    VerifyingKey::<Sha256>::new(pub_key)
        .verify(&data, &sig)
        .is_ok()
}

/// 宿主加载部署配置前的签名校验入口:
/// require_signed_config=true 时要求 .sig 有效（fail-closed）；false 时跳过
pub(crate) fn check_config_signature(config: &ServiceConfig, path: &Path) -> Result<(), String> {
    if !config.require_signed_config {
        return Ok(());
    }
    if verify_config_signature(path) {
        Ok(())
    } else {
        Err(f(
            "Config signature verification failed for '{0}' (missing .sig, invalid signature, or public key not found next to the executable). Set require_signed_config=false to allow unsigned configs.",
            &[&path.display().to_string()],
        ))
    }
}

// ==================== SCM 宿主入口 & 服务安装部署 ====================

/// SCM 宿主入口（无参数、非交互时由 CLI 路由调用）
pub(crate) fn run_service_host() {
    scm_entry(false, None);
}

/// 共享宿主部署入口: SCM 以 `-internal --run <name>` 启动，显式指定服务名
pub(crate) fn run_service_host_with_name(name: &str) {
    scm_entry(false, Some(name.to_string()));
}

/// 服务刷新程序服务入口（-internal --refresher）
pub(crate) fn run_svc_refresher_service() {
    scm_entry(true, None);
}

/// 快速安装: 校验服务名/可执行路径合规，生成临时 TOML 配置，返回其路径。
/// 名称校验与 install_from_config_path 一致（含保留名）；路径须为已存在的绝对路径。
pub(crate) fn write_quick_config(name: &str, exe_path: &str) -> String {
    if !is_valid_service_name(name) {
        error(&f(INVALID_NAME_MSG, &[name]));
    }
    if is_refresher_reserved_name(name) {
        error(&f(RESERVED_NAME_MSG, &[name]));
    }
    let rooted = Path::new(exe_path).is_absolute() || exe_path.starts_with('\\');
    if !rooted {
        error(&f(
            "Quick install requires an absolute executable path (got: '{0}'). Use a full path like 'C:\\app\\service.exe'.",
            &[exe_path],
        ));
    }
    let exe = std::fs::canonicalize(exe_path).unwrap_or_else(|_| PathBuf::from(exe_path));
    if !exe.exists() {
        error(&f(
            "Invalid file path in service config: '{0}' does not exist or is not accessible. Check the executable path and try again.",
            &[exe_path],
        ));
    }
    // canonicalize 会带 \\?\ 前缀，写入配置前剥掉（与普通安装的配置路径格式保持一致）
    let exe = strip_verbatim_prefix(&exe);
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
    let content = toml::to_string_pretty(&config).unwrap_or_else(|e| {
        panic!(
            "{}",
            f("Failed to serialize config: {0}", &[&e.to_string()])
        )
    });
    // tmp 原子创建: 目录收紧仅 SYSTEM/Admin 可写后 create_new 拒绝替换（TOCTOU 防护）。
    // 目录选 ProgramData\Osmium\quick 而非系统 temp——temp 所有用户可读， 快速安装配置（可能含密码）落到 temp 会被其他用户读取
    let quick_dir = registry_dir()
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData\\Osmium"))
        .join("quick");
    let _ = std::fs::create_dir_all(&quick_dir);
    if !secure_directory(&quick_dir.to_string_lossy()) {
        error(&f(
            "Quick install failed: cannot secure temporary config directory '{0}'",
            &[&quick_dir.to_string_lossy()],
        ));
    }
    // 清理历史残留的临时配置（安装中途报错退出时未来得及删除；配置不含敏感字段，
    // 超过 1 小时即视为残留——正常快速安装全程远小于 1 小时）
    sweep_stale_quick_configs(&quick_dir);
    let tmp = quick_dir.join(format!("osmium-quick-{}-{}.toml", process::id(), name));
    let write = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(content.as_bytes())
            })
    };
    write().unwrap_or_else(|e| {
        panic!(
            "{}",
            f("Failed to write temp config: {0}", &[&e.to_string()])
        )
    });
    tmp.to_string_lossy().to_string()
}

/// 清理 quick 目录中超过 1 小时的历史临时配置（osmium-quick-*.toml，快速安装失败退出时的残留）
pub(crate) fn sweep_stale_quick_configs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("osmium-quick-") || !name.ends_with(".toml") {
            continue;
        }
        let stale = std::fs::metadata(entry.path())
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| t.elapsed().unwrap_or_default() > Duration::from_secs(3600))
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

pub(crate) fn install_from_config_path(config_path_str: &str) {
    let config_path =
        std::fs::canonicalize(config_path_str).unwrap_or_else(|_| PathBuf::from(config_path_str));

    if !config_path.exists() {
        error(&f(
            "Config file not found: '{0}'. Check the path and try again.",
            &[config_path_str],
        ));
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

    // 保留名冲突: "Osmium Service Refresher" 是内部开机刷新程序的服务名，
    // 若允许用户服务同名，install-refresher 会误停/误卸用户的服务
    if is_refresher_reserved_name(&svc_name) {
        error(&f(RESERVED_NAME_MSG, &[&svc_name]));
        return;
    }

    let svc_display_name = config.service_display_name.clone();
    let svc_description = config.service_description.clone();
    // 部署目录判定提前（inplace = exe 所在目录，平台 = svcs\<name>）:
    // 安装校验与宿主运行时的 %BASE% 语义保持一致，先展开 %VAR%/%BASE% 再 canonicalize， 否则含环境变量/相对路径的 exe 路径会被误判"不存在"
    let inplace = config.deploy_inplace;
    let own_exe = get_own_path();
    let install_deploy_dir = if inplace {
        Path::new(&own_exe)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        base_dir(&svc_name).to_string_lossy().to_string()
    };
    let exe_expanded =
        crate::service_host::expand_env_value(&config.service_executable_path, &install_deploy_dir);
    let svc_exe_path =
        std::fs::canonicalize(&exe_expanded).unwrap_or_else(|_| PathBuf::from(&exe_expanded));

    println!("{CLI_PREFIX}: Verifying service registration info");
    // 仅校验"安装时即应存在"的普通绝对路径: download_url 目标启动时才下载、
    // 相对路径按部署目录解析，安装时不存在属正常
    let has_download = has_download(&config);
    let exe_path_str = &exe_expanded;
    let rooted = Path::new(exe_path_str).is_absolute() || exe_path_str.starts_with('\\');
    if !has_download && rooted && !svc_exe_path.exists() {
        error(&f(
            "Invalid file path in service config: service_executable_path '{0}' does not exist. Check the path in the config and try again.",
            &[exe_path_str],
        ));
        return;
    }
    // P0-3: 平台部署同样校验目标 exe 及其目录不被非管理员可写（对齐 inplace 的 P0-1）。
    // 若 exe 位于 Downloads/Public/工作区等可写位置，任意用户可替换它，宿主以 LocalSystem 启动时即提权；工作目录同理（可放恶意 DLL 侧加载）。
    let mut unsafe_paths: Vec<String> = Vec::new();
    collect_unsafe_paths(
        &config,
        &install_deploy_dir,
        &svc_exe_path,
        &mut unsafe_paths,
    );
    if !unsafe_paths.is_empty() {
        error(&f(
            "Application error: {0}",
            &[&format!(
                "service_executable_path (or working_directory) is writable by unprivileged users: {}. Move the executable to a SYSTEM/Administrators-only location (e.g. Program Files).",
                unsafe_paths.join(", ")
            )],
        ));
        return;
    }

    // 原地模式（deploy_inplace）: 不复制宿主到 ProgramData，直接用当前 exe 注册。
    // 宿主启动时按"同目录同名 toml"读取配置，因此配置必须与 exe 同名同目录
    if inplace {
        check_inplace_requirements(&svc_name, &own_exe, config_path_str, &config_path);
    }

    // 已注册判定以 SCM 为准。不能用 is_registered:
    // 同名外部服务会被其绕过冲突检测，失败回滚还会误删外部服务
    // 更新路径的 logs 备份: 注册全链路成功或失败收尾时统一还原（见下方各分支）
    let mut logs_backup: Option<PathBuf> = None;
    let is_update = if service_exists(&svc_name) {
        // 来源冲突检测: 防止同名但来源不同的服务被误覆盖
        if inplace {
            // 原地模式: 已注册服务的 ImagePath 必须与当前 exe 一致；
            // 未注册/ImagePath 读不到时跳过冲突检测
            if let Some(current_image) = get_service_image_path(&svc_name)
                && !current_image
                    .trim_matches('"')
                    .eq_ignore_ascii_case(&own_exe)
            {
                error(&f(ALREADY_REGISTERED_MSG, &[&svc_name]));
            }
        } else {
            // 平台部署: 已部署 .osiml 可对比时要求可执行路径/参数一致才允许覆盖更新；
            // .osiml 缺失/损坏时退回 ImagePath 归属判定，仅 Osmium 部署可覆盖修复
            let config_dest = deployed_config_path(&svc_name);
            if !can_overwrite_source(
                config_dest.to_str().unwrap_or(""),
                config_path_str,
                &svc_name,
            ) {
                error(&f(ALREADY_REGISTERED_MSG, &[&svc_name]));
            }
        }
        // 更新已注册服务：force_remove_service 会删除整个 svcs 目录（含 logs），
        // 先临时挪出 logs 到系统临时目录；还原时机后置——注册全链路成功才回填，
        // 任一失败分支也先还原再退出（旧实现失败时 logs 随部署目录一并被删）
        logs_backup = backup_service_logs(&svc_name);
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
        // 防普通用户预建目录/junction 诱导 SYSTEM 刷新器误删服务；加固失败必须中止安装（防 P0-2）
        let osmium_dir = registry_dir()
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| registry_dir().to_string_lossy().to_string());
        let _ = std::fs::create_dir_all(&osmium_dir);
        let _ = std::fs::create_dir_all(registry_dir());
        let _ = std::fs::create_dir_all(&base_dir);
        if !secure_directory(&osmium_dir)
            || !secure_directory(&registry_dir().to_string_lossy())
            || !secure_directory(&base_dir.to_string_lossy())
        {
            rollback_registration(
                &svc_name,
                &base_dir,
                &mut logs_backup,
                "Failed to deploy service files",
            );
        }
        let config_dest = deployed_config_path(&svc_name);
        if !write_deployed_config(config_path_str, &config_dest) {
            rollback_registration(
                &svc_name,
                &base_dir,
                &mut logs_backup,
                "Failed to deploy service files",
            );
        }
        // 配置签名: exe 旁存在 osmium-sign.key 时对部署配置自动签名（<name>.sig），
        // 宿主 require_signed_config=true 时校验；签名失败必须中止（防未签名配置伪装成已签名）
        if key_file_adjacent(SIGN_KEY_FILE).is_some() && !sign_config_file(&config_dest) {
            rollback_registration(
                &svc_name,
                &base_dir,
                &mut logs_backup,
                "Failed to sign deployed config",
            );
        }
        // 共享宿主: 所有服务复用框架安装目录的同一份 exe（不再每服务复制副本）；
        // 框架未安装（源码直跑）时回退当前 exe；服务名允许空格，ImagePath 须引号包裹
        let shared_host = if install_path().exists() {
            install_path()
        } else {
            PathBuf::from(&own_exe)
        };
        // ImagePath 必须加引号: 服务名允许空格，未加引号的路径会被 SCM 按首空格截断解析，
        // 攻击者可投放较短前缀路径对应的恶意 EXE 由 LocalSystem 启动
        format!(
            "\"{}\" -internal --run \"{}\"",
            shared_host.display(),
            svc_name
        )
    };

    let (start_mode, delayed_auto) = parse_start_mode(config.service_start_mode.as_deref());
    let failure_reset = if config.failure_reset_sec > 0 {
        config.failure_reset_sec
    } else {
        86400
    };
    let restart_delay = if config.restart_delay_ms > 0 {
        config.restart_delay_ms
    } else {
        60000
    };

    // 交互式服务（interactive=true）仅允许 LocalSystem 账户（CreateServiceW 对
    // 其他账户返回 ERROR_INVALID_PARAMETER 0x80070057，提前给出明确提示）
    if config.interactive
        && config
            .service_account
            .as_deref()
            .is_some_and(|a| !a.trim().is_empty())
    {
        let account = config.service_account.as_deref().unwrap_or("");
        if !account.eq_ignore_ascii_case("LocalSystem")
            && !account.eq_ignore_ascii_case("NT AUTHORITY\\SYSTEM")
        {
            error(&f(
                "Application error: {0}",
                &[
                    "interactive=true requires the LocalSystem account. Remove service_account or set it to LocalSystem.",
                ],
            ));
        }
    }

    // service_account="virtual" → NT SERVICE\<服务名> 虚拟账户（免密码、权限最小化）；
    // 其余值原样传递（含 LocalSystem 与自定义账户）
    let virtual_account = if config
        .service_account
        .as_deref()
        .map(|a| a.eq_ignore_ascii_case("virtual"))
        .unwrap_or(false)
    {
        Some(format!("NT SERVICE\\{}", svc_name))
    } else {
        config.service_account.clone()
    };

    // gMSA 检测: 账户名以 $ 结尾（如 DOMAIN\svc-gmsa$）是组托管服务账户——
    // 需域环境且不能配密码（SCM 自动取托管凭据），提前给出提示避免配置混淆
    if let Some(account) = config.service_account.as_deref()
        && account.trim().ends_with('$')
        && !account.trim().eq_ignore_ascii_case("LocalSystem")
    {
        println!(
            "{CLI_PREFIX}: Note: service_account ends with '$' (gMSA). Group Managed Service Accounts are resolved by the domain controller and must not use service_password."
        );
    }

    match install_service_scm(&InstallServiceParams {
        service_name: &svc_name,
        display_name: &svc_display_name,
        description: &svc_description,
        executable_path: &bin_path,
        start_mode,
        failure_reset_sec: failure_reset as u32,
        restart_delay_ms: restart_delay as u32,
        dependencies: config.service_dependencies.as_deref(),
        service_account: virtual_account.as_deref(),
        // 虚拟账户无密码；显式提供密码时仍透传（自定义账户场景）
        password: if virtual_account.is_some() {
            None
        } else {
            config.service_password.as_deref()
        },
        delayed_auto_start: delayed_auto,
        interactive: config.interactive,
        failure_action: config.failure_action.as_deref(),
        // virtual 虚拟账户必须授予 SeServiceLogonRight（自动开启，免用户配置）
        allow_service_logon: config.allow_service_logon || virtual_account.is_some(),
        security_descriptor: config.security_descriptor.as_deref(),
    }) {
        Ok(()) => {
            // 更新路径: 注册全链路成功，把备份的 logs 回填到重建的部署目录（首次安装无备份为空操作）
            restore_service_logs(&svc_name, logs_backup.take());
            // virtual 账户: 授权 NT SERVICE\<name> 遍历部署链（Osmium/svcs 仅 X 权限，
            // 不可读其他服务目录）并读写自身部署目录（M）——目录 ACL 默认仅 SYSTEM/Admin； inplace 时部署目录 = exe 所在目录（logs/pid/metrics 写在那里）
            if let Some(acct) = &virtual_account {
                let target_dir = if inplace {
                    Path::new(&own_exe)
                        .parent()
                        .map(|p| p.to_string_lossy().to_string())
                        .unwrap_or_default()
                } else {
                    base_dir.to_string_lossy().to_string()
                };
                grant_virtual_account_access(acct, &target_dir);
            }
            println!(
                "{CLI_PREFIX}: {}",
                if is_update {
                    "Service updated successfully"
                } else {
                    "Service registered successfully"
                }
            );
            // 配置变更审计（event_log=true 时写事件日志，ID 1005）
            if config.event_log {
                crate::service_host::report_event_log(
                    &f(
                        "Osmium config {0} for service '{1}'",
                        &[if is_update { "updated" } else { "installed" }, &svc_name],
                    ),
                    1005,
                    windows::Win32::System::EventLog::EVENTLOG_INFORMATION_TYPE,
                );
            }
        }
        Err(e) => {
            rollback_registration(&svc_name, &base_dir, &mut logs_backup, &e);
        }
    }
}

/// 注册失败回滚（4 处失败分支共用）: 卸载已建服务 + 清理部署目录 + 还原备份日志后报错退出
fn rollback_registration(
    svc_name: &str,
    base_dir: &Path,
    logs_backup: &mut Option<PathBuf>,
    reason: &str,
) -> ! {
    let _ = uninstall_service_scm(svc_name);
    safe_delete_dir(base_dir); // inplace 模式无部署目录，删除为空操作
    restore_service_logs(svc_name, logs_backup.take());
    error(&f("Service registration failed: {0}", &[reason]));
    unreachable!("error() 以 exit 结束")
}

// ==================== CLI 动作辅助 ====================

/// 收集"低权限用户可写"的路径列表（P0-3 校验）:
/// exe 自身/目录、working_directory、download_to、downloads[].to（绝对路径时） （先展开 %VAR%/%BASE%，与 exe 路径检查一致，防含环境变量的路径误判）
fn collect_unsafe_paths(
    config: &ServiceConfig,
    install_deploy_dir: &str,
    svc_exe_path: &Path,
    out: &mut Vec<String>,
) {
    collect_unsafe_paths_from(config, install_deploy_dir, svc_exe_path, out);
}

/// 可写路径收集的共享实现: validate_config（--check）与安装校验（collect_unsafe_paths）同源
fn collect_unsafe_paths_from(
    config: &ServiceConfig,
    base_dir: &str,
    svc_exe_path: &Path,
    out: &mut Vec<String>,
) {
    if let Some(exe) = svc_exe_path.to_str() {
        let exe_dir = Path::new(exe)
            .parent()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_user_writable(&exe_dir) || is_user_writable(exe) {
            out.push(exe.to_string());
        }
    }
    if let Some(workdir) = config.working_directory.as_deref()
        && !workdir.trim().is_empty()
    {
        let workdir = crate::service_host::expand_env_value(workdir, base_dir);
        if is_user_writable(&workdir) {
            out.push(format!("working_directory '{workdir}'"));
        }
    }
    // 下载目标: 绝对路径指向可写位置时同样可被预放恶意文件替换
    //（若 sha 匹配或未配置则跳过下载直接执行），纳入可写性校验
    if let Some(to) = config.download_to.as_deref() {
        let to = crate::service_host::expand_env_value(to, base_dir);
        if (Path::new(&to).is_absolute() || to.starts_with('\\')) && is_user_writable(&to) {
            out.push(format!("download_to '{to}'"));
        }
    }
    if let Some(list) = config.downloads.as_deref() {
        for d in list {
            let to = crate::service_host::expand_env_value(d.to.trim(), base_dir);
            if (Path::new(&to).is_absolute() || to.starts_with('\\')) && is_user_writable(&to) {
                out.push(format!("downloads[].to '{to}'"));
            }
        }
    }
}

/// inplace 部署前置检查（失败即 error 退出）:
/// 配置文件名 == exe 名（.toml 旁置）、exe 目录/DACL 不可被低权限用户改写（P0-1）、 服务名 == exe 文件名（SCM 分派要求）、exe 旁有私钥时自动签名配置
fn check_inplace_requirements(
    svc_name: &str,
    own_exe: &str,
    config_path_str: &str,
    config_path: &Path,
) {
    // 配置名: 与 exe 同名，后缀 .toml（宿主读取时按同名 toml）
    let exe_stem = Path::new(own_exe)
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    let expected_names = [format!("{}.toml", exe_stem)];
    // canonicalize 会产生 \\?\ 前缀，与 own_exe 的普通路径前缀不一致，先去除再比较
    let config_file = strip_verbatim_prefix(config_path)
        .file_name()
        .map(|s| s.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if !expected_names
        .iter()
        .any(|n| n.to_lowercase() == config_file)
    {
        error(&f(
            "deploy_inplace: config file must be named '{0}' next to the executable (host reads its own .toml by name).",
            &[&format!("{}.toml", exe_stem)],
        ));
    }
    // 原地注册宿主以 LocalSystem 运行，若 EXE 目录允许低权限用户写入（Downloads/Public/工作区等），
    // 任何用户可替换 EXE 获得 SYSTEM 执行；目录/DACL 与 EXE/TOML 的 ACL 须仅允许管理员改写（P0-1）
    let exe_dir = Path::new(own_exe)
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| ".".to_string());
    if is_user_writable(&exe_dir) || is_user_writable(own_exe) || is_user_writable(config_path_str)
    {
        error(&f(
            "Application error: {0}",
            &[
                "deploy_inplace: directory (or its exe/toml) is writable by unprivileged users. Move the executable to a SYSTEM/Administrators-only location (e.g. Program Files).",
            ],
        ));
    }
    // 宿主 scm_svc_name 固定取 exe 文件名（os），SCM 要求注册名与 dispatcher 服务名一致，
    // inplace 不重命名 exe，故服务名必须等于 exe 文件名，否则注册成功却无法启动
    if !svc_name.eq_ignore_ascii_case(&exe_stem) {
        error(&f(
            "Application error: {0}",
            &[&format!(
                "deploy_inplace: service_name must equal the executable file name '{}', otherwise SCM cannot dispatch the service.",
                exe_stem
            )],
        ));
    }
    // 配置签名（inplace 与平台一致）: exe 旁存在 osmium-sign.key 时对配置自动签名（<toml>.sig）
    if key_file_adjacent(SIGN_KEY_FILE).is_some() && !sign_config_file(config_path) {
        error(&f(
            "Service registration failed: {0}",
            &["Failed to sign inplace config"],
        ));
    }
}

/// 校验服务名并确认已注册；任一失败即报错退出（6 个服务操作命令共用）
pub(crate) fn require_registered(svc_name: &str) {
    if !is_valid_service_name(svc_name) {
        error(&f(INVALID_NAME_MSG, &[svc_name]));
    }
    if !is_registered(svc_name) {
        error(&f(
            "Service not found in registry: '{0}'. Use --list to see registered services.",
            &[svc_name],
        ));
    }
}

pub(crate) fn do_uninstall(svc_name: &str, force_delete: bool) {
    if !do_stop(svc_name) {
        // 停止失败未完成卸载必须以非零码退出（P2-3）
        error(&f(
            "Cannot uninstall service '{0}' — failed to stop it. Check service state with --status '{0}' and try again.",
            &[svc_name],
        ));
    }
    match uninstall_service_scm(svc_name) {
        Ok(()) => {
            // 与 install 的更新路径一致: 等待 SCM 完全移除，避免立即重装同名服务
            // 触发延迟删除竞态（服务注册成功但稍后从 SCM 消失）
            wait_service_deleted(svc_name);
            safe_delete_dir(&base_dir(svc_name));
            println!(
                "{CLI_PREFIX}: {}",
                if force_delete {
                    "Service force-deleted"
                } else {
                    "Service unregistered successfully"
                }
            );
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
            cli_error_out(&f(
                "Failed to stop service '{0}': {1}. Check service state with --status '{0}'.",
                &[svc_name, &e],
            ));
            false
        }
    }
}

/// -m --refresh `<name>`: 从已部署配置重新同步 SCM 服务注册属性（对应 WinSW refresh）。
/// 不重建服务、不触碰 ImagePath/部署文件——显示名/描述/启动类型/依赖/账户/故障恢复/ 延迟启动/交互标志/SDDL 全部按 .osiml（inplace 为 exe 旁同名 toml）重写
pub(crate) fn refresh_service(svc_name: &str) -> Result<(), String> {
    if !is_valid_service_name(svc_name) {
        return Err(f(INVALID_NAME_MSG, &[svc_name]));
    }
    // 配置来源: 平台部署读 svcs\<name>\<name>.osiml；inplace 读 exe 旁同名 toml
    let config_path = if is_osmium_deployed(svc_name) {
        deployed_config_path(svc_name)
    } else if is_inplace_service(svc_name) {
        let image = get_service_image_path(svc_name).unwrap_or_default();
        crate::service_host::config_path_next_to(Path::new(image.trim_matches('"')))
    } else {
        return Err(f(
            "Service '{0}' is not managed by Osmium. Use --list to see registered services.",
            &[svc_name],
        ));
    };
    if !config_path.exists() {
        return Err(f(
            "Service config file not found: {0}. Reinstall the service if the file is missing.",
            &[&config_path.display().to_string()],
        ));
    }
    let config = load_config(&config_path);

    // service_account="virtual" → NT SERVICE\<name> 虚拟账户（与 install 一致）:
    // 若不映射，"virtual" 字面量会被当成真实账户名传给 ChangeServiceConfigW 导致刷新后账户非法
    let virtual_account = if config
        .service_account
        .as_deref()
        .map(|a| a.eq_ignore_ascii_case("virtual"))
        .unwrap_or(false)
    {
        Some(format!("NT SERVICE\\{}", svc_name))
    } else {
        config.service_account.clone()
    };

    let (start_mode, delayed_auto) = parse_start_mode(config.service_start_mode.as_deref());
    let failure_reset = if config.failure_reset_sec > 0 {
        config.failure_reset_sec
    } else {
        86400
    };
    let restart_delay = if config.restart_delay_ms > 0 {
        config.restart_delay_ms
    } else {
        60000
    };

    unsafe {
        let name_wide = to_wide(svc_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(name_wide.as_ptr()),
            windows::Win32::System::Services::SERVICE_ALL_ACCESS,
        )
        .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        // 宽字符串必须保持存活直到 ChangeServiceConfigW 调用完成
        let dep_str = build_dependency_string(config.service_dependencies.as_deref());
        let dep_wide = dep_str.as_deref().map(to_wide);
        let dep_pcwstr = dep_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let account_wide = virtual_account.as_deref().map(to_wide);
        let account_pcwstr = account_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        // virtual 虚拟账户无密码（与 install 一致），显式密码仅自定义账户场景透传
        let password_wide = if virtual_account.is_some() {
            None
        } else {
            config.service_password.as_deref().map(to_wide)
        };
        let password_pcwstr = password_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let display_wide = to_wide(&config.service_display_name);

        // 服务类型: interactive 标志按配置重算（ImagePath/错误控制保持 SERVICE_NO_CHANGE）
        let mut service_type = SERVICE_WIN32_OWN_PROCESS;
        if config.interactive {
            service_type |= ENUM_SERVICE_TYPE(SERVICE_INTERACTIVE_PROCESS);
        }
        let change = ChangeServiceConfigW(
            svc,
            service_type,
            start_mode,
            SERVICE_ERROR(SERVICE_NO_CHANGE),
            PCWSTR::null(),
            PCWSTR::null(),
            None,
            dep_pcwstr,
            account_pcwstr,
            password_pcwstr,
            PCWSTR::from_raw(display_wide.as_ptr()),
        );
        if let Err(e) = change {
            let _ = CloseServiceHandle(svc);
            let _ = CloseServiceHandle(scm);
            return Err(format!("{}: {e}", "Failed to update service configuration"));
        }

        // 描述/故障恢复/延迟启动/SDDL: 统一闭包内执行，失败统一关句柄后传播
        let apply = (|| -> Result<(), String> {
            let desc_wide = to_wide(&config.service_description);
            let desc_info = SERVICE_DESCRIPTIONW {
                lpDescription: PWSTR::from_raw(desc_wide.as_ptr() as *mut _),
            };
            ChangeServiceConfig2W(
                svc,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&desc_info as *const _ as *const _),
            )
            .map_err(|e| format!("{}: {e}", "Failed to set service description"))?;
            if failure_reset > 0 {
                set_failure_actions(
                    svc,
                    failure_reset as u32,
                    restart_delay as u32,
                    config.failure_action.as_deref(),
                )?;
            }
            // 延迟启动: 显式按配置写入 true/false（refresh 需精确同步）
            let delay_info = SERVICE_DELAYED_AUTO_START_INFO {
                fDelayedAutostart: delayed_auto.into(),
            };
            ChangeServiceConfig2W(
                svc,
                SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                Some(&delay_info as *const _ as *const _),
            )
            .map_err(|e| format!("{}: {e}", "Failed to set delayed auto start"))?;
            if let Some(sddl) = config.security_descriptor.as_deref() {
                apply_service_sddl(svc, sddl)
                    .map_err(|e| format!("{}: {e}", "Failed to set service security descriptor"))?;
            }
            Ok(())
        })();
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        apply?;
    }

    // allow_service_logon: 使用自定义账户时授予"作为服务登录"权限（失败仅告警，与 install 一致）
    if let Some(acct) = &virtual_account {
        // virtual 账户: 重新授权遍历部署链并读写自身部署目录（幂等，安装后补充授权/目录迁移场景）
        let target_dir = if is_inplace_service(svc_name) {
            get_service_image_path(svc_name)
                .map(|p| {
                    Path::new(p.trim_matches('"'))
                        .parent()
                        .map(|x| x.to_string_lossy().to_string())
                        .unwrap_or_default()
                })
                .unwrap_or_default()
        } else {
            base_dir(svc_name).to_string_lossy().to_string()
        };
        grant_virtual_account_access(acct, &target_dir);
    } else if config.allow_service_logon
        && let Some(account) = config.service_account.as_deref()
    {
        grant_service_logon_right(account);
    }
    // 配置变更审计（refresh 成功后写事件日志，ID 1005）
    if config.event_log {
        crate::service_host::report_event_log(
            &f("Osmium service properties refreshed for '{0}'", &[svc_name]),
            1005,
            windows::Win32::System::EventLog::EVENTLOG_INFORMATION_TYPE,
        );
    }
    Ok(())
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
        let Ok(h) = GetStdHandle(STD_ERROR_HANDLE) else {
            return;
        };
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
        let Ok(h) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return;
        };
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
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
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

/// 是否配置了下载（download_url 非空，或 downloads 数组含任一条 from 非空）:
/// 数组模式（downloads）下目标 exe 同样可能由下载提供、安装/扫描时本机尚不存在， 只认 download_url 会导致首次安装被误拒、刷新器开机误删
pub(crate) fn has_download(config: &ServiceConfig) -> bool {
    if config
        .download_url
        .as_deref()
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    config
        .downloads
        .as_deref()
        .map(|l| l.iter().any(|d| !d.from.trim().is_empty()))
        .unwrap_or(false)
}

pub fn load_config(path: impl AsRef<Path>) -> ServiceConfig {
    let path = path.as_ref();
    // 配置大小上限（1MB）: 防超大 .osiml 解析 DoS（恶意/损坏配置撑爆内存）
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > 1024 * 1024
    {
        panic!(
            "{}",
            f(
                "Config file '{0}' exceeds the 1 MB size limit",
                &[&path.display().to_string()]
            )
        );
    }
    let content = std::fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "{}",
            f(
                "Failed to parse config '{0}': {1}",
                &[&path.display().to_string(), &e.to_string()]
            )
        )
    });
    let mut config: ServiceConfig = toml::from_str(&content).unwrap_or_else(|e| {
        panic!(
            "{}",
            f(
                "Failed to parse config '{0}': {1}",
                &[&path.display().to_string(), &e.to_string()]
            )
        )
    });
    // 部署配置的敏感字段为 DPAPI 密文，解析后统一解密
    decrypt_sensitive(&mut config);
    config
}

/// --check 预检: 校验配置合法性（不安装）——解析/服务名/保留名/路径存在性/可写性/下载目标；
/// Ok(通过项列表) 或 Err(失败项列表)
pub(crate) fn validate_config(config_path: &Path) -> Result<Vec<String>, Vec<String>> {
    let mut ok_msgs = Vec::new();
    let mut errors = Vec::new();
    if !config_path.exists() {
        return Err(vec![format!(
            "Config file not found: {}",
            config_path.display()
        )]);
    }
    let config = match std::panic::catch_unwind(|| load_config(config_path)) {
        Ok(c) => c,
        Err(p) => return Err(vec![panic_msg(&*p, "Unknown error")]),
    };
    ok_msgs.push(format!(
        "Config parsed successfully ({})",
        config_path.display()
    ));
    if !is_valid_service_name(&config.service_name) {
        errors.push(f(INVALID_NAME_MSG, &[&config.service_name]));
    } else {
        ok_msgs.push(format!("Service name '{}' is valid", config.service_name));
    }
    // 启动类型: 未知值会被静默按 automatic 注册（拼错 delayed_auto 无感知）——预检显式提示
    if let Some(mode) = config
        .service_start_mode
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
    {
        const KNOWN_MODES: [&str; 6] = [
            "automatic",
            "delayed_auto",
            "delayed-auto",
            "delayedauto",
            "manual",
            "disabled",
        ];
        if KNOWN_MODES.iter().any(|k| k.eq_ignore_ascii_case(mode))
            || mode.eq_ignore_ascii_case("once")
        {
            ok_msgs.push(format!("Start mode '{mode}' is valid"));
        } else {
            errors.push(format!(
                "service_start_mode: unknown value '{mode}' (expected automatic | delayed_auto | manual | disabled | once)"
            ));
        }
    }
    if is_refresher_reserved_name(&config.service_name) {
        errors.push(f(RESERVED_NAME_MSG, &[&config.service_name]));
    }
    // 展开 %VAR%/%BASE% 再校验: %BASE% 语义 = 配置所在目录（与宿主 test/部署模式一致），
    // 含环境变量/相对路径的 exe 路径不会被误判"不存在"
    let check_base = config_path
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let exe_expanded =
        crate::service_host::expand_env_value(&config.service_executable_path, &check_base);
    let svc_exe =
        std::fs::canonicalize(&exe_expanded).unwrap_or_else(|_| PathBuf::from(&exe_expanded));
    let has_download = has_download(&config);
    let rooted = Path::new(&exe_expanded).is_absolute() || exe_expanded.starts_with('\\');
    if !has_download && rooted && !svc_exe.exists() {
        errors.push(format!(
            "service_executable_path '{}' does not exist",
            exe_expanded
        ));
    } else {
        ok_msgs.push("Executable path check passed".into());
    }
    // 可写性: exe 目录/工作目录/下载目标（与安装校验同源，防任意用户替换提权）
    let mut unsafe_paths: Vec<String> = Vec::new();
    collect_unsafe_paths_from(&config, &check_base, &svc_exe, &mut unsafe_paths);
    if unsafe_paths.is_empty() {
        ok_msgs.push("Path writability check passed".into());
    } else {
        errors.push(format!(
            "Paths writable by unprivileged users: {}",
            unsafe_paths.join(", ")
        ));
    }
    // 插件存在性: 配置引用了插件（plugins 数组 / download_auth=sspi / shared_directory_mappers /
    // 内置告警通道）但 exe 目录下没有可用插件 → 预检提示（运行时仅非致命告警，这里显式告知）
    let uses_plugins = config.plugins.as_deref().is_some_and(|l| !l.is_empty())
        || config
            .download_auth
            .as_deref()
            .map(|a| a.eq_ignore_ascii_case("sspi"))
            .unwrap_or(false)
        || config
            .shared_directory_mappers
            .as_deref()
            .is_some_and(|l| !l.is_empty())
        || config
            .notify_url
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || config
            .smtp_host
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty())
        || config
            .syslog_host
            .as_deref()
            .is_some_and(|s| !s.trim().is_empty());
    if uses_plugins {
        let plugins = crate::service_host::discover_plugins();
        if plugins.is_empty() {
            errors.push("Config references plugins (plugins/sspi/netmap/alerts) but no .osx plugin was found next to the executable".into());
        } else {
            ok_msgs.push(format!("{} plugin(s) available", plugins.len()));
        }
    }
    // 内置告警通道: notify_url 必须是合法 http(s) URL；smtp 需同时提供 smtp_from/smtp_to
    if let Some(url) = config.notify_url.as_deref()
        && !url.trim().is_empty()
        && url::Url::parse(url).is_err()
    {
        errors.push(format!("notify_url: invalid URL '{url}'"));
    }
    if let Some(host) = config.smtp_host.as_deref()
        && !host.trim().is_empty()
        && (config
            .smtp_from
            .as_deref()
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
            || config
                .smtp_to
                .as_deref()
                .map(|s| s.trim().is_empty())
                .unwrap_or(true))
    {
        errors.push(
            "smtp_host requires smtp_from and smtp_to (alert email channel skipped otherwise)"
                .into(),
        );
    }
    // security_descriptor 合法性: SDDL 能解析为安全描述符（防安装后才暴露的非法 SDDL）
    if let Some(sddl) = config.security_descriptor.as_deref()
        && !sddl.trim().is_empty()
    {
        match security_descriptor_from_sddl(sddl) {
            Ok(sd) => {
                unsafe {
                    let _ = windows::Win32::Foundation::LocalFree(Some(
                        windows::Win32::Foundation::HLOCAL(sd.0),
                    ));
                }
                ok_msgs.push("Security descriptor (SDDL) is valid".into());
            }
            Err(_) => errors.push("Security descriptor (SDDL) is invalid".into()),
        }
    }
    // schedules 时刻格式: daily_at 必须可解析（"HH:mm" / "HH:mm:ss"），every_secs 必须为正
    if let Some(list) = config.schedules.as_deref() {
        for (i, s) in list.iter().enumerate() {
            if s.every_secs.is_none() && s.daily_at.is_none() {
                errors.push(format!(
                    "schedules[{}]: must set every_secs or daily_at",
                    i + 1
                ));
            }
            if let Some(e) = s.every_secs
                && e <= 0
            {
                errors.push(format!("schedules[{}]: every_secs must be positive", i + 1));
            }
            if let Some(at) = s.daily_at.as_deref()
                && !crate::service_host::parse_daily_time_check(at)
            {
                errors.push(format!(
                    "schedules[{}]: invalid daily_at '{at}' (expected \"HH:mm\" or \"HH:mm:ss\")",
                    i + 1
                ));
            }
        }
    }
    // 定点滚动时刻格式: log_auto_roll_at 必须可解析（"HH:mm" / "HH:mm:ss"）
    if let Some(at) = config.log_auto_roll_at.as_deref()
        && !at.trim().is_empty()
        && !crate::service_host::parse_daily_time_check(at)
    {
        errors.push(format!(
            "log_auto_roll_at: invalid time '{at}' (expected \"HH:mm\" or \"HH:mm:ss\")"
        ));
    }
    // 数值字段合法性: 负值/越界在宿主侧会被钳制但配置是错的，预检显式提示。
    // 注意: 0 对这些字段 = "未配置"（宿主按默认处理），只有负值才真正非法；
    // 有明确语义上限的字段（线程数/重试/实例数/备份数）按上限校验
    for (name, v, min, max) in [
        ("download_threads", config.download_threads as i64, 0, 64),
        ("download_retries", config.download_retries, 0, 20),
        ("log_max_size_mb", config.log_max_size_mb, 0, i64::MAX),
        (
            "log_max_backup_count",
            config.log_max_backup_count as i64,
            0,
            1000,
        ),
        (
            "health_check_interval_secs",
            config.health_check_interval_secs,
            0,
            86400,
        ),
        (
            "health_check_timeout_secs",
            config.health_check_timeout_secs,
            0,
            300,
        ),
        (
            "health_check_failures",
            config.health_check_failures,
            0,
            1000,
        ),
        ("process_count", config.process_count, 0, 64),
        ("stop_timeout_secs", config.stop_timeout_secs, 0, 3600),
    ] {
        if v < min || v > max {
            errors.push(format!("{name}: value {v} out of range [{min}..{max}]"));
        }
    }
    if config.download_rate_limit_kbps < 0 {
        errors.push(format!(
            "download_rate_limit_kbps: value {} must be >= 0",
            config.download_rate_limit_kbps
        ));
    }
    if let Some(s) = config.schedules.as_deref()
        && s.is_empty()
    {
        ok_msgs.push("schedules: empty array (no-op, remove it)".into());
    }
    // 健康检查 URL: tcp:// 目标格式校验 / http(s) URL 格式校验
    if let Some(url) = config.health_check_url.as_deref()
        && !url.trim().is_empty()
    {
        if url.to_ascii_lowercase().starts_with("tcp://") {
            if !crate::service_host::parse_tcp_target_check(&url[6..]) {
                errors.push(format!(
                    "health_check_url: invalid tcp target '{url}' (expected tcp://host:port)"
                ));
            } else {
                ok_msgs.push("Health check target (tcp://) is valid".into());
            }
        } else if url::Url::parse(url).is_err() {
            errors.push(format!("health_check_url: invalid URL '{url}'"));
        } else {
            ok_msgs.push("Health check URL is valid".into());
        }
    }
    // 不安全下载检查（http 无 sha256 / basic 走明文 http）: 与宿主启动时同源判定，
    // 让 --install 前就能发现——管理员装上后才在启动失败是糟糕的反馈闭环
    match crate::service_host::warn_if_insecure_download(&config) {
        Ok(()) => {
            if crate::service_core::has_download(&config) {
                ok_msgs.push("Download targets are secure (https or sha256-protected)".into());
            }
        }
        Err(e) => errors.push(e),
    } // 签名密钥可篡改检查: 私钥文件/所在目录对低权限用户可写时，--sign-config 的签名不可信
    if let Some(key) = key_file_adjacent(SIGN_KEY_FILE)
        && key_file_tamperable_by_unprivileged(&key)
    {
        errors.push(format!(
            "osmium-sign.key at '{}' is writable by unprivileged users — the signing key can be replaced. Protect the key file and its directory.",
            key.display()
        ));
    }
    if errors.is_empty() {
        Ok(ok_msgs)
    } else {
        Err(errors)
    }
}

/// 平台部署覆盖判定: toml 可解析时对比可执行路径/参数同源；toml 缺失/损坏时退回 ImagePath 归属判定，
/// 仅 Osmium 部署才允许覆盖修复
pub(crate) fn can_overwrite_source(
    deployed_config: &str,
    config_path: &str,
    svc_name: &str,
) -> bool {
    if !Path::new(deployed_config).exists() {
        return is_osmium_deployed(svc_name);
    }
    std::panic::catch_unwind(|| {
        let existing = load_config(deployed_config);
        let current = load_config(config_path);
        // 路径与参数均忽略大小写，未填写的参数视为空串
        existing
            .service_executable_path
            .eq_ignore_ascii_case(current.service_executable_path.as_str())
            && existing
                .service_executable_args
                .as_deref()
                .unwrap_or("")
                .eq_ignore_ascii_case(current.service_executable_args.as_deref().unwrap_or(""))
    })
    .unwrap_or_else(|_| is_osmium_deployed(svc_name))
}

/// 写部署配置: 敏感字段（service_password / download_password / 共享映射密码）DPAPI 加密后落盘，
/// 避免明文密码在 .osiml 中（P1-2）；配置无法解析（非标准 TOML）时退回按行剥离敏感键的旧逻辑； 加密失败必须 fail-closed（返回 false 中止安装）——绝不允许把密码静默清空或明文落盘
pub(crate) fn write_deployed_config(source: &str, dest: &Path) -> bool {
    let Ok(content) = std::fs::read_to_string(source) else {
        return false;
    };
    // Ok(Some(text)) 加密完成 | Ok(None) 配置无法解析（走剥离 fallback）| Err(msg) 加密/序列化失败（fail-closed）
    let encrypted = std::panic::catch_unwind(|| -> Result<Option<String>, String> {
        // 解析失败返回 Ok(None) 走旧兼容的按行剥离路径；只有解析成功后加密失败才 fail-closed
        let Ok(config) = toml::from_str::<ServiceConfig>(&content) else {
            return Ok(None);
        };
        let mut config = config;
        if let Some(p) = &mut config.service_password {
            *p =
                dpapi_encrypt(p).ok_or("Failed to encrypt service_password (DPAPI)".to_string())?;
        }
        if let Some(p) = &mut config.download_password {
            *p = dpapi_encrypt(p)
                .ok_or("Failed to encrypt download_password (DPAPI)".to_string())?;
        }
        // smtp 认证密码同样属凭据，纳入机器级加密（不落明文）
        if let Some(p) = &mut config.smtp_password {
            *p = dpapi_encrypt(p).ok_or("Failed to encrypt smtp_password (DPAPI)".to_string())?;
        }
        if let Some(mappers) = &mut config.shared_directory_mappers {
            for m in mappers {
                if let Some(p) = &mut m.password {
                    *p = dpapi_encrypt(p)
                        .ok_or("Failed to encrypt shared mapper password (DPAPI)".to_string())?;
                }
            }
        }
        Ok(Some(
            toml::to_string_pretty(&config)
                .map_err(|e| format!("Failed to serialize config: {e}"))?,
        ))
    });
    match encrypted {
        Ok(Ok(Some(text))) => std::fs::write(dest, text).is_ok(),
        Ok(Err(msg)) => {
            eprintln!("{}", red(&f("Warning: {0}", &[&msg])));
            false
        }
        // 配置无法解析（非标准 TOML）: 按行剥离全部敏感键明文后写盘（旧兼容行为）——
        // service_password / download_password / smtp_password / 共享映射 password 都是凭据，缺一即泄漏
        Ok(Ok(None)) | Err(_) => {
            let filtered: Vec<&str> = content
                .lines()
                .filter(|l| {
                    let t = l.trim_start().to_ascii_lowercase();
                    !t.starts_with("service_password")
                        && !t.starts_with("download_password")
                        && !t.starts_with("smtp_password")
                        && !t.starts_with("password")
                })
                .collect();
            std::fs::write(dest, filtered.join("\r\n")).is_ok()
        }
    }
}

/// 返回 (启动类型, 是否延迟自动启动)
pub(crate) fn parse_start_mode(mode: Option<&str>) -> (SERVICE_START_TYPE, bool) {
    match mode.map(|s| s.to_lowercase()).as_deref() {
        Some("delayed_auto") | Some("delayed-auto") | Some("delayedauto") => {
            (SERVICE_AUTO_START, true)
        }
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
        .map(|d| {
            if d.ends_with('\\') {
                d
            } else {
                format!("{}\\", d)
            }
        })
        .unwrap_or_else(|_| "C:\\".to_string());
    PathBuf::from(root)
        .join("ProgramData")
        .join("Osmium")
        .join("svcs")
}

/// 平台部署服务的配置文件路径（共享宿主按名加载）: svcs`<name>``<name>`.osiml
pub(crate) fn deployed_config_path(name: &str) -> PathBuf {
    registry_dir().join(name).join(format!("{}.osiml", name))
}

/// Job Object 状态文件路径（宿主写入 `<配置名>`.job，--status 读取显示）:
/// 平台部署 = svcs`<name>``<name>`.job；inplace = exe 旁 `<同名>`.job
pub(crate) fn job_state_path(name: &str) -> PathBuf {
    if is_inplace_service(name)
        && let Some(image) = get_service_image_path(name)
    {
        return crate::service_host::config_path_next_to(Path::new(image.trim_matches('"')))
            .with_extension("job");
    }
    deployed_config_path(name).with_extension("job")
}

/// --status 指标摘要: 读取部署配置的 metrics_file（相对部署目录），返回最后一条导出记录。
/// inplace 服务的配置在 exe 旁（与 job_state_path 同款分支），不能只读 svcs 部署配置
pub(crate) fn last_metrics_line(name: &str) -> Option<String> {
    let config_path = if is_inplace_service(name)
        && let Some(image) = get_service_image_path(name)
    {
        crate::service_host::config_path_next_to(Path::new(image.trim_matches('"')))
    } else {
        deployed_config_path(name)
    };
    let config = std::panic::catch_unwind(|| load_config(&config_path)).ok()?;
    let mf = config.metrics_file.as_deref()?.trim();
    if mf.is_empty() {
        return None;
    }
    let deploy_dir = config_path.parent()?.to_string_lossy().to_string();
    let p = if Path::new(mf).is_absolute() || mf.starts_with('\\') {
        PathBuf::from(mf)
    } else {
        PathBuf::from(&deploy_dir).join(mf)
    };
    std::fs::read_to_string(&p)
        .ok()?
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.to_string())
}

/// 服务刷新程序日志目录 — 与 svcs 并列（ProgramData/Osmium/refresher），
/// 避免占用 svcs/refresh 目录，防止与真实名为 refresh 的服务冲突
fn refresher_log_dir() -> PathBuf {
    registry_dir()
        .parent()
        .map(|p| p.join("refresher"))
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData\\Osmium\\refresher"))
}

/// 是否 Osmium 管理的服务: 平台部署按 SCM ImagePath 是否位于 svcs 判定（而非仅目录存在，
/// 防对同名非 Osmium 部署服务误删/启停）；inplace 按 ImagePath 指向 os.exe 判定
fn is_registered(svc_name: &str) -> bool {
    service_exists(svc_name) && (is_osmium_deployed(svc_name) || is_inplace_service(svc_name))
}

/// 测试/CLI 探针: is_registered 的只读版本（--check 服务名解析用）
pub(crate) fn is_registered_probe(svc_name: &str) -> bool {
    is_registered(svc_name)
}

/// 判定已注册服务是否为 inplace 原地注册: ImagePath 指向的 exe 文件名与服务名一致
///（inplace 注册要求 service_name == exe 文件名），且不在 svcs 平台部署目录内
pub(crate) fn is_inplace_service(svc_name: &str) -> bool {
    let Some(image) = get_service_image_path(svc_name) else {
        return false;
    };
    let image = image.trim_matches('"');
    // 平台部署的 ImagePath 整行以 "宿主 -internal --run <name>" 结尾，file_name 恒不匹配服务名
    if !Path::new(image)
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.trim_end_matches(".exe").eq_ignore_ascii_case(svc_name))
        .unwrap_or(false)
    {
        return false;
    }
    // inplace 服务指向用户自己位置的 exe；svcs 目录内的是平台部署副本（名为 {svcName}.exe）
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
pub(crate) fn get_service_image_path(service_name: &str) -> Option<String> {
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
/// 供刷新器/--list 按目录名操作前校验，防止误操作外部服务或被同名目录诱导
fn is_osmium_deployed(service_name: &str) -> bool {
    let Some(image) = get_service_image_path(service_name) else {
        return false;
    };
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
/// 定位 "-internal" 之后第一个 "--run"，再取其后内容并去外层引号，兼容服务名含空格 （install 时引号包裹）。用 to_ascii_lowercase 保证字节偏移不变；先定位 -internal 可避免宿主安装路径自身含 "--run" 子串时误切（如 C:\app--run\os.exe）
pub(crate) fn parse_run_service_name(image: &str) -> Option<String> {
    let s = image.trim();
    let lower = s.to_ascii_lowercase();
    let internal = lower.find("-internal")?;
    let idx = lower[internal..].find("--run")? + internal;
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
        let status = RegOpenKeyExW(
            root,
            PCWSTR::from_raw(subkey_wide.as_ptr()),
            Some(0),
            flags,
            &mut key,
        );
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
    let ok =
        matches!(&own, Ok(o) if o.status.success()) && matches!(&acl, Ok(a) if a.status.success());
    if !ok {
        let err = match &acl {
            Ok(a) if !a.status.success() => String::from_utf8_lossy(&a.stderr).trim().to_string(),
            _ => "ACL hardening failed".to_string(),
        };
        eprintln!(
            "{}",
            red(&f(
                "Warning: failed to secure deployment directory '{0}': {1}",
                &[path, &err]
            ))
        );
    }
    ok
}

/// 可写性判定进程内缓存（PowerShell 启动开销大；同一进程内 ACL 不变化，缓存安全）
fn writable_cache() -> &'static Mutex<Option<std::collections::HashMap<String, bool>>> {
    static CACHE: Mutex<Option<std::collections::HashMap<String, bool>>> = Mutex::new(None);
    &CACHE
}

/// 对象（目录/文件）是否允许低权限主体改写: 用 PowerShell 输出 SDDL 解析所有者与 DACL；
/// 解析失败/无法判定一律视为可写（fail-closed），拒绝在不可信位置注册 SYSTEM 服务（防 P0-1）。 目标尚不存在时（如下载目标）按父目录判定——新建文件继承父目录 ACL， 父目录可写即等于目标可被预创建替换
pub(crate) fn is_user_writable(path: &str) -> bool {
    // 缓存键大小写归一（Windows 路径大小写不敏感）: 同一路径的大小写变体命中同一缓存，
    // 避免重复触发 PowerShell（如 'C:\App' 与 'c:\app' 是同一对象）
    let key = path.to_lowercase();
    if let Some(cache) = writable_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        && let Some(v) = cache.get(&key)
    {
        return *v;
    }
    let r = is_user_writable_uncached(path);
    writable_cache()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get_or_insert_with(Default::default)
        .insert(key, r);
    r
}

/// 清空可写性判定缓存（测试隔离用；真实场景同一进程内 ACL 不变化无需清理）
#[cfg(test)]
pub(crate) fn clear_writable_cache() {
    *writable_cache().lock().unwrap_or_else(|e| e.into_inner()) = None;
}

fn is_user_writable_uncached(path: &str) -> bool {
    let p = Path::new(path);
    if !p.exists() {
        let parent = p
            .parent()
            .map(|x| x.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string());
        if parent == path {
            return true; // 无父目录（根），无法判定按可写处理
        }
        return is_user_writable(&parent);
    }
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
    if !out.status.success() {
        return true;
    }
    let sddl = String::from_utf8_lossy(&out.stdout);
    let sddl = sddl.trim();
    let Some(dacl_at) = sddl.find("D:") else {
        return true;
    };
    let owner_ok = sddl_owner_is_administrative(&sddl[..dacl_at]);
    if !owner_ok {
        return true;
    }
    sddl_dacl_grants_non_admin_write(&sddl[dacl_at..])
}

/// SDDL 所有者段（"O:xxx"）是否管理员级主体（SYSTEM / Administrators / 域管理员 / 内建管理员 RID）
pub(crate) fn sddl_owner_is_administrative(owner_segment: &str) -> bool {
    let Some(o) = owner_segment.find("O:") else {
        return false;
    };
    let sid = owner_segment[o + 2..].trim();
    sddl_sid_is_administrative(sid)
}

/// SDDL DACL 段是否授予非管理员级主体写能力
pub(crate) fn sddl_dacl_grants_non_admin_write(dacl: &str) -> bool {
    let mut rest = dacl;
    while let Some(start) = rest.find('(') {
        let Some(end) = rest[start..].find(')') else {
            break;
        };
        let ace = &rest[start + 1..start + end];
        rest = &rest[start + end + 1..];
        // 格式: A|D;<flags>;<rights>;<objectGUID>;<inheritObjectGUID>;<sid>
        let parts: Vec<&str> = ace.split(';').collect();
        if parts.len() < 6 {
            continue;
        }
        let ace_type = parts[0];
        // 仅传播给子对象的 InheritOnly ACE（如 Program Files 标准 ACL 中 CREATOR OWNER 的
        // 继承 FullControl）不影响当前对象本身的可写性，须跳过，否则会被误判为"非管理员可写"
        if parts[1].contains("IO") {
            continue;
        }
        let rights = parts[2];
        let sid = parts[5].trim();
        let write = sddl_rights_include_write(rights);
        if !write {
            continue;
        }
        let admin = sddl_sid_is_administrative(sid.trim());
        if ace_type == "A" && !admin {
            return true;
        }
        if ace_type == "D" && admin {
            return true;
        }
    }
    false
}

/// SDDL 权限是否含写能力: 字母令牌按子串扫描——精确等值会漏判组合形式（如 GRGW）,
/// 把低权限用户实际可写的目录误判为安全；十六进制展开具体写位并叠加通用写权限位
fn sddl_rights_include_write(rights: &str) -> bool {
    let r = rights.trim();
    if r.is_empty() {
        return false;
    }
    // 十六进制前缀不区分大小写（手写 SDDL 可能写成 0X）；解析失败按非写处理由字母分支兜底
    let lower = r.to_ascii_lowercase();
    if let Some(mask) = lower
        .strip_prefix("0x")
        .and_then(|h| u32::from_str_radix(h, 16).ok())
    {
        // 具体写权限位: 写数据/追加(创建文件/子目录)/写扩展属性/删子项/写属性/DELETE/写 DACL/写所有者
        // 注意: 0x20(FILE_EXECUTE) 与 0x100000(SYNCHRONIZE) 是只读/同步位, 绝不能算写
        const WRITE_BITS: u32 = 0x2 | 0x4 | 0x10 | 0x40 | 0x100 | 0x10000 | 0x40000 | 0x80000;
        // 通用权限位映射: GENERIC_WRITE / GENERIC_ALL 均隐含写能力
        const GENERIC_WRITE_BITS: u32 = 0x4000_0000 | 0x1000_0000;
        return mask & (WRITE_BITS | GENERIC_WRITE_BITS) != 0;
    }
    // 文件系统 SDDL 字母令牌（Windows 实测位值）: DC=0x2 创建文件, LC=0x4 创建子目录,
    // DT=0x40 删除子项, SD=0x10000 DELETE, WD=0x40000 写 DACL, AD 非有效文件令牌；
    // CC=0x1 列出目录是只读, 绝不能加入（会把只读目录误判为可写）
    [
        "fa", "fw", "m", "wd", "wo", "ga", "gw", "dc", "lc", "dt", "sd", "wdac", "wown",
    ]
    .iter()
    .any(|t| lower.contains(t))
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
            sid.rsplit('-')
                .next()
                .map(|r| r == "500" || r == "512")
                .unwrap_or(false)
        }
        _ => false,
    }
}

fn base_dir(svc_name: &str) -> PathBuf {
    registry_dir().join(svc_name)
}

fn get_service_names() -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(registry_dir()) else {
        return vec![];
    };
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
/// junction/symlink，攻击者可放置指向任意目录的 junction 诱导 SYSTEM 刷新器递归删除其目标（#4）。
/// 根路径自身也须检查 reparse: 根为 junction 时 read_dir 枚举的是目标内容，子项检查拦不住
pub(crate) fn delete_dir_tree(path: &Path) -> bool {
    if !path.exists() {
        return true;
    }
    // 根路径自身是 junction/symlink → 拒绝删除（防诱导删除 junction 目标的内容；仅移除链接本体）
    if crate::service_host::is_reparse_path(path) {
        return std::fs::remove_dir(path).is_ok();
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
                    if std::fs::remove_file(&p).is_err() && std::fs::remove_dir(&p).is_err() {
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

// ==================== 服务刷新程序 — 元数据 & 命令 ====================

/// 返回 os.exe 的安装路径
fn install_path() -> PathBuf {
    let prog_files =
        std::env::var("ProgramFiles").unwrap_or_else(|_| "C:\\Program Files".to_string());
    PathBuf::from(prog_files).join("Osmium").join("os.exe")
}

/// 校验当前进程是否运行在安装路径，防止恶意副本执行敏感命令
fn require_install_path() {
    let own = get_own_path();
    let canonical = install_path();
    if !own.eq_ignore_ascii_case(canonical.to_str().unwrap_or("")) {
        eprintln!(
            "{}",
            red("Error: This command must be run from the installed location:")
        );
        eprintln!("{}", red(&f("  {0}", &[&canonical.display().to_string()])));
        eprintln!("{}", red(&f("Current: {0}", &[&own])));
        process::exit(1);
    }
}

/// -internal --install-refresher: 将 Osmium 自身注册为开机服务刷新程序
pub(crate) fn install_svc_refresher_command() {
    require_install_path();

    if service_exists("Osmium Service Refresher") {
        force_remove_service("Osmium Service Refresher", false);
    }

    // 刷新器以 SYSTEM 运行并对 svcs 目录做清理/删除: 加固 Osmium 根目录与 svcs 目录，
    // 防普通用户预建目录放 junction（删除根 reparse 已拒，目录本身也应收紧防伪造内容）
    let root = registry_dir()
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "C:\\ProgramData\\Osmium".to_string());
    let _ = std::fs::create_dir_all(&root);
    let _ = std::fs::create_dir_all(registry_dir());
    let _ = secure_directory(&root);
    let _ = secure_directory(&registry_dir().to_string_lossy());

    let own_exe = get_own_path();
    let bin_path = format!("\"{}\" -internal --refresher", own_exe);

    match install_service_scm(&InstallServiceParams {
        service_name: "Osmium Service Refresher",
        display_name: "Osmium Service Refresher",
        description: "Boot-time maintenance service: removes stale Osmium services and orphaned directories, cleans up expired logs, and stops after running once.",
        executable_path: &bin_path,
        start_mode: SVC_REFRESHER_START_MODE,
        failure_reset_sec: SVC_REFRESHER_FAILURE_RESET_SEC,
        restart_delay_ms: SVC_REFRESHER_RESTART_DELAY_MS,
        dependencies: None,
        service_account: None,
        password: None,
        delayed_auto_start: true,
        interactive: false,
        failure_action: None,
        allow_service_logon: false,
        security_descriptor: None,
    }) {
        Ok(()) => println!("{CLI_PREFIX}: Service refresher registered (runs on boot)"),
        Err(e) => error(&f("Service refresher registration failed: {0}", &[&e])),
    }
}

/// -internal --uninstall-refresher: 移除服务刷新程序
pub(crate) fn uninstall_svc_refresher_command() {
    require_install_path();

    if !service_exists("Osmium Service Refresher") {
        println!("{CLI_PREFIX}: Service refresher not found");
        return;
    }
    // 尽力停止后卸载（停止失败也继续卸载）
    let _ = stop_service(
        "Osmium Service Refresher",
        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
    );
    match uninstall_service_scm("Osmium Service Refresher") {
        Ok(()) => println!("{CLI_PREFIX}: Service refresher removed"),
        Err(e) => error(&f("Service refresher removal failed: {0}", &[&e])),
    }
}

// ==================== 服务刷新程序 — 开机维护 & 清理 ====================

/// 删除各服务日志目录以及服务刷新程序日志目录中超过 LOG_RETENTION_DAYS 天的日志文件；
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

    // 清理服务刷新程序日志（自身无 log_zip 配置，不归档直接删）
    let refresher_log_dir = refresher_log_dir();
    if refresher_log_dir.exists() {
        deleted += delete_old_logs(&refresher_log_dir, cutoff, false);
    }

    // panic.log 位于 svcs 根目录（独立于各服务 logs 子目录，无日期前缀），按 mtime 纳入清理
    let panic_log = panic_log_path();
    if panic_log.exists() {
        let stale = std::fs::metadata(&panic_log)
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|t| {
                let dt: chrono::DateTime<chrono::Local> = t.into();
                dt.date_naive() < cutoff
            })
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_file(&panic_log);
            deleted += 1;
        }
    }

    if deleted > 0 {
        println!(
            "{}",
            f(
                "  Log cleanup: removed {0} expired log file(s) (>{1}d)",
                &[&deleted.to_string(), &LOG_RETENTION_DAYS.to_string()]
            )
        );
    }
}

/// 读取服务配置的 log_zip 开关；配置缺失/损坏时保守按 false 处理（不归档）
fn service_log_zip(svc_name: &str) -> bool {
    let cfg = deployed_config_path(svc_name);
    std::panic::catch_unwind(|| load_config(&cfg).log_zip).unwrap_or(false)
}

/// 删除过期日志；zip_archives=true 时删除普通日志前先压缩为 .zip 归档（先归档再删除，
/// 保证每个过期日志都有归档机会，与 WinSW zipOlderThanNumDays 语义对齐）；失败保留原文件
pub(crate) fn delete_old_logs(
    log_dir: &Path,
    cutoff: chrono::NaiveDate,
    zip_archives: bool,
) -> i32 {
    // zip 归档独立保留期（约半年），普通日志沿用传入 cutoff（30 天）
    let zip_cutoff =
        chrono::Local::now().date_naive() - chrono::Duration::days(LOG_ZIP_RETENTION_DAYS);
    let mut deleted = 0;
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return 0;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        // 主日志/err 分流（.log）、roll 模式 .old、滚动备份（.N）、zip 归档（.zip）都纳入清理
        let is_log = ext == "log" || ext == "old" || ext.parse::<u32>().is_ok() || ext == "zip";
        if !is_log {
            continue;
        }
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // 优先取文件名开头日期段判定（兼容滚动备份 .log.1 与 err 分流 .err.log）；
        // 自定义文件名/非 %Y-%m-%d 前缀解析失败时回退按 mtime 判定（否则永不被清理，G4 修复）
        let date = name
            .get(..10)
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
            if zip_archives && ext != "zip" && !crate::service_host::zip_backup_file(&path, "") {
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

/// 写入日志条目: `<log_dir>`/yyyy-MM-dd.log（服务宿主与刷新程序共用）
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

/// 写入服务刷新程序日志: ProgramData/Osmium/refresher/yyyy-MM-dd.log
fn write_refresher_log(channel: &str, message: &str) {
    write_log_line(&refresher_log_dir(), channel, message);
}

/// 开机维护: 校验并清理失效服务 / 孤儿目录 / 过期日志；仅扫描 svcs 平台部署目录。
/// 共享宿主复用安装目录同一份 exe，宿主升级由重装安装包覆盖；inplace 服务平台不兜底
fn refresh_outdated_hosts() {
    let services = get_service_names();
    if services.is_empty() {
        write_refresher_log(
            "refresher",
            "No registered services found, skipping cleanup",
        );
        cleanup_old_logs();
        return;
    }

    // 校验并清理失效服务 / 孤儿目录（共享宿主部署: 所有服务复用框架安装目录的同一份 exe，
    // 宿主升级由重装安装包覆盖共享 exe 完成，刷新器不再逐服务替换宿主副本）
    for svc_name in &services {
        // 刷新程序自身不部署 svcs 目录，跳过保留名目录
        if !svc_name.eq_ignore_ascii_case("Osmium Service Refresher") {
            cleanup_invalid_service(svc_name);
        }
    }

    let services = get_service_names();
    if services.is_empty() {
        write_refresher_log("refresher", "All services were stale, nothing to clean");
        cleanup_old_logs();
        return;
    }
    write_refresher_log(
        "refresher",
        &f(
            "Scanning {0} registered service(s)",
            &[&services.len().to_string()],
        ),
    );

    cleanup_old_logs();
}

/// 校验服务配置有效性: toml 缺失/可执行路径不存在/解析失败则从 SCM 移除并删宿主目录，
/// 并清理 SCM 无记录但目录仍在的孤儿；仅扫描 svcs 部署目录，inplace 服务不兜底清理
fn cleanup_invalid_service(svc_name: &str) {
    let base = registry_dir().join(svc_name);
    // 卸载残留: 卸载流程中断可能只删了 SCM 记录而遗留目录
    if !service_exists(svc_name) {
        write_refresher_log(
            "warn",
            &f(
                "[{0}] Service not in SCM, removing orphaned directory",
                &[svc_name],
            ),
        );
        safe_delete_dir(&base);
        return;
    }
    // 安全边界: 仅当目录对应 Osmium 部署的服务才可操作；普通用户可伪造与系统服务同名的空目录，
    // 直接按目录名停止/卸载会诱导 SYSTEM 刷新器删除无关服务
    if !is_osmium_deployed(svc_name) {
        write_refresher_log(
            "warn",
            &f(
                "[{0}] Invalid config ({1}), removing stale service",
                &[svc_name, "not an Osmium-managed service"],
            ),
        );
        return;
    }
    let config_path = deployed_config_path(svc_name);

    if !config_path.exists() {
        write_refresher_log(
            "warn",
            &f(
                "[{0}] Config file missing, removing stale service",
                &[svc_name],
            ),
        );
        remove_stale_service(svc_name);
        return;
    }

    // 解析失败用 catch_unwind 兜底；配置 download_url 的服务启动时才下载，
    // 开机扫描时跳过存在性校验避免误删。 存在性检查前须展开 %VAR%/%BASE%（%BASE% = 配置所在目录 = svcs\<name>）—— 部署配置可能含环境变量（如 %ProgramFiles%\app.exe），不展开会被误判"不存在"而删服务
    let invalid_exe = std::panic::catch_unwind(|| {
        let config = load_config(&config_path);
        let has_download = has_download(&config);
        if !has_download {
            let base = config_path
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            let exe = crate::service_host::expand_env_value(&config.service_executable_path, &base);
            if !Path::new(&exe).exists() {
                Some(exe)
            } else {
                None
            }
        } else {
            None
        }
    });
    match invalid_exe {
        Ok(Some(exe_path)) => {
            write_refresher_log(
                "warn",
                &f(
                    "[{0}] Invalid executable path '{1}', removing stale service",
                    &[svc_name, &exe_path],
                ),
            );
            remove_stale_service(svc_name);
        }
        Ok(None) => {}
        Err(payload) => {
            let detail = panic_msg(&*payload, "unknown error");
            write_refresher_log(
                "warn",
                &f(
                    "[{0}] Invalid config ({1}), removing stale service",
                    &[svc_name, &detail],
                ),
            );
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

/// 挪出 svcs`<name>`\logs 到系统临时目录（install 更新前调用），无 logs 或挪出失败返回 None
fn backup_service_logs(svc_name: &str) -> Option<PathBuf> {
    backup_logs_dir(&base_dir(svc_name), svc_name)
}

/// install 更新后把备份的 logs 还原回新建的 svcs`<name>` 目录
fn restore_service_logs(svc_name: &str, backup: Option<PathBuf>) {
    restore_logs_dir(&base_dir(svc_name), backup);
}

/// 挪出 base\logs 到系统临时目录（供测试复用），tag 用于保证备份路径唯一
pub(crate) fn backup_logs_dir(base: &Path, tag: &str) -> Option<PathBuf> {
    let logs = base.join("logs");
    if !logs.is_dir() {
        return None;
    }
    let backup = std::env::temp_dir().join(format!("osmium-logs-backup-{tag}"));
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&logs, &backup).ok().map(|_| backup)
}

/// 还原备份的 logs 到 base 目录（目录不存在时先创建）
pub(crate) fn restore_logs_dir(base: &Path, backup: Option<PathBuf>) {
    if let Some(b) = backup {
        let _ = std::fs::create_dir_all(base);
        let _ = std::fs::rename(&b, base.join("logs"));
    }
}

/// 移除失效服务: 停止 → 卸载 SCM 服务 → 等待删除 → 删除宿主目录
fn remove_stale_service(svc_name: &str) {
    force_remove_service(svc_name, true);
    write_refresher_log("refresher", &f("[{0}] Stale service removed", &[svc_name]));
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

/// 限速包装 reader: 按 rate_bps（字节/秒）节流读取（0 = 不限速）。
/// 简单令牌桶: 每读一段后按"已读总量/速率"应耗时长 sleep，突发后平滑到平均速率；
/// shared 计数器提供时多请求共享同一配额（分块并行下载的聚合带宽仍≈配置值，而非 ×N）
struct RateLimitedReader<R> {
    inner: R,
    rate_bps: u64,
    /// 已读总量（本地计数或共享分块配额）
    bytes: u64,
    shared: Option<std::sync::Arc<AtomicU64>>,
    start: Instant,
}

impl<R: std::io::Read> RateLimitedReader<R> {
    fn new(inner: R, rate_bps: u64) -> Self {
        Self {
            inner,
            rate_bps,
            bytes: 0,
            shared: None,
            start: Instant::now(),
        }
    }

    /// 共享配额模式: total 传入各 worker 已共享的原子计数器与统一计时起点
    fn new_shared(
        inner: R,
        rate_bps: u64,
        shared: std::sync::Arc<AtomicU64>,
        start: Instant,
    ) -> Self {
        Self {
            inner,
            rate_bps,
            bytes: 0,
            shared: Some(shared),
            start,
        }
    }
}

impl<R: std::io::Read> std::io::Read for RateLimitedReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if self.rate_bps > 0 && n > 0 {
            let total = match &self.shared {
                Some(counter) => counter.fetch_add(n as u64, Ordering::Relaxed) + n as u64,
                None => {
                    self.bytes += n as u64;
                    self.bytes
                }
            };
            let expected = total as f64 / self.rate_bps as f64;
            let elapsed = self.start.elapsed().as_secs_f64();
            if elapsed < expected {
                thread::sleep(Duration::from_secs_f64(expected - elapsed));
            }
        }
        Ok(n)
    }
}

/// 构造下载 Agent（全局超时覆盖整个下载；4xx/5xx 不转错误，调用方按状态码处理）
/// Agent 缓存（按 timeout+proxy 复用连接池/DNS）: 多下载条目/重试循环不再每次重建；
/// 小 map 覆盖常见组合（单槽在 timeout/proxy 交替时会反复重建）
fn cached_agent(timeout_secs: u64, proxy: Option<&str>) -> ureq::Agent {
    use std::collections::HashMap;
    static CACHE: Mutex<Option<HashMap<String, ureq::Agent>>> = Mutex::new(None);
    let key = format!("{timeout_secs}|{proxy:?}");
    let mut guard = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    let map = guard.get_or_insert_with(HashMap::new);
    if let Some(agent) = map.get(&key) {
        return agent.clone();
    }
    let agent = build_agent(timeout_secs, proxy);
    // 上限 8 槽防无限增长（超限丢弃最早项）
    if map.len() >= 8
        && let Some(oldest) = map.keys().next().cloned()
    {
        map.remove(&oldest);
    }
    map.insert(key, agent.clone());
    agent
}

fn build_agent(timeout_secs: u64, proxy: Option<&str>) -> ureq::Agent {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .max_redirects(0); // 关闭自动重定向: 由 single_download 手动跟随并拒绝 https→http 降级
    if let Some(proxy_url) = proxy
        && let Ok(p) = ureq::Proxy::new(proxy_url)
    {
        builder = builder.proxy(Some(p));
    }
    ureq::Agent::new_with_config(builder.build())
}

/// 多线程分块下载核心: HEAD 探测 Range，支持且 >1MiB 时分块并发（threads 0/1 禁用），失败回退单线程；
/// tmp 以 CreateNew 创建（TOCTOU 防护）；304 视为完成删 tmp 保留原目标； rate_bps > 0 时按速率节流（限速下载）
#[allow(clippy::too_many_arguments)] // 全部为下载所需配置项，参数打包反增调用点负担
pub(crate) fn download_core(
    url: &str,
    tmp: &str,
    timeout_secs: u64,
    auth: DownloadAuth<'_>,
    proxy: Option<&str>,
    threads: i32,
    if_modified_since: Option<String>,
    rate_bps: u64,
) -> Result<(), (bool, String)> {
    let client = cached_agent(timeout_secs, proxy);

    // CreateNew 原子创建，拒绝预创建文件替换；残留同名文件清理后重试一次。
    // 断点续传: tmp 非空时复用（分块模式下按块跳过已下载区间，避免整文件重传）。 必须带 read 权限——chunk_already_done 的 seek_read 判定需要读 tmp（仅 write 会拒绝访问）
    let create = || {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(tmp)
    };
    let file = match create() {
        Ok(f) => f,
        Err(_) => {
            let reuse = std::fs::metadata(tmp).map(|m| m.len() > 0).unwrap_or(false);
            if reuse {
                std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(tmp)
                    .map_err(|e| (false, e.to_string()))?
            } else {
                let _ = std::fs::remove_file(tmp);
                create().map_err(|e| (false, e.to_string()))?
            }
        }
    };

    // 断点归属标记文件（随 tmp 派生）: 记录上次下载源（URL 哈希）与总长，复用前校验——
    // 防止更换下载源后旧资源的块被 chunk_already_done 误判已完成而混合污染；
    // 成功完成后清理，失败残留由下次复用时的校验覆盖
    let marker_path = PathBuf::from(format!("{tmp}.resume"));

    // 304 优化: 目标已存在且无 sha 校验时发送 If-Modified-Since，并强制单线程
    //（Range+If-Modified-Since 组合无意义，未变化时服务器直接回 304）
    if let Some(date) = if_modified_since {
        // 单线程路径从头写：清空残留 tmp + 显式定位到文件头
        //（set_len 在 Windows 上不一定把文件位置重置到 0，io::copy 会从当前偏移写）
        use std::io::{Seek, SeekFrom};
        let _ = file.set_len(0);
        let _ = (&file).seek(SeekFrom::Start(0));
        return match single_download(&client, url, &file, auth, Some(&date), rate_bps) {
            Ok(SingleOutcome::NotModified) => {
                // 目标未变化: 保留原文件，删除空 tmp 与断点标记，视为下载完成
                let _ = std::fs::remove_file(tmp);
                let _ = std::fs::remove_file(&marker_path);
                Ok(())
            }
            Ok(SingleOutcome::Downloaded) => {
                let _ = std::fs::remove_file(&marker_path);
                Ok(())
            }
            Err(e) => Err(e),
        };
    }

    // 探测: HEAD 取 Content-Length 与 Accept-Ranges；HEAD 异常视为不支持 Range，直接单线程。
    // 认证资源首探 401/403 时带凭据重试一次——否则 Basic 下载永远无法分块/预检磁盘
    let probe = match client.head(url).call() {
        Ok(r)
            if !r.status().is_success()
                && [401, 403].contains(&r.status().as_u16())
                && let DownloadAuth::Basic(user, pass) = auth =>
        {
            use base64::Engine as _;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            client
                .head(url)
                .header("authorization", format!("Basic {token}"))
                .call()
        }
        other => other,
    };
    if let Ok(resp) = probe
        && resp.status().is_success()
    {
        let ranges_ok = resp
            .headers()
            .get("accept-ranges")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("bytes"))
            .unwrap_or(false);
        if ranges_ok
            && let Some(size) = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
        {
            // 磁盘空间预检: 目标卷剩余空间不足（预留 64MB 余量）则拒绝下载，
            // 防大文件写满磁盘导致系统异常
            if !disk_space_ok(tmp, size) {
                return Err((
                    false,
                    format!("insufficient disk space for {size} bytes at '{tmp}'"),
                ));
            }
            // 断点复用前校验 tmp 归属: 标记内容（URL 哈希+长度）与本次不符说明是旧源残留，
            // 清零整体重下——否则旧资源的数据块会被 chunk_already_done 误判已完成而混合污染；
            // 标记匹配且 tmp 不超长时保留（正常断点续传，半成品必然短于远端）。
            // 用完整 URL 的哈希而非脱敏串: 脱敏会剥离 query，仅换查询串的换源（CDN cache-busting）会被漏判
            let url_hash = {
                use sha2::{Digest, Sha256};
                let mut h = Sha256::new();
                h.update(url.as_bytes());
                h.finalize()
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect::<String>()
            };
            let expected = format!("{url_hash}\n{size}");
            let trusted = std::fs::read_to_string(&marker_path)
                .map(|m| m == expected)
                .unwrap_or(false);
            let tmp_len = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
            if !trusted || tmp_len > size {
                let _ = file.set_len(0);
                let _ = std::fs::write(&marker_path, &expected);
            }
            if size > CHUNK_SIZE
                && threads >= 2
                && chunked_download(&client, url, &file, auth, size, threads as u64, rate_bps)
                    .is_ok()
            {
                let _ = std::fs::remove_file(&marker_path);
                return Ok(());
            }
            // 分块失败（服务器实际不支持 Range/网络异常）→ 清零后回退单线程
            let _ = file.set_len(0);
        }
    }
    // 单线程路径从头写：清空可能复用的残留 tmp + 显式定位到文件头
    //（set_len 在 Windows 上不一定把文件位置重置到 0，io::copy 会从当前偏移写）
    use std::io::{Seek, SeekFrom};
    let _ = file.set_len(0);
    let _ = (&file).seek(SeekFrom::Start(0));
    let outcome = single_download(&client, url, &file, auth, None, rate_bps);
    // 下载产物已就绪，断点标记完成使命一并清理（失败路径残留由下次复用校验覆盖）
    let _ = std::fs::remove_file(&marker_path);
    outcome.map(|_| ())
}

/// 单线程完整下载（不支持 Range / 小文件 / 分块回退路径；可选 If-Modified-Since 头）;
/// 服务器回 304 时返回 NotModified 且不写内容；手动跟随重定向（最多 10 次）， 拒绝 https→http 降级（凭据经明文链路外泄/响应被篡改防护）
fn single_download(
    client: &ureq::Agent,
    url: &str,
    file: &std::fs::File,
    auth: DownloadAuth<'_>,
    if_modified_since: Option<&str>,
    rate_bps: u64,
) -> Result<SingleOutcome, (bool, String)> {
    // 凭据同源判定基准（scheme+host+端口）: Basic 认证仅在原始服务器及其同源重定向链上发送，
    // 跨主机/跨端口重定向不携带 Authorization——防凭据经 302 外泄到第三方服务器
    let origin = url::Url::parse(url).ok().map(same_origin_key);
    let mut current = url.to_string();
    for _ in 0..10 {
        let mut req = client.get(&current);
        let same_origin = url::Url::parse(&current)
            .ok()
            .map(same_origin_key)
            .is_some_and(|k| Some(k) == origin);
        if same_origin && let DownloadAuth::Basic(user, pass) = auth {
            use base64::Engine as _;
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
            req = req.header("authorization", format!("Basic {token}"));
        }
        if let Some(date) = if_modified_since {
            req = req.header("if-modified-since", date);
        }
        let resp = req
            .call()
            .map_err(|e| (matches!(e, ureq::Error::Timeout(_)), e.to_string()))?;
        let status = resp.status().as_u16();
        if status == 304 {
            return Ok(SingleOutcome::NotModified);
        }
        // 手动跟随重定向（max_redirects=0 时返回 3xx 响应）: https→http 降级直接拒绝
        if (300..400).contains(&status) {
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string);
            let Some(loc) = loc else {
                return Err((
                    false,
                    format!("redirect without Location header (HTTP {status})"),
                ));
            };
            let next = resolve_redirect_url(&current, &loc);
            if current.starts_with("https://") && next.starts_with("http://") {
                return Err((
                    false,
                    format!("insecure redirect refused: {current} -> {next}"),
                ));
            }
            current = next;
            continue;
        }
        if !resp.status().is_success() {
            // 401: 明确提示认证配置问题（Basic 凭据错误/未配置）
            let hint = if status == 401 {
                "server returned HTTP 401 Unauthorized — check download_username/download_password or server authentication requirements"
            } else {
                &format!("server returned HTTP {}", status)
            };
            return Err((false, hint.to_string()));
        }
        // 完整性校验基准: 服务器声明 Content-Length 时，实际收到的字节数必须一致——
        // 连接提前干净关闭（无 IO 错误）的截断响应按失败处理，防静默损坏
        let expected_len = resp
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.parse::<u64>().ok());
        let mut reader = RateLimitedReader::new(resp.into_body().into_reader(), rate_bps);
        let mut out = file.try_clone().map_err(|e| (false, e.to_string()))?;
        // 防御: 明确从文件头写（try_clone 句柄共享文件位置，不依赖调用方已 seek）
        use std::io::{Seek, SeekFrom};
        let _ = out.seek(SeekFrom::Start(0));
        let copied = std::io::copy(&mut reader, &mut out).map_err(|e| (false, e.to_string()))?;
        if let Some(expect) = expected_len
            && copied != expect
        {
            return Err((
                false,
                format!("truncated download: got {copied} of {expect} bytes"),
            ));
        }
        return Ok(SingleOutcome::Downloaded);
    }
    Err((false, "too many redirects (10)".to_string()))
}

/// 重定向同源判定键: (scheme, host, 端口)——三者全部一致才视为同一服务器
fn same_origin_key(u: url::Url) -> (String, String, Option<u16>) {
    (
        u.scheme().to_ascii_lowercase(),
        u.host_str().unwrap_or_default().to_ascii_lowercase(),
        u.port_or_known_default(),
    )
}

/// 解析重定向 Location（相对/绝对）: 基于当前 URL 做 RFC 3986 解析
pub(crate) fn resolve_redirect_url(current: &str, location: &str) -> String {
    url::Url::parse(current)
        .ok()
        .and_then(|base| base.join(location).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| location.to_string())
}

/// 按 CHUNK_SIZE 分块并发下载到预分配文件（各块独立线程，Windows seek_write 按偏移写）
fn chunked_download(
    client: &ureq::Agent,
    url: &str,
    file: &std::fs::File,
    auth: DownloadAuth<'_>,
    size: u64,
    max_workers: u64,
    rate_bps: u64,
) -> Result<(), (bool, String)> {
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
    // 取消标志: 任一块最终失败即置位，其余 worker 尽快退出（不再白拉剩余块）
    let cancelled = Arc::new(AtomicBool::new(false));
    // 共享限速配额: 各 worker 的读取量计入同一计数器（聚合带宽≈rate_bps，而非 rate×workers）
    let quota = Arc::new(AtomicU64::new(0));
    let quota_start = Instant::now();
    let mut handles = Vec::new();
    for w in 0..workers {
        let client = client.clone();
        let file = file.clone();
        let url = url.to_string();
        let auth_owned = auth_owned.clone();
        let cancelled = cancelled.clone();
        let quota = quota.clone();
        handles.push(thread::spawn(move || {
            let mut i = w;
            while i < chunk_count {
                if cancelled.load(Ordering::Relaxed) {
                    return Err((false, "download cancelled".into()));
                }
                let start = i * CHUNK_SIZE;
                let end = (start + CHUNK_SIZE - 1).min(size - 1);
                // 断点续传: 复用 tmp 中已完整写入的块（长度覆盖块尾且区间非全零）直接跳过
                if chunk_already_done(&file, start, end) {
                    i += workers;
                    continue;
                }
                let mut attempt = 0u32;
                loop {
                    if download_chunk(
                        &client,
                        &url,
                        &file,
                        auth_owned.as_ref().map(|(u, p)| (u.as_str(), p.as_str())),
                        start,
                        end,
                        rate_bps,
                        Some(quota.clone()),
                        quota_start,
                    )
                    .is_ok()
                    {
                        break;
                    }
                    attempt += 1;
                    if attempt > CHUNK_MAX_RETRIES {
                        cancelled.store(true, Ordering::Relaxed);
                        return Err((
                            false,
                            format!("chunk {}-{} failed after retries", start, end),
                        ));
                    }
                    if cancelled.load(Ordering::Relaxed) {
                        return Err((false, "download cancelled".into()));
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

/// 解析 Content-Range: "bytes 0-1023/2048" → (0, 1023)；格式非法返回 None
fn parse_content_range(value: &str) -> Option<(u64, u64)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, _total) = rest.split_once('/')?;
    let (a, b) = range.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

/// 下载单个分块（Range 请求）并写入文件偏移；服务器必须返回 206。
/// quota 提供时读取量计入分块共享限速配额（None = 本地独立计数）
#[allow(clippy::too_many_arguments)] // 全部为分块下载所需配置项，参数打包反增调用点负担
fn download_chunk(
    client: &ureq::Agent,
    url: &str,
    file: &std::fs::File,
    auth: Option<(&str, &str)>,
    start: u64,
    end: u64,
    rate_bps: u64,
    quota: Option<std::sync::Arc<AtomicU64>>,
    quota_start: Instant,
) -> Result<(), (bool, String)> {
    use std::io::Read;
    use std::os::windows::fs::FileExt;

    let mut req = client
        .get(url)
        .header("range", format!("bytes={}-{}", start, end));
    if let Some((user, pass)) = auth {
        use base64::Engine as _;
        let token = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        req = req.header("authorization", format!("Basic {token}"));
    }
    let resp = req
        .call()
        .map_err(|e| (matches!(e, ureq::Error::Timeout(_)), e.to_string()))?;
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
        return Err((
            false,
            format!(
                "server returned HTTP {} for ranged request",
                resp.status().as_u16()
            ),
        ));
    }
    // Content-Range 校验: 响应声明的区间必须与请求一致——恶意/异常服务器回错位片段
    // 会静默拼坏文件（仅 sha 配置时兜底），区间不符直接失败
    let declared = resp
        .headers()
        .get("content-range")
        .and_then(|v| v.to_str().ok())
        .and_then(parse_content_range)
        .ok_or_else(|| {
            (
                false,
                format!("chunk {start}-{end}: missing/invalid Content-Range header"),
            )
        })?;
    if declared != (start, end) {
        return Err((
            false,
            format!(
                "chunk {start}-{end}: server returned mismatched Content-Range {}",
                resp.headers()
                    .get("content-range")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
            ),
        ));
    }
    let mut reader = match quota {
        Some(q) => {
            RateLimitedReader::new_shared(resp.into_body().into_reader(), rate_bps, q, quota_start)
        }
        None => RateLimitedReader::new(resp.into_body().into_reader(), rate_bps),
    };
    let mut buf = [0u8; 64 * 1024];
    let mut offset = start;
    loop {
        let n = reader.read(&mut buf).map_err(|e| (false, e.to_string()))?;
        if n == 0 {
            break;
        }
        file.seek_write(&buf[..n], offset)
            .map_err(|e| (false, e.to_string()))?;
        offset += n as u64;
    }
    // 完整性校验: 实际收到的字节数必须等于请求的区间长度——
    // 服务器提前干净关闭连接（无 IO 错误）的截断响应按失败处理，防块数据静默缺失
    if offset - start != end - start + 1 {
        return Err((
            false,
            format!(
                "chunk {}-{} truncated: got {} of {} bytes",
                start,
                end,
                offset - start,
                end - start + 1
            ),
        ));
    }
    Ok(())
}

/// 计算文件 SHA-256（小写十六进制）并比较；未提供校验值视为匹配。
/// 流式分块读取: 大文件（多 GB 下载产物）不全量载入内存
pub(crate) fn sha256_matches(path: &str, expected: Option<&str>) -> bool {
    use sha2::{Digest, Sha256};
    use std::io::Read;
    let Some(sha) = expected else { return true };
    let sha = sha.trim();
    if sha.is_empty() {
        return true;
    }
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 64 * 1024];
    loop {
        match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => hasher.update(&buf[..n]),
            Err(_) => return false,
        }
    }
    let hex: String = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    hex == sha.to_lowercase()
}

/// 断点续传判定: 分块区间是否已完整写入 tmp（文件长度覆盖块尾且区间**全部非零**——
/// 部分写入的块（中断/半块）必须重新下载，any 判定会把残缺块误判为已完成导致数据缺失）。
/// 已知取舍: 内容恰好全零的完整块会被重复下载（正确性不受影响，仅续传效率退化）
pub(crate) fn chunk_already_done(file: &std::fs::File, start: u64, end: u64) -> bool {
    use std::os::windows::fs::FileExt;
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len < end + 1 {
        return false;
    }
    let mut buf = vec![0u8; (end - start + 1) as usize];
    file.seek_read(&mut buf, start)
        .map(|n| n == buf.len() && buf.iter().all(|&b| b != 0))
        .unwrap_or(false)
}

/// 磁盘空间预检: 目标文件所在卷的调用者可用空间（配额感知）是否足够（need 字节 + 64MB 余量）；
/// 查询失败（路径无效等）视为通过（由后续写入报错兜底）
fn disk_space_ok(path: &str, need: u64) -> bool {
    use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
    let wide = to_wide(path);
    let mut free_to_caller = 0u64;
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            PCWSTR::from_raw(wide.as_ptr()),
            Some(&mut free_to_caller),
            None,
            None,
        )
        .is_ok()
    };
    if !ok {
        return true;
    }
    // 预留 64MB 余量（下载产物/解压等后续操作也需要空间）；用配额口径的可用空间而非卷总量
    free_to_caller >= need.saturating_add(64 * 1024 * 1024)
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

        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;

        // 宽字符串必须保持存活直到 CreateServiceW 调用完成
        let dep_str = build_dependency_string(p.dependencies);
        let dep_wide = dep_str.as_deref().map(to_wide);
        let dep_pcwstr = dep_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let account_wide = p.service_account.map(to_wide);
        let account_pcwstr = account_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());
        let password_wide = p.password.map(to_wide);
        let password_pcwstr = password_wide
            .as_deref()
            .map(|w| PCWSTR::from_raw(w.as_ptr()))
            .unwrap_or(PCWSTR::null());

        // interactive=true 时附加 SERVICE_INTERACTIVE_PROCESS（可交互桌面）
        let mut service_type = SERVICE_WIN32_OWN_PROCESS;
        if p.interactive {
            service_type |= ENUM_SERVICE_TYPE(SERVICE_INTERACTIVE_PROCESS);
        }

        // DeleteService 后 SCM 可能仍处于"已标记删除"（1072）状态，立即以同名重建会失败。
        // wait_service_deleted 已尽量等待，此处再做最后防线: 遇到 1072 时短暂重试（最长 5 秒）
        let mut svc = Err(windows::core::Error::from_hresult(
            windows::core::HRESULT::from_win32(0),
        ));
        for attempt in 0..31 {
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
                Err(e) if e.code().0 as u32 & 0xFFFF == 1072 && attempt < 30 => {
                    thread::sleep(Duration::from_millis(500));
                }
                Err(_) => break,
            }
        }
        let svc = svc.map_err(|e| format!("{}: {e}", "Failed to create service"))?;

        // 服务创建后的全部配置步骤在闭包内执行: 任一步失败都先关闭句柄再传播错误
        //（旧实现 `?` 提前返回会泄漏 svc/scm 句柄）
        let configured = (|| -> Result<(), String> {
            // 设置描述（失败必须传播，不能静默缺失，P2-3）
            let desc_wide = to_wide(p.description);
            let desc_info = SERVICE_DESCRIPTIONW {
                lpDescription: PWSTR::from_raw(desc_wide.as_ptr() as *mut _),
            };
            ChangeServiceConfig2W(
                svc,
                SERVICE_CONFIG_DESCRIPTION,
                Some(&desc_info as *const _ as *const _),
            )
            .map_err(|e| format!("{}: {e}", "Failed to set service description"))?;

            // 设置故障恢复（failure_action 决定动作序列）
            if p.failure_reset_sec > 0 {
                set_failure_actions(
                    svc,
                    p.failure_reset_sec,
                    p.restart_delay_ms,
                    p.failure_action,
                )?;
            }

            // 延迟自动启动
            if p.delayed_auto_start {
                let delay_info = SERVICE_DELAYED_AUTO_START_INFO {
                    fDelayedAutostart: true.into(),
                };
                ChangeServiceConfig2W(
                    svc,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    Some(&delay_info as *const _ as *const _),
                )
                .map_err(|e| format!("{}: {e}", "Failed to set delayed auto start"))?;
            }

            // 服务安全描述符（SDDL）: 应用到服务 DACL，控制谁能管理该服务（对应 WinSW securityDescriptor）
            if let Some(sddl) = p.security_descriptor {
                apply_service_sddl(svc, sddl)
                    .map_err(|e| format!("{}: {e}", "Failed to set service security descriptor"))?;
            }
            Ok(())
        })();
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        configured?;
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
pub(crate) fn security_descriptor_from_sddl(
    sddl: &str,
) -> Result<PSECURITY_DESCRIPTOR, windows::core::Error> {
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
                windows::core::HRESULT::from_win32(0),
            ));
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
    use windows::Win32::Security::Authentication::Identity::{
        LSA_HANDLE, LSA_OBJECT_ATTRIBUTES, LSA_UNICODE_STRING, LsaAddAccountRights, LsaClose,
        LsaOpenPolicy, POLICY_AUDIT_LOG_ADMIN, POLICY_CREATE_ACCOUNT, POLICY_CREATE_PRIVILEGE,
        POLICY_CREATE_SECRET, POLICY_GET_PRIVATE_INFORMATION, POLICY_LOOKUP_NAMES,
        POLICY_NOTIFICATION, POLICY_SERVER_ADMIN, POLICY_SET_AUDIT_REQUIREMENTS,
        POLICY_SET_DEFAULT_QUOTA_LIMITS, POLICY_TRUST_ADMIN, POLICY_VIEW_AUDIT_INFORMATION,
        POLICY_VIEW_LOCAL_INFORMATION, SE_SERVICE_LOGON_NAME,
    };

    // 解析账户名 → SID（".\user" 是 cmd/net 语法，LookupAccountNameW 不支持，需剥离前缀）
    unsafe {
        let name = account.strip_prefix(".\\").unwrap_or(account);
        let name_wide = to_wide(name);
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

/// virtual 账户部署目录授权: NT SERVICE`<name>` 需遍历 ProgramData\Osmium 与 svcs
/// （仅 X 权限，不可读其他服务目录的 osiml）并读写自身部署目录（M）； 失败仅告警（安装继续，服务启动时若仍缺权限由日志体现）
fn grant_virtual_account_access(account: &str, base_dir: &str) {
    let osmium_dir = registry_dir()
        .parent()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let svcs_dir = registry_dir().to_string_lossy().to_string();
    let dirs: [(&str, &str); 3] = [
        (&osmium_dir, "(OI)(CI)X"),
        (&svcs_dir, "(OI)(CI)X"),
        (base_dir, "(OI)(CI)M"),
    ];
    for (dir, perm) in dirs {
        if dir.is_empty() {
            continue;
        }
        let _ = process::Command::new("icacls.exe")
            .args([dir, "/grant", &format!("{account}:{perm}"), "/q"])
            .output();
    }
}

/// 配置故障恢复: 按 failure_action 选择动作序列（restart 默认/reboot/none）
fn set_failure_actions(
    svc: SC_HANDLE,
    reset_sec: u32,
    delay_ms: u32,
    failure_action: Option<&str>,
) -> Result<(), String> {
    unsafe {
        use windows::Win32::System::Services::SC_ACTION;
        let action_kind = match failure_action.map(|s| s.to_lowercase()).as_deref() {
            Some("reboot") => SC_ACTION_REBOOT,
            Some("none") => windows::Win32::System::Services::SC_ACTION_NONE,
            _ => SC_ACTION_RESTART,
        };
        let actions = [
            SC_ACTION {
                Type: action_kind,
                Delay: delay_ms,
            },
            SC_ACTION {
                Type: action_kind,
                Delay: delay_ms,
            },
        ];

        let fa = SERVICE_FAILURE_ACTIONSW {
            dwResetPeriod: reset_sec,
            lpRebootMsg: PWSTR::null(),
            lpCommand: PWSTR::null(),
            cActions: actions.len() as u32,
            lpsaActions: actions.as_ptr() as *mut _,
        };

        // 失败必须传播，不能静默缺失（P2-3）
        ChangeServiceConfig2W(
            svc,
            SERVICE_CONFIG_FAILURE_ACTIONS,
            Some(&fa as *const _ as *const _),
        )
        .map_err(|e| format!("{}: {e}", "Failed to set failure actions"))
    }
}

/// 将分号/逗号分隔的依赖字符串转换为 SC multi-sz 格式。
/// 不把 ':' 当分隔符: SCM 服务名允许含冒号（如某些驱动服务），按名拆分会静默错配依赖
pub(crate) fn build_dependency_string(dependencies: Option<&str>) -> Option<String> {
    let deps = dependencies?;
    if deps.trim().is_empty() {
        return None;
    }
    let parts: Vec<&str> = deps
        .split(&[';', ','][..])
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
        )
        .map_err(|e| format!("{}: {e}", "Failed to open service"))?;
        let result = DeleteService(svc);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        if let Err(e) = result {
            return Err(format!("Failed to delete service: {e}"));
        }
    }
    Ok(())
}

/// 等待服务从 SCM 完全移除，避免立即以同名重建触发延迟删除竞态（注册成功但稍后消失）。
/// 刚停止的服务被 DeleteService 后先处于"标记删除"（1072）状态——SCM 在最后一个服务句柄 关闭后才真正移除，因此 1072 也必须继续等待（只认 1060 会在更新安装时撞 ERROR_SERVICE_MARKED_FOR_DELETE 竞态失败）
fn wait_service_deleted(service_name: &str) {
    for _ in 0..100 {
        // 最长 20 秒
        unsafe {
            let name_wide = to_wide(service_name);
            let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS);
            let scm = match scm {
                Ok(h) => h,
                Err(_) => return,
            };
            let result = OpenServiceW(
                scm,
                PCWSTR::from_raw(name_wide.as_ptr()),
                SERVICE_QUERY_STATUS,
            );
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

pub(crate) fn start_service(service_name: &str, timeout: Duration) -> Result<(), String> {
    let status = get_status_raw(service_name)?;
    if status.dwCurrentState == windows::Win32::System::Services::SERVICE_RUNNING {
        return Ok(());
    }

    unsafe {
        let name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(name_wide.as_ptr()),
            SERVICE_START | SERVICE_QUERY_STATUS,
        )
        .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        let result = StartServiceW(svc, None);
        if let Err(e) = result {
            let _ = CloseServiceHandle(svc);
            let _ = CloseServiceHandle(scm);
            return Err(format!("Failed to start service: {e}"));
        }

        // 等待运行状态（复用 svc 句柄轮询，不再每次重开 SCM）
        let deadline = Instant::now() + timeout;
        let mut svc_status = SERVICE_STATUS::default();
        loop {
            let ok = QueryServiceStatus(svc, &mut svc_status).is_ok();
            if ok && svc_status.dwCurrentState == windows::Win32::System::Services::SERVICE_RUNNING
            {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(scm);
                return Ok(());
            }
            if Instant::now() > deadline {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(scm);
                return Err("Timeout waiting for service to start".into());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

pub(crate) fn stop_service(service_name: &str, timeout: Duration) -> Result<(), String> {
    let status = get_status_raw(service_name)?;
    if status.dwCurrentState == SERVICE_STOPPED {
        return Ok(());
    }
    unsafe {
        let name_wide = to_wide(service_name);
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_ALL_ACCESS)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(name_wide.as_ptr()),
            SERVICE_STOP | SERVICE_QUERY_STATUS,
        )
        .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        // 正在停止中（STOP_PENDING，如故障恢复/前一次停止尚未完成）: 跳过 ControlService，
        // 直接进入等待循环（对停止中的服务再发 STOP 会被 SCM 拒绝 ERROR_SERVICE_CANNOT_ACCEPT_CTRL）
        let mut svc_status = SERVICE_STATUS::default();
        if status.dwCurrentState != SERVICE_STOP_PENDING {
            let result = ControlService(svc, SERVICE_CONTROL_STOP, &mut svc_status);
            if let Err(e) = result {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(scm);
                return Err(format!("Failed to stop service: {e}"));
            }
        }

        // 等待停止（复用 svc 句柄轮询）
        let deadline = Instant::now() + timeout;
        loop {
            let ok = QueryServiceStatus(svc, &mut svc_status).is_ok();
            if ok && svc_status.dwCurrentState == SERVICE_STOPPED {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(scm);
                return Ok(());
            }
            if Instant::now() > deadline {
                let _ = CloseServiceHandle(svc);
                let _ = CloseServiceHandle(scm);
                return Err("Timeout waiting for service to stop".into());
            }
            thread::sleep(Duration::from_millis(200));
        }
    }
}

/// 重启间隔常量: 停止到启动之间留出的 SCM 状态稳定窗口（毫秒）
const RESTART_GAP_MS: u64 = 2000;

pub(crate) fn restart_service(
    service_name: &str,
    stop_timeout: Duration,
    start_timeout: Duration,
) -> Result<(), String> {
    stop_service(service_name, stop_timeout)?;
    thread::sleep(Duration::from_millis(RESTART_GAP_MS));
    start_service(service_name, start_timeout)
}

pub(crate) fn get_status(service_name: &str) -> Result<String, String> {
    let status = get_status_raw(service_name)?;
    match status.dwCurrentState {
        windows::Win32::System::Services::SERVICE_RUNNING => Ok("Running".into()),
        SERVICE_STOPPED => Ok("Stopped".into()),
        windows::Win32::System::Services::SERVICE_START_PENDING => Ok("Start Pending".into()),
        SERVICE_STOP_PENDING => Ok("Stop Pending".into()),
        windows::Win32::System::Services::SERVICE_PAUSED => Ok("Paused".into()),
        windows::Win32::System::Services::SERVICE_PAUSE_PENDING => Ok("Pause Pending".into()),
        windows::Win32::System::Services::SERVICE_CONTINUE_PENDING => Ok("Continue Pending".into()),
        _ => Ok(format!("Unknown ({:?})", status.dwCurrentState)),
    }
}

/// 服务注册属性详情（--status 增强用）: 启动类型/运行账户/故障恢复动作序列/重置周期
pub(crate) fn query_service_details(service_name: &str) -> Result<Vec<(String, String)>, String> {
    unsafe {
        let name_wide = to_wide(service_name);
        // 只读查询用 CONNECT 最小权限打开 SCM（与 get_status_raw 同款，免管理员）
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(name_wide.as_ptr()),
            SERVICE_QUERY_STATUS | SERVICE_QUERY_CONFIG,
        )
        .map_err(|e| {
            let _ = CloseServiceHandle(scm);
            format!("{}: {e}", "Failed to open service")
        })?;
        let result = (|| -> Result<Vec<(String, String)>, String> {
            let mut details = Vec::new();
            // QueryServiceConfigW: 启动类型 + 运行账户（两次调用: 先取所需大小再填充）。
            // 第一次调用传空缓冲必然返回 ERROR_INSUFFICIENT_BUFFER(122)——windows crate 映射为 Err，
            // 这是"查大小"的标准模式，必须容忍该错误码（needed 会返回实际所需大小）
            let mut needed = 0u32;
            let size_call = QueryServiceConfigW(svc, None, 0, &mut needed);
            let size_ok = match size_call {
                Ok(_) => true,
                Err(e) if e.code().0 as u32 & 0xFFFF == 122 => true, // ERROR_INSUFFICIENT_BUFFER
                Err(_) => false,
            };
            if !size_ok || needed == 0 {
                return Err("Failed to query service config".into());
            }
            let mut buf = vec![0u8; needed as usize];
            QueryServiceConfigW(
                svc,
                Some(buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW),
                needed,
                &mut needed,
            )
            .map_err(|e| format!("Failed to query service config: {e}"))?;
            let config = &*(buf.as_ptr() as *const QUERY_SERVICE_CONFIGW);
            let start_type = match config.dwStartType {
                SERVICE_AUTO_START => "Automatic",
                SERVICE_DEMAND_START => "Manual",
                SERVICE_DISABLED => "Disabled",
                _ => "Unknown",
            };
            details.push(("Start type".to_string(), start_type.to_string()));
            // 延迟启动标志（SERVICE_CONFIG_DELAYED_AUTO_START_INFO 独立查询）
            if config.dwStartType == SERVICE_AUTO_START {
                let mut needed_d = 0u32;
                let _ = QueryServiceConfig2W(
                    svc,
                    SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                    None,
                    &mut needed_d,
                );
                if needed_d > 0 {
                    let mut buf_d = vec![0u8; needed_d as usize];
                    if QueryServiceConfig2W(
                        svc,
                        SERVICE_CONFIG_DELAYED_AUTO_START_INFO,
                        Some(&mut buf_d),
                        &mut needed_d,
                    )
                    .is_ok()
                    {
                        let info = &*(buf_d.as_ptr() as *const SERVICE_DELAYED_AUTO_START_INFO);
                        if info.fDelayedAutostart.as_bool() {
                            details.push((
                                "Start type".to_string(),
                                "Automatic (Delayed)".to_string(),
                            ));
                        }
                    }
                }
            }
            let account = if config.lpServiceStartName.is_null() {
                String::new()
            } else {
                String::from_utf16_lossy(std::slice::from_raw_parts(
                    config.lpServiceStartName.0,
                    wcs_len(config.lpServiceStartName),
                ))
            };
            details.push(("Run as".to_string(), account));
            // QueryServiceConfig2W: 故障恢复动作序列 + 重置周期
            let mut needed2 = 0u32;
            let _ = QueryServiceConfig2W(svc, SERVICE_CONFIG_FAILURE_ACTIONS, None, &mut needed2);
            if needed2 > 0 {
                let mut buf2 = vec![0u8; needed2 as usize];
                if QueryServiceConfig2W(
                    svc,
                    SERVICE_CONFIG_FAILURE_ACTIONS,
                    Some(&mut buf2),
                    &mut needed2,
                )
                .is_ok()
                {
                    let fa = &*(buf2.as_ptr() as *const SERVICE_FAILURE_ACTIONSW);
                    if !fa.lpsaActions.is_null() && fa.cActions > 0 {
                        let seq: Vec<String> =
                            std::slice::from_raw_parts(fa.lpsaActions, fa.cActions as usize)
                                .iter()
                                .map(|a| {
                                    let name = match a.Type {
                                        SC_ACTION_RESTART => "restart",
                                        SC_ACTION_REBOOT => "reboot",
                                        _ => "none",
                                    };
                                    if a.Delay > 0 {
                                        format!("{name}({}s)", a.Delay / 1000)
                                    } else {
                                        name.to_string()
                                    }
                                })
                                .collect();
                        details.push(("Failure actions".to_string(), seq.join(", ")));
                    }
                    if fa.dwResetPeriod > 0 {
                        details.push((
                            "Failure reset".to_string(),
                            format!("{}s", fa.dwResetPeriod),
                        ));
                    }
                }
            }
            Ok(details)
        })();
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);
        result
    }
}

/// 宽字符串长度（以 null 结尾，wcslen 等价）
fn wcs_len(ptr: PWSTR) -> usize {
    let mut len = 0usize;
    unsafe {
        while *ptr.0.add(len) != 0 {
            len += 1;
        }
    }
    len
}

fn get_status_raw(service_name: &str) -> Result<SERVICE_STATUS, String> {
    unsafe {
        let name_wide = to_wide(service_name);
        // 只读查询用 CONNECT 最小权限打开 SCM: 状态/详情查询免管理员（写操作路径仍 ALL_ACCESS）
        let scm = OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SC_MANAGER_CONNECT)
            .map_err(|e| format!("{}: {e}", "Failed to open service control manager"))?;
        let svc = OpenServiceW(
            scm,
            PCWSTR::from_raw(name_wide.as_ptr()),
            SERVICE_QUERY_STATUS,
        )
        .map_err(|e| format!("{}: {e}", "Failed to open service"))?;

        let mut status = SERVICE_STATUS::default();
        let result = QueryServiceStatus(svc, &mut status);
        let _ = CloseServiceHandle(svc);
        let _ = CloseServiceHandle(scm);

        if let Err(e) = result {
            return Err(format!("Failed to query status: {e}"));
        }
        Ok(status)
    }
}

fn service_exists(service_name: &str) -> bool {
    get_status_raw(service_name).is_ok()
}

// ==================== 服务宿主/刷新程序入口 (SCM) ====================

/// 当前进程是否为刷新程序模式（true=-internal --refresher, false=宿主）
static SCM_REFRESHER_MODE: Mutex<Option<bool>> = Mutex::new(None);
/// 共享宿主显式服务名（-internal --run `<name>` 传入；None 时取 exe 文件名）
static SCM_EXPLICIT_NAME: Mutex<Option<String>> = Mutex::new(None);
static STOP_FLAG: AtomicBool = AtomicBool::new(false);
static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);
/// 是否启用 SCM preshutdown 通知（由宿主导入，scm_status_params 读取后决定上报 SERVICE_ACCEPT_PRESHUTDOWN）
static PRESHUTDOWN_ENABLED: AtomicBool = AtomicBool::new(false);
/// 是否仅执行带有效 Authenticode 签名的插件（require_signed_plugins 配置，宿主运行时导入）
static REQUIRE_SIGNED_PLUGINS: AtomicBool = AtomicBool::new(false);

/// 是否已收到 SCM 停止/关机信号（故障恢复 delay 分段等待期间轮询，保证管理员可随时停止服务）
pub(crate) fn scm_stop_requested() -> bool {
    STOP_FLAG.load(Ordering::SeqCst) || SHUTDOWN_FLAG.load(Ordering::SeqCst)
}
/// SCM 状态上报 dwWaitHint（毫秒），默认 1 小时（覆盖 prestart 钩子 60s 与启动前下载 300s）
static SCM_WAIT_HINT_MS: AtomicU32 = AtomicU32::new(3_600_000);
/// 宿主主循环 SCM 信号轮询间隔（毫秒）
static SCM_SLEEP_TIME_MS: AtomicU32 = AtomicU32::new(500);

/// 开关 SCM preshutdown 通知（host 在 on_start 读取配置后调用）
pub(crate) fn set_preshutdown_enabled(enabled: bool) {
    PRESHUTDOWN_ENABLED.store(enabled, Ordering::SeqCst);
}

/// 是否要求插件带有效 Authenticode 签名（require_signed_plugins 配置，host 在 on_start 读取后调用）
pub(crate) fn require_signed_plugins() -> bool {
    REQUIRE_SIGNED_PLUGINS.load(Ordering::SeqCst)
}

/// 开关插件签名强制（host 在 on_start 读取配置后调用）
pub(crate) fn set_require_signed_plugins(enabled: bool) {
    REQUIRE_SIGNED_PLUGINS.store(enabled, Ordering::SeqCst);
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

/// 当前 SCM 注册的服务名: 刷新程序使用保留名，共享宿主按显式名，普通宿主取自身文件名
fn scm_svc_name(refresher: bool) -> String {
    if refresher {
        "Osmium Service Refresher".to_string()
    } else if let Some(name) = SCM_EXPLICIT_NAME.lock().unwrap().clone() {
        name
    } else {
        crate::service_host::ServiceHost::svc_name()
    }
}

fn scm_entry(refresher_mode: bool, explicit_name: Option<String>) {
    use windows::Win32::System::Services::{SERVICE_TABLE_ENTRYW, StartServiceCtrlDispatcherW};

    *SCM_REFRESHER_MODE.lock().unwrap() = Some(refresher_mode);
    *SCM_EXPLICIT_NAME.lock().unwrap() = explicit_name;
    let svc_name = scm_svc_name(refresher_mode);

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
            eprintln!(
                "{}",
                red("Error: service control dispatcher failed — must be launched by SCM")
            );
        }
    }
}

fn scm_service_main() {
    use windows::Win32::Foundation::NO_ERROR;
    use windows::Win32::System::Services::{
        RegisterServiceCtrlHandlerExW, SERVICE_RUNNING, SERVICE_START_PENDING,
        SERVICE_STOP_PENDING, SERVICE_STOPPED,
    };

    let refresher = SCM_REFRESHER_MODE.lock().unwrap().unwrap_or(false);

    let svc_name = scm_svc_name(refresher);
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
                    || x == windows::Win32::System::Services::SERVICE_CONTROL_PRESHUTDOWN
                        as i32 =>
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
                eprintln!(
                    "{}",
                    red(&f(
                        "Failed to register SCM control handler for '{0}': {1}",
                        &[&svc_name, &e.to_string()]
                    ))
                );
                return;
            }
        };

        // SCM 默认只等待 30 秒启动完成，但 prestart 钩子最长 60s、启动前下载最长 300s，
        // 必须先申请额外启动时间（waitHint，可配 scm_wait_hint_ms），否则 SCM 会判定服务无响应并终止
        report_scm_status(
            status_handle,
            SERVICE_START_PENDING.0,
            0,
            SCM_WAIT_HINT_MS.load(Ordering::SeqCst),
        );

        if refresher {
            report_scm_status(status_handle, SERVICE_RUNNING.0, 0, 0);
            refresh_outdated_hosts();
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
                    report_scm_status(
                        status_handle,
                        SERVICE_STOP_PENDING.0,
                        0,
                        SCM_WAIT_HINT_MS.load(Ordering::SeqCst),
                    );
                    host.on_stop();
                    break;
                }
                if SHUTDOWN_FLAG.load(Ordering::SeqCst) {
                    host.write_log("host", "SCM shutdown signal received");
                    report_scm_status(
                        status_handle,
                        SERVICE_STOP_PENDING.0,
                        0,
                        SCM_WAIT_HINT_MS.load(Ordering::SeqCst),
                    );
                    host.on_shutdown();
                    break;
                }
                // 子进程退出监控与异常自动重启由宿主内部处理
                if !host.tick() {
                    break;
                }
                thread::sleep(Duration::from_millis(
                    SCM_SLEEP_TIME_MS.load(Ordering::SeqCst) as u64,
                ));
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
            eprintln!(
                "{}",
                red(&f("[scm] SetServiceStatus failed: {0}", &[&e.to_string()]))
            );
        }
    }
}

/// SCM 状态上报参数: 返回 (dwControlsAccepted, dwCheckPoint)。
/// PENDING/STOPPED 阶段不得接受停止/关机控制码，仅 RUNNING 接受；PENDING checkpoint 非零（P2-1）
pub(crate) fn scm_status_params(state: u32) -> (u32, u32) {
    use windows::Win32::System::Services::{
        SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP,
        SERVICE_START_PENDING, SERVICE_STOP_PENDING, SERVICE_STOPPED,
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

// ==================== 文件版本工具（供测试与刷新器使用） ====================

/// 读取文件版本（4 段）; 刷新器逐服务替换已移除，现仅供单元测试验证版本读取
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
        if GetFileVersionInfoW(
            PCWSTR::from_raw(path_wide.as_ptr()),
            Some(0),
            size,
            buf.as_mut_ptr() as *mut _,
        )
        .is_err()
        {
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
        )
        .as_bool()
        {
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
    let parse =
        |v: &str| -> Vec<u32> { v.split('.').filter_map(|s| s.parse::<u32>().ok()).collect() };
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
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_LOCAL_MACHINE, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
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
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            out_blob.pbData as *mut std::ffi::c_void,
        )));
        Some(format!("{}{}", DPAPI_ENC_PREFIX, b64))
    }
}

/// DPAPI 解密: 仅处理 enc:OSMIUM1: 前缀的值；明文/旧格式/解密失败原样返回（兼容 inplace 手写配置）。
/// 带前缀但解密失败（配置跨机迁移/密文损坏）时明确告警——密文被当明文使用是难排查的隐性故障
pub(crate) fn dpapi_decrypt(value: &str) -> String {
    let Some(rest) = value.strip_prefix(DPAPI_ENC_PREFIX) else {
        return value.to_string();
    };
    use base64::Engine as _;
    use windows::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };
    let warn = |why: &str| {
        eprintln!(
            "{}",
            red(&f(
                "Warning: DPAPI decrypt failed ({0}) — encrypted value is used as-is. The config was likely moved from another machine or the ciphertext is corrupted.",
                &[why]
            ))
        );
    };
    let Ok(cipher) = base64::engine::general_purpose::STANDARD.decode(rest) else {
        warn("invalid base64");
        return value.to_string();
    };
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: cipher.len() as u32,
        pbData: cipher.as_ptr() as *mut u8,
    };
    let mut out_blob = CRYPT_INTEGER_BLOB::default();
    unsafe {
        if CryptUnprotectData(
            &in_blob,
            None,
            None,
            None,
            None,
            CRYPTPROTECT_UI_FORBIDDEN,
            &mut out_blob,
        )
        .is_err()
        {
            warn("CryptUnprotectData failed");
            return value.to_string();
        }
        if out_blob.pbData.is_null() {
            return value.to_string();
        }
        let plain = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let s = String::from_utf8_lossy(plain).into_owned();
        let _ = windows::Win32::Foundation::LocalFree(Some(windows::Win32::Foundation::HLOCAL(
            out_blob.pbData as *mut std::ffi::c_void,
        )));
        s
    }
}

/// 解密配置中的敏感字段（service_password / download_password / smtp_password / 共享映射密码）
pub(crate) fn decrypt_sensitive(config: &mut ServiceConfig) {
    if let Some(p) = &mut config.service_password {
        *p = dpapi_decrypt(p);
    }
    if let Some(p) = &mut config.download_password {
        *p = dpapi_decrypt(p);
    }
    if let Some(p) = &mut config.smtp_password {
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
