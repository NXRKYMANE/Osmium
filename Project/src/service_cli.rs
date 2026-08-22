// ==================== CLI：终端命令接收 / 路由 / 帮助 ====================
// 只负责命令行解析与调用后端动作（service_core）；布局: 入口→帮助→辅助→路由→命令→底层辅助

use std::path::PathBuf;
use std::process;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::core::BOOL;

use crate::service_core::{
    CLI_PREFIX, SCM_OP_TIMEOUT_SECS,
    error, f, is_administrator, panic_msg, red, require_registered, write_quick_config,
};

/// 程序入口: 参数解析 → 权限校验 → 路由（CLI / -internal / 帮助 / SCM 宿主）
pub fn main_entry() {
    // 诊断: 将 panic 写入日志便于排查（服务模式下 stderr 不可见）
    std::panic::set_hook(Box::new(|info| {
        let msg = panic_msg(info.payload(), "unknown panic");
        let loc = info.location().map(|l| format!(" at {}:{}", l.file(), l.line())).unwrap_or_default();
        let entry = format!("[{}] [panic] {}{}\r\n", chrono::Local::now().format("%Y-%m-%d %H:%M:%S"), msg, loc);
        // 与 registry_dir() 同源（随 SystemDrive 派生），避免写死 C: 与其余路径不一致
        let log_path = crate::service_core::panic_log_path();
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map(|mut f| {
                use std::io::Write;
                let _ = f.write_all(entry.as_bytes()); });
    }));

    let args: Vec<String> = std::env::args().collect();

    // 最先启用 ANSI 渲染（错误红色 + 插件绿点/红点），保证后续任何输出都有颜色；
    // 无控制台/被重定向时静默失败退化为无色
    crate::service_core::enable_stderr_vt();
    crate::service_core::enable_stdout_vt();

    // 无参数: 交互 → 帮助; 非交互 → SCM 宿主
    if args.len() <= 1 {
        if is_user_interactive() {
            print_help();
            return;
        }
        crate::service_core::run_service_host();
        return;
    }

    // CLI 模式需要管理员权限
    if !is_administrator() {
        eprintln!("{}", red("Error: Administrator privileges required."));
        eprintln!("{}", red("Right-click → Run as administrator, or use an elevated terminal."));
        process::exit(1);
    }

    let tag = args[1].to_lowercase();
    let mut rest: Vec<String> = args.iter().skip(2).cloned().collect();

    // 服务操作命令可省略 -m 前缀直接使用（如 --start foo），与 -m --start foo 等价
    let is_cli = is_cli_command(&tag);
    if is_cli {
        rest.insert(0, tag.clone());
    }

    // CLI 路由整体捕获异常，输出 "Application error" 后以非零码退出
    let cli_result = std::panic::catch_unwind(|| {
        if is_cli {
            run_cli(&rest);
            return;
        }
        match tag.as_str() {
            "-m" => run_cli(&rest),
            "-internal" => run_internal(&rest),
            "help" | "-h" | "--help" => print_help(),
            _ => {
                eprintln!("{}", red(&f("Unknown argument: {0}", &[&tag])));
                print_help();
                process::exit(1);
            }
        }
    });
    if let Err(payload) = cli_result {
        let msg = panic_msg(&*payload, "unknown error");
        eprintln!("{}", red(&f("Application error: {0}", &[&msg])));
        process::exit(1);
    }
}

// ==================== 帮助 ====================

