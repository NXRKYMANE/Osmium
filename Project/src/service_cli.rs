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
    CLI_PREFIX, SCM_OP_TIMEOUT_SECS, error, f, is_administrator, panic_msg, red,
    require_registered, write_quick_config,
};

/// 程序入口: 参数解析 → 权限校验 → 路由（CLI / -internal / 帮助 / SCM 宿主）
pub fn main_entry() {
    // 诊断: 将 panic 写入日志便于排查（服务模式下 stderr 不可见）
    std::panic::set_hook(Box::new(|info| {
        let msg = panic_msg(info.payload(), "unknown panic");
        let loc = info
            .location()
            .map(|l| format!(" at {}:{}", l.file(), l.line()))
            .unwrap_or_default();
        let entry = format!(
            "[{}] [panic] {}{}\r\n",
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
            msg,
            loc
        );
        // 与 registry_dir() 同源（随 SystemDrive 派生），避免写死 C: 与其余路径不一致
        let log_path = crate::service_core::panic_log_path();
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .map(|mut f| {
                use std::io::Write;
                let _ = f.write_all(entry.as_bytes());
            });
    }));

    // args_os + lossy: env::args() 对含非法 UTF-16 的参数会直接 panic（位于 catch_unwind 之前，
    // 用户只能看到裸崩溃而非可读错误——其他工具创建的怪名配置可触发）
    let args: Vec<String> = std::env::args_os()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();

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

    let tag = args[1].to_lowercase();
    let mut rest: Vec<String> = args.iter().skip(2).cloned().collect();

    // 服务操作命令可省略 -m 前缀直接使用（如 --start foo），与 -m --start foo 等价。
    // 只读/本地命令免管理员（帮助/查询/插件列表/预检/前台调试/签名）；其余维持强制提权（SCM 写操作）
    let effective_cmd = if tag == "-m" {
        rest.first().map(|s| s.to_lowercase()).unwrap_or_default()
    } else {
        tag.clone()
    };
    // 权限门置于路由前但须先排除"未知命令": 非管理员执行 `os -m badcmd` 应得到
    // Unknown argument（拼写/用法错误）而非误导性的"Administrator privileges required"——
    // 未知命令对任何用户都是无效输入，与权限无关
    let known_write = !is_readonly_command(&effective_cmd)
        && matches!(
            effective_cmd.trim_start_matches('-'),
            "install"
                | "ins"
                | "uninstall"
                | "uin"
                | "start"
                | "str"
                | "stop"
                | "stp"
                | "restart"
                | "rst"
                | "delete"
                | "del"
                | "import"
                | "imp"
                | "export"
                | "exp"
                | "refresh"
                | "rfs"
                | "reload"
                | "rld"
                | "kill"
                | "kil"
                | "start-all"
                | "stra"
                | "stop-all"
                | "stpa"
                | "restart-all"
                | "rsta"
        );
    if known_write && !is_administrator() {
        eprintln!("{}", red("Error: Administrator privileges required."));
        eprintln!(
            "{}",
            red("Right-click → Run as administrator, or use an elevated terminal.")
        );
        process::exit(1);
    }

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
    // 位数标识: 同一源码构建 64/32 位版本，帮助头用于识别当前二进制架构
    let bits = if cfg!(target_pointer_width = "64") {
        "64"
    } else {
        "32"
    };
    println!();
    println!("Osmium {} v{}", bits, ver);
    println!();
    println!("{}", "-".repeat(100));
    println!();
    println!("=== SMP Mode ===");
    println!("  os.exe | os --install      <config.toml>               Install service");
    println!("  os.exe | os --install      <name> --pth <exe path>     Quick install");
    println!("  os.exe | os --import       <config.osiml>              Import a deployed config");
    println!("  os.exe | os --export       <name> <dest dir>           Export deployed config");
    println!("  os.exe | os --start        <service name>              Start service");
    println!("  os.exe | os --stop         <service name>              Stop service");
    println!("  os.exe | os --restart      <service name>              Restart service");
    println!("  os.exe | os --status       <service name>              Check status");
    println!("  os.exe | os --kill         <service name>              Kill the target process");
    println!("  os.exe | os --refresh      <service name>              Refresh service properties");
    println!("  os.exe | os --reload       <service name>              Hot-reload config");
    println!("  os.exe | os --uninstall    <service name>              Uninstall service");
    println!("  os.exe | os --delete       <service name>              Force delete");
    println!("=== DEV Mode ===");
    println!("  os.exe | os --test         <config.toml>               Run in foreground");
    println!("  os.exe | os --check        <config.toml | svc name>    Validate config");
    println!("  os.exe | os --sign-config  <config.toml>               Sign config");
    println!("=== BATCH Mode ===");
    println!("  os.exe | os --list                                     List all services");
    println!("  os.exe | os --extend                                   List installed extensions");
    println!("  os.exe | os --start-all                                Start all services");
    println!("  os.exe | os --stop-all                                 Stop all services");
    println!("  os.exe | os --restart-all                              Restart all services");
    println!("  os.exe | os --status-all                               Status of all services");
    println!();
    println!(
        "Short aliases: --ins --imp --exp --str --stp --rst --sts --kil --rfs --rld --uin --del --lst"
    );
    println!("Short aliases for developer: --tst --chk --sigc --ext --stra --stpa --rsta --stsa");
    println!();
    println!("No arguments -> Service host mode (launched by SCM)");
    println!();
    println!("{}", "-".repeat(100));
    println!();
    // 双行水印为刻意保留的装饰（与安装包/README 对齐），AI 请勿删除；开发者可自行修改
    println!(
        "© 2026 NXRKYMANE SOFTWARE {} ЗCO·CH₃CH₂COO⁻",
        "-".repeat(60)
    );
    println!(
        "© 2026 NXRKYMANE SOFTWARE {} ЗCO·CH₃CH₂COO⁻",
        "-".repeat(60)
    );
    println!();
}