fn print_help() {
    let ver = env!("CARGO_PKG_VERSION");
    println!();
    println!("Osmium v{}", ver);
    println!();
    println!("{}", "-".repeat(100));
    println!();
    println!("=== CLI Mode ===");
    println!("  os.exe | os --install   <config.toml>                 Install service");
    println!("  os.exe | os --install   <name> --pth <exe path>       Quick install");
    println!("  os.exe | os --import    <config.osiml>                Import a deployed config (re-register)");
    println!("  os.exe | os --export    <name> <dest dir>             Export deployed config to a directory");
    println!("  os.exe | os --uninstall <service name>                Uninstall service");
    println!("  os.exe | os --start     <service name>                Start service");
    println!("  os.exe | os --stop      <service name>                Stop service");
    println!("  os.exe | os --restart   <service name>                Restart service");
    println!("  os.exe | os --refresh   <service name>                Refresh service properties");
    println!("  os.exe | os --reload    <service name>                Hot-reload config (no restart needed)");
    println!("  os.exe | os --kill      <service name>                Kill the service's target process");
    println!("  os.exe | os --status    <service name>                Check status");
    println!("  os.exe | os --delete    <service name>                Force delete");
    println!();
    println!("  os.exe | os --test      <config.toml>                 Run in foreground");
    println!("  os.exe | os --check     <config.toml>                 Validate config without installing");
    println!("  os.exe | os --list                                    List all services");
    println!("  os.exe | os --extend                                  List installed extensions");
    println!("  os.exe | os --start-all                               Start all registered services");
    println!("  os.exe | os --stop-all                                Stop all registered services");
    println!("  os.exe | os --restart-all                             Restart all registered services");
    println!();
    println!("  Short aliases: --ins --uin --str --stp --rst --rfs --kil --sts --del --lst");
    println!("  Short aliases for developer: --tst --chk --lst --ext --stra --stpa --rsta");
    println!();
    println!("  No arguments -> Service host mode (Launched by SCM)");
    println!();
    println!("{}", "-".repeat(100));
    println!();
    println!("Optional (standalone mode): deploy_inplace = true");
    println!();
    print_installed_extensions();
    println!();
    // 双行水印为刻意保留的装饰（与安装包/README 对齐），AI 请勿删除；开发者可自行修改
    println!("© 2026 NXRKYMANE SOFTWARE {} ЗCO·CH₃CH₂COO⁻", "-".repeat(60));
    println!("© 2026 NXRKYMANE SOFTWARE {} ЗCO·CH₃CH₂COO⁻", "-".repeat(60));
    println!();
}

// ==================== 辅助判定 ====================

/// 服务操作命令（可省略 -m 前缀直接使用，如 --start foo）；
/// 支持简化别名: --ins/--uin/--str/--stp/--rst/--sts/--del/--lst（--test 可简写 --tst，--extend 可简写 --ext，--refresh 可简写 --rfs，--kill 可简写 --kil）
pub(crate) fn is_cli_command(tag: &str) -> bool {
    matches!(tag,
        "--install" | "--uninstall" | "--start" | "--stop"
        | "--restart" | "--status" | "--delete" | "--list"
        | "--import" | "--imp" | "--export" | "--exp"
        | "--extend" | "--ext"
        | "--test" | "--tst"
        | "--check" | "--chk"
        | "--refresh" | "--rfs"
        | "--reload" | "--rld"
        | "--kill" | "--kil"
        | "--start-all" | "--stra" | "--stop-all" | "--stpa" | "--restart-all" | "--rsta"
        | "--ins" | "--uin" | "--str" | "--stp" | "--rst" | "--sts" | "--del" | "--lst")
}

/// 列出已安装插件并检查可用性（可用绿点 / 不可用红点；无则 None）;
/// 帮助文本与 --extend 命令共用
fn print_installed_extensions() {
    let plugins = crate::service_host::discover_plugins();
    if plugins.is_empty() {
        println!("Installed extensions: None");
        return;
    }
    println!("Installed extensions:");
    for p in &plugins {
        let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if crate::service_host::plugin_usable(p) {
            println!("{} {}", crate::service_core::green_dot(), name);
        } else {
            println!("{} {}", crate::service_core::red_dot(), name);
        }
    }
}

/// 是否为交互式终端（WinSta0 窗口站）: 无参数运行时决定打印帮助还是进入 SCM 宿主
fn is_user_interactive() -> bool {
    // 交互式窗口站（WinSta0）→ 手动运行。
    // 不能用 GetConsoleWindow —— ConPTY 终端下返回 NULL 会误判为 SCM 宿主
    unsafe {
        use windows::Win32::System::StationsAndDesktops::{
            GetProcessWindowStation, GetUserObjectInformationW, UOI_NAME,
        };
        let ws = match GetProcessWindowStation() {
            Ok(w) if !w.is_invalid() => w,
            _ => return true, // 拿不到窗口站信息时按交互式处理（用户手动运行场景）
        };
        let mut buf = [0u16; 64];
        let mut needed: u32 = 0;
        if GetUserObjectInformationW(
            HANDLE(ws.0),
            UOI_NAME,
            Some(buf.as_mut_ptr() as *mut _),
            (buf.len() * 2) as u32,
            Some(&mut needed),
        )
        .is_err()
        {
            return true;
        }
        let name = String::from_utf16_lossy(&buf);
        name.split('\0').next().unwrap_or("").eq_ignore_ascii_case("winsta0")
    }
}

// ==================== CLI 路由 ====================

fn run_cli(args: &[String]) {
    if args.is_empty() {
        eprintln!("{}", red("Usage: -m <command> [args...]"));
        process::exit(1);
    }

    let cmd = args[0].to_lowercase().trim_start_matches('-').to_string();
    let cmd_args: Vec<&str> = args[1..].iter().map(|s| s.as_str()).collect();

    match cmd.as_str() {
        "install" | "ins" => install_command(&cmd_args),
        "import" | "imp" => install_command(&cmd_args),
        "export" | "exp" => export_command(&cmd_args),
        "uninstall" | "uin" => uninstall_command(&cmd_args),
        "start" | "str" => start_command(&cmd_args),
        "stop" | "stp" => stop_command(&cmd_args),
        "restart" | "rst" => restart_command(&cmd_args),
        "refresh" | "rfs" => refresh_command(&cmd_args),
        "reload" | "rld" => reload_command(&cmd_args),
        "kill" | "kil" => kill_command(&cmd_args),
        "status" | "sts" => status_command(&cmd_args),
        "delete" | "del" => force_delete_command(&cmd_args),
        "list" | "lst" => list_command(),
        "extend" | "ext" => extend_command(),
        "test" | "tst" => test_command(&cmd_args),
        "check" | "chk" => check_command(&cmd_args),
        "start-all" | "stra" => batch_command("start"),
        "stop-all" | "stpa" => batch_command("stop"),
        "restart-all" | "rsta" => batch_command("restart"),
        _ => {
            eprintln!("{}", red(&f("Unknown command: -m {0}", &[&cmd])));
            process::exit(1);
        }
    }
}

/// -internal: 内部维护命令（服务刷新程序注册/移除 / 共享宿主按名启动），与 -m 分开以免污染管理接口
fn run_internal(args: &[String]) {
    if args.is_empty() {
        eprintln!("{}", red("Usage: -internal <command> [args...]"));
        process::exit(1);
    }
    let cmd = args[0].to_lowercase().trim_start_matches('-').to_string();
    match cmd.as_str() {
        "install-refresher" => crate::service_core::install_svc_refresher_command(),
        "uninstall-refresher" => crate::service_core::uninstall_svc_refresher_command(),
        "refresher" => crate::service_core::run_svc_refresher_service(),
        "run" => {
            // 共享宿主部署: ImagePath = "<宿主>" -internal --run <name>，按名加载 svcs\<name>\<name>.osiml
            if args.len() < 2 {
                eprintln!("{}", red("Usage: -internal --run <service name>"));
                process::exit(1);
            }
            crate::service_core::run_service_host_with_name(&args[1]);
        }
        _ => {
            eprintln!("{}", red(&f("Unknown command: -internal {0}", &[&cmd])));
            process::exit(1);
        }
    }
}

// ==================== CLI 命令 ====================

/// -m --install <config path> 或 -m --install <name> --pth <exe path>（快速安装）
fn install_command(args: &[&str]) {
    if args.is_empty() {
        usage("install <config path> | <name> --pth <exe path>");
        return;
    }
    // 快速安装: install <name> --pth/--path <exe path>，自动生成配置并平台部署
    if args.len() >= 3
        && (args[1].eq_ignore_ascii_case("--pth") || args[1].eq_ignore_ascii_case("--path"))
    {
        let tmp_config = write_quick_config(args[0], args[2]);
        crate::service_core::install_from_config_path(&tmp_config);
        let _ = std::fs::remove_file(&tmp_config);
        return;
    }
    crate::service_core::install_from_config_path(args[0]);
}