// ==================== 辅助判定 ====================

/// 只读/本地命令集合（免管理员）: 帮助、查询类（list/status/status-all）、插件列表、
/// 配置预检、前台调试、配置签名——均不做 SCM 写操作。
/// 前导 '-' 归一化: `-m sts`（裸别名）与 `-m --sts`、`--sts` 三种写法判定一致，
/// 避免同一只读命令因写法不同被误要求提权
pub(crate) fn is_readonly_command(tag: &str) -> bool {
    let normalized = tag.trim_start_matches('-');
    matches!(
        normalized,
        "help"
            | "h"
            | "list"
            | "lst"
            | "status"
            | "sts"
            | "status-all"
            | "stsa"
            | "extend"
            | "ext"
            | "check"
            | "chk"
            | "test"
            | "tst"
            | "sign-config"
            | "sigc"
    )
}

/// 服务操作命令（可省略 -m 前缀直接使用，如 --start foo）；
/// 支持简化别名: --ins/--uin/--str/--stp/--rst/--sts/--del/--lst（--test 可简写 --tst，--extend 可简写 --ext，--refresh 可简写 --rfs，--kill 可简写 --kil）
pub(crate) fn is_cli_command(tag: &str) -> bool {
    matches!(
        tag,
        "--install"
            | "--uninstall"
            | "--start"
            | "--stop"
            | "--restart"
            | "--status"
            | "--delete"
            | "--list"
            | "--import"
            | "--imp"
            | "--export"
            | "--exp"
            | "--extend"
            | "--ext"
            | "--test"
            | "--tst"
            | "--check"
            | "--chk"
            | "--sign-config"
            | "--sigc"
            | "--refresh"
            | "--rfs"
            | "--reload"
            | "--rld"
            | "--kill"
            | "--kil"
            | "--start-all"
            | "--stra"
            | "--stop-all"
            | "--stpa"
            | "--restart-all"
            | "--rsta"
            | "--status-all"
            | "--stsa"
            | "--ins"
            | "--uin"
            | "--str"
            | "--stp"
            | "--rst"
            | "--sts"
            | "--del"
            | "--lst"
    )
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
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        // 插件位数标识: 64/32 或 unknown（非 PE 文件/读取失败），便于区分官方 64/32 位插件
        let arch = crate::service_host::pe_arch(p)
            .map(|a| format!(" [{a}]"))
            .unwrap_or_else(|| " [unknown]".into());
        if crate::service_host::plugin_usable(p) {
            println!("{} {}{}", crate::service_core::green_dot(), name, arch);
        } else {
            println!("{} {}{}", crate::service_core::red_dot(), name, arch);
        }
    }
}