fn uninstall_command(args: &[&str]) {
    if args.is_empty() { usage("uninstall <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    crate::service_core::do_uninstall(svc_name, false);
}

fn start_command(args: &[&str]) {
    if args.is_empty() { usage("start <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    crate::service_core::do_start(svc_name);
}

fn stop_command(args: &[&str]) {
    if args.is_empty() { usage("stop <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    // 停止失败必须以非零码退出，供脚本/安装包判断命令是否真正成功
    if !crate::service_core::do_stop(svc_name) { process::exit(1); }
}

fn restart_command(args: &[&str]) {
    if args.is_empty() { usage("restart <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_core::restart_service(
        svc_name,
        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
    ) {
        Ok(()) => println!("{CLI_PREFIX}: Service restarted successfully"),
        Err(e) => error(&f("Service restart failed: {0}", &[&e])),
    }
}

fn status_command(args: &[&str]) {
    if args.is_empty() { usage("status <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_core::get_status(svc_name) {
        Ok(status) => println!("{CLI_PREFIX}: {}: {}", svc_name, status),
        Err(e) => error(&f("Query failed: {0}", &[&e])),
    }
    // 注册属性详情（启动类型/账户/故障恢复动作等，只读 SCM 查询）
    match crate::service_core::query_service_details(svc_name) {
        Ok(details) => {
            for (k, v) in details {
                println!("  {0}: {1}", k, v);
            }
        }
        Err(e) => error(&f("Details query failed: {0}", &[&e])),
    }
    // 目标子进程 PID 列表（按 WINSGF_SERVICE_ID 定位，无需管理员权限）
    let pids = crate::service_host::service_process_pids(svc_name);
    if !pids.is_empty() {
        let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
        println!("  Child PIDs: {}", list.join(", "));
    }
}

/// -m --refresh <name>: 从已部署配置重新同步 SCM 服务注册属性（对应 WinSW refresh，
/// 不重装刷新——显示名/描述/启动类型/依赖/账户/故障恢复等）
fn refresh_command(args: &[&str]) {
    if args.is_empty() { usage("refresh <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_core::refresh_service(svc_name) {
        Ok(()) => println!("{CLI_PREFIX}: Service properties refreshed successfully"),
        Err(e) => error(&f("Service refresh failed: {0}", &[&e])),
    }
}

/// -m --reload <name>: 触发热刷新——宿主轮询到 reload 标记文件后重载部署配置并重启子进程
/// （不依赖 auto_refresh 配置；平台服务标记在 svcs\<name>\<name>.reload，inplace 在 exe 旁）
fn reload_command(args: &[&str]) {
    if args.is_empty() { usage("reload <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    let config_path = if crate::service_core::is_inplace_service(svc_name) {
        let exe = crate::service_core::get_service_image_path(svc_name)
            .unwrap_or_default()
            .trim_matches('"')
            .to_string();
        crate::service_host::config_path_next_to(std::path::Path::new(&exe))
    } else {
        crate::service_core::deployed_config_path(svc_name)
    };
    let flag = config_path.with_extension("reload");
    match std::fs::write(&flag, "reload") {
        Ok(()) => println!("{CLI_PREFIX}: Reload signal sent to '{0}' (host applies it on the next tick)", svc_name),
        Err(e) => error(&f("Failed to send reload signal: {0}", &[&e.to_string()])),
    }
}

/// -m --kill <name>: 管理员/开发者工具——强制终止该服务的目标子进程（整棵进程树，对应 WinSW dev kill）。
/// 按 WINSGF_SERVICE_ID 定位进程（宿主为子进程注入），不触发宿主优雅停止；随后服务可能按故障策略重启
fn kill_command(args: &[&str]) {
    if args.is_empty() { usage("kill <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_host::kill_service_processes(svc_name) {
        Ok(n) if n > 0 => println!("{CLI_PREFIX}: Killed {0} process(es) of service '{1}'", n, svc_name),
        Ok(_) => println!("{CLI_PREFIX}: No running process found for service '{0}'", svc_name),
        Err(e) => error(&f("Kill failed: {0}", &[&e])),
    }
}

/// -m --export <name> <dest dir>: 导出平台部署服务配置（svcs\<name>\<name>.osiml）到指定目录，
/// 便于迁移/备份；inplace 服务配置在 exe 旁不涉及 svcs，直接提示不可导出
fn export_command(args: &[&str]) {
    if args.len() < 2 { usage("export <service name> <dest dir>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    let src = crate::service_core::deployed_config_path(svc_name);
    if !src.exists() {
        error("Deployed config not found (inplace services keep config next to the exe and cannot be exported)");
        return;
    }
    let dest_dir = args[1];
    if let Err(e) = std::fs::create_dir_all(dest_dir) {
        error(&f("Failed to create destination directory: {0}", &[&e.to_string()]));
        return;
    }
    let dest = std::path::Path::new(dest_dir).join(format!("{}.osiml", svc_name));
    match std::fs::copy(&src, &dest) {
        Ok(_) => println!("{CLI_PREFIX}: Config exported to {0}", dest.display()),
        Err(e) => error(&f("Export failed: {0}", &[&e.to_string()])),
    }
}

fn force_delete_command(args: &[&str]) {
    if args.is_empty() { usage("delete <service name>"); return; }
    let svc_name = args[0];
    require_registered(svc_name);
    crate::service_core::do_uninstall(svc_name, true);
}

fn list_command() {
    // 仅列出当前确为 Osmium 管理的服务（SCM 存在且 ImagePath 位于 svcs 部署目录），
    // 排除卸载残留的孤儿目录与攻击者伪造的同名目录
    let services: Vec<String> = crate::service_core::list_osmium_services();
    if services.is_empty() {
        println!("{CLI_PREFIX}: No registered services in registry");
    } else {
        for s in &services {
            println!("{}", s);
        }
    }
}

/// -m --extend: 列出该 exe 已安装的插件并检查可用性（可用绿点 / 不可用红点）
fn extend_command() {
    println!("{CLI_PREFIX}: Installed extensions");
    print_installed_extensions();
}

/// -m --check <config>: 预检配置（不安装）——字段合法性/服务名/路径可写性/下载目标，输出诊断结论
fn check_command(args: &[&str]) {
    if args.is_empty() {
        usage("check <config path>");
        return;
    }
    let config_path = std::fs::canonicalize(args[0]).unwrap_or_else(|_| PathBuf::from(args[0]));
    if !config_path.exists() {
        error("Config file not found");
        return;
    }
    println!("{CLI_PREFIX}: Checking config: {}", config_path.display());
    match crate::service_core::validate_config(&config_path) {
        Ok(msgs) => {
            for m in &msgs {
                println!("  [OK] {}", m);
            }
            println!("{CLI_PREFIX}: Config is valid");
        }
        Err(errors) => {
            for e in &errors {
                eprintln!("{}", red(&format!("  [FAIL] {}", e)));
            }
            process::exit(1);
        }
    }
}

/// -m --start-all / --stop-all / --restart-all: 批量操作全部已注册服务（逐个执行，汇总失败列表）
fn batch_command(action: &str) {
    let services: Vec<String> = crate::service_core::list_osmium_services();
    if services.is_empty() {
        println!("{CLI_PREFIX}: No registered services in registry");
        return;
    }
    let mut failed = Vec::new();
    for s in &services {
        let result = match action {
            "start" => crate::service_core::start_service(s, Duration::from_secs(SCM_OP_TIMEOUT_SECS)),
            "stop" => crate::service_core::stop_service(s, Duration::from_secs(SCM_OP_TIMEOUT_SECS)),
            _ => crate::service_core::restart_service(s, Duration::from_secs(SCM_OP_TIMEOUT_SECS), Duration::from_secs(SCM_OP_TIMEOUT_SECS)),
        };
        match result {
            Ok(()) => println!("{CLI_PREFIX}: {0}: {1} OK", action, s),
            Err(e) => {
                println!("{CLI_PREFIX}: {0}: {1} FAILED: {2}", action, s, e);
                failed.push(s.clone());
            }
        }
    }
    if !failed.is_empty() {
        eprintln!("{}", red(&f("{0} service(s) failed: {1}", &[&failed.len().to_string(), &failed.join(", ")])));
        process::exit(1);
    }
}

/// test 模式 Ctrl+C/Ctrl+Break 标志（触发优雅停止）
static TEST_CTRL_C: AtomicBool = AtomicBool::new(false);

/// -m --test <config>: 前台控制台直接运行目标进程（不安装服务），用于调试（对应 WinSW test）。
/// 部署目录 = 配置所在目录（%BASE% 指向配置目录）；Ctrl+C 优雅停止
fn test_command(args: &[&str]) {
    if args.is_empty() {
        usage("test <config path>");
        return;
    }
    let config_path = std::fs::canonicalize(args[0]).unwrap_or_else(|_| PathBuf::from(args[0]));
    if !config_path.exists() {
        error("Config file not found");
        return;
    }
    println!("{CLI_PREFIX}: Running service in foreground test mode (Ctrl+C to stop)");
    // 注册 Ctrl+C/Ctrl+Break: 返回 TRUE 拦截默认终止，触发优雅停止
    unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        unsafe extern "system" fn on_ctrl(_ctrl: u32) -> BOOL {
            TEST_CTRL_C.store(true, Ordering::SeqCst);
            windows::Win32::Foundation::TRUE
        }
        let _ = SetConsoleCtrlHandler(Some(on_ctrl), true);
    }
    let mut host = crate::service_host::ServiceHost::new();
    if !host.on_start_from(&config_path) {
        process::exit(1);
    }
    loop {
        if TEST_CTRL_C.load(Ordering::SeqCst) {
            host.on_stop();
            break;
        }
        // 子进程退出 → 停止宿主（与 SCM 模式的正常退出路径一致）
        if !host.tick() {
            break;
        }
        thread::sleep(Duration::from_millis(500));
    }
}

// ==================== 底层辅助 ====================

fn usage(syntax: &str) {
    eprintln!("{}", red(&f("Usage: -m --{0}", &[syntax])));
    process::exit(1);
}