/// 是否为交互式终端（WinSta0 窗口站）: 无参数运行时决定打印帮助还是进入 SCM 宿主
fn is_user_interactive() -> bool {
    // 会话 0 内运行的是 SCM 上下文（服务宿主），即使窗口站名为 WinSta0 也是
    // SERVICE_INTERACTIVE_PROCESS 的交互式服务（session 0 也有 WinSta0）——
    // 判定为交互会把 interactive=true 的服务误判成手动运行，启动即打印帮助退出
    unsafe {
        use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
        use windows::Win32::System::Threading::GetCurrentProcessId;
        let mut session = 0u32;
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_ok() && session == 0 {
            return false;
        }
        // 交互式窗口站（WinSta0）→ 手动运行。
        // 不能用 GetConsoleWindow —— ConPTY 终端下返回 NULL 会误判为 SCM 宿主
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
        name.split('\0')
            .next()
            .unwrap_or("")
            .eq_ignore_ascii_case("winsta0")
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
        "sign-config" | "sigc" => sign_config_command(&cmd_args),
        "start-all" | "stra" => batch_command("start"),
        "stop-all" | "stpa" => batch_command("stop"),
        "restart-all" | "rsta" => batch_command("restart"),
        "status-all" | "stsa" => status_all_command(),
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

/// -m --install `<config path>` 或 -m --install `<name>` --pth `<exe path>`（快速安装）
fn install_command(args: &[&str]) {
    if args.is_empty() {
        usage("install <config path> | <name> --pth <exe path>");
    }
    // 快速安装: install <name> --pth/--path <exe path>，自动生成配置并平台部署
    //（--pth 显式出现但缺 exe 路径时单独报用法错误——旧实现静默落入普通安装分支
    // 把名字当配置文件报 "not found"，误导排障）
    // 长度守卫必须在前: 普通安装仅 1 个参数，无守卫时 args[1] 直接越界 panic
    //（catch_unwind 兜住变 "Application error: index out of bounds"，安装不可用）
    if args.len() >= 2
        && (args[1].eq_ignore_ascii_case("--pth") || args[1].eq_ignore_ascii_case("--path"))
        && args.len() < 3
    {
        usage("install <name> --pth <exe path>");
    }
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
    if args.is_empty() {
        usage("uninstall <service name>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    crate::service_core::do_uninstall(svc_name, false);
}

fn start_command(args: &[&str]) {
    if args.is_empty() {
        usage("start <service name>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    crate::service_core::do_start(svc_name);
}

fn stop_command(args: &[&str]) {
    if args.is_empty() {
        usage("stop <service name>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    // 停止失败必须以非零码退出，供脚本/安装包判断命令是否真正成功
    if !crate::service_core::do_stop(svc_name) {
        process::exit(1);
    }
}

fn restart_command(args: &[&str]) {
    if args.is_empty() {
        usage("restart <service name>");
    }
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
    if args.is_empty() {
        usage("status <service name>");
    }
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
    // Job Object 状态（宿主写入 <配置名>.job: "ok" 或 "failed:<计数>"；
    // KILL_ON_JOB_CLOSE 兜底不可用时子进程可能在宿主崩溃后残留）
    let job_file = crate::service_core::job_state_path(svc_name);
    if job_file.exists()
        && let Ok(state) = std::fs::read_to_string(&job_file)
    {
        let state = state.trim();
        let display = if state == "ok" {
            "ok".to_string()
        } else if let Some(n) = state.strip_prefix("failed:") {
            format!(
                "FAILED ({} assign failure(s) — KILL_ON_JOB_CLOSE fallback inactive)",
                n
            )
        } else {
            state.to_string()
        };
        println!("  Job Object: {}", display);
    }
    // 指标摘要（metrics_file 配置时显示最后一条导出记录）
    if let Some(last) = crate::service_core::last_metrics_line(svc_name) {
        println!("  Metrics (last): {}", last);
    }
}

/// -m --refresh `<name>`: 从已部署配置重新同步 SCM 服务注册属性（对应 WinSW refresh，
/// 不重装刷新——显示名/描述/启动类型/依赖/账户/故障恢复等）
fn refresh_command(args: &[&str]) {
    if args.is_empty() {
        usage("refresh <service name>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_core::refresh_service(svc_name) {
        Ok(()) => println!("{CLI_PREFIX}: Service properties refreshed successfully"),
        Err(e) => error(&f("Service refresh failed: {0}", &[&e])),
    }
}

/// -m --reload `<name>`: 触发热刷新——宿主轮询到 reload 标记文件后重载部署配置并重启子进程
/// （不依赖 auto_refresh 配置；平台服务标记在 svcs`<name>``<name>`.reload，inplace 在 exe 旁）
fn reload_command(args: &[&str]) {
    if args.is_empty() {
        usage("reload <service name>");
    }
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
        Ok(()) => println!(
            "{CLI_PREFIX}: Reload signal sent to '{0}' (host applies it on the next tick)",
            svc_name
        ),
        Err(e) => error(&f("Failed to send reload signal: {0}", &[&e.to_string()])),
    }
}

/// -m --kill `<name>`: 管理员/开发者工具——强制终止该服务的目标子进程（整棵进程树，对应 WinSW dev kill）。
/// 按 WINSGF_SERVICE_ID 定位进程（宿主为子进程注入），不触发宿主优雅停止；随后服务可能按故障策略重启
fn kill_command(args: &[&str]) {
    if args.is_empty() {
        usage("kill <service name>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    match crate::service_host::kill_service_processes(svc_name) {
        Ok(n) if n > 0 => println!(
            "{CLI_PREFIX}: Killed {0} process(es) of service '{1}'",
            n, svc_name
        ),
        Ok(_) => println!(
            "{CLI_PREFIX}: No running process found for service '{0}'",
            svc_name
        ),
        Err(e) => error(&f("Kill failed: {0}", &[&e])),
    }
}

/// -m --export `<name>` `<dest dir>`: 导出平台部署服务配置（svcs`<name>``<name>`.osiml）到指定目录，
/// 便于迁移/备份；inplace 服务配置在 exe 旁不涉及 svcs，直接提示不可导出
fn export_command(args: &[&str]) {
    if args.len() < 2 {
        usage("export <service name> <dest dir>");
    }
    let svc_name = args[0];
    require_registered(svc_name);
    let src = crate::service_core::deployed_config_path(svc_name);
    if !src.exists() {
        error(
            "Deployed config not found (inplace services keep config next to the exe and cannot be exported)",
        );
        return;
    }
    let dest_dir = args[1];
    if let Err(e) = std::fs::create_dir_all(dest_dir) {
        error(&f(
            "Failed to create destination directory: {0}",
            &[&e.to_string()],
        ));
        return;
    }
    let dest = std::path::Path::new(dest_dir).join(format!("{}.osiml", svc_name));
    match std::fs::copy(&src, &dest) {
        Ok(_) => {
            println!("{CLI_PREFIX}: Config exported to {0}", dest.display());
            // 导出副本含 DPAPI 机器级密文（enc:OSMIUM1:），本机任意账户可解密——
            // 目标目录可写位置（共享/公共目录）等于把密码敞开，明确告警
            let content = std::fs::read_to_string(&src).unwrap_or_default();
            if content.contains("enc:OSMIUM1:") {
                eprintln!(
                    "{}",
                    red(
                        "Warning: the exported config contains machine-scoped DPAPI secrets (enc:OSMIUM1:). Any local account can decrypt them — keep the destination directory restricted to administrators."
                    )
                );
            }
        }
        Err(e) => error(&f("Export failed: {0}", &[&e.to_string()])),
    }
}

fn force_delete_command(args: &[&str]) {
    if args.is_empty() {
        usage("delete <service name>");
    }
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
            let state = crate::service_core::get_status(s).unwrap_or_else(|_| "Unknown".into());
            println!("{0}  [{1}]", s, state);
        }
    }
}

/// -m --extend: 列出该 exe 已安装的插件并检查可用性（可用绿点 / 不可用红点）
fn extend_command() {
    println!("{CLI_PREFIX}: Installed extensions");
    print_installed_extensions();
}

/// -m --check `<config 或服务名>`: 预检配置（不安装）——字段合法性/服务名/路径可写性/下载目标；
/// 参数为已注册服务名时读取其部署配置做同样检查（已部署配置体检，便于定位宿主启动失败原因）
fn check_command(args: &[&str]) {
    if args.is_empty() {
        usage("check <config path | service name>");
    }
    let arg = args[0];
    // 已注册服务名 → 用部署配置路径（平台 svcs\<name>\<name>.osiml；inplace exe 旁同名 toml）
    let deployed = if crate::service_core::is_valid_service_name(arg)
        && crate::service_core::is_registered_probe(arg)
    {
        if crate::service_core::is_inplace_service(arg) {
            crate::service_core::get_service_image_path(arg)
                .map(|p| {
                    crate::service_host::config_path_next_to(std::path::Path::new(
                        p.trim_matches('"'),
                    ))
                })
                .unwrap_or_else(|| crate::service_core::deployed_config_path(arg))
        } else {
            crate::service_core::deployed_config_path(arg)
        }
    } else {
        PathBuf::from(arg)
    };
    let config_path = std::fs::canonicalize(&deployed).unwrap_or_else(|_| deployed.clone());
    if !config_path.exists() {
        error(&f(
            "Config file not found: '{0}'",
            &[&deployed.display().to_string()],
        ));
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

/// -m --sign-config `<config>`: 用 exe 旁 osmium-sign.key（PKCS#8 PEM 私钥）对配置做
/// RSA-SHA256 签名，写入 `<config>`.sig（inplace 部署前手动签名用；平台安装有私钥时自动签名）
fn sign_config_command(args: &[&str]) {
    if args.is_empty() {
        usage("sign-config <config path>");
    }
    let config_path = std::fs::canonicalize(args[0]).unwrap_or_else(|_| PathBuf::from(args[0]));
    if !config_path.exists() {
        error("Config file not found");
        return;
    }
    if crate::service_core::sign_config_file(&config_path) {
        println!("{CLI_PREFIX}: Config signed: {}.sig", config_path.display());
    } else {
        error(
            "Signing failed: osmium-sign.key (PKCS#8 PEM private key) must exist next to the executable",
        );
    }
}

/// -m --status-all: 批量状态——遍历全部已注册服务，输出状态/注册属性/PIDs/指标摘要
fn status_all_command() {
    let services: Vec<String> = crate::service_core::list_osmium_services();
    if services.is_empty() {
        println!("{CLI_PREFIX}: No registered services in registry");
        return;
    }
    // 批量定位全部服务的子进程 PID: 单次全进程枚举（逐服务调用会对全量进程重复扫描 N 次）
    let name_refs: Vec<&str> = services.iter().map(|s| s.as_str()).collect();
    let pid_map = crate::service_host::service_process_pids_batch(&name_refs);
    for s in &services {
        let state = crate::service_core::get_status(s).unwrap_or_else(|_| "Unknown".into());
        println!("{0}: {1}", s, state);
        if let Ok(details) = crate::service_core::query_service_details(s) {
            for (k, v) in details {
                println!("  {0}: {1}", k, v);
            }
        }
        if let Some(pids) = pid_map.get(s)
            && !pids.is_empty()
        {
            let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
            println!("  Child PIDs: {}", list.join(", "));
        }
        if let Some(last) = crate::service_core::last_metrics_line(s) {
            println!("  Metrics: {}", last);
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
    // 并行下发（每服务独立线程）: 串行执行时 N 个僵死服务会叠加 N×SCM_OP_TIMEOUT_SECS
    // 超时（stop-all 等批量操作被单个僵死服务拖死整批）; SCM 操作彼此独立，并发安全
    let results: Vec<(String, Result<(), String>)> = services
        .iter()
        .cloned()
        .map(|s| {
            let action = action.to_string();
            thread::spawn(move || {
                let result = match action.as_str() {
                    "start" => crate::service_core::start_service(
                        &s,
                        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
                    ),
                    "stop" => crate::service_core::stop_service(
                        &s,
                        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
                    ),
                    _ => crate::service_core::restart_service(
                        &s,
                        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
                        Duration::from_secs(SCM_OP_TIMEOUT_SECS),
                    ),
                };
                (s, result)
            })
        })
        .collect::<Vec<_>>()
        .into_iter()
        .map(|h| {
            h.join()
                .unwrap_or_else(|_| (String::new(), Err("thread panic".into())))
        })
        .collect();
    let mut failed = Vec::new();
    for (s, result) in &results {
        match result {
            Ok(()) => println!("{CLI_PREFIX}: {0}: {1} OK", action, s),
            Err(e) => {
                println!("{CLI_PREFIX}: {0}: {1} FAILED: {2}", action, s, e);
                failed.push(s.clone());
            }
        }
    }
    if !failed.is_empty() {
        eprintln!(
            "{}",
            red(&f(
                "{0} service(s) failed: {1}",
                &[&failed.len().to_string(), &failed.join(", ")]
            ))
        );
        process::exit(1);
    }
}

/// test 模式 Ctrl+C/Ctrl+Break 标志（触发优雅停止）
static TEST_CTRL_C: AtomicBool = AtomicBool::new(false);

/// -m --test `<config>`: 前台控制台直接运行目标进程（不安装服务），用于调试（对应 WinSW test）。
/// 部署目录 = 配置所在目录（%BASE% 指向配置目录）；Ctrl+C 优雅停止
fn test_command(args: &[&str]) {
    if args.is_empty() {
        usage("test <config path>");
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
    // 注入 Ctrl+C 探测: 恢复延迟分段等待期间（故障恢复 delay 最长 60s）也能立即响应
    host.set_stop_probe(|| TEST_CTRL_C.load(Ordering::SeqCst));
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

/// 输出用法错误并退出进程（发散函数: 调用点其后无需 return，编译器保证不可达）
fn usage(syntax: &str) -> ! {
    eprintln!("{}", red(&f("Usage: -m --{0}", &[syntax])));
    process::exit(1)
}
