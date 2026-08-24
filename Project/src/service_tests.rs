// ==================== 单元测试（独立模块，从 service_core.rs / service_host.rs 提取） ====================

use std::io::{Read, Write};
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use chrono::TimeZone;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Services::{
    SERVICE_AUTO_START, SERVICE_DEMAND_START, SERVICE_DISABLED,
};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_TERMINATE, TerminateProcess};

use crate::service_config::{DownloadConfig, ServiceConfig, SharedMapperConfig};
use crate::service_core::{
    DownloadAuth, build_dependency_string, can_overwrite_source, compare_versions,
    decrypt_sensitive, delete_dir_tree, delete_old_logs, deployed_config_path, download_core,
    dpapi_decrypt, dpapi_encrypt, get_file_version, get_own_path, green_dot, has_download,
    is_refresher_reserved_name, is_user_writable, is_valid_service_name, load_config,
    parse_start_mode, red, red_dot, resolve_redirect_url, safe_delete_dir, scm_sleep_time_ms,
    scm_status_params, scm_wait_hint_ms, sddl_dacl_grants_non_admin_write,
    sddl_owner_is_administrative, secure_directory, security_descriptor_from_sddl,
    set_preshutdown_enabled, set_scm_sleep_time_ms, set_scm_wait_hint_ms, sha256_matches,
    strip_verbatim_prefix, validate_config, write_deployed_config, write_quick_config,
};
use crate::service_host::{
    LogOptions, apply_log_mode, auto_roll_logs, build_child_command, collect_descendants,
    current_log_name, download_auth_from_entry, download_entries, download_entry_stage,
    download_stage_is, escape_invisible, expand_env_value, expand_stop_pid, ext_phase_matches,
    failure_action_chain, http_date_from_mtime, log_pattern_safe, process_alive, process_cpu_100ns,
    process_env_var, process_working_set_mb, redact_url, reset_auto_roll_state, reset_current_logs,
    resolve_download_target, roll_by_time_if_due, roll_if_needed, roll_logs_to_old, run_hook,
    run_stop_command, runaway_cleanup_pid_file, runaway_exceeded, set_process_priority,
    warn_if_insecure_download, write_log_entry, write_metrics_file, zip_backup_file,
};

/// 本地 HTTP 测试服务器: handler 接收 (方法, 请求行列表)，返回 (状态行, 头部, 响应体)；
/// 返回 (地址, 停止标志, 已处理请求计数)
fn spawn_http_server<F>(handler: F) -> (std::net::SocketAddr, Arc<AtomicBool>, Arc<AtomicUsize>)
where
    F: Fn(&str, &[String]) -> (String, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
{
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let count = Arc::new(AtomicUsize::new(0));
    let (s1, c1) = (stop.clone(), count.clone());
    thread::spawn(move || {
        while !s1.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    c1.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.set_nonblocking(false);
                    // 读取请求头直到空行
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 2048];
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    let lines: Vec<String> = String::from_utf8_lossy(&buf)
                        .lines()
                        .map(|s| s.to_string())
                        .collect();
                    let method = lines
                        .first()
                        .and_then(|l| l.split_whitespace().next())
                        .unwrap_or("")
                        .to_string();
                    let (status, headers, body) = handler(&method, &lines);
                    let mut head = format!("HTTP/1.1 {}\r\n", status);
                    for (k, v) in headers {
                        head.push_str(&format!("{k}: {v}\r\n"));
                    }
                    head.push_str("\r\n");
                    if stream.write_all(head.as_bytes()).is_err() {
                        continue;
                    }
                    let _ = stream.write_all(&body);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
    });
    (addr, stop, count)
}

// ==================== CLI 别名（--test 可简写 --tst） ====================

#[test]
fn cli_short_aliases_cover_test() {
    // 服务操作命令与全部简化别名（含 --test/--tst、--extend/--ext、--refresh/--rfs、--kill/--kil、
    // 批量命令 --start-all/--stra、--stop-all/--stpa、--restart-all/--rsta）均可省略 -m 直接使用
    for tag in [
        "--install",
        "--uninstall",
        "--start",
        "--stop",
        "--restart",
        "--status",
        "--delete",
        "--list",
        "--test",
        "--tst",
        "--extend",
        "--ext",
        "--refresh",
        "--rfs",
        "--kill",
        "--kil",
        "--import",
        "--imp",
        "--export",
        "--exp",
        "--reload",
        "--rld",
        "--check",
        "--chk",
        "--sign-config",
        "--sigc",
        "--start-all",
        "--stra",
        "--stop-all",
        "--stpa",
        "--restart-all",
        "--rsta",
        "--status-all",
        "--stsa",
        "--ins",
        "--uin",
        "--str",
        "--stp",
        "--rst",
        "--sts",
        "--del",
        "--lst",
    ] {
        assert!(
            crate::service_cli::is_cli_command(tag),
            "{tag} should be recognized as a CLI command"
        );
    }
    // 非命令参数不应误判为 CLI 命令
    assert!(!crate::service_cli::is_cli_command("-m"));
    assert!(!crate::service_cli::is_cli_command("--help"));
    assert!(!crate::service_cli::is_cli_command("--refresher"));
    assert!(!crate::service_cli::is_cli_command("my-service"));
}

// ==================== 共享宿主 ImagePath 解析（-internal --run） ====================

#[test]
fn parse_run_service_name_extracts_from_image_path() {
    // 新格式: 引号包裹的宿主路径 + -internal --run <name>
    assert_eq!(
        crate::service_core::parse_run_service_name(
            r#""C:\Program Files\Osmium\os.exe" -internal --run my-service"#
        ),
        Some("my-service".to_string())
    );
    // 服务名含空格（install 时引号包裹，解析时还原）
    assert_eq!(
        crate::service_core::parse_run_service_name(
            r#""C:\Osmium\os.exe" -internal --run "my service""#
        ),
        Some("my service".to_string())
    );
    // --run 大小写不敏感
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\x.exe" -internal --RUN foo"#),
        Some("foo".to_string())
    );
    // 宿主安装路径自身含 "--run" 子串时（如 C:\app--run\os.exe），必须按 -internal 后第一个 --run 切分
    assert_eq!(
        crate::service_core::parse_run_service_name(
            r#""C:\app--run\os.exe" -internal --run my-service"#
        ),
        Some("my-service".to_string())
    );
    // 服务名含 --run 子串（合法字符）同样不被误切
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\os.exe" -internal --run "svc--runx""#),
        Some("svc--runx".to_string())
    );
}

#[test]
fn parse_run_service_name_rejects_non_run_formats() {
    // 无 --run 参数（inplace 旧格式 / 外部服务）
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\ProgramData\Osmium\svcs\a\a.exe""#),
        None
    );
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\foo.exe""#),
        None
    );
    assert_eq!(crate::service_core::parse_run_service_name(""), None);
    // --run 后无参数
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\x.exe" -internal --run"#),
        None
    );
}

#[test]
fn shared_host_rejects_invalid_service_name_from_scm() {
    // P1: 共享宿主按名加载配置，服务名来自 SCM ImagePath——路径穿越/非法名必须拒绝，
    // 防 deployed_config_path 拼接逃出 svcs 目录
    let mut host = crate::service_host::ServiceHost::new();
    // 非法服务名（路径穿越）→ 启动失败且不访问文件系统
    assert!(!host.on_start_with_name("..\\..\\Windows\\evil"));
    assert!(!host.on_start_with_name("a/b"));
    assert!(!host.on_start_with_name(""));
    // 合法服务名但配置不存在 → 启动失败（而非 panic）
    assert!(!host.on_start_with_name("nonexistent-svc-xyz"));
}

#[test]
fn refresh_service_rejects_invalid_name() {
    // 非法服务名（路径穿越/DOS 设备名/控制字符）→ Err，绝不触碰 SCM
    for bad in [
        "..",
        "..\\..\\Windows\\evil",
        "a/b",
        "",
        "CON",
        "bad\x01name",
    ] {
        let err = crate::service_core::refresh_service(bad).unwrap_err();
        assert!(
            err.contains("Invalid service name"),
            "bad name '{bad}': {err}"
        );
    }
}

#[test]
fn refresh_service_rejects_non_osmium_service() {
    // 系统服务（services.exe，非 Osmium 管理）→ 拒绝刷新，不修改其注册属性
    let err = crate::service_core::refresh_service("EventLog").unwrap_err();
    assert!(err.contains("not managed"), "{err}");
}

#[test]
fn refresh_service_rejects_unknown_service() {
    // 不存在的服务: ImagePath 读不到 → 同样按"非 Osmium 管理"拒绝（不创建/不修改）
    let err = crate::service_core::refresh_service("osmium-no-such-svc-xyz").unwrap_err();
    assert!(err.contains("not managed"), "{err}");
}

#[test]
fn deployed_config_path_builds_svcs_layout() {
    // 平台部署配置路径: ProgramData\Osmium\svcs\<name>\<name>.osiml
    let p = deployed_config_path("my-svc");
    let s = p.to_string_lossy();
    assert!(
        s.ends_with("ProgramData\\Osmium\\svcs\\my-svc\\my-svc.osiml"),
        "路径布局错误: {s}"
    );
    // 服务名含空格/特殊字符时原样拼接（不做清理，防穿越依赖服务名校验）
    let p2 = deployed_config_path("svc with space");
    assert!(
        p2.to_string_lossy()
            .ends_with("svc with space\\svc with space.osiml")
    );
}

// ==================== 版本比对 ====================

#[test]
fn compare_versions_basic() {
    assert_eq!(compare_versions("1.0.0", "1.0.0"), 0);
    assert_eq!(compare_versions("2.0.0", "1.9.9"), 1);
    assert_eq!(compare_versions("1.0.0", "1.0.1"), -1);
    assert_eq!(compare_versions("1.2", "1.2.3"), -1);
    assert_eq!(compare_versions("10.0.0", "9.9.9"), 1);
}

#[test]
fn get_file_version_reads_own_version() {
    let v = get_file_version(&get_own_path());
    // build.rs 生成 4 段 FileVersion（major.minor.build.revision，缺段补 0），
    // 与 FileVersionInfo.FileVersion 读取口径一致
    let expected = format!("{}.0", env!("CARGO_PKG_VERSION"));
    assert_eq!(v.as_deref(), Some(expected.as_str()));
}

// ==================== 服务名校验 ====================

#[test]
fn is_valid_service_name_rejects_path_escape() {
    assert!(is_valid_service_name("my-service"));
    assert!(is_valid_service_name("带 空格 的服务"));
    assert!(is_valid_service_name("a")); // 单字符
    assert!(is_valid_service_name(&"x".repeat(256))); // 恰好 256 字符
    // 路径穿越 / 路径分隔符 / 空名必须拒绝
    assert!(!is_valid_service_name("."));
    assert!(!is_valid_service_name(".."));
    assert!(!is_valid_service_name("a\\b"));
    assert!(!is_valid_service_name("a/b"));
    assert!(!is_valid_service_name(""));
    assert!(!is_valid_service_name("   "));
    assert!(!is_valid_service_name(&"x".repeat(257))); // 超过 256 上限
    assert!(!is_valid_service_name("a\u{1}b")); // 控制字符
    assert!(!is_valid_service_name("a\tb")); // tab 控制字符
}

#[test]
fn is_refresher_reserved_name_case_insensitive() {
    assert!(is_refresher_reserved_name("Osmium Service Refresher"));
    assert!(is_refresher_reserved_name("osmium service refresher")); // 大小写不敏感
    assert!(is_refresher_reserved_name("OSMIUM SERVICE REFRESHER"));
    assert!(!is_refresher_reserved_name("checker"));
    assert!(!is_refresher_reserved_name("Osmium"));
    assert!(!is_refresher_reserved_name(""));
}

#[test]
fn has_download_trims_blank_url() {
    let mut c = ServiceConfig::default();
    assert!(!has_download(&c), "无下载配置应为 false");
    c.download_url = Some("".to_string());
    assert!(!has_download(&c), "空串应为 false");
    c.download_url = Some("   ".to_string());
    assert!(!has_download(&c), "纯空白应为 false");
    c.download_url = Some("https://example.com/app.zip".to_string());
    assert!(has_download(&c), "有值应为 true");
}

#[test]
fn has_download_detects_downloads_array() {
    // 数组模式（downloads）: 任一条 from 非空即视为有下载（exe 可能由下载提供、本机尚不存在）
    use crate::service_config::DownloadConfig;
    assert!(
        !has_download(&ServiceConfig {
            downloads: Some(vec![DownloadConfig::default()]),
            ..Default::default()
        }),
        "全空条目不应视为有下载"
    );
    assert!(
        has_download(&ServiceConfig {
            downloads: Some(vec![DownloadConfig {
                from: "https://example.com/x.zip".into(),
                to: "x".into(),
                ..Default::default()
            }]),
            ..Default::default()
        }),
        "数组含有效条目应为 true"
    );
}

// ==================== 启动模式解析 ====================

#[test]
fn parse_start_mode_rules() {
    // 与 WinSW 启动模式语义一致
    assert_eq!(parse_start_mode(None), (SERVICE_AUTO_START, false));
    assert_eq!(parse_start_mode(Some("")), (SERVICE_AUTO_START, false));
    assert_eq!(
        parse_start_mode(Some("automatic")),
        (SERVICE_AUTO_START, false)
    );
    assert_eq!(
        parse_start_mode(Some("delayed_auto")),
        (SERVICE_AUTO_START, true)
    );
    assert_eq!(
        parse_start_mode(Some("delayed-auto")),
        (SERVICE_AUTO_START, true)
    );
    assert_eq!(
        parse_start_mode(Some("delayedauto")),
        (SERVICE_AUTO_START, true)
    );
    assert_eq!(
        parse_start_mode(Some("DELAYED_AUTO")),
        (SERVICE_AUTO_START, true)
    ); // 大小写不敏感
    assert_eq!(
        parse_start_mode(Some("manual")),
        (SERVICE_DEMAND_START, false)
    );
    assert_eq!(
        parse_start_mode(Some("disabled")),
        (SERVICE_DISABLED, false)
    );
    assert_eq!(
        parse_start_mode(Some("unknown")),
        (SERVICE_AUTO_START, false)
    ); // 未知回退自动
}

// ==================== 依赖字符串 multi-sz ====================

#[test]
fn build_dependency_string_multi_sz() {
    // CreateService 期望 "Svc1\0Svc2\0\0"（multi-sz 双 null 结尾）
    assert_eq!(
        build_dependency_string(Some("EventLog;WinRM")),
        Some("EventLog\0WinRM\0\0".to_string())
    );
    assert_eq!(
        build_dependency_string(Some("EventLog, WinRM")),
        Some("EventLog\0WinRM\0\0".to_string())
    );
    // 冒号不再作分隔符: SCM 服务名可含冒号，按名保留（旧实现会错拆为两个依赖）
    assert_eq!(
        build_dependency_string(Some("A:B")),
        Some("A:B\0\0".to_string())
    );
    assert_eq!(build_dependency_string(None), None);
    assert_eq!(build_dependency_string(Some("")), None);
    assert_eq!(build_dependency_string(Some("  ;  ")), None);
}

// ==================== 过期日志清理 ====================

#[test]
fn delete_old_logs_cleans_split_and_rollover() {
    // 修复回归: .err.log 分流与 .N 滚动备份此前从不被清理
    let dir = std::env::temp_dir().join(format!("osmium_log_cleanup_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let names = [
        "2020-01-01.log",       // 主日志（旧）
        "2020-01-01.err.log",   // err 分流（旧）
        "2020-01-01.log.1",     // 滚动备份（旧）
        "2020-01-01.err.log.2", // err 滚动备份（旧）
        "2020-01-01.log.3.zip", // zip 归档（超半年，删除）
        "2099-01-01.log",       // 未来日志（保留）
        "2099-01-01.log.1.zip", // 未来 zip 归档（保留，日期未过期）
        "notes.txt",            // 非日志（保留）
    ];
    for n in &names {
        std::fs::write(dir.join(n), "x").unwrap();
    }
    let cutoff = chrono::Local::now().date_naive();
    // 90 天前的 zip 归档：未超半年保留期（180 天），必须保留
    let recent_zip = format!(
        "{}.log.2.zip",
        (cutoff - chrono::Duration::days(90)).format("%Y-%m-%d")
    );
    std::fs::write(dir.join(&recent_zip), "x").unwrap();
    let deleted = delete_old_logs(&dir, cutoff, false);
    assert_eq!(deleted, 5, "应清理 5 个过期日志（含超半年 zip 归档）");
    let remaining: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .collect();
    assert!(remaining.contains(&"2099-01-01.log".to_string()));
    assert!(remaining.contains(&"notes.txt".to_string()));
    assert!(remaining.contains(&recent_zip), "半年内 zip 归档应保留");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete_old_logs_archives_before_delete_when_enabled() {
    let dir = std::env::temp_dir().join(format!("osmium_log_zipclean_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let cutoff = chrono::Local::now().date_naive();
    // 40 天前的日志（超过 30 天保留期，未到 zip 半年保留期）
    let old_log = format!(
        "{}.log",
        (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d")
    );
    let old_backup = format!(
        "{}.log.1",
        (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d")
    );
    std::fs::write(dir.join(&old_log), "expired-content").unwrap();
    std::fs::write(dir.join(&old_backup), "expired-backup").unwrap();

    // 开启归档: 过期日志先压成 .zip 再删原文件
    let deleted = delete_old_logs(&dir, cutoff, true);
    assert_eq!(deleted, 2, "两个过期日志都应清理");
    assert!(!dir.join(&old_log).exists(), "原日志应被删除");
    assert!(
        dir.join(format!("{old_log}.zip")).exists(),
        "应先生成 zip 归档"
    );
    assert!(!dir.join(&old_backup).exists(), "原滚动备份应被删除");
    assert!(
        dir.join(format!("{old_backup}.zip")).exists(),
        "滚动备份也应先生成 zip 归档"
    );

    // 关闭归档: 直接删除，不产生 zip
    let recent_log = format!(
        "{}.log",
        (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d")
    );
    let _ = std::fs::remove_file(dir.join(format!("{recent_log}.zip")));
    std::fs::write(dir.join(&recent_log), "no-archive").unwrap();
    let deleted = delete_old_logs(&dir, cutoff, false);
    assert_eq!(deleted, 1);
    assert!(!dir.join(&recent_log).exists());
    assert!(
        !dir.join(format!("{recent_log}.zip")).exists(),
        "未开启归档时不应产生 zip"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete_old_logs_falls_back_to_mtime_for_custom_names() {
    // 修复 G4: 自定义文件名（log_out_filename）与非 %Y-%m-%d 前缀 pattern 无法从文件名解析日期，
    // 此前永不被清理；现在回退按 mtime 判定
    let dir = unique_temp_dir("log_mtime");
    let cutoff = chrono::Local::now().date_naive();
    let old = std::time::SystemTime::now() - chrono::Duration::days(40).to_std().unwrap();

    // 旧 mtime 的自定义文件名（无日期前缀）→ 按 mtime 清理
    let custom = dir.join("app.out.log");
    std::fs::write(&custom, "x").unwrap();
    set_file_mtime(&custom, old);
    // 新 mtime 的同款自定义文件 → 保留
    let fresh = dir.join("app2.out.log");
    std::fs::write(&fresh, "x").unwrap();
    // 非 %Y-%m-%d 前缀 pattern 的旧文件（%Y%m%d 风格）→ 按 mtime 清理
    let compact = dir.join("20250801.log");
    std::fs::write(&compact, "x").unwrap();
    set_file_mtime(&compact, old);
    // 非日志扩展名 → 永不清理
    let note = dir.join("notes.txt");
    std::fs::write(&note, "x").unwrap();
    // roll 模式的 .old（带日期前缀）→ 纳入清理
    let old_roll = dir.join("2026-01-01.log.old");
    std::fs::write(&old_roll, "x").unwrap();

    let deleted = delete_old_logs(&dir, cutoff, false);
    assert_eq!(
        deleted, 3,
        "应清理旧 mtime 自定义文件、紧凑日期文件与 .old: {deleted}"
    );
    assert!(!custom.exists(), "旧 mtime 自定义文件应被清理");
    assert!(fresh.exists(), "新文件应保留");
    assert!(!compact.exists(), "紧凑日期前缀旧文件应按 mtime 清理");
    assert!(note.exists(), "非日志扩展名应保留");
    assert!(!old_roll.exists(), ".old 应纳入清理");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 进程树收集（真实进程集成测试） ====================

/// kill_process_tree 核心: BFS 收集进程树（powershell 父进程 → ping 孙进程）
#[test]
fn collect_descendants_finds_grandchild() {
    let pid_file = std::env::temp_dir().join("osmium_tree_test.txt");
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        "Start-Process -FilePath 'C:\\Windows\\System32\\ping.exe' -ArgumentList '-t','127.0.0.1' -WindowStyle Hidden -PassThru | ForEach-Object {{ $_.Id | Out-File -FilePath '{}' -Encoding ascii }}; Start-Sleep -Seconds 30",
        pid_file.display()
    );
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", &script])
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");

    let mut ping_pid = 0u32;
    // 等待窗口 30s（CI 并行跑全部测试时 CPU 竞争激烈，10s 偶发超时）
    for _ in 0..150 {
        if let Ok(s) = std::fs::read_to_string(&pid_file)
            && let Ok(v) = s.trim().parse::<u32>()
        {
            ping_pid = v;
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    assert_ne!(ping_pid, 0, "ping pid not written");

    let descendants = collect_descendants(child.id());
    assert!(
        descendants.contains(&ping_pid),
        "descendants {:?} should contain {}",
        descendants,
        ping_pid
    );

    // 清理: 终止整棵树 + 主进程
    for p in descendants {
        unsafe {
            if let Ok(h) = OpenProcess(PROCESS_TERMINATE, false, p) {
                let _ = TerminateProcess(h, 1);
                let _ = CloseHandle(h);
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

// ==================== 日志注入防护 ====================

/// 日志注入防护: 控制字符转义为可见序列（对应 WinSW #924 / EscapeInvisible）
#[test]
fn escape_invisible_escapes_control_chars() {
    assert_eq!(escape_invisible("\r\n\t"), "\\r\\n\\t");
    assert_eq!(escape_invisible("a\rb\nc\td\x01"), "a\\rb\\nc\\td\\x01");
    assert_eq!(escape_invisible("\x01"), "\\x01");
    assert_eq!(escape_invisible("\x7f"), "\\x7F"); // 大写十六进制（{:02X} 格式）
    assert_eq!(escape_invisible("plain text"), "plain text");
    assert_eq!(escape_invisible("a\nb"), "a\\nb");
}

// ==================== 安全修复回归（DOS 设备名 / 尾空格点 / URL 去敏 / 暴力输入） ====================

#[test]
fn is_valid_service_name_rejects_dos_devices() {
    for name in [
        "CON", "con", "PRN", "AUX", "NUL", "COM1", "com3", "LPT9", "CON.txt", "nul.log",
    ] {
        assert!(
            !is_valid_service_name(name),
            "should reject DOS device name: {}",
            name
        );
    }
}

#[test]
fn is_valid_service_name_rejects_trailing_space_or_dot() {
    assert!(!is_valid_service_name("my-service "));
    assert!(!is_valid_service_name("my-service."));
    assert!(!is_valid_service_name("my-service ."));
}

#[test]
fn is_valid_service_name_accepts_valid_still() {
    assert!(is_valid_service_name("a b c"));
    assert!(is_valid_service_name("带空格-中文.服务"));
    assert!(is_valid_service_name("my-service.v2"));
}

// ==================== URL 去敏（防凭据进日志） ====================

#[test]
fn redact_url_strips_query_and_fragment() {
    assert_eq!(
        redact_url("https://example.com/path?token=secret&x=1#frag"),
        "https://example.com/path"
    );
    assert_eq!(
        redact_url("https://example.com/download/app.exe?auth=abc"),
        "https://example.com/download/app.exe"
    );
    assert_eq!(redact_url("http://host:8080/a?b=c"), "http://host:8080/a");
}

#[test]
fn redact_url_keeps_plain_url() {
    assert_eq!(
        redact_url("https://example.com/app.exe"),
        "https://example.com/app.exe"
    );
    assert_eq!(redact_url("not-a-url"), "not-a-url"); // 非法 URL 原样返回
}

#[test]
fn redact_url_strips_userinfo_credentials() {
    // 内嵌凭据（http://user:pass@host）须去敏，防明文凭据随下载日志落盘
    assert_eq!(
        redact_url("https://alice:secret@example.com/app.exe"),
        "https://example.com/app.exe"
    );
    assert_eq!(
        redact_url("http://alice:secret@example.com:8080/a?token=x#f"),
        "http://example.com:8080/a"
    );
    // 仅用户名无密码同样去除
    assert_eq!(
        redact_url("https://user@example.com/a"),
        "https://example.com/a"
    );
}

// ==================== 暴力测试: 随机输入不 panic（纯函数稳定性） ====================

#[test]
fn is_valid_service_name_stress_random_inputs_no_panic() {
    // 简单 xorshift 伪随机，避免引入 rand 依赖
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let chars: Vec<char> =
        "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .\\/:\t-_\u{1}\u{7f}中文"
            .chars()
            .collect();
    for _ in 0..100_000 {
        let len = (next() % 270) as usize;
        let s: String = (0..len)
            .map(|_| chars[(next() as usize) % chars.len()])
            .collect();
        let _ = is_valid_service_name(&s); // 只要求不 panic
    }
}

// ==================== P0-1 修复回归: inplace 权限检查拦截普通用户写操作（对齐 IsUserWritable_*） ====================

fn unique_temp_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "osmium_p01_{}_{}_{}",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// 把文件 mtime 改为指定时间（构造"旧日志"场景；Rust 1.75+ FileTimes，需写权限句柄）
fn set_file_mtime(path: &std::path::Path, t: std::time::SystemTime) {
    use std::fs::{FileTimes, OpenOptions};
    let f = OpenOptions::new().write(true).open(path).unwrap();
    let _ = f.set_times(FileTimes::new().set_modified(t));
}

fn icacls_ok(args: &[&str]) -> bool {
    Command::new("icacls.exe")
        .args(args)
        .creation_flags(0x08000000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

fn takeown_admins(path: &str) -> bool {
    Command::new("takeown.exe")
        .args(["/F", path, "/A"])
        .creation_flags(0x08000000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn is_user_writable_rejects_everyone_write_on_dir() {
    // 模拟攻击场景: 目录对 Everyone 开放写（共享/公共目录），低权限用户可替换 EXE 获得 SYSTEM 执行
    let dir = unique_temp_dir("everyone");
    let d = dir.to_string_lossy().to_string();
    let _ = takeown_admins(&d); // 尽力把所有者设为 Administrators，确保走 DACL 判定路径
    assert!(icacls_ok(&[&d, "/grant", "*S-1-1-0:(OI)(CI)M"]));
    assert!(
        is_user_writable(&d),
        "Everyone 可写目录必须判可写（拦截安装）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn is_user_writable_rejects_users_write_on_dir() {
    // 模拟攻击场景: BUILTIN\Users 组可写
    let dir = unique_temp_dir("users");
    let d = dir.to_string_lossy().to_string();
    let _ = takeown_admins(&d);
    assert!(icacls_ok(&[&d, "/grant", "*S-1-5-32-545:(OI)(CI)M"]));
    assert!(is_user_writable(&d));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn is_user_writable_rejects_interactive_write_on_dir() {
    // 模拟攻击者预创建目录并授予"交互式登录"低权限主体（S-1-5-4，Everyone/Users 之外的真实账户）写权限
    let dir = unique_temp_dir("interactive");
    let d = dir.to_string_lossy().to_string();
    let _ = takeown_admins(&d);
    assert!(icacls_ok(&[&d, "/grant", "*S-1-5-4:(OI)(CI)M"]));
    assert!(is_user_writable(&d));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn is_user_writable_rejects_everyone_write_on_file() {
    // 模拟攻击场景: EXE/TOML 文件自身对 Everyone 开放写（仅查目录会漏过此替换入口）
    let dir = unique_temp_dir("file");
    let file = dir.join("app.exe");
    std::fs::write(&file, [1u8, 2, 3]).unwrap();
    let f = file.to_string_lossy().to_string();
    let _ = takeown_admins(&f);
    assert!(icacls_ok(&[&f, "/grant", "*S-1-1-0:W"]));
    assert!(is_user_writable(&f));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn is_user_writable_allows_system_admin_secured_dir() {
    // 对照场景: 用生产加固流程构造"仅 SYSTEM/Administrators 写"的目录，必须放行；
    // 非管理员环境无法构造（takeown 需要管理员），跳过
    let dir = unique_temp_dir("secured");
    let d = dir.to_string_lossy().to_string();
    if !secure_directory(&d) {
        eprintln!("skip: 当前环境无法构造仅 SYSTEM/Administrators 的目录（需要管理员）");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert!(
        !is_user_writable(&d),
        "仅 SYSTEM/Administrators 的目录必须放行"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== SDDL 解析（纯函数，直接验证解析器） ====================

#[test]
fn sddl_parse_detects_low_priv_write_aces() {
    // 攻击方 ACE: Everyone(WD)/Users(BU)/Authenticated Users(AU)/交互式(IU) 写
    assert!(sddl_dacl_grants_non_admin_write(
        "D:PAI(A;;0x1301bf;;;WD)(A;;FA;;;SY)"
    ));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;M;;;BU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FA;;;AU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FW;;;IU)"));
    // 攻击方显式账户 SID（非 RID 500/512）
    assert!(sddl_dacl_grants_non_admin_write(
        "D:PAI(A;;FA;;;S-1-5-21-1111-2222-3333-1001)"
    ));
    // 仅 SYSTEM/Administrators → 无低权限写
    assert!(!sddl_dacl_grants_non_admin_write(
        "D:PAI(A;;FA;;;SY)(A;;FA;;;BA)"
    ));
    assert!(!sddl_dacl_grants_non_admin_write(
        "D:PAI(A;;FR;;;WD)(A;;FA;;;SY)"
    ));
}

#[test]
fn sddl_parse_ignores_inherit_only_creator_owner_ace() {
    // 回归: Program Files 等标准 ACL 含 CREATOR OWNER 的 InheritOnly(IO) 全控 ACE，
    // 它只传播给子对象、不影响当前对象可写性，修复前会误判为"非管理员可写"导致 inplace 安装被拒
    assert!(!sddl_dacl_grants_non_admin_write(
        "D:PAI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;OICIIOID;GA;;;CO)(A;ID;0x1200a9;;;BU)"
    ));
    // 非 InheritOnly 的 CREATOR OWNER 全控 ACE（当前对象生效）仍必须判可写
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;GA;;;CO)"));
}

#[test]
fn sddl_parse_handles_combined_generic_right_tokens() {
    // 回归: 组合字母令牌（GRGW 等）此前被精确等值匹配漏判——低权限用户实际可写的
    // 目录会被误判为安全放行安装；子串扫描后必须正确识别
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;GRGW;;;BU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FRFW;;;IU)"));
    // 只读+执行组合（GRGX）不含任何写令牌，必须保持不可写判定
    assert!(!sddl_dacl_grants_non_admin_write("D:PAI(A;;GRGX;;;BU)"));
    // 十六进制的 GENERIC_WRITE / GENERIC_ALL 同样隐含写能力
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;0x40000000;;;WD)"));
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;0x10000000;;;AU)"));
    // 十六进制前缀/位字母大小写混排均须识别（手写 SDDL 可能写 0X 与大写位字母）
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;0X1301BF;;;WD)"));
    assert!(!sddl_dacl_grants_non_admin_write("D:P(A;;0X1200A9;;;BU)"));
    // 大小写混写不漏判（SDDL 规范为大写，手写配置可能混排）
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;gwgw;;;BU)"));
    // 仅"创建子目录/删除子项/DELETE"的目录写权限（Windows 实测位值）必须判可写:
    // LC=0x4(FILE_ADD_SUBDIRECTORY), DT=0x40(FILE_DELETE_CHILD), SD=0x10000(DELETE)
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;LC;;;BU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;DT;;;BU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;SD;;;BU)"));
    // 组合: DCLC=0x6（创建文件+创建子目录，无修改/删除）同样判可写（上传目录场景）
    assert!(sddl_dacl_grants_non_admin_write("D:P(A;;DCLC;;;BU)"));
    // 同步+执行位（0x100020 = SYNCHRONIZE|FILE_EXECUTE）不含任何写能力 → 必须判不可写
    assert!(!sddl_dacl_grants_non_admin_write("D:P(A;;0x100020;;;BU)"));
}

#[test]
fn sddl_parse_flags_admin_denied_ace_as_untrusted() {
    // 锁定 Deny ACE 语义: 管理员被显式拒绝全控视为异常配置（判不可信=拒绝安装）；
    // 非管理员被拒绝不影响整体判定（剩余仅管理员允许时仍为安全）
    assert!(sddl_dacl_grants_non_admin_write(
        "D:PAI(D;;FA;;;BA)(A;;FR;;;WD)"
    ));
    assert!(!sddl_dacl_grants_non_admin_write(
        "D:PAI(D;;FW;;;BU)(A;;FA;;;SY)"
    ));
}

#[test]
fn sddl_parse_owner_rules() {
    assert!(sddl_owner_is_administrative("O:BA"));
    assert!(sddl_owner_is_administrative("O:SY"));
    assert!(sddl_owner_is_administrative(
        "O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464"
    )); // TrustedInstaller
    assert!(!sddl_owner_is_administrative("O:WD"));
    assert!(!sddl_owner_is_administrative("O:BU"));
    assert!(!sddl_owner_is_administrative(
        "O:S-1-5-21-1111-2222-3333-1001"
    ));
}

// ==================== P0-2/P1-2/P1-4/P2-1/P2-2 安全修复回归 ====================

#[test]
fn secure_directory_removes_attacker_aces() {
    // 模拟攻击者预创建目录并留下 Everyone/Users 写 ACE: 加固后不得再允许低权限主体改写（P0-2）；
    // 非管理员环境无法加固（takeown 需要管理员），跳过
    let dir = unique_temp_dir("harden");
    let d = dir.to_string_lossy().to_string();
    assert!(icacls_ok(&[
        &d,
        "/grant",
        "*S-1-1-0:(OI)(CI)M",
        "/grant",
        "*S-1-5-32-545:(OI)(CI)M"
    ]));
    if !secure_directory(&d) {
        eprintln!("skip: 当前环境无法完成目录加固（需要管理员）");
        let _ = std::fs::remove_dir_all(&dir);
        return;
    }
    assert!(!is_user_writable(&d), "加固后不得再允许低权限主体改写");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_deployed_config_strips_service_password() {
    // 运行时配置不得含明文 service_password（P1-2），其余内容保留
    let dir = unique_temp_dir("cfgs");
    let src = dir.join("src.toml");
    let dst = dir.join("dst.toml");
    std::fs::write(&src, "service_name = \"my-svc\"\nservice_password = \"sup3r-secret\"\nservice_executable_path = \"C:\\\\app.exe\"\n").unwrap();
    assert!(write_deployed_config(&src.to_string_lossy(), &dst));
    let deployed = std::fs::read_to_string(&dst).unwrap();
    assert!(!deployed.contains("sup3r-secret"));
    assert!(!deployed.contains("service_password"));
    assert!(deployed.contains("service_name = \"my-svc\""));
    assert!(deployed.contains("C:\\\\app.exe"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_deployed_config_unparsable_strips_all_credential_keys() {
    // 配置无法解析（非标准 TOML）走按行剥离 fallback 时，所有凭据键都不得明文落盘
    //（service_password / download_password / smtp_password / 共享映射 password，缺一即泄漏）
    let dir = unique_temp_dir("cfgs2");
    let src = dir.join("src.toml");
    let dst = dir.join("dst.toml");
    let content = "service_name = \"s\"\nservice_password = \"pw-1\"\ndownload_password = \"pw-2\"\n\
        smtp_password = \"pw-4\"\n\
        [[shared_directory_mappers]]\nlocal_path = \"Z:\"\nremote_path = \"\\\\srv\\share\"\npassword = \"pw-3\"\n";
    // 末尾缺 ] 使整个 TOML 解析失败（非标准配置）
    std::fs::write(&src, content).unwrap();
    assert!(write_deployed_config(&src.to_string_lossy(), &dst));
    let deployed = std::fs::read_to_string(&dst).unwrap();
    for secret in ["pw-1", "pw-2", "pw-3", "pw-4"] {
        assert!(!deployed.contains(secret), "明文凭据不得落盘: {secret}");
    }
    assert!(deployed.contains("remote_path"), "非凭据内容应保留");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn warn_if_insecure_download_refuses_http_without_sha_even_when_fail_on_error_false() {
    // P1-4: fail_on_error=false 也不能关闭明文 HTTP 完整性保护
    let cfg = ServiceConfig {
        download_url: Some("http://example.com/app.exe".into()),
        download_fail_on_error: false,
        ..Default::default()
    };
    assert!(warn_if_insecure_download(&cfg).is_err());
}

#[test]
fn warn_if_insecure_download_allows_https_or_with_sha() {
    let https = ServiceConfig {
        download_url: Some("https://example.com/app.exe".into()),
        ..Default::default()
    };
    assert!(warn_if_insecure_download(&https).is_ok());

    let with_sha = ServiceConfig {
        download_url: Some("http://example.com/app.exe".into()),
        download_sha256: Some("abc123".into()),
        ..Default::default()
    };
    assert!(warn_if_insecure_download(&with_sha).is_ok());

    assert!(warn_if_insecure_download(&ServiceConfig::default()).is_ok()); // 无下载配置
}

#[test]
fn scm_status_params_follows_scm_protocol() {
    use windows::Win32::System::Services::{
        SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP, SERVICE_RUNNING, SERVICE_START_PENDING,
        SERVICE_STOP_PENDING, SERVICE_STOPPED,
    };
    // PENDING/STOPPED 阶段不得接受停止/关机控制码，PENDING 阶段 checkpoint 非零（P2-1）
    assert_eq!(scm_status_params(SERVICE_START_PENDING.0), (0, 1));
    assert_eq!(scm_status_params(SERVICE_STOP_PENDING.0), (0, 1));
    assert_eq!(scm_status_params(SERVICE_STOPPED.0), (0, 0));
    assert_eq!(
        scm_status_params(SERVICE_RUNNING.0),
        (SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0)
    );
}

#[test]
fn is_valid_service_name_rejects_windows_reserved_chars() {
    // Windows 文件名保留字符: 服务名兼作 svcs 目录名（P2-2）
    for c in ['<', '>', ':', '"', '|', '?', '*'] {
        assert!(
            !is_valid_service_name(&format!("my-svc{}1", c)),
            "应拒绝字符: {c}"
        );
    }
}

// ==================== 功能全覆盖: 配置解析 / 同源判定 / SHA-256 / 下载路径 / 前缀清理 ====================

#[test]
fn load_config_parses_valid_toml() {
    let dir = unique_temp_dir("cfg");
    let f = dir.join("ok.toml");
    std::fs::write(
        &f,
        "service_name = \"my-svc\"\nservice_display_name = \"My Service\"\nservice_description = \"desc\"\nservice_executable_path = \"C:\\\\app.exe\"\nservice_executable_args = \"--flag\"\n",
    )
    .unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.service_name, "my-svc");
    assert_eq!(cfg.service_display_name, "My Service");
    assert_eq!(cfg.service_executable_path, "C:\\app.exe");
    assert_eq!(cfg.service_executable_args.as_deref(), Some("--flag"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn quick_config_serializes_sane_defaults() {
    // 以测试进程自身为"目标 exe"（真实存在的绝对路径）
    let exe = std::env::current_exe().unwrap();
    let tmp = write_quick_config("quick-test-svc", exe.to_str().unwrap());
    let cfg = load_config(&tmp);
    let _ = std::fs::remove_file(&tmp);
    assert_eq!(cfg.service_name, "quick-test-svc");
    assert_eq!(cfg.service_display_name, "quick-test-svc");
    assert_eq!(cfg.service_description, "quick-test-svc");
    assert_eq!(
        cfg.service_executable_path,
        strip_verbatim_prefix(&std::fs::canonicalize(&exe).unwrap())
            .to_string_lossy()
            .to_string()
    );
    // 显式写入的 serde 默认值，避免派生 Default 把布尔/数值序列化成 false/0
    assert_eq!(cfg.failure_reset_sec, 86400);
    assert_eq!(cfg.restart_delay_ms, 60000);
    assert!(cfg.kill_process_tree);
    assert!(cfg.log_enabled);
    assert_eq!(cfg.log_max_backup_count, 5);
    assert_eq!(cfg.download_threads, 16);
    assert!(!cfg.deploy_inplace);
    assert_eq!(cfg.service_executable_args.as_deref(), None);
}

#[test]
fn download_threads_defaults_to_16_when_omitted() {
    // 缺失 download_threads → serde 默认 16（修复"缺省被当成 0 禁用多线程"的回归）
    let dir = unique_temp_dir("cfgthr");
    let f = dir.join("ok.toml");
    std::fs::write(
        &f,
        "service_name = \"my-svc\"\nservice_display_name = \"My Service\"\nservice_description = \"desc\"\nservice_executable_path = \"C:\\\\app.exe\"\n",
    )
    .unwrap();
    assert_eq!(load_config(&f).download_threads, 16);
    // 显式 0/1 仍为禁用多线程，不能被默认函数覆盖
    std::fs::write(
        &f,
        "service_name = \"my-svc\"\nservice_display_name = \"My Service\"\nservice_description = \"desc\"\nservice_executable_path = \"C:\\\\app.exe\"\ndownload_threads = 0\n",
    )
    .unwrap();
    assert_eq!(load_config(&f).download_threads, 0);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_panics_on_invalid_toml() {
    let dir = unique_temp_dir("cfgbad");
    let f = dir.join("bad.toml");
    std::fs::write(&f, "service_name = [unclosed").unwrap();
    let r = std::panic::catch_unwind(|| {
        let _ = load_config(&f);
    });
    assert!(
        r.is_err(),
        "损坏的 toml 必须 panic（调用方捕获后按失效服务清理）"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn can_overwrite_source_same_and_different() {
    let dir = unique_temp_dir("overwrite");
    let a = dir.join("a.toml");
    let b = dir.join("b.toml");
    let c = dir.join("c.toml");
    let base = "service_name = \"x\"\nservice_display_name = \"X\"\nservice_description = \"d\"\nservice_executable_path = ";
    std::fs::write(
        &a,
        format!("{base}\"C:\\\\app.exe\"\nservice_executable_args = \"--a\"\n"),
    )
    .unwrap();
    std::fs::write(
        &b,
        format!("{base}\"C:\\\\app.exe\"\nservice_executable_args = \"--a\"\n"),
    )
    .unwrap();
    std::fs::write(&c, format!("{base}\"C:\\\\other.exe\"\n")).unwrap();
    let (sa, sb, sc) = (
        a.to_string_lossy(),
        b.to_string_lossy(),
        c.to_string_lossy(),
    );
    assert!(can_overwrite_source(&sa, &sb, "x")); // 同源 → 允许覆盖更新
    assert!(!can_overwrite_source(&sa, &sc, "x")); // 不同 exe → 拒绝
    // 已部署 toml 缺失 → 退回 ImagePath 归属判定；未注册服务名 → 不可覆盖
    let missing_path = dir.join("missing.toml");
    let missing = missing_path.to_string_lossy();
    assert!(!can_overwrite_source(
        &missing,
        &sa,
        "definitely-not-a-service"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sha256_matches_validates_file() {
    use sha2::{Digest, Sha256};
    let dir = unique_temp_dir("sha");
    let f = dir.join("payload.bin");
    std::fs::write(&f, b"hello osmium").unwrap();
    let hex: String = Sha256::digest(std::fs::read(&f).unwrap())
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    let fs = f.to_string_lossy();
    assert!(sha256_matches(&fs, Some(&hex)));
    assert!(!sha256_matches(&fs, Some(&"0".repeat(64))));
    assert!(sha256_matches(&fs, None)); // 未配置校验值视为匹配
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_download_target_path_resolution() {
    let rel = ServiceConfig {
        download_url: Some("http://x/app.exe".into()),
        download_to: Some("sub\\app.exe".into()),
        service_executable_path: "C:\\ignored.exe".into(),
        ..Default::default()
    };
    assert_eq!(
        resolve_download_target(&rel, "C:\\deploy"),
        "C:\\deploy\\sub\\app.exe"
    );

    let abs = ServiceConfig {
        download_url: Some("http://x/app.exe".into()),
        download_to: Some("C:\\abs\\app.exe".into()),
        service_executable_path: "C:\\ignored.exe".into(),
        ..Default::default()
    };
    assert_eq!(
        resolve_download_target(&abs, "C:\\deploy"),
        "C:\\abs\\app.exe"
    );

    let name = ServiceConfig {
        download_url: Some("http://x/app.exe".into()),
        service_executable_path: "C:\\prog\\target.exe".into(),
        ..Default::default()
    };
    assert_eq!(
        resolve_download_target(&name, "C:\\deploy"),
        "C:\\deploy\\target.exe"
    );
}

#[test]
fn strip_verbatim_prefix_removes_windows_prefix() {
    assert_eq!(
        strip_verbatim_prefix(std::path::Path::new("\\\\?\\C:\\x\\y")),
        std::path::PathBuf::from("C:\\x\\y")
    );
    assert_eq!(
        strip_verbatim_prefix(std::path::Path::new("C:\\plain")),
        std::path::PathBuf::from("C:\\plain")
    );
}

// ==================== 功能全覆盖: 日志滚动 / 删目录 / 钩子 ====================

#[test]
fn roll_if_needed_rotates_log_chain() {
    let dir = unique_temp_dir("roll");
    let log = dir.join("2026-08-02.log");
    std::fs::write(&log, "x".repeat(1_600_000)).unwrap();
    std::fs::write(dir.join("2026-08-02.log.1"), "backup-1").unwrap();
    std::fs::write(dir.join("2026-08-02.log.2"), "backup-2").unwrap();
    std::fs::write(dir.join("2026-08-02.log.3"), "backup-3").unwrap(); // 最旧备份，滚动时清理

    roll_if_needed(&log, 1, 3, false, "");

    assert_eq!(
        std::fs::read_to_string(dir.join("2026-08-02.log.3")).unwrap(),
        "backup-2"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("2026-08-02.log.2")).unwrap(),
        "backup-1"
    );
    assert!(
        std::fs::metadata(dir.join("2026-08-02.log.1"))
            .unwrap()
            .len()
            >= 1_000_000
    );
    assert!(!log.exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn safe_delete_dir_removes_tree_without_following_links() {
    let dir = unique_temp_dir("rmdir");
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("sub/a.txt"), "x").unwrap();
    assert!(delete_dir_tree(&dir));
    assert!(!dir.exists());
    safe_delete_dir(&dir); // 不存在: 不 panic
}

#[test]
fn delete_dir_tree_refuses_junction_root() {
    // S1 回归: 根路径自身是 junction 时必须拒绝递归删除（防诱导 SYSTEM 刷新器删 junction 目标内容）——
    // 子项 file_type 检查拦不住根是 junction 的场景（read_dir 枚举的是目标内容）
    let target = unique_temp_dir("jt-target");
    std::fs::create_dir_all(target.join("keep")).unwrap();
    std::fs::write(target.join("keep/secret.txt"), "x").unwrap();
    let link = unique_temp_dir("jt-link");
    // 用 cmd mklink /J 创建 junction（无需管理员）
    let ok = std::process::Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            "/J",
            &link.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .creation_flags(0x08000000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        // 删除 junction 根: 不得递归进入目标删除其内容（链接本体移除与否均可，目标内容必须保留）
        let _ = delete_dir_tree(&link);
        assert!(
            target.join("keep/secret.txt").exists(),
            "junction 目标内容不应被删除"
        );
    }
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_dir_all(&link);
}

#[test]
fn run_hook_executes_injects_env_and_logs() {
    let dir = unique_temp_dir("hook");
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let env: Vec<(String, String)> = vec![
        ("WINSGF_CHILD_PID".into(), "42".into()),
        ("WINSGF_CHILD_EXIT_CODE".into(), "7".into()),
    ];
    run_hook(
        Some("echo PID=%WINSGF_CHILD_PID% EXIT=%WINSGF_CHILD_EXIT_CODE%"),
        "prestart",
        5000,
        dir.to_string_lossy().to_string(),
        Some(&env),
        &opts,
        None,
        None,
    );
    let log = dir.join(format!("{}.log", chrono::Local::now().format("%Y-%m-%d")));
    let content = std::fs::read_to_string(&log).unwrap();
    assert!(content.contains("PID=42"));
    assert!(content.contains("EXIT=7"));
    assert!(content.contains("prestart"));
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 多线程分块下载 ====================

/// 本地 Range HTTP 服务器，验证 download_core 分块并行下载与源数据一致（aria2 风格）
#[test]
fn chunked_download_parallel_matches_source() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    // 3 MiB 可预测伪随机数据（分块数 3，覆盖 1MiB 分块路径）
    let size = 3 * 1024 * 1024;
    let mut data = vec![0u8; size];
    let mut x: u64 = 0x9e3779b97f4a7c15;
    for b in data.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    let data = Arc::new(data);

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_data = data.clone();
    let server_stop = stop.clone();

    let server = thread::spawn(move || {
        let mut handled = 0usize;
        while !server_stop.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    handled += 1;
                    let _ = stream.set_nonblocking(false);
                    let mut buf = [0u8; 8192];
                    if stream.read(&mut buf).is_err() {
                        continue;
                    }
                    let req = String::from_utf8_lossy(&buf);
                    let len = server_data.len();
                    let (status, headers, body): (&str, String, &[u8]) = if req.starts_with("HEAD")
                    {
                        (
                            "200 OK",
                            format!("Content-Length: {}\r\nAccept-Ranges: bytes\r\n", len),
                            &[],
                        )
                    } else if let Some(range) = req.lines().find(|l| l.starts_with("Range: bytes="))
                    {
                        let spec = range.trim_start_matches("Range: bytes=");
                        let (a, b) = spec.split_once('-').unwrap();
                        let start: usize = a.parse().unwrap();
                        let end: usize = if b.is_empty() {
                            len - 1
                        } else {
                            b.parse().unwrap()
                        };
                        (
                            "206 Partial Content",
                            format!(
                                "Content-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
                                start,
                                end,
                                len,
                                end - start + 1
                            ),
                            &server_data[start..=end],
                        )
                    } else {
                        (
                            "200 OK",
                            format!("Content-Length: {}\r\nAccept-Ranges: bytes\r\n", len),
                            server_data.as_slice(),
                        )
                    };
                    let head = format!("HTTP/1.1 {}\r\n{}\r\n", status, headers);
                    if stream.write_all(head.as_bytes()).is_err() {
                        continue;
                    }
                    let _ = stream.write_all(body);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
        handled
    });

    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = unique_temp_dir("tmp").join("osmium-chunk-test.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        None,
        0,
    );
    stop.store(true, Ordering::Relaxed);
    let handled = server.join().unwrap();
    eprintln!("DIAG handled={handled}");
    eprintln!(
        "DIAG result={:?}",
        result.as_ref().map(|_| ()).map_err(|(t, e)| (t, e.clone()))
    );

    result.unwrap();
    let got = std::fs::read(&tmp).unwrap();
    eprintln!(
        "DIAG got_len={} nonzero={}",
        got.len(),
        got.iter().filter(|b| **b != 0).count()
    );
    assert_eq!(got, *data);
    // HEAD 探测 + 3 个分块请求；少于 4 说明分块路径未生效（回退单线程）
    assert!(
        handled >= 4,
        "expected HEAD + chunk requests, got {}",
        handled
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn expand_env_value_resolves_base_and_vars() {
    // %BASE% 特指部署目录
    assert_eq!(
        expand_env_value("D:/data/%BASE%/log", "C:\\deploy"),
        "D:/data/C:\\deploy/log"
    );
    // 已定义环境变量正常展开（PATH 必存在）
    let path = std::env::var("PATH").unwrap_or_default();
    assert_eq!(expand_env_value("x%PATH%y", "base"), format!("x{path}y"));
    // 未定义变量展开为空串
    assert_eq!(expand_env_value("%OSMIUM_UNDEFINED_XYZ%", "base"), "");
    // 普通文本与中文原样保留
    assert_eq!(
        expand_env_value("C:\\程序\\run.exe", "base"),
        "C:\\程序\\run.exe"
    );
}

#[test]
fn roll_if_needed_zips_oldest_backup() {
    let dir = unique_temp_dir("rollzip");
    let log = dir.join("2026-08-03.log");
    std::fs::write(&log, "x".repeat(1_600_000)).unwrap();
    std::fs::write(dir.join("2026-08-03.log.1"), "backup-1").unwrap();
    std::fs::write(dir.join("2026-08-03.log.2"), "backup-2").unwrap();
    std::fs::write(dir.join("2026-08-03.log.3"), "backup-3").unwrap(); // 最旧备份应被压缩归档

    roll_if_needed(&log, 1, 3, true, "");

    // 最旧备份已归档为 zip，其余备份照常顺延（.3 = 原 .2）
    assert!(dir.join("2026-08-03.log.3.zip").exists());
    assert_eq!(
        std::fs::read_to_string(dir.join("2026-08-03.log.3")).unwrap(),
        "backup-2"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unzip_missing_plugin_reports_error() {
    // zip 解压经 osmium-kit-unzip 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin(
        "unzip",
        &serde_json::json!({ "src": "C:\\x.zip", "dest": "C:\\out" }),
        5,
    );
    assert!(err.is_err(), "未安装插件时解压必须失败");
    assert!(err.unwrap_err().contains("unzip"), "错误信息应含插件名");
}

// ==================== 下载增强: 单线程回退 / Basic 认证 / 404 / 超时 / 分块回退 ====================

#[test]
fn download_core_falls_back_to_single_thread_when_no_range() {
    // 服务器不支持 Range（无 Accept-Ranges）: 大文件也应走单线程，数据一致
    let data = vec![0xABu8; 2 * 1024 * 1024];
    let d2 = data.clone();
    let (addr, stop, count) = spawn_http_server(move |method, _lines| {
        if method == "HEAD" {
            (
                "200 OK".to_string(),
                vec![("Content-Length".into(), d2.len().to_string())],
                vec![],
            )
        } else {
            (
                "200 OK".to_string(),
                vec![("Content-Length".into(), d2.len().to_string())],
                d2.clone(),
            )
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = unique_temp_dir("tmp").join("osmium-norange.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    // HEAD 探测 + 1 次单线程 GET = 2 请求；分块路径会更多
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "应走单线程回退（仅 HEAD+GET）"
    );
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_core_basic_auth_header_sent() {
    // 服务器校验 Authorization: Basic base64(user:pass)，凭据错误返回 401
    let got_auth = Arc::new(AtomicBool::new(false));
    let got = got_auth.clone();
    let data = b"auth-payload".to_vec();
    let d2 = data.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        if method == "HEAD" {
            return (
                "200 OK".to_string(),
                vec![("Accept-Ranges".into(), "none".into())],
                vec![],
            );
        }
        let auth = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .cloned()
            .unwrap_or_default();
        if auth.contains("Basic dXNlcjpwYXNz") {
            // base64("user:pass")
            got.store(true, Ordering::Relaxed);
            (
                "200 OK".to_string(),
                vec![("Content-Length".into(), d2.len().to_string())],
                d2.clone(),
            )
        } else {
            ("401 Unauthorized".to_string(), vec![], b"denied".to_vec())
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = unique_temp_dir("tmp").join("osmium-auth.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::Basic("user", "pass"),
        None,
        16,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert!(
        got_auth.load(Ordering::Relaxed),
        "服务器必须收到 Basic 认证头"
    );
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_core_404_returns_err() {
    let (addr, stop, _count) = spawn_http_server(|_m, _l| {
        (
            "404 Not Found".to_string(),
            vec![("Content-Length".into(), "4".into())],
            b"nope".to_vec(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-404.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        None,
        0,
    );
    stop.store(true, Ordering::Relaxed);
    assert!(result.is_err(), "404 必须返回错误");
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_core_timeout_reports_timeout_flag() {
    // 服务器 accept 后永不响应: reqwest 超时后必须返回 Err((true, _))
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    thread::spawn(move || {
        while !s.load(Ordering::Relaxed) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    // 读走请求后保持连接打开、永不回写，客户端只能等到 reqwest 超时；
                    // 若提前 drop 连接会触发 RST 而非超时
                    let _ = stream.set_nonblocking(false);
                    let mut tmp = [0u8; 1024];
                    let _ = stream.read(&mut tmp);
                    while !s.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(50));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = unique_temp_dir("tmp").join("osmium-timeout.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(
        &url,
        tmp.to_str().unwrap(),
        2,
        DownloadAuth::None,
        None,
        16,
        None,
        0,
    );
    stop.store(true, Ordering::Relaxed);
    assert!(
        matches!(result, Err((true, _))),
        "超时必须返回 (true, 消息)，实际 {:?}",
        result
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_core_chunk_failure_falls_back_to_single() {
    // 服务器声称支持 Range，但对 Range 请求忽略并返回 200 全文:
    // 分块因非 206 失败后必须清零并回退单线程整体下载
    let data = vec![0x55u8; 2 * 1024 * 1024];
    let d2 = data.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, _lines| {
        // HEAD 声明支持 Range，GET 一律返回 200 全文（无视 Range 头）
        let body = if method == "HEAD" { vec![] } else { d2.clone() };
        let headers = vec![
            ("Content-Length".into(), d2.len().to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
        ];
        ("200 OK".to_string(), headers, body)
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = unique_temp_dir("tmp").join("osmium-fallback.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(std::fs::read(&tmp).unwrap(), data, "回退后数据必须完整一致");
    let _ = std::fs::remove_file(&tmp);
}

// ==================== 下载断点复用防跨源污染 / 指标落盘（prometheus 重写 + json 滚动） ====================

#[test]
fn download_resume_discards_stale_tmp_from_other_resource() {
    // 回归: 复用断点前必须校验 tmp 与远端长度一致——更换下载源后旧 URL 的残留 tmp
    // 若按块跳过会把新旧数据混合（无 sha 配置时静默损坏并被执行）
    let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let d2 = data.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        let common = vec![
            ("Content-Length".into(), d2.len().to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
        ];
        if method == "HEAD" {
            return ("200 OK".to_string(), common, vec![]);
        }
        // GET: 解析 Range: bytes=a-b，命中则回 206 区间（分块并行下载依赖）
        let ranged = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()))
            .and_then(|spec| {
                spec.split_once('-')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .filter(|(_, e)| *e < d2.len());
        if let Some((s, e)) = ranged {
            let mut h = vec![
                ("Content-Length".into(), (e - s + 1).to_string()),
                (
                    "Content-Range".into(),
                    format!("bytes {}-{}/{}", s, e, d2.len()),
                ),
            ];
            h.extend(common);
            return ("206 Partial Content".to_string(), h, d2[s..=e].to_vec());
        }
        ("200 OK".to_string(), common, d2.clone())
    });
    let dir = unique_temp_dir("tmp");
    let tmp = dir.join("stale-resume.tmp");
    let _ = std::fs::remove_file(&tmp);
    // 伪造"旧资源残留断点": 长度与远端不一致的全非零内容（旧实现会按块跳过造成混合）
    std::fs::write(&tmp, vec![7u8; 512 * 1024]).unwrap();
    let url = format!("http://{}:{}/big.bin", addr.ip(), addr.port());
    download_core(
        &url,
        tmp.to_str().unwrap(),
        60,
        DownloadAuth::None,
        None,
        8,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        std::fs::read(&tmp).unwrap(),
        data,
        "长度不符的陈旧断点必须清零重下，不得残留旧资源数据"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_resume_keeps_valid_partial_tmp() {
    // 回归（防修复过度）: 长度与远端一致的合法断点必须按块跳过续传——
    // 预填完整的第 0 块，服务器只应收到第 1 块的 Range 请求，最终内容完整
    let second: Vec<u8> = (0..1024 * 1024).map(|i| (i % 251 + 1) as u8).collect();
    let first = vec![0xA5u8; 1024 * 1024]; // 全非零 → chunk_already_done 判定已完成
    let mut data = first.clone();
    data.extend_from_slice(&second);
    let d2 = data.clone();
    let range_hits = Arc::new(AtomicUsize::new(0));
    let rh = range_hits.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        let common = vec![
            ("Content-Length".into(), d2.len().to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
        ];
        if method == "HEAD" {
            return ("200 OK".to_string(), common, vec![]);
        }
        let ranged = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()))
            .and_then(|spec| {
                spec.split_once('-')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .filter(|(_, e)| *e < d2.len());
        if let Some((s, e)) = ranged {
            rh.fetch_add(1, Ordering::Relaxed);
            let mut h = vec![
                ("Content-Length".into(), (e - s + 1).to_string()),
                (
                    "Content-Range".into(),
                    format!("bytes {}-{}/{}", s, e, d2.len()),
                ),
            ];
            h.extend(common);
            return ("206 Partial Content".to_string(), h, d2[s..=e].to_vec());
        }
        ("200 OK".to_string(), common, d2.clone())
    });
    let dir = unique_temp_dir("tmp");
    let tmp = dir.join("valid-resume.tmp");
    let url = format!("http://{}:{}/resume.bin", addr.ip(), addr.port());
    // 合法断点: 与远端等长、第 0 块完整写入（全非零）、第 1 块全零待续传；
    // 同时预填 .resume 归属标记（模拟上次同 URL 下载中断的残留——无标记的 tmp 视为旧源残留会清零）
    std::fs::write(&tmp, &first).unwrap();
    // 标记内容与实现一致: 完整 URL 的 SHA-256 + 长度（哈希判归属，防换源误复用且不落明文凭据）
    use sha2::{Digest, Sha256};
    let mut mh = Sha256::new();
    mh.update(url.as_bytes());
    let url_hash: String = mh.finalize().iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(
        dir.join("valid-resume.tmp.resume"),
        format!("{url_hash}\n{}", data.len()),
    )
    .unwrap();
    download_core(
        &url,
        tmp.to_str().unwrap(),
        60,
        DownloadAuth::None,
        None,
        4,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        range_hits.load(Ordering::Relaxed),
        1,
        "已完成的第 0 块必须被跳过，只下载第 1 块"
    );
    assert_eq!(
        std::fs::read(&tmp).unwrap(),
        data,
        "合法断点续传后内容必须完整且保留已下载部分"
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn metrics_file_prometheus_rewrites_and_json_rotates() {
    let dir = unique_temp_dir("metrics");
    let path = dir.join("metrics.out");
    let ps = path.to_str().unwrap();
    // prometheus: # TYPE 行须全局唯一——重复写入必须是整文件重写而非追加，
    // 否则 textfile 采集器解析失败
    let prom_line = "# TYPE osmium_cpu_percent gauge\nosmium_cpu_percent{pid=\"1\"} 1.0";
    write_metrics_file(ps, prom_line, true);
    write_metrics_file(ps, prom_line, true);
    let content = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        content.matches("# TYPE").count(),
        1,
        "prometheus 重写不得累积重复 # TYPE 行"
    );
    // json: 超过滚动阈值把当前挪到 .1，新写入从空文件开始追加
    let big = "x".repeat(crate::service_host::METRICS_ROTATE_BYTES as usize);
    write_metrics_file(ps, &big, false);
    write_metrics_file(ps, "{\"small\":1}", false);
    let rolled = std::fs::read_to_string(dir.join("metrics.out.1")).unwrap();
    assert!(rolled.contains(&big), "滚动文件保留触发滚动前的历史内容");
    assert!(
        !rolled.contains("{\"small\":1}"),
        "滚动后的最新记录不得进入 .1"
    );
    let cur = std::fs::read_to_string(&path).unwrap();
    assert!(
        cur.contains("{\"small\":1}") && !cur.contains(&big),
        "滚动后新文件只含最新记录"
    );
}

// ==================== 日志底层: 分流 / 转义 / 空目录 / 归档失败 / 滚动空操作 ====================

#[test]
fn write_log_entry_splits_err_and_escapes() {
    let dir = unique_temp_dir("wlog");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: true,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    write_log_entry(&d, "err", "line from stderr", &opts);
    write_log_entry(&d, "host", "bad\r\ninjected", &opts);
    write_log_entry(&d, "out", "normal out", &opts);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let main = std::fs::read_to_string(dir.join(format!("{date}.log"))).unwrap();
    let err = std::fs::read_to_string(dir.join(format!("{date}.err.log"))).unwrap();
    // err 通道写 .err.log
    assert!(err.contains("line from stderr"));
    assert!(!main.contains("line from stderr"), "stderr 不得混入主日志");
    // host 通道控制字符必须转义（防日志注入）
    assert!(main.contains("bad\\r\\ninjected"));
    assert!(!main.contains("bad\r\ninjected"), "原始换行不得写入");
    // out 通道原样写入
    assert!(main.contains("normal out"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_log_entry_empty_dir_is_noop() {
    // log_dir 为空串表示禁用: 不 panic、不产生文件
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    write_log_entry("", "host", "should not appear", &opts);
}

#[test]
fn zip_backup_file_missing_returns_false() {
    let dir = unique_temp_dir("zbak");
    assert!(!zip_backup_file(&dir.join("nope.log"), ""));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roll_if_needed_noop_when_unconfigured() {
    let dir = unique_temp_dir("rollnoop");
    let log = dir.join("2026-08-04.log");
    std::fs::write(&log, "small").unwrap();
    roll_if_needed(&log, 0, 5, false, ""); // max_size_mb=0 → 不滚动
    roll_if_needed(&log, 1, 0, false, ""); // backup_count=0 → 不滚动
    assert_eq!(std::fs::read_to_string(&log).unwrap(), "small");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 钩子超时强杀 ====================

#[test]
fn run_hook_timeout_kills_hung_hook() {
    let dir = unique_temp_dir("hookto");
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let start = Instant::now();
    // ping -t 永不退出，验证超时强杀后 run_hook 尽快返回
    run_hook(
        Some("ping -t 127.0.0.1"),
        "prestart",
        800,
        dir.to_string_lossy().to_string(),
        None,
        &opts,
        None,
        None,
    );
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_secs() < 5,
        "超时钩子必须被强杀，实际耗时 {:?}",
        elapsed
    );
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let content = std::fs::read_to_string(dir.join(format!("{date}.log"))).unwrap();
    assert!(
        content.contains("timed out"),
        "日志应记录超时强杀: {content}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 进程优先级 / ANSI 红色 ====================

#[test]
fn set_process_priority_unknown_and_invalid_no_panic() {
    set_process_priority(u32::MAX, Some("high")); // 无效 pid: OpenProcess 失败静默
    set_process_priority(std::process::id(), Some("bogus")); // 未知优先级: 直接返回
    set_process_priority(std::process::id(), None); // None: 返回
    set_process_priority(std::process::id(), Some("belownormal")); // 有效值: 设置当前进程不报错
}

#[test]
fn red_vt_disabled_returns_plain_text() {
    // 测试环境 stderr 无 VT（enable_stderr_vt 未调用）→ red 原样返回，无 ANSI 转义
    let out = red("some error message");
    assert!(out.contains("some error message"));
    assert!(!out.contains("\x1b["), "VT 未启用时不应出现 ANSI 转义");
    assert!(!out.contains("[31m"));
}

#[test]
fn dots_no_vt_plain_rendering() {
    // 测试环境 stdout VT 未启用 → 绿点/红点均无色渲染（重定向场景无转义乱码）
    let g = green_dot();
    let r = red_dot();
    assert!(g.contains("●"));
    assert!(r.contains("●"));
    assert!(!g.contains("\x1b["), "VT 未启用时绿点不应有 ANSI 转义");
    assert!(!r.contains("\x1b["), "VT 未启用时红点不应有 ANSI 转义");
    assert!(!g.contains("32"), "绿点不应含绿色转义码");
    assert!(!r.contains("31"), "红点不应含红色转义码");
}

// ==================== 边缘 / 暴力: 版本比对 / env 展开 / 转义 / 下载目标 / URL 去敏 ====================

#[test]
fn compare_versions_edge_cases() {
    assert_eq!(compare_versions("", ""), 0);
    assert_eq!(compare_versions("abc", "abc"), 0); // 非数字段全丢弃
    assert_eq!(compare_versions("abc", "1"), -1); // [] vs [1] → 0 < 1
    assert_eq!(compare_versions("1.0.0.0.0", "1.0"), 0); // 多余 0 段不改变比较
    assert_eq!(compare_versions("1.10", "1.9"), 1); // 十进制定位，非字典序
    assert_eq!(compare_versions("1.a.5", "1.0.4"), 1); // 中间非数字段按 0 处理
    assert_eq!(compare_versions("2", "1.9.9.9.9"), 1);
    assert_eq!(compare_versions("4294967295", "4294967294"), 1); // u32 上限边界
    assert_eq!(compare_versions("1.0.0", "1.0.0-beta"), 0); // 尾部非数字段丢弃
    assert_eq!(compare_versions("1..2", "1.2"), 0); // 空段按 0（[1,2] vs [1,2]）
}

#[test]
fn compare_versions_stress_random_no_panic() {
    let mut state: u64 = 0xdeadbeefcafe;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let tokens = [
        "0",
        "1",
        "2",
        "10",
        "999",
        "abc",
        "",
        "1.2.3",
        "0.0.1",
        "99999999999999999999",
    ];
    for _ in 0..50_000 {
        let a = (0..(next() % 5) as usize)
            .map(|_| tokens[(next() as usize) % tokens.len()])
            .collect::<Vec<_>>()
            .join(".");
        let b = (0..(next() % 5) as usize)
            .map(|_| tokens[(next() as usize) % tokens.len()])
            .collect::<Vec<_>>()
            .join(".");
        let r = compare_versions(&a, &b);
        assert!(r == -1 || r == 0 || r == 1);
    }
}

#[test]
fn expand_env_value_edge_cases() {
    assert_eq!(expand_env_value("", "base"), "");
    assert_eq!(expand_env_value("%BASE%", "C:\\x"), "C:\\x");
    assert_eq!(expand_env_value("pre%BASE%post", "b"), "prebpost");
    assert_eq!(expand_env_value("%BASE%%BASE%", "b"), "bb");
    assert_eq!(expand_env_value("%OSMIUM_UNSET_1%text", "b"), "text");
    assert_eq!(expand_env_value("unclosed%BASE", "b"), "unclosed%BASE"); // 未闭合 % 原样
    assert_eq!(expand_env_value("trailing%", "b"), "trailing%"); // 尾部孤立 %
    assert_eq!(expand_env_value("a%b", "b"), "a%b"); // 无闭合对
    // 中文与真实环境变量混用
    let path = std::env::var("PATH").unwrap_or_default();
    assert_eq!(
        expand_env_value("%BASE%\\中文\\%PATH%", "D:\\d"),
        format!("D:\\d\\中文\\{path}")
    );
    // %PID% 占位符保留原样（停止命令执行时才替换，对应 WinSW #217）
    assert_eq!(expand_env_value("--pid %PID%", "C:\\base"), "--pid %PID%");
    assert_eq!(
        expand_env_value("%pid% %BASE%", "C:\\base"),
        "%pid% C:\\base"
    );
    // 空变量名（%%）原样保留，不静默吞掉
    assert_eq!(expand_env_value("a%%b", "base"), "a%%b");
    assert_eq!(expand_env_value("%%BASE%", "base"), "%%BASE%");
    assert_eq!(expand_env_value("%%", "base"), "%%");
}

#[test]
fn escape_invisible_edge_cases() {
    assert_eq!(escape_invisible(""), "");
    assert_eq!(escape_invisible("\x00"), "\\x00");
    assert_eq!(escape_invisible("中文\n尾"), "中文\\n尾"); // 非 ASCII 保留
    assert_eq!(escape_invisible("\x1f\x7f"), "\\x1F\\x7F"); // 上边界
    assert_eq!(escape_invisible(" normal \x20"), " normal \x20"); // 空格保留
    let long = "a\r\n".repeat(5_000);
    assert_eq!(escape_invisible(&long).len(), 25_000); // 长输入不崩溃（3 字符 → 5 字符）
}

#[test]
fn resolve_download_target_edge_cases() {
    let mut c = ServiceConfig {
        download_url: Some("http://x/f.exe".into()),
        // download_to 空白 → 回退到 exe 文件名
        download_to: Some("   ".into()),
        service_executable_path: "C:\\prog\\t.exe".into(),
        ..Default::default()
    };
    assert_eq!(
        resolve_download_target(&c, "C:\\deploy"),
        "C:\\deploy\\t.exe"
    );
    // 无文件名（exe 路径以 \ 结尾的目录）→ Windows file_name 取最后一段目录名
    c.download_to = None;
    c.service_executable_path = "C:\\prog\\".into();
    assert_eq!(
        resolve_download_target(&c, "C:\\deploy"),
        "C:\\deploy\\prog"
    );
    // UNC / 以 \ 开头的相对路径视为绝对
    c.download_to = Some("\\\\server\\share\\f.exe".into());
    assert_eq!(
        resolve_download_target(&c, "C:\\deploy"),
        "\\\\server\\share\\f.exe"
    );
}

#[test]
fn redact_url_edge_cases() {
    // 内嵌凭据（user:pass@host）一并去除（防凭据进日志）
    assert_eq!(
        redact_url("https://user:pass@example.com/a?x=1#f"),
        "https://example.com/a"
    );
    assert_eq!(
        redact_url("http://example.com?only=query"),
        "http://example.com/"
    );
    assert_eq!(
        redact_url("http://example.com#onlyfrag"),
        "http://example.com/"
    );
    assert_eq!(redact_url(""), "");
    assert_eq!(redact_url("https://example.com/"), "https://example.com/");
    assert_eq!(redact_url("ftp://x/y?z=1"), "ftp://x/y");
}

// ==================== 边缘: 同源判定大小写 / 全字段配置 / SCM 状态 / 进程树 / SDDL 畸形 ====================

#[test]
fn can_overwrite_source_case_insensitive() {
    let dir = unique_temp_dir("overci");
    let a = dir.join("a.toml");
    let b = dir.join("b.toml");
    let base = "service_name = \"x\"\nservice_display_name = \"X\"\nservice_description = \"d\"\nservice_executable_path = ";
    std::fs::write(
        &a,
        format!("{base}\"C:\\\\App.Exe\"\nservice_executable_args = \"--X\"\n"),
    )
    .unwrap();
    std::fs::write(
        &b,
        format!("{base}\"c:\\\\app.exe\"\nservice_executable_args = \"--x\"\n"),
    )
    .unwrap();
    // 路径与参数均忽略大小写 → 视为同源允许覆盖
    assert!(can_overwrite_source(
        &a.to_string_lossy(),
        &b.to_string_lossy(),
        "x"
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_full_fields_roundtrip() {
    let dir = unique_temp_dir("cfgfull");
    let f = dir.join("full.toml");
    std::fs::write(
        &f,
        r#"
service_name = "full-svc"
service_display_name = "Full"
service_description = "all fields"
service_executable_path = 'C:\app.exe'
working_directory = 'D:\work'
process_priority = "high"
stop_executable = 'C:\stop.exe'
stop_arguments = "--drain"
interactive = true
failure_action = "reboot"
allow_service_logon = true
event_log = true
log_zip = true
log_max_size_mb = 20
download_auth = "basic"
download_username = "u"
download_password = "p"
download_proxy = "http://127.0.0.1:8080"
download_unzip = true

[[extensions]]
phase = "start"
command = 'echo start'

[[extensions]]
phase = "stop"
command = 'echo stop'
"#,
    )
    .unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.working_directory.as_deref(), Some("D:\\work"));
    assert_eq!(cfg.process_priority.as_deref(), Some("high"));
    assert_eq!(cfg.stop_executable.as_deref(), Some("C:\\stop.exe"));
    assert_eq!(cfg.stop_arguments.as_deref(), Some("--drain"));
    assert!(cfg.interactive);
    assert_eq!(cfg.failure_action.as_deref(), Some("reboot"));
    assert!(cfg.allow_service_logon);
    assert!(cfg.event_log);
    assert!(cfg.log_zip);
    assert_eq!(cfg.log_max_size_mb, 20);
    assert_eq!(cfg.download_auth.as_deref(), Some("basic"));
    assert_eq!(cfg.download_username.as_deref(), Some("u"));
    assert_eq!(cfg.download_proxy.as_deref(), Some("http://127.0.0.1:8080"));
    assert!(cfg.download_unzip);
    let exts = cfg.extensions.unwrap();
    assert_eq!(exts.len(), 2);
    assert_eq!(exts[0].phase, "start");
    assert_eq!(exts[1].phase, "stop");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn scm_status_params_unknown_state_defaults_running() {
    use windows::Win32::System::Services::{SERVICE_ACCEPT_SHUTDOWN, SERVICE_ACCEPT_STOP};
    // 未知状态值按运行中处理（else 分支），可接受停止/关机
    assert_eq!(
        scm_status_params(999),
        (SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0)
    );
}

#[test]
fn collect_descendants_invalid_pid_empty() {
    assert!(
        collect_descendants(u32::MAX).is_empty(),
        "无效 pid 必须返回空且不 panic"
    );
}

#[test]
fn sddl_malformed_inputs_no_panic() {
    for s in [
        "",
        "garbage",
        "D:",
        "D:PAI(",
        "D:PAI(A;;FA;;;SY)",
        "O:",
        "D:P(A;;GA;;;WD)",
        "(A;;FA;;;WD)",
    ] {
        let d = std::panic::catch_unwind(|| sddl_dacl_grants_non_admin_write(s));
        assert!(d.is_ok(), "sddl_dacl 畸形输入不得 panic: {s:?}");
        let o = std::panic::catch_unwind(|| sddl_owner_is_administrative(s));
        assert!(o.is_ok(), "sddl_owner 畸形输入不得 panic: {s:?}");
    }
}

// ==================== 收尾边界: 缺失文件 / 滚动阈值边界 ====================

#[test]
fn sha256_matches_missing_file_false() {
    let dir = unique_temp_dir("shamiss");
    // 未配置校验值 → 一律放行（不校验），无论文件是否存在
    assert!(sha256_matches(
        &dir.join("nope.bin").to_string_lossy(),
        None
    ));
    // 配置了校验值但文件缺失 → false
    assert!(!sha256_matches(
        &dir.join("nope.bin").to_string_lossy(),
        Some(&"0".repeat(64))
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_deployed_config_missing_source_false() {
    let dir = unique_temp_dir("cfgmiss");
    assert!(!write_deployed_config(
        &dir.join("nope.toml").to_string_lossy(),
        &dir.join("out.toml")
    ));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn delete_dir_tree_missing_returns_true() {
    let dir = unique_temp_dir("rmmiss");
    assert!(delete_dir_tree(&dir.join("no-such-dir")));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roll_if_needed_threshold_boundary() {
    let dir = unique_temp_dir("rollbd");
    // 恰好等于 1MiB 上限 → 触发滚动
    let exact = dir.join("2026-08-05.log");
    std::fs::write(&exact, vec![b'x'; 1024 * 1024]).unwrap();
    roll_if_needed(&exact, 1, 2, false, "");
    assert!(dir.join("2026-08-05.log.1").exists(), "恰好达到阈值应滚动");
    // 差 1 字节 → 不滚动
    let under = dir.join("2026-08-06.log");
    std::fs::write(&under, vec![b'x'; 1024 * 1024 - 1]).unwrap();
    roll_if_needed(&under, 1, 2, false, "");
    assert!(under.exists(), "未达阈值不应滚动");
    assert!(!dir.join("2026-08-06.log.1").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 第二轮 WinSW 对齐新增功能测试 ====================

#[test]
fn dpapi_roundtrip_and_legacy_pass_through() {
    // 加密 → 解密还原
    let enc = dpapi_encrypt("sup3r-secret").expect("DPAPI 加密应成功");
    assert!(enc.starts_with("enc:OSMIUM1:"), "加密值必须带版本化前缀");
    assert!(!enc.contains("sup3r-secret"), "密文不得含明文");
    assert_eq!(dpapi_decrypt(&enc), "sup3r-secret");
    // 非前缀（明文/旧格式）原样返回
    assert_eq!(dpapi_decrypt("plain-value"), "plain-value");
    assert_eq!(dpapi_decrypt(""), "");
    // 非法 base64 前缀原样返回
    assert_eq!(
        dpapi_decrypt("enc:OSMIUM1:!!!not-base64!!!"),
        "enc:OSMIUM1:!!!not-base64!!!"
    );
}

#[test]
fn decrypt_sensitive_covers_all_fields() {
    // 四个敏感字段逐一加密后统一解密还原；明文/无前缀值原样透传
    let enc_svc = dpapi_encrypt("svc-pass").unwrap();
    let enc_dl = dpapi_encrypt("dl-pass").unwrap();
    let enc_map = dpapi_encrypt("map-pass").unwrap();
    let enc_smtp = dpapi_encrypt("smtp-pass").unwrap();
    let mut config = ServiceConfig {
        service_password: Some(enc_svc.clone()),
        download_password: Some(enc_dl.clone()),
        smtp_password: Some(enc_smtp.clone()),
        shared_directory_mappers: Some(vec![
            SharedMapperConfig {
                local_path: "Z:".to_string(),
                remote_path: r"\\server\share".to_string(),
                username: None,
                password: Some(enc_map.clone()),
            },
            SharedMapperConfig {
                local_path: "Y:".to_string(),
                remote_path: r"\\server\share2".to_string(),
                username: None,
                password: Some("plain-map-pass".to_string()), // 明文透传
            },
        ]),
        ..Default::default()
    };
    decrypt_sensitive(&mut config);
    assert_eq!(config.service_password.as_deref(), Some("svc-pass"));
    assert_eq!(config.download_password.as_deref(), Some("dl-pass"));
    assert_eq!(config.smtp_password.as_deref(), Some("smtp-pass"));
    let mappers = config.shared_directory_mappers.as_ref().unwrap();
    assert_eq!(mappers[0].password.as_deref(), Some("map-pass"));
    assert_eq!(mappers[1].password.as_deref(), Some("plain-map-pass"));
}

#[test]
fn write_deployed_config_encrypts_sensitive_fields() {
    let dir = unique_temp_dir("cryptcfg");
    let src = dir.join("src.toml");
    std::fs::write(
        &src,
        concat!(
            "service_name = \"crypt-svc\"\n",
            "service_display_name = \"Crypt\"\n",
            "service_description = \"x\"\n",
            "service_executable_path = \"C:\\\\app.exe\"\n",
            "service_password = \"svc-pass-123\"\n",
            "download_password = \"dl-pass-456\"\n",
            "smtp_password = \"smtp-pass-789\"\n",
        ),
    )
    .unwrap();
    let dst = dir.join("deployed.osiml");
    assert!(write_deployed_config(&src.to_string_lossy(), &dst));
    let text = std::fs::read_to_string(&dst).unwrap();
    assert!(!text.contains("svc-pass-123"), "部署文件不得含明文密码");
    assert!(!text.contains("dl-pass-456"));
    assert!(!text.contains("smtp-pass-789"));
    assert!(text.contains("enc:OSMIUM1:"), "部署文件应含 DPAPI 密文");
    // load_config 解密还原
    let cfg = load_config(&dst);
    assert_eq!(cfg.service_password.as_deref(), Some("svc-pass-123"));
    assert_eq!(cfg.download_password.as_deref(), Some("dl-pass-456"));
    assert_eq!(cfg.smtp_password.as_deref(), Some("smtp-pass-789"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn failure_action_chain_defaults_and_filters() {
    // 未配置 → 旧行为: 3 次 restart + 1 次 none
    let plain: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n").unwrap();
    let chain = failure_action_chain(&plain);
    assert_eq!(chain.len(), 4);
    assert!(chain[..3].iter().all(|a| a.action == "restart"));
    assert_eq!(chain[3].action, "none");
    // failure_action 自定义动作
    let reboot: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\nfailure_action = \"none\"\n").unwrap();
    let chain = failure_action_chain(&reboot);
    assert!(chain.iter().all(|a| a.action == "none"));
    // 显式序列 + 非法动作过滤
    let explicit: ServiceConfig = toml::from_str(concat!(
        "service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n",
        "[[failure_actions]]\naction = \"restart\"\ndelay_secs = 10\n",
        "[[failure_actions]]\naction = \"bogus\"\n",
        "[[failure_actions]]\naction = \"reboot\"\n",
    )).unwrap();
    let chain = failure_action_chain(&explicit);
    assert_eq!(chain.len(), 2, "非法动作必须被过滤");
    assert_eq!(chain[0].delay_secs, 10);
    assert_eq!(chain[1].action, "reboot");
}

#[test]
fn download_stage_defaults_to_before_start() {
    let plain: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n").unwrap();
    assert!(download_stage_is(&plain, "before_start"));
    assert!(!download_stage_is(&plain, "after_start"));
    let after: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\ndownload_stage = \"after_stop\"\n").unwrap();
    assert!(download_stage_is(&after, "after_stop"));
    assert!(!download_stage_is(&after, "before_start"));
}

#[test]
fn log_pattern_safe_and_custom_filename() {
    assert!(log_pattern_safe("yyyyMMdd"));
    assert!(log_pattern_safe("%Y%m%d"));
    assert!(log_pattern_safe("%Y-%m-%d"));
    assert!(log_pattern_safe(""));
    assert!(!log_pattern_safe("yyyy\\MM"), "路径分隔符必须拒绝");
    assert!(!log_pattern_safe("a/../b"));
    let now = chrono::Local::now();
    let opts = LogOptions {
        split_out_err: true,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: "%Y%m%d".into(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let main = current_log_name(&opts, "host", &now);
    assert_eq!(main, format!("{}.log", now.format("%Y%m%d")));
    let err = current_log_name(&opts, "err", &now);
    assert_eq!(err, format!("{}.err.log", now.format("%Y%m%d")));
}

#[test]
fn write_log_entry_uses_custom_pattern_and_reset() {
    let dir = unique_temp_dir("logpat");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: "%Y%m".into(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    write_log_entry(&d, "host", "custom-pattern-entry", &opts);
    let name = format!("{}.log", chrono::Local::now().format("%Y%m"));
    assert!(
        std::fs::read_to_string(dir.join(&name))
            .unwrap()
            .contains("custom-pattern-entry")
    );
    // reset 清空当日文件
    let reset_opts = LogOptions {
        reset: true,
        ..opts
    };
    reset_current_logs(&d, &reset_opts);
    assert_eq!(
        std::fs::read_to_string(dir.join(&name)).unwrap(),
        "",
        "reset 应清空日志"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn ext_phase_matches_backward_compatible() {
    assert!(ext_phase_matches("start", "start_before"));
    assert!(ext_phase_matches("Start_Before", "start_before"));
    assert!(ext_phase_matches("start_before", "start_before"));
    assert!(!ext_phase_matches("start", "start_after"));
    assert!(ext_phase_matches("stop", "stop_after"));
    assert!(ext_phase_matches("stop_before", "stop_before"));
    assert!(!ext_phase_matches("stop_after", "stop_before"));
    assert!(ext_phase_matches("start_after", "start_after"));
}

#[test]
fn process_samples_work_for_self() {
    let pid = std::process::id();
    assert!(process_cpu_100ns(pid).is_some(), "当前进程 CPU 采样应成功");
    assert!(
        process_working_set_mb(pid).is_some(),
        "当前进程内存采样应成功"
    );
    // 不存在进程 → None 不 panic
    assert!(process_cpu_100ns(u32::MAX).is_none());
    assert!(process_working_set_mb(u32::MAX).is_none());
}

#[test]
fn netmap_missing_plugin_reports_error() {
    // 共享目录映射经 osmium-kit-netmap 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin(
        "netmap",
        &serde_json::json!({ "action": "map", "mappers": [] }),
        5,
    );
    assert!(err.is_err(), "未安装插件时映射必须失败");
    assert!(err.unwrap_err().contains("netmap"), "错误信息应含插件名");
}

#[test]
fn sspi_missing_plugin_reports_error() {
    // sspi 认证下载经 osmium-kit-sspi 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin(
        "sspi",
        &serde_json::json!({ "url": "http://x", "to": "C:\\x" }),
        5,
    );
    assert!(err.is_err(), "未安装插件时 sspi 下载必须失败");
    assert!(err.unwrap_err().contains("sspi"), "错误信息应含插件名");
}

#[test]
fn reboot_missing_plugin_reports_error() {
    // 系统重启经 osmium-kit-reboot 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin("reboot", &serde_json::json!({}), 5);
    assert!(err.is_err(), "未安装插件时重启必须失败");
    assert!(err.unwrap_err().contains("reboot"), "错误信息应含插件名");
}

#[test]
fn discover_plugins_returns_osx_entries_only() {
    // 扫描环境: 插件目录（exe 同级）随运行环境而定——
    // 未安装插件时为空；若存在插件则逐项校验扩展名与目录跳过规则
    crate::service_host::clear_plugin_cache();
    let plugins = crate::service_host::discover_plugins();
    for p in &plugins {
        assert_eq!(
            p.extension()
                .map(|e| e.to_string_lossy().to_lowercase())
                .unwrap_or_default(),
            "osx",
            "发现的条目必须是 .osx 文件: {}",
            p.display()
        );
    }
    // 安装环境（Publish 存在真实插件）时不应发现隐藏目录条目
    let names: Vec<String> = plugins
        .iter()
        .map(|p| {
            p.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        !names.iter().any(|n| n.starts_with('.')),
        "不得包含隐藏条目: {names:?}"
    );
}

#[test]
fn pe_arch_detects_machine_type() {
    // 当前测试进程是 PE 可执行文件 → 架构与自身位数一致（64/32 构建通用断言）
    let self_exe = std::env::current_exe().unwrap();
    let expect = if cfg!(target_pointer_width = "64") {
        Some("64")
    } else {
        Some("32")
    };
    assert_eq!(crate::service_host::pe_arch(&self_exe).as_deref(), expect);
    // 非 PE 文件（无 MZ/PE 签名）→ None（显示 unknown）
    let dir = unique_temp_dir("pearch");
    let txt = dir.join("not-pe.osx");
    std::fs::write(&txt, "not an executable").unwrap();
    assert_eq!(crate::service_host::pe_arch(&txt), None);
    // 不存在文件 → None
    assert_eq!(crate::service_host::pe_arch(&dir.join("missing.osx")), None);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn plugin_usable_rejects_inert_executable() {
    // 非协议可执行（cmd.exe 无 ping 响应）: 5 秒超时后判定不可用
    let cmd = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
        + "\\System32\\cmd.exe";
    assert!(
        !crate::service_host::plugin_usable(std::path::Path::new(&cmd)),
        "cmd.exe 不响应 ping 协议，必须判定不可用"
    );
}

// ==================== 第二轮 WinSW 对齐: 冒烟 / 暴力 / 边缘测试 ====================

#[test]
fn auto_roll_logs_rolls_once_per_day() {
    reset_auto_roll_state();
    let dir = unique_temp_dir("autoroll");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: Some("00:00:00".into()),
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    // 构造"到达定点时刻后"的固定时间，并预写"到达前"的当日日志
    let date = "2026-08-11";
    let now = chrono::Local
        .with_ymd_and_hms(2026, 8, 11, 0, 0, 5)
        .single()
        .unwrap();
    std::fs::write(dir.join(format!("{date}.log")), "legacy-before-roll").unwrap();
    // 到达时刻后的首次写入 → 当日日志归档为 {date}.{HHmmss}.log
    auto_roll_logs(&d, &opts, &now);
    let archived = format!("{date}.000005.log");
    assert!(dir.join(&archived).exists(), "到达定点时刻后必须滚动归档");
    assert_eq!(
        std::fs::read_to_string(dir.join(&archived)).unwrap(),
        "legacy-before-roll"
    );
    // 同日再次到达 → 防重复滚动（不产生新归档）
    auto_roll_logs(&d, &opts, &now);
    let others = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&format!("{date}.")) && n.ends_with(".log") && n != archived
        })
        .count();
    assert_eq!(others, 0, "同日不得重复滚动");
    // 未到达时刻（早于 auto_roll_at）→ 不滚动
    reset_auto_roll_state();
    let opts_late = LogOptions {
        auto_roll_at: Some("23:59:59".into()),
        ..opts
    };
    let early = chrono::Local
        .with_ymd_and_hms(2026, 8, 12, 12, 0, 0)
        .single()
        .unwrap();
    std::fs::write(dir.join("2026-08-12.log"), "early").unwrap();
    auto_roll_logs(&d, &opts_late, &early);
    assert!(dir.join("2026-08-12.log").exists());
    assert!(!dir.join("2026-08-12.120000.log").exists());
    let _ = std::fs::remove_dir_all(&dir);
    reset_auto_roll_state();
}

#[test]
fn run_hook_redirects_stdout_to_file() {
    let dir = unique_temp_dir("hookredir");
    let d = dir.to_string_lossy().to_string();
    let out_file = dir.join("hook-out.log");
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    run_hook(
        Some("echo REDIRECTED-OUTPUT"),
        "prestart",
        5000,
        d.clone(),
        None,
        &opts,
        Some(out_file.to_str().unwrap()),
        None,
    );
    // 独立文件收到原始输出
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(
        content.contains("REDIRECTED-OUTPUT"),
        "重定向文件必须含钩子输出"
    );
    // 宿主日志不再有 hook 通道条目（输出已重定向；仅 host 通道的 executing/completed 保留）
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let host_log = std::fs::read_to_string(dir.join(format!("{date}.log"))).unwrap();
    assert!(
        !host_log.contains("[hook]"),
        "重定向后宿主日志不应再有 hook 通道输出"
    );
    assert!(host_log.contains("Hook [prestart] executing"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_core_threads_one_disables_chunking() {
    // 服务器支持 Range 且文件 >1MiB，但 threads=1 → 禁用分块，仅 HEAD+GET 两请求
    let data = vec![0x5Au8; 2 * 1024 * 1024];
    let d2 = data.clone();
    let (addr, stop, count) = spawn_http_server(move |method, _lines| {
        if method == "HEAD" {
            (
                "200 OK".to_string(),
                vec![
                    ("Accept-Ranges".into(), "bytes".into()),
                    ("Content-Length".into(), d2.len().to_string()),
                ],
                vec![],
            )
        } else {
            (
                "200 OK".to_string(),
                vec![("Content-Length".into(), d2.len().to_string())],
                d2.clone(),
            )
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-t1.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        1,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        count.load(Ordering::Relaxed),
        2,
        "threads=1 必须走单线程（仅 HEAD+GET）"
    );
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn process_cpu_sample_is_monotonic() {
    let pid = std::process::id();
    let first = process_cpu_100ns(pid).expect("首次采样应成功");
    thread::sleep(Duration::from_millis(150));
    let second = process_cpu_100ns(pid).expect("二次采样应成功");
    assert!(
        second >= first,
        "CPU 时间采样必须单调不减: {first} -> {second}"
    );
}

#[test]
fn download_stage_is_case_insensitive() {
    let cfg: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\ndownload_stage = \"After_Start\"\n").unwrap();
    assert!(download_stage_is(&cfg, "after_start"));
    assert!(!download_stage_is(&cfg, "before_start"));
}

#[test]
fn failure_action_chain_filters_all_invalid() {
    // 全非法动作 → 过滤为空序列（tick 遇到空序列安全降级为停止）
    let cfg: ServiceConfig = toml::from_str(concat!(
        "service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n",
        "[[failure_actions]]\naction = \"explode\"\n",
        "[[failure_actions]]\naction = \"hack\"\n",
    )).unwrap();
    assert!(failure_action_chain(&cfg).is_empty());
}

#[test]
fn split_credential_bruteforce_no_panic() {
    // split_credential 已随 SSPI 迁移至插件; 此处验证宿主下载条目的 userinfo 不会因畸形输入 panic
    for input in [
        "",
        "\\",
        "\\\\",
        "a\\",
        "\\b",
        "domain\\",
        "a\\b\\c",
        " ",
        "\\\u{4e2d}\\\\",
    ] {
        let _ = redact_url(&format!("http://{input}@host/x"));
    }
}

// ==================== WinSW 对齐补全: 启动参数/日志文件名/SDDL/preshutdown/runaway 启动清理 ====================

#[test]
fn build_child_command_injects_env_and_passes_args() {
    use std::collections::HashMap;
    let mut env = HashMap::new();
    env.insert("OSMIUM_TEST_VAR".to_string(), "hello-env".to_string());
    let mut cmd = build_child_command(
        "cmd.exe",
        Some("/c echo %OSMIUM_TEST_VAR%"),
        ".",
        Some(&env),
        ".",
        true,
        true,
        true,
        None,
    );
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let _ = child.wait();
    assert!(out.contains("hello-env"), "env 注入未生效: {}", out);
    // 参数为空串时不追加 raw_arg（避免裸 /c 报错）；stdin 置 null 防 cmd 挂起等待输入
    let mut cmd2 = build_child_command("cmd.exe", Some(""), ".", None, ".", true, true, true, None);
    cmd2.stdin(std::process::Stdio::null());
    let mut child2 = cmd2.spawn().unwrap();
    assert!(child2.id() > 0);
    let _ = child2.wait();
}

#[test]
fn sspi_download_missing_plugin_fails_clearly() {
    // download_auth=sspi 经 osmium-kit-sspi 插件完成: 插件缺失时必须启动失败并给出明确原因
    // （禁止静默降级为无认证下载，防凭据/完整性保护被静默关闭）
    let dir = unique_temp_dir("sspirej");
    let cfg_path = dir.join("svc.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "service_name = \"sspi-rej\"\n\
         service_display_name = \"SSPI Reject\"\n\
         service_description = \"x\"\n\
         service_executable_path = '{}'\n\
         download_url = \"https://x/a.exe\"\n\
         download_to = \"a.exe\"\n\
         download_auth = \"sspi\"\n",
            std::env::current_exe().unwrap().display()
        ),
    )
    .unwrap();
    let mut host = crate::service_host::ServiceHost::new();
    assert!(
        !host.on_start_from(&cfg_path),
        "sspi 插件缺失时启动必须失败"
    );
    // 日志必须留下插件调用失败原因（而非静默无认证下载）
    let logs_dir = dir.join("logs");
    let mut log_text = String::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for e in entries.flatten() {
            if let Ok(t) = std::fs::read_to_string(e.path()) {
                log_text.push_str(&t);
            }
        }
    }
    assert!(
        log_text.contains("sspi"),
        "日志应含 sspi 失败详情: {log_text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_parses_plugins_array() {
    // 生命周期插件调用配置: kit/phase/payload/fail_on_error 全字段解析
    let dir = unique_temp_dir("plcfg");
    let toml = r#"
service_name = "pl-svc"
service_display_name = "PL"
service_description = "d"
service_executable_path = 'C:\app.exe'
[[plugins]]
kit = "backup"
phase = "start_after"
payload = { mode = "full", count = 3 }
fail_on_error = true
[[plugins]]
kit = "cleanup"
phase = "stop"
"#;
    let p = dir.join("svc.toml");
    std::fs::write(&p, toml).unwrap();
    let cfg = load_config(&p);
    let plugins = cfg.plugins.unwrap();
    assert_eq!(plugins.len(), 2);
    assert_eq!(plugins[0].kit, "backup");
    assert_eq!(plugins[0].phase, "start_after");
    assert!(plugins[0].fail_on_error);
    let payload = plugins[0].payload.as_object().expect("payload 应为对象");
    assert_eq!(payload["mode"].as_str(), Some("full"));
    assert_eq!(payload["count"].as_i64(), Some(3));
    assert_eq!(plugins[1].kit, "cleanup");
    assert_eq!(plugins[1].phase, "stop");
    assert!(!plugins[1].fail_on_error, "缺省 fail_on_error=false");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_parses_builtin_alert_fields() {
    // 内置告警通道全字段解析: notify_url/notify_format + smtp_* + syslog_*
    let dir = unique_temp_dir("alertcfg");
    let toml = r#"
service_name = "alert-svc"
service_display_name = "AL"
service_description = "d"
service_executable_path = 'C:\app.exe'
notify_url = "https://hooks.example.com/osmium"
notify_format = "teams"
smtp_host = "mail.example.com:25"
smtp_from = "alerts@example.com"
smtp_to = "ops@example.com"
smtp_subject = "[Osmium] crashed"
smtp_username = "smtp-user"
smtp_password = "smtp-pass"
syslog_host = "192.168.1.10:514"
syslog_facility = 3
syslog_severity = 2
syslog_tag = "MyService"
"#;
    let p = dir.join("svc.toml");
    std::fs::write(&p, toml).unwrap();
    let cfg = load_config(&p);
    assert_eq!(
        cfg.notify_url.as_deref(),
        Some("https://hooks.example.com/osmium")
    );
    assert_eq!(cfg.notify_format.as_deref(), Some("teams"));
    assert_eq!(cfg.smtp_host.as_deref(), Some("mail.example.com:25"));
    assert_eq!(cfg.smtp_from.as_deref(), Some("alerts@example.com"));
    assert_eq!(cfg.smtp_to.as_deref(), Some("ops@example.com"));
    assert_eq!(cfg.smtp_subject.as_deref(), Some("[Osmium] crashed"));
    assert_eq!(cfg.smtp_username.as_deref(), Some("smtp-user"));
    assert_eq!(cfg.smtp_password.as_deref(), Some("smtp-pass"));
    assert_eq!(cfg.syslog_host.as_deref(), Some("192.168.1.10:514"));
    assert_eq!(cfg.syslog_facility, Some(3));
    assert_eq!(cfg.syslog_severity, Some(2));
    assert_eq!(cfg.syslog_tag.as_deref(), Some("MyService"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn builtin_alert_plugins_builds_crash_calls() {
    // 内置告警通道 → crash 插件调用: 全配置 3 条 / smtp 缺 from 跳过 / 空配置 None
    let full: ServiceConfig = toml::from_str(concat!(
        "service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n",
        "notify_url = \"https://hooks.example.com/osmium\"\n",
        "notify_format = \"feishu\"\n",
        "smtp_host = \"mail.example.com:25\"\n",
        "smtp_from = \"alerts@example.com\"\n",
        "smtp_to = \"ops@example.com\"\n",
        "smtp_subject = \"[Osmium] crashed\"\n",
        "smtp_username = \"u\"\n",
        "smtp_password = \"p\"\n",
        "syslog_host = \"192.168.1.10:514\"\n",
        "syslog_facility = 3\n",
        "syslog_severity = 2\n",
        "syslog_tag = \"MyService\"\n",
    ))
    .unwrap();
    let calls = crate::service_host::builtin_alert_plugins(&full).unwrap();
    assert_eq!(calls.len(), 3);
    let notify = calls.iter().find(|c| c.kit == "notify").unwrap();
    assert_eq!(notify.phase, "crash");
    let np = notify.payload.as_object().unwrap();
    assert_eq!(np["url"].as_str(), Some("https://hooks.example.com/osmium"));
    assert_eq!(np["format"].as_str(), Some("feishu"));
    let smtp = calls.iter().find(|c| c.kit == "smtp").unwrap();
    let sp = smtp.payload.as_object().unwrap();
    assert_eq!(sp["host"].as_str(), Some("mail.example.com:25"));
    assert_eq!(sp["from"].as_str(), Some("alerts@example.com"));
    assert_eq!(sp["to_addr"].as_str(), Some("ops@example.com"));
    assert_eq!(sp["subject"].as_str(), Some("[Osmium] crashed"));
    assert_eq!(sp["username"].as_str(), Some("u"));
    assert_eq!(sp["password"].as_str(), Some("p"));
    let syslog = calls.iter().find(|c| c.kit == "syslog").unwrap();
    let yp = syslog.payload.as_object().unwrap();
    assert_eq!(yp["syslog_host"].as_str(), Some("192.168.1.10:514"));
    assert_eq!(yp["facility"].as_u64(), Some(3));
    assert_eq!(yp["severity"].as_u64(), Some(2));
    assert_eq!(yp["tag"].as_str(), Some("MyService"));
    // smtp 缺 from → 仅 notify/syslog 两条
    let no_from: ServiceConfig = toml::from_str(concat!(
        "service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n",
        "notify_url = \"https://hooks.example.com/osmium\"\n",
        "smtp_host = \"mail.example.com:25\"\n",
        "smtp_to = \"ops@example.com\"\n",
        "syslog_host = \"192.168.1.10:514\"\n",
    ))
    .unwrap();
    let calls2 = crate::service_host::builtin_alert_plugins(&no_from).unwrap();
    assert_eq!(calls2.len(), 2);
    assert!(calls2.iter().all(|c| c.kit != "smtp"));
    // 空配置 → None（不生成调用）
    let plain: ServiceConfig = toml::from_str("service_name = \"s\"\nservice_display_name = \"s\"\nservice_description = \"s\"\nservice_executable_path = \"C:\\\\a.exe\"\n").unwrap();
    assert!(crate::service_host::builtin_alert_plugins(&plain).is_none());
}

/// 构造带 plugins 配置的宿主并启动（exe 用 cmd.exe /c exit 快速退出，避免拉起测试 harness）
fn start_host_with_plugins(plugins_toml: &str) -> (bool, String) {
    let dir = unique_temp_dir("plhost");
    let cfg_path = dir.join("svc.toml");
    std::fs::write(
        &cfg_path,
        format!(
            "service_name = \"pl-host\"\n\
         service_display_name = \"PL\"\n\
         service_description = \"d\"\n\
         service_executable_path = 'C:\\Windows\\System32\\cmd.exe'\n\
         service_executable_args = \"/c exit\"\n\
         {plugins_toml}"
        ),
    )
    .unwrap();
    let mut host = crate::service_host::ServiceHost::new();
    let ok = host.on_start_from(&cfg_path);
    // 收集日志文本供断言
    let logs_dir = dir.join("logs");
    let mut log_text = String::new();
    if let Ok(entries) = std::fs::read_dir(&logs_dir) {
        for e in entries.flatten() {
            if let Ok(t) = std::fs::read_to_string(e.path()) {
                log_text.push_str(&t);
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    (ok, log_text)
}

#[test]
fn plugin_call_failure_non_fatal_does_not_block_start() {
    // 插件缺失 + fail_on_error=false: 启动不阻断，日志留下明确告警
    let (ok, log_text) = start_host_with_plugins(
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"start_before\"\n",
    );
    assert!(ok, "fail_on_error=false 时插件失败不得阻断启动");
    assert!(
        log_text.contains("nonexistent-kit"),
        "日志应记录失败的插件名: {log_text}"
    );
    assert!(
        log_text.contains("non-fatal"),
        "应标记为 non-fatal: {log_text}"
    );
}

#[test]
fn plugin_call_failure_fatal_blocks_start() {
    // 插件缺失 + fail_on_error=true（start 阶段）: 阻断启动
    let (ok, log_text) = start_host_with_plugins(
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"start_before\"\nfail_on_error = true\n",
    );
    assert!(!ok, "fail_on_error=true 时插件失败必须阻断启动");
    assert!(log_text.contains("failed"), "日志应含失败详情: {log_text}");
}

#[test]
fn plugin_call_other_phase_does_not_block_start() {
    // stop 阶段配置的失败插件不得影响启动（phase 过滤生效）
    let (ok, _log) = start_host_with_plugins(
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"stop_before\"\nfail_on_error = true\n",
    );
    assert!(ok, "非 start 阶段的插件配置不得阻断启动");
}

#[test]
fn download_auth_from_entry_maps_modes() {
    // 经 download_entries 归一化验证条目级认证映射（含配置级字段回退）
    let c = ServiceConfig {
        download_auth: Some("basic".into()),
        download_username: Some("DOMAIN\\u".into()),
        download_password: Some("p".into()),
        ..Default::default()
    };
    let mut e = download_entries(&c).remove(0);
    assert!(matches!(
        download_auth_from_entry(&e),
        DownloadAuth::Basic("DOMAIN\\u", "p")
    ));
    e.auth = Some("Basic".into());
    e.password = None; // 清空密码（用户名保留）→ Basic("DOMAIN\u", "")
    assert!(matches!(
        download_auth_from_entry(&e),
        DownloadAuth::Basic("DOMAIN\\u", "")
    ));
    e.auth = None;
    assert!(matches!(download_auth_from_entry(&e), DownloadAuth::None));
    e.auth = Some("kerberos".into()); // 未知方式 → 无认证
    assert!(matches!(download_auth_from_entry(&e), DownloadAuth::None));
}

#[test]
fn download_entries_normalizes_array_and_legacy() {
    // 数组模式: 条目缺省回退配置级值
    let mut c = ServiceConfig {
        download_auth: Some("basic".into()),
        download_username: Some("u".into()),
        download_password: Some("p".into()),
        download_fail_on_error: false,
        download_unzip: true,
        downloads: Some(vec![
            DownloadConfig {
                from: "http://x/a".into(),
                to: "a.bin".into(),
                ..Default::default()
            },
            DownloadConfig {
                from: "http://x/b".into(),
                to: "b.bin".into(),
                sha256: Some("abc".into()),
                stage: Some("after_start".into()),
                ..Default::default()
            },
        ]),
        ..Default::default()
    };
    let entries = download_entries(&c);
    assert_eq!(entries.len(), 2);
    // 条目 1: 缺省回退配置级 auth/username/password/fail_on_error/unzip
    assert_eq!(entries[0].auth.as_deref(), Some("basic"));
    assert_eq!(entries[0].username.as_deref(), Some("u"));
    assert_eq!(entries[0].password.as_deref(), Some("p"));
    assert_eq!(entries[0].fail_on_error, Some(false));
    assert_eq!(entries[0].unzip, Some(true));
    // 条目 2: 显式覆盖保留
    assert_eq!(entries[1].sha256.as_deref(), Some("abc"));
    assert_eq!(entries[1].stage.as_deref(), Some("after_start"));
    // 条目级阶段解析: 缺省 before_start，回退配置级 download_stage
    assert_eq!(download_entry_stage(&entries[0], &c), "before_start");
    assert_eq!(download_entry_stage(&entries[1], &c), "after_start");
    c.download_stage = Some("after_stop".into());
    assert_eq!(download_entry_stage(&entries[0], &c), "after_stop");

    // 旧单条模式: 构造一条合并条目
    let legacy = ServiceConfig {
        download_url: Some("http://x/exe".into()),
        download_sha256: Some("deadbeef".into()),
        download_fail_on_error: true,
        ..Default::default()
    };
    let le = download_entries(&legacy);
    assert_eq!(le.len(), 1);
    assert_eq!(le[0].from, "http://x/exe");
    assert_eq!(le[0].sha256.as_deref(), Some("deadbeef"));
    assert_eq!(le[0].fail_on_error, Some(true));
}

#[test]
fn load_config_parses_downloads_array_and_log_scm_fields() {
    let dir = unique_temp_dir("dlarr");
    let toml = r#"
service_name = "dl-arr"
service_display_name = "DL"
service_description = "d"
service_executable_path = 'C:\app.exe'
downloads = [
  { from = "http://x/a.bin", to = "a.bin", fail_on_error = true },
  { from = "https://x/b.bin", to = "b.bin", auth = "basic" },
]
download_unsecure_auth = true
log_mode = "roll-by-size-time"
log_roll_period_days = 3
log_zip_date_format = "%Y%m%d"
scm_wait_hint_ms = 9000
scm_sleep_time_ms = 100
"#;
    let p = dir.join("svc.toml");
    std::fs::write(&p, toml).unwrap();
    let cfg = load_config(&p);
    let dl = cfg.downloads.unwrap();
    assert_eq!(dl.len(), 2);
    assert_eq!(dl[0].to, "a.bin");
    assert_eq!(dl[0].fail_on_error, Some(true));
    assert_eq!(dl[1].auth.as_deref(), Some("basic"));
    assert!(cfg.download_unsecure_auth);
    assert_eq!(cfg.log_mode.as_deref(), Some("roll-by-size-time"));
    assert_eq!(cfg.log_roll_period_days, 3);
    assert_eq!(cfg.log_zip_date_format.as_deref(), Some("%Y%m%d"));
    assert_eq!(cfg.scm_wait_hint_ms, 9000);
    assert_eq!(cfg.scm_sleep_time_ms, 100);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_core_304_skips_and_keeps_target() {
    let (addr, stop, _) =
        spawn_http_server(|_, _| ("304 Not Modified".into(), Vec::new(), Vec::new()));
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-304-test.tmp");
    let _ = std::fs::remove_file(&tmp);
    // 服务器对 If-Modified-Since 回 304 → download_core 删除 tmp 并视为成功（保留原目标文件）
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert!(!tmp.exists(), "304 时应删除 tmp（保留原目标）");
}

#[test]
fn download_core_sends_if_modified_since_header() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let s2 = seen.clone();
    let (addr, stop, _) = spawn_http_server(move |_, lines| {
        for l in lines {
            if l.to_ascii_lowercase().starts_with("if-modified-since:") {
                s2.lock().unwrap().push(l.clone());
            }
        }
        ("304 Not Modified".into(), Vec::new(), Vec::new())
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-304-header.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        16,
        Some("Mon, 01 Jan 2024 00:00:00 GMT".into()),
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    let got = seen.lock().unwrap();
    assert_eq!(got.len(), 1, "必须发送 If-Modified-Since 头: {:?}", got);
    assert!(got[0].contains("01 Jan 2024"), "头内容不符: {}", got[0]);
}

#[test]
fn http_date_from_mtime_formats_rfc1123() {
    // 目标已存在时从 mtime 生成 RFC 1123 GMT 日期（If-Modified-Since 头）
    let file = std::env::temp_dir().join(format!("osmium_httpdate_{}.txt", std::process::id()));
    std::fs::write(&file, "x").unwrap();
    let d = http_date_from_mtime(file.to_str().unwrap()).unwrap();
    assert_eq!(d.len(), 29, "RFC 1123 日期长度应固定: {d}");
    assert!(d.ends_with(" GMT"), "应以 GMT 结尾: {d}");
    // 缺失文件返回 None（不发送条件请求）
    assert!(http_date_from_mtime("Z:\\nonexistent_osmium_file").is_none());
    let _ = std::fs::remove_file(&file);
}

#[test]
fn warn_if_insecure_download_unsecure_auth_flag() {
    // basic + http（有 sha）: 未显式放行 → 拒绝
    let mut cfg = ServiceConfig {
        downloads: Some(vec![DownloadConfig {
            from: "http://x/a.bin".into(),
            to: "a.bin".into(),
            auth: Some("basic".into()),
            sha256: Some("abc".into()),
            ..Default::default()
        }]),
        ..Default::default()
    };
    let e = warn_if_insecure_download(&cfg).unwrap_err();
    assert!(e.contains("unsecure_auth"), "{e}");
    // 条目级 unsecure_auth=true → 放行
    cfg.downloads.as_mut().unwrap()[0].unsecure_auth = Some(true);
    assert!(warn_if_insecure_download(&cfg).is_ok());
    // 配置级 download_unsecure_auth 回退 → 放行
    cfg.downloads.as_mut().unwrap()[0].unsecure_auth = None;
    cfg.download_unsecure_auth = true;
    assert!(warn_if_insecure_download(&cfg).is_ok());
}

#[test]
fn apply_log_mode_maps_winsw_modes() {
    let mut enabled = true;
    let base = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 5,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("none"), &mut enabled, &mut o);
    assert!(!enabled);
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("reset"), &mut enabled, &mut o);
    assert!(o.reset);
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("roll"), &mut enabled, &mut o);
    assert!(o.roll_at_start);
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("roll-by-size"), &mut enabled, &mut o);
    assert_eq!(o.max_size_mb, 10);
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("roll-by-time"), &mut enabled, &mut o);
    assert_eq!(o.roll_period_days, 1);
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("roll-by-size-time"), &mut enabled, &mut o);
    assert_eq!(o.max_size_mb, 10);
    assert_eq!(o.roll_period_days, 1);
    // 未知/缺省 → 不改动
    let mut o = LogOptions { ..base.clone() };
    apply_log_mode(Some("weird"), &mut enabled, &mut o);
    assert_eq!(o.max_size_mb, 0);
    assert_eq!(o.roll_period_days, 0);
    apply_log_mode(None, &mut enabled, &mut o);
    assert_eq!(o.max_size_mb, 0);
}

#[test]
fn roll_logs_to_old_renames_and_overwrites() {
    let dir = unique_temp_dir("rollold");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: true,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: true,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    std::fs::write(dir.join(format!("{date}.log")), "main").unwrap();
    std::fs::write(dir.join(format!("{date}.err.log")), "err").unwrap();
    roll_logs_to_old(&d, &opts);
    assert_eq!(
        std::fs::read_to_string(dir.join(format!("{date}.log.old"))).unwrap(),
        "main"
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(format!("{date}.err.log.old"))).unwrap(),
        "err"
    );
    // 二次启动 → 覆盖旧 .old
    std::fs::write(dir.join(format!("{date}.log")), "main2").unwrap();
    roll_logs_to_old(&d, &opts);
    assert_eq!(
        std::fs::read_to_string(dir.join(format!("{date}.log.old"))).unwrap(),
        "main2"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roll_by_time_if_due_rolls_stale_log() {
    let dir = unique_temp_dir("rolltime");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 1,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    let now = chrono::Local
        .with_ymd_and_hms(2026, 8, 11, 12, 0, 0)
        .single()
        .unwrap();
    let path = dir.join("2026-08-11.log");
    std::fs::write(&path, "stale").unwrap();
    // 文件 mtime 为当前 → 未到期不滚动
    roll_by_time_if_due(&d, &opts, &now);
    assert!(path.exists());
    // mtime 改到 3 天前 → 到期滚动为 {date}.{HHmmss}.log
    let old: std::time::SystemTime = (now - chrono::Duration::days(3)).into();
    std::fs::File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_modified(old)
        .unwrap();
    roll_by_time_if_due(&d, &opts, &now);
    assert!(!path.exists());
    assert!(dir.join("2026-08-11.120000.log").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zip_backup_file_uses_date_format() {
    let dir = unique_temp_dir("zipfmt");
    let log = dir.join("2026-08-11.log");
    std::fs::write(&log, "data").unwrap();
    assert!(zip_backup_file(&log, "%Y%m%d"));
    let expected = format!(
        "2026-08-11.log.{}.zip",
        chrono::Local::now().format("%Y%m%d")
    );
    assert!(
        dir.join(&expected).exists(),
        "期望 zip 归档名: {}",
        expected
    );
    // 空格式 → 保持 {file}.zip
    assert!(zip_backup_file(&log, ""));
    assert!(dir.join("2026-08-11.log.zip").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn zip_backup_file_refuses_reparse_target() {
    // S2 回归: 归档目标 {file}.zip 自身是 junction/symlink 时拒绝（防日志归档写穿到系统文件）
    let target = unique_temp_dir("zipjt-target");
    std::fs::create_dir_all(&target).unwrap();
    let src_dir = unique_temp_dir("zipjt-src");
    std::fs::create_dir_all(&src_dir).unwrap();
    let src = src_dir.join("2026-08-11.log");
    std::fs::write(&src, "data").unwrap();
    // 归档目标 = src_dir\2026-08-11.log.zip → 把它做成指向 target 的 symlink
    let zip_target = src_dir.join("2026-08-11.log.zip");
    let ok = std::process::Command::new("cmd.exe")
        .args([
            "/c",
            "mklink",
            &zip_target.to_string_lossy(),
            &target.to_string_lossy(),
        ])
        .creation_flags(0x08000000)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if ok {
        assert!(
            !zip_backup_file(&src, ""),
            "zip 目标为 reparse point 时必须拒绝"
        );
        assert!(
            std::fs::read_dir(&target)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
            "归档不得写穿 symlink 到目标目录"
        );
    }
    let _ = std::fs::remove_dir_all(&target);
    let _ = std::fs::remove_dir_all(&src_dir);
}

#[test]
fn scm_param_setters_store_and_clamp() {
    set_scm_wait_hint_ms(5000);
    assert_eq!(scm_wait_hint_ms(), 5000);
    set_scm_wait_hint_ms(100); // < 1000 → 钳到 1000
    assert_eq!(scm_wait_hint_ms(), 1000);
    set_scm_sleep_time_ms(250);
    assert_eq!(scm_sleep_time_ms(), 250);
    set_scm_sleep_time_ms(10); // < 50 → 钳到 50
    assert_eq!(scm_sleep_time_ms(), 50);
    // 还原默认（避免污染其他测试）
    set_scm_wait_hint_ms(3_600_000);
    set_scm_sleep_time_ms(500);
}

#[test]
fn run_stop_command_completes_and_kills_on_timeout() {
    let dir = unique_temp_dir("stopcmd");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    // 快速退出的停止命令 → 正常结束
    run_stop_command("cmd.exe", "/c exit 0", 4242, 5, d.clone(), &opts);
    // 常驻命令 → 超时强杀（返回后进程必须已死）
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");
    let pid = child.id();
    let pid_file = dir.join("sleep.pid");
    std::fs::write(&pid_file, pid.to_string()).unwrap();
    run_stop_command(
        "powershell.exe",
        "-NoProfile -Command \"Start-Sleep -Seconds 60\"",
        4242,
        1,
        d.clone(),
        &opts,
    );
    // 超时强杀路径已覆盖（run_stop_command 内部 terminate_pid_tree）
    let _ = child.kill();
    let _ = child.wait();
    // 日志断言: 快速命令有 "exited with code"，常驻命令有 "timed out"
    let now = chrono::Local::now();
    let log = dir.join(current_log_name(&opts, "host", &now));
    let content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        content.contains("exited with code 0"),
        "日志缺失快速退出记录: {}",
        content
    );
    assert!(
        content.contains("timed out after 1s, killing"),
        "日志缺失超时强杀记录: {}",
        content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_stop_command_injects_child_pid() {
    let dir = unique_temp_dir("stoppid");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    // %PID% 占位符与 WINSGF_CHILD_PID 环境变量同时注入（echo 输出进日志可断言）
    run_stop_command(
        "cmd.exe",
        "/c echo pid=%PID% env=%WINSGF_CHILD_PID%",
        4242,
        5,
        d.clone(),
        &opts,
    );
    let now = chrono::Local::now();
    let log = dir.join(current_log_name(&opts, "host", &now));
    let content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(
        content.contains("pid=4242 env=4242"),
        "日志缺失 PID 注入输出: {}",
        content
    );
    assert!(
        content.contains("Stop executable: cmd.exe /c echo pid=4242 env=%WINSGF_CHILD_PID%"),
        "日志应展示已展开 %PID% 的停止命令: {}",
        content
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_stop_pid_placeholder_cases() {
    assert_eq!(expand_stop_pid("--pid %PID%", 123), "--pid 123");
    assert_eq!(expand_stop_pid("--pid %pid%", 123), "--pid 123");
    assert_eq!(expand_stop_pid("%PID%", 123), "123");
    assert_eq!(expand_stop_pid("no placeholder", 123), "no placeholder");
    // 未闭合/其他变量原样保留
    assert_eq!(expand_stop_pid("a%PID", 123), "a%PID");
    assert_eq!(expand_stop_pid("%BASE%", 123), "%BASE%");
    assert_eq!(expand_stop_pid("中文%PID%尾部", 7), "中文7尾部");
    // 与配置全局展开串行: %PID% 先被 expand_env_value 保留，再由 expand_stop_pid 替换
    assert_eq!(
        expand_stop_pid(&expand_env_value("%PID%", "C:\\base"), 456),
        "456"
    );
}

#[test]
fn runaway_exceeded_decides_limits() {
    assert!(runaway_exceeded(Some(200), Some(100), None, None));
    assert!(runaway_exceeded(None, None, Some(90.0), Some(50.0)));
    assert!(!runaway_exceeded(
        Some(50),
        Some(100),
        Some(10.0),
        Some(50.0)
    ));
    assert!(!runaway_exceeded(None, None, None, None));
    assert!(!runaway_exceeded(None, Some(100), None, None)); // 采样缺失不触发
}

#[test]
fn runaway_cleanup_pid_file_terminates_leftover() {
    let dir = unique_temp_dir("runawaypid");
    // 无 pid 文件 → 无操作
    assert_eq!(
        runaway_cleanup_pid_file(
            &dir.join("missing.txt").to_string_lossy(),
            5000,
            false,
            None
        )
        .unwrap(),
        None
    );
    // 非法内容 → 告警
    std::fs::write(dir.join("bad.txt"), "not-a-pid").unwrap();
    assert!(
        runaway_cleanup_pid_file(&dir.join("bad.txt").to_string_lossy(), 5000, false, None)
            .is_err()
    );
    // 指向真实常驻进程（带匹配的服务标识）→ 清理整棵树
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .env("WINSGF_SERVICE_ID", "test-svc")
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");
    let pid = child.id();
    let pid_file = dir.join("pid.txt");
    std::fs::write(&pid_file, pid.to_string()).unwrap();
    assert_eq!(
        runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 5000, false, Some("test-svc"))
            .unwrap(),
        Some(pid)
    );
    assert!(!process_alive(pid), "残留进程应已被终止");
    let _ = child.kill();
    let _ = child.wait();
    // 已退出/0 → 无操作
    std::fs::write(&pid_file, "0").unwrap();
    assert_eq!(
        runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 5000, false, None).unwrap(),
        None
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn runaway_cleanup_pid_file_skips_foreign_pid() {
    let dir = unique_temp_dir("runawayforeign");
    // 常驻进程但服务标识不匹配（PID 被复用/无关进程）→ 跳过不清理（对齐 WinSW #237）
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");
    let pid = child.id();
    let pid_file = dir.join("foreign.txt");
    std::fs::write(&pid_file, pid.to_string()).unwrap();
    let err = runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 500, false, Some("my-svc"))
        .unwrap_err();
    assert!(err.contains("WINSGF_SERVICE_ID"), "{err}");
    assert!(process_alive(pid), "标识不匹配的进程不得被误杀");
    let _ = child.kill();
    let _ = child.wait();
    // 匹配标识（process_env_var 读取）→ 正常清理
    let mut child2 = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 60"])
        .env("WINSGF_SERVICE_ID", "my-svc")
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");
    let pid2 = child2.id();
    let pid_file2 = dir.join("mine.txt");
    std::fs::write(&pid_file2, pid2.to_string()).unwrap();
    assert_eq!(
        runaway_cleanup_pid_file(&pid_file2.to_string_lossy(), 500, false, Some("my-svc")).unwrap(),
        Some(pid2)
    );
    let _ = child2.kill();
    let _ = child2.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn kill_service_processes_matches_env_var_and_kills_tree() {
    // 两层进程树（powershell 父 + ping 孙，均继承 WINSGF_SERVICE_ID）: kill 按标识匹配并杀整树
    let svc = "osmium-kill-test-svc";
    let script = "Start-Process -FilePath 'C:\\Windows\\System32\\ping.exe' -ArgumentList '-t','127.0.0.1' -WindowStyle Hidden; Start-Sleep -Seconds 30";
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", script])
        .env("WINSGF_SERVICE_ID", svc)
        .creation_flags(0x08000000)
        .spawn()
        .expect("spawn powershell");
    let pid = child.id();
    // 等孙进程起来（Start-Process 异步）
    thread::sleep(Duration::from_millis(1500));
    let killed = crate::service_host::kill_service_processes(svc).expect("kill should succeed");
    assert!(killed >= 1, "should kill at least the parent process");
    // 父进程已被终止（wait 立即返回）
    let status = child.wait().expect("wait should succeed");
    assert!(!status.success(), "parent process must be terminated");
    assert!(!process_alive(pid), "parent must be dead");
}

#[test]
fn kill_service_processes_unknown_service_returns_zero() {
    // 不存在/无运行进程的服务: 枚举匹配不到 → Ok(0)，不动任何进程
    let killed = crate::service_host::kill_service_processes("osmium-no-such-svc-xyz")
        .expect("unknown service must not error");
    assert_eq!(killed, 0);
}

#[test]
fn process_env_var_reads_child_environment() {
    // PEB 环境块读取: 读子进程注入的变量与真实 PATH（对齐 WinSW 防误杀校验的数据来源）
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
        .env("WINSGF_SERVICE_ID", "peb-test-svc")
        .creation_flags(0x08000000)
        .spawn()
        .unwrap();
    let pid = child.id();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(
        process_env_var(pid, "WINSGF_SERVICE_ID").as_deref(),
        Some("peb-test-svc")
    );
    assert!(process_env_var(pid, "PATH").is_some());
    assert_eq!(process_env_var(pid, "OSMIUM_DOES_NOT_EXIST_XYZ"), None);
    assert_eq!(process_env_var(u32::MAX, "PATH"), None); // 不存在进程
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn build_child_command_injects_base_and_service_id() {
    use std::collections::HashMap;
    // 自动注入 BASE（部署目录）与 WINSGF_SERVICE_ID
    let mut cmd = build_child_command(
        "cmd.exe",
        Some("/c echo %BASE%+%WINSGF_SERVICE_ID%"),
        ".",
        None,
        "C:\\deploy",
        true,
        true,
        true,
        Some("svc-1"),
    );
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let _ = child.wait();
    assert!(
        out.contains("C:\\deploy+svc-1"),
        "BASE/WINSGF_SERVICE_ID 注入未生效: {}",
        out
    );
    // 用户显式配置 BASE（大小写不敏感）→ 以用户为准
    let mut env = HashMap::new();
    env.insert("base".to_string(), "user-base".to_string());
    let mut cmd2 = build_child_command(
        "cmd.exe",
        Some("/c echo %BASE%"),
        ".",
        Some(&env),
        "C:\\deploy",
        true,
        true,
        true,
        None,
    );
    let mut child2 = cmd2.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out2 = String::new();
    child2
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out2)
        .unwrap();
    let _ = child2.wait();
    assert!(
        out2.contains("user-base"),
        "用户 env 应覆盖自动 BASE: {}",
        out2
    );
    assert!(!out2.contains("C:\\deploy"), "不得注入默认 BASE: {}", out2);
}

#[test]
fn process_alive_detects_running_and_missing() {
    assert!(process_alive(std::process::id()));
    assert!(!process_alive(0));
    assert!(!process_alive(u32::MAX));
}

#[test]
fn current_log_name_custom_filenames_override() {
    let now = chrono::Local::now();
    let opts = LogOptions {
        split_out_err: true,
        max_size_mb: 0,
        backup_count: 0,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: "app.out.log".into(),
        err_filename: "app.err.log".into(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: Vec::new(),
    };
    assert_eq!(current_log_name(&opts, "host", &now), "app.out.log");
    assert_eq!(current_log_name(&opts, "out", &now), "app.out.log");
    assert_eq!(current_log_name(&opts, "err", &now), "app.err.log");
    // 未分流时 err 通道仍走主日志名
    let opts_merged = LogOptions {
        split_out_err: false,
        ..opts
    };
    assert_eq!(current_log_name(&opts_merged, "err", &now), "app.out.log");
}

#[test]
fn scm_status_params_honors_preshutdown_flag() {
    use windows::Win32::System::Services::{SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_RUNNING};
    set_preshutdown_enabled(true);
    let (controls, _) = scm_status_params(SERVICE_RUNNING.0);
    assert_ne!(
        controls & SERVICE_ACCEPT_PRESHUTDOWN,
        0,
        "preshutdown 开启时应上报接受码"
    );
    set_preshutdown_enabled(false);
    let (controls, _) = scm_status_params(SERVICE_RUNNING.0);
    assert_eq!(controls & SERVICE_ACCEPT_PRESHUTDOWN, 0);
}

#[test]
fn security_descriptor_from_sddl_parses_valid_and_rejects_bad() {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    let sd =
        security_descriptor_from_sddl("D:(A;;GA;;;SY)(A;;GA;;;BA)").expect("合法 SDDL 应解析成功");
    assert!(!sd.0.is_null());
    unsafe {
        let _ = LocalFree(Some(HLOCAL(sd.0)));
    }
    assert!(security_descriptor_from_sddl("not a valid sddl !!!").is_err());
}

#[test]
fn load_config_parses_new_winsw_fields() {
    let dir = unique_temp_dir("cfgnew");
    let f = dir.join("ok.toml");
    std::fs::write(
        &f,
        concat!(
            "service_name = \"s\"\n",
            "service_display_name = \"s\"\n",
            "service_description = \"s\"\n",
            "service_executable_path = \"C:\\\\a.exe\"\n",
            "service_executable_args = \"--normal\"\n",
            "start_arguments = \"--start\"\n",
            "security_descriptor = \"D:(A;;GA;;;SY)\"\n",
            "preshutdown = true\n",
            "log_out_filename = \"custom.out\"\n",
            "log_err_filename = \"custom.err\"\n",
            "runaway_pid_file = \"runaway.pid\"\n",
            "runaway_stop_timeout_ms = 3000\n",
            "runaway_stop_parent_first = true\n",
        ),
    )
    .unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.start_arguments.as_deref(), Some("--start"));
    assert_eq!(cfg.security_descriptor.as_deref(), Some("D:(A;;GA;;;SY)"));
    assert!(cfg.preshutdown);
    assert_eq!(cfg.log_out_filename.as_deref(), Some("custom.out"));
    assert_eq!(cfg.log_err_filename.as_deref(), Some("custom.err"));
    assert_eq!(cfg.runaway_pid_file.as_deref(), Some("runaway.pid"));
    assert_eq!(cfg.runaway_stop_timeout_ms, 3000);
    assert!(cfg.runaway_stop_parent_first);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn expand_config_expands_base_and_env_in_paths() {
    use crate::service_host::ServiceHost;
    let mut host = ServiceHost::new();
    host.deploy_dir = "C:\\base".to_string();
    let mut c = ServiceConfig {
        service_executable_path: "%BASE%\\app.exe".into(),
        service_executable_args: Some("--cfg %BASE%\\cfg.ini".into()),
        working_directory: Some("%BASE%\\work".into()),
        download_url: Some("http://x/%BASE%/file.bin".into()),
        download_to: Some("%BASE%\\target.bin".into()),
        log_dir: Some("%BASE%\\logs".into()),
        runaway_pid_file: Some("%BASE%\\svc.pid".into()),
        ..Default::default()
    };
    unsafe {
        std::env::set_var("OSMIUM_TEST_EXPAND", "hello");
    }
    c.stop_executable = Some("%OSMIUM_TEST_EXPAND%\\stop.exe".into());
    let e = host.expand_config(&c);
    assert_eq!(e.service_executable_path, "C:\\base\\app.exe");
    assert_eq!(
        e.service_executable_args.as_deref(),
        Some("--cfg C:\\base\\cfg.ini")
    );
    assert_eq!(e.working_directory.as_deref(), Some("C:\\base\\work"));
    assert_eq!(
        e.download_url.as_deref(),
        Some("http://x/C:\\base/file.bin")
    );
    assert_eq!(e.download_to.as_deref(), Some("C:\\base\\target.bin"));
    assert_eq!(e.log_dir.as_deref(), Some("C:\\base\\logs"));
    assert_eq!(e.runaway_pid_file.as_deref(), Some("C:\\base\\svc.pid"));
    assert_eq!(e.stop_executable.as_deref(), Some("hello\\stop.exe"));
    unsafe {
        std::env::remove_var("OSMIUM_TEST_EXPAND");
    }
}

// ==================== 配置热刷新（autoRefresh） ====================

/// 配置热刷新集成测试: 配置文件 mtime 变化后 tick 应检测并优雅重启子进程（PID 变化）。
/// 目标进程用 ping -n 30（约 30 秒自退），断言失败时也不会永久残留进程
#[test]
fn auto_refresh_restarts_child_on_config_change() {
    use crate::service_host::ServiceHost;
    let dir = unique_temp_dir("refresh");
    let config_path = dir.join("refresh.toml");
    let write_cfg = |args: &str| {
        std::fs::write(
            &config_path,
            format!(
                "service_name = \"refresh-test\"\n\
             service_display_name = \"refresh-test\"\n\
             service_description = \"refresh-test\"\n\
             service_executable_path = 'C:\\Windows\\System32\\ping.exe'\n\
             service_executable_args = \"{args}\"\n\
             auto_refresh = true\n"
            ),
        )
        .unwrap();
    };
    write_cfg("-n 30 127.0.0.1");
    let mut host = ServiceHost::new();
    assert!(host.on_start_from(&config_path), "宿主应启动成功");
    let pid1 = host.child.first().unwrap().id();
    assert!(process_alive(pid1), "子进程应运行中");

    // 修改配置（args 变化 → mtime 变化）→ 下一次 tick 应检测到并重启子进程
    write_cfg("-n 30 127.0.0.2");
    thread::sleep(Duration::from_millis(20)); // 文件系统 mtime 粒度兜底
    assert!(host.tick(), "tick 应返回 true（子进程仍在运行）");
    let pid2 = host.child.first().unwrap().id();
    assert_ne!(pid1, pid2, "配置变化后子进程应被重启（PID 变化）");

    // 清理: 终止并回收子进程（stop_child_process 私有，直接 kill）
    for mut c in std::mem::take(&mut host.child) {
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// 配置热刷新开关解析: auto_refresh 缺省 false、显式 true 生效
#[test]
fn load_config_parses_auto_refresh_flag() {
    let dir = unique_temp_dir("refresh_cfg");
    let f = dir.join("refresh.toml");
    std::fs::write(&f, "service_name = \"r\"\nservice_display_name = \"r\"\nservice_description = \"r\"\nservice_executable_path = 'C:\\x.exe'\nauto_refresh = true\n").unwrap();
    let cfg = load_config(&f);
    assert!(cfg.auto_refresh, "auto_refresh=true 应生效");
    std::fs::write(&f, "service_name = \"r\"\nservice_display_name = \"r\"\nservice_description = \"r\"\nservice_executable_path = 'C:\\x.exe'\n").unwrap();
    let cfg = load_config(&f);
    assert!(!cfg.auto_refresh, "auto_refresh 缺省应为 false");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 错误信息 & 日志写入（底层工具） ====================

#[test]
fn panic_msg_extracts_string_payloads() {
    // &str 与 String payload 均需提取，未知类型回退兜底文案
    assert_eq!(crate::service_core::panic_msg(&"boom", "fallback"), "boom");
    assert_eq!(
        crate::service_core::panic_msg(&String::from("boom2"), "fallback"),
        "boom2"
    );
    assert_eq!(
        crate::service_core::panic_msg(&42u32, "fallback"),
        "fallback"
    );
}

#[test]
fn panic_log_path_follows_install_location() {
    // 平台安装（Program Files\Osmium\os.exe）→ svcs 根；其他位置 → exe 同目录
    let inplace = std::env::temp_dir().join("osmium-panic-path-test");
    std::fs::create_dir_all(&inplace).unwrap();
    let exe = inplace.join("os.exe");
    std::fs::write(&exe, [1u8, 2, 3]).unwrap();
    // 无法替换 current_exe，改为验证函数存在性依赖: 非安装路径返回 exe 旁 panic.log（含目录名）
    // （通过 get_own_path 分支: 当前测试进程非安装路径 → 返回 exe 同目录）
    let path = crate::service_core::panic_log_path();
    assert_eq!(
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        "panic.log"
    );
    // 安装路径分支: 模拟 get_own_path 返回安装 exe（直接比对路径派生逻辑）
    let install = get_own_path();
    if install.eq_ignore_ascii_case("C:\\Program Files\\Osmium\\os.exe") {
        assert!(path.to_string_lossy().contains("ProgramData\\Osmium\\svcs"));
    }
    let _ = std::fs::remove_dir_all(&inplace);
}

#[test]
fn write_log_line_appends_dated_entry() {
    // 刷新程序日志底层: 写入 yyyy-MM-dd.log，条目含时间戳与通道名
    let dir = unique_temp_dir("wlogline");
    crate::service_core::write_log_line(&dir, "refresher", "test-entry");
    let today = chrono::Local::now().format("%Y-%m-%d");
    let content = std::fs::read_to_string(dir.join(format!("{today}.log"))).unwrap();
    assert!(content.contains("[refresher]"), "应含通道名: {content}");
    assert!(content.contains("test-entry"), "应含消息: {content}");
    assert!(content.contains(&today.to_string()), "应含日期: {content}");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 安装更新保留日志（install 更新不丢 logs） ====================

#[test]
fn backup_restore_logs_preserves_log_dir() {
    // install 更新: logs 先挪出系统临时目录，删除宿主目录后还原，内容完整
    let dir = unique_temp_dir("logs_keep");
    let logs = dir.join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    std::fs::write(logs.join("2026-08-18.log"), "keep-me").unwrap();

    let backup = crate::service_core::backup_logs_dir(&dir, "logs_keep");
    assert!(backup.is_some(), "有 logs 时应成功挪出");
    assert!(!logs.exists(), "挪出后原目录应消失");

    // 模拟 force_remove_service 删除宿主目录后还原
    std::fs::remove_dir_all(&dir).unwrap();
    crate::service_core::restore_logs_dir(&dir, backup);
    assert_eq!(
        std::fs::read_to_string(dir.join("logs").join("2026-08-18.log")).unwrap(),
        "keep-me",
        "还原后日志内容应完整"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn backup_logs_returns_none_without_logs_dir() {
    // 无 logs 目录（首次安装）时不应产生备份，还原为空操作
    let dir = unique_temp_dir("logs_none");
    std::fs::create_dir_all(&dir).unwrap();
    assert!(crate::service_core::backup_logs_dir(&dir, "logs_none").is_none());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn set_eco_qos_toggles_on_self() {
    // ProcessPowerThrottling 设置: 对自身进程开/关效率模式均应成功（Get 读回系统不支持，仅断言 Set）
    let pid = std::process::id();
    assert!(
        crate::service_host::set_eco_qos(pid, true),
        "开启效率模式应成功"
    );
    assert!(
        crate::service_host::set_eco_qos(pid, false),
        "关闭效率模式应成功"
    );
    // 无效 PID 静默失败不 panic
    assert!(!crate::service_host::set_eco_qos(0, true));
}

#[test]
fn write_log_entry_redacts_configured_patterns() {
    // 日志脱敏: log_redact 字面串写入前替换为 ***（防密码/令牌经日志泄漏）
    let dir = unique_temp_dir("wlog_redact");
    let opts = LogOptions {
        split_out_err: false,
        max_size_mb: 0,
        backup_count: 5,
        zip_backup: false,
        pattern: String::new(),
        auto_roll_at: None,
        out_enabled: true,
        err_enabled: true,
        reset: false,
        out_filename: String::new(),
        err_filename: String::new(),
        roll_at_start: false,
        roll_period_days: 0,
        zip_date_format: String::new(),
        redact: vec!["secret-token".into(), "P@ssw0rd".into()],
    };
    write_log_entry(
        dir.to_str().unwrap(),
        "host",
        "login secret-token ok P@ssw0rd done",
        &opts,
    );
    let today = chrono::Local::now().format("%Y-%m-%d");
    let content = std::fs::read_to_string(dir.join(format!("{today}.log"))).unwrap();
    assert!(
        content.contains("login *** ok *** done"),
        "敏感串应被脱敏: {content}"
    );
    assert!(
        !content.contains("secret-token") && !content.contains("P@ssw0rd"),
        "原文不应残留: {content}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn resolve_redirect_url_parses_relative_and_absolute() {
    // 重定向 Location 解析: 相对路径基于当前 URL 拼接，绝对路径原样
    assert_eq!(
        resolve_redirect_url("https://example.com/a/b", "/c"),
        "https://example.com/c"
    );
    assert_eq!(
        resolve_redirect_url("https://example.com/a/b", "c"),
        "https://example.com/a/c"
    );
    assert_eq!(
        resolve_redirect_url("https://example.com/a", "https://other.com/x"),
        "https://other.com/x"
    );
    // 非法 Location 按相对路径解析（RFC 3986 语义，不 panic）
    assert_eq!(
        resolve_redirect_url("https://example.com", "://bad"),
        "https://example.com/://bad"
    );
}

#[test]
fn validate_config_reports_ok_and_issues() {
    // --check 预检: 合法配置返回通过项；不存在的可执行路径报错
    let dir = unique_temp_dir("chkcfg");
    // 合法 exe 用受保护目录（System32）内的真实文件: 目录非用户可写，可写性校验应通过
    let good = dir.join("good.toml");
    std::fs::write(&good, "service_name = \"chk-svc\"\nservice_display_name = \"Chk\"\nservice_description = \"d\"\nservice_executable_path = 'C:\\Windows\\System32\\cmd.exe'\n").unwrap();
    let msgs = validate_config(&good).expect("合法配置应通过");
    assert!(
        msgs.iter().any(|m| m.contains("valid")),
        "应含通过项: {msgs:?}"
    );

    let bad = dir.join("bad.toml");
    std::fs::write(&bad, "service_name = \"chk-svc\"\nservice_display_name = \"Chk\"\nservice_description = \"d\"\nservice_executable_path = 'C:\\no\\such\\app.exe'\n").unwrap();
    let errs = validate_config(&bad).expect_err("不存在的 exe 应报错");
    assert!(
        errs.iter().any(|e| e.contains("does not exist")),
        "应含路径错误: {errs:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_parses_new_guardrail_fields() {
    // 新增字段解析: 健康检查/下载重试/Job 对象/插件签名/定时调度
    let dir = unique_temp_dir("grdfld");
    let cfg = dir.join("c.toml");
    std::fs::write(
        &cfg,
        r#"
service_name = "gr"
service_display_name = "Gr"
service_description = "d"
service_executable_path = 'C:\Windows\System32\cmd.exe'
health_check_url = "http://127.0.0.1:8080/health"
health_check_interval_secs = 15
health_check_timeout_secs = 3
health_check_failures = 5
health_check_expected_status = 204
download_retries = 4
download_retry_backoff_ms = 1000
job_object = false
require_signed_plugins = true
[[schedules]]
every_secs = 3600
action = "hook"
command = 'echo tick'
[[schedules]]
daily_at = "03:30"
action = "restart"
"#,
    )
    .unwrap();
    let c = load_config(&cfg);
    assert_eq!(
        c.health_check_url.as_deref(),
        Some("http://127.0.0.1:8080/health")
    );
    assert_eq!(c.health_check_interval_secs, 15);
    assert_eq!(c.health_check_timeout_secs, 3);
    assert_eq!(c.health_check_failures, 5);
    assert_eq!(c.health_check_expected_status, 204);
    assert_eq!(c.download_retries, 4);
    assert_eq!(c.download_retry_backoff_ms, 1000);
    assert!(!c.job_object, "job_object=false 应解析");
    assert!(
        c.require_signed_plugins,
        "require_signed_plugins=true 应解析"
    );
    let s = c.schedules.expect("schedules 应解析");
    assert_eq!(s.len(), 2);
    assert_eq!(s[0].every_secs, Some(3600));
    assert_eq!(s[0].action, "hook");
    assert_eq!(s[0].command.as_deref(), Some("echo tick"));
    assert_eq!(s[1].daily_at.as_deref(), Some("03:30"));
    assert_eq!(s[1].action, "restart");
    // 缺省值: job_object 默认 true、重试默认 0、健康检查默认值
    let cfg2 = dir.join("c2.toml");
    std::fs::write(&cfg2, "service_name = \"gr2\"\nservice_display_name = \"G2\"\nservice_description = \"d\"\nservice_executable_path = 'C:\\Windows\\System32\\cmd.exe'\n").unwrap();
    let c2 = load_config(&cfg2);
    assert!(c2.job_object, "job_object 默认 true");
    assert_eq!(c2.download_retries, 0);
    assert_eq!(c2.health_check_interval_secs, 0);
    assert!(
        !c2.require_signed_plugins,
        "require_signed_plugins 默认 false"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn job_object_create_and_assign_child() {
    // Job Object: 创建成功并可把 spawn 的子进程放入（测试进程自身可能在父 Job 中，
    // 直接 assign 会因嵌套限制失败——用 cmd 子进程验证赋值路径）
    let job = crate::service_host::JobObject::create().expect("Job 对象应创建成功");
    let mut child = Command::new("cmd.exe")
        .args(["/c", "ping -n 3 127.0.0.1 > nul"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .expect("spawn cmd 应成功");
    let pid = child.id();
    let h = unsafe {
        OpenProcess(
            windows::Win32::System::Threading::PROCESS_SET_INFORMATION
                | windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION,
            false,
            pid,
        )
        .expect("打开子进程应成功")
    };
    let assigned = job.assign(h);
    unsafe {
        let _ = CloseHandle(h);
    }
    // 子进程已在其他 Job（如系统服务宿主）时 assign 会失败——两种结果都合法，重点验证不 panic
    if assigned.is_ok() {
        // 加入 Job 后 Job 被 drop → KILL_ON_JOB_CLOSE 应立即终止子进程
        drop(job);
        let status = child.wait().expect("wait 应成功");
        assert!(status.code().is_some(), "Job drop 后子进程应被终止");
    } else {
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[test]
fn verify_file_signature_rejects_unsigned_and_missing() {
    // 插件签名校验: 未签名文件与不存在文件均返回 false（require_signed_plugins 的拒绝路径）
    assert!(
        !crate::service_host::verify_file_signature("C:\\no\\such\\plugin.osx"),
        "不存在文件应为 false"
    );
    let dir = unique_temp_dir("sgnchk");
    let f = dir.join("plain.txt");
    std::fs::write(&f, "plain text, no signature").unwrap();
    assert!(
        !crate::service_host::verify_file_signature(&f.to_string_lossy()),
        "未签名文件应为 false"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn service_process_pids_unknown_service_empty() {
    // 子进程 PID 枚举: 未运行的服务返回空列表（不 panic）
    assert!(crate::service_host::service_process_pids("osmium-no-such-svc-xyz").is_empty());
}

#[test]
fn schedule_due_interval_and_daily() {
    // 定时到点判断: every_secs 间隔 / daily_at 当日到点与防重复 / 非法配置
    use crate::service_config::ScheduleConfig;
    use crate::service_host::schedule_due;
    use std::time::{Duration, Instant};
    let mk = |every: Option<i64>, daily: Option<&str>, action: &str| ScheduleConfig {
        every_secs: every,
        daily_at: daily.map(String::from),
        action: action.into(),
        command: None,
    };
    let now = chrono::Local::now();
    // every_secs: 未触发过 → 立即到点；距上次 10s < 间隔 60s → 未到；距上次 61s ≥ 60s → 到点
    let s = mk(Some(60), None, "restart");
    assert!(schedule_due(&s, None, None, now), "首次触发应到点");
    let last = Instant::now() - Duration::from_secs(10);
    assert!(!schedule_due(&s, Some(last), None, now), "未到间隔不应触发");
    let last2 = Instant::now() - Duration::from_secs(61);
    assert!(schedule_due(&s, Some(last2), None, now), "超间隔应触发");
    // daily_at: 已到点且当日未触发 → 触发；当日已触发 → 不重复
    let d = mk(None, Some("00:00"), "restart");
    assert!(
        schedule_due(&d, None, None, now),
        "今日 00:00 已过且未触发应到点"
    );
    assert!(
        !schedule_due(&d, None, Some(now.date_naive()), now),
        "当日已触发不应重复"
    );
    // 未来时刻未到 → 不到点；非法时刻/空配置 → 不到点。
    // 注意 now+5h 跨天（深夜运行测试）时当日已无更晚时刻，该分支无法构造，跳过
    use chrono::Timelike;
    if now.time().hour() < 19 {
        let future = (now.time() + chrono::Duration::hours(5))
            .format("%H:%M:%S")
            .to_string();
        let f = mk(None, Some(&future), "restart");
        assert!(!schedule_due(&f, None, None, now), "未来时刻不应触发");
    }
    let bad = mk(None, Some("25:99"), "restart");
    assert!(!schedule_due(&bad, None, None, now), "非法时刻不应触发");
    let none = mk(None, None, "restart");
    assert!(!schedule_due(&none, None, None, now), "未配置不应触发");
}

#[test]
fn download_retries_recover_after_transient_failure() {
    // 下载重试: 服务器第一次 500、第二次 200 → 重试后成功（download_retries=1）
    let dir = unique_temp_dir("dlretry");
    let addr = {
        let attempts = Arc::new(AtomicUsize::new(0));
        let a2 = attempts.clone();
        let (a, _stop, _) = spawn_http_server(move |_, _| {
            let n = a2.fetch_add(1, Ordering::SeqCst);
            if n == 0 {
                (
                    "500 Internal Server Error".into(),
                    vec![("Content-Length".into(), "0".into())],
                    Vec::new(),
                )
            } else {
                (
                    "200 OK".into(),
                    vec![("Content-Length".into(), "5".into())],
                    b"hello".to_vec(),
                )
            }
        });
        a
    };
    let cfg_path = dir.join("c.toml");
    std::fs::write(&cfg_path, "service_name = \"dl-retry\"\nservice_display_name = \"D\"\nservice_description = \"d\"\nservice_executable_path = 'C:\\Windows\\System32\\cmd.exe'\ndownload_retries = 1\ndownload_retry_backoff_ms = 100\n").unwrap();
    let config = load_config(&cfg_path);
    let entry = DownloadConfig {
        from: format!("http://{}/app.exe", addr),
        to: dir.join("app.exe").to_string_lossy().into_owned(),
        // sha256 提供后 http 放行（P1-4 安全策略）: "hello" 的标准 sha256
        sha256: Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into()),
        fail_on_error: Some(true),
        auth: None,
        username: None,
        password: None,
        unsecure_auth: None,
        proxy: None,
        unzip: Some(false),
        stage: None,
    };
    let deploy = dir.to_string_lossy().into_owned();
    crate::service_host::run_download_entry(&config, &entry, &deploy, "", &Default::default())
        .expect("重试后应下载成功");
    let content = std::fs::read_to_string(dir.join("app.exe")).unwrap();
    assert_eq!(content, "hello", "应下载到第二次响应内容");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== 健壮性增强回归（TCP 探针/签名/断点续传/多实例钳制/超时参数化） ====================

#[test]
fn parse_tcp_target_formats() {
    // tcp:// 健康探针目标解析: host / host:port / [::1] / [::1]:port / 非法格式
    use crate::service_host::parse_tcp_target_check;
    assert!(parse_tcp_target_check("127.0.0.1:8080"));
    assert!(parse_tcp_target_check("example.com"));
    assert!(parse_tcp_target_check("[::1]:514"));
    assert!(parse_tcp_target_check("[::1]"));
    assert!(!parse_tcp_target_check("host:"));
    assert!(!parse_tcp_target_check("host:abc"));
    assert!(!parse_tcp_target_check(":8080"));
    assert!(!parse_tcp_target_check(""));
    assert!(!parse_tcp_target_check("host:99999"));
}

#[test]
fn config_signature_roundtrip_and_tamper_detection() {
    // 配置签名: 生成 RSA 密钥 → 签名 → 校验通过 → 篡改内容后校验失败（fail-closed）
    let dir = std::env::temp_dir().join(format!("osmium-sig-test-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let key_path = dir.join("test-key.pem");
    let pub_path = dir.join("test-pub.pem");
    // 用 openssl 生成密钥（测试环境可用）；失败则跳过（不阻塞其他测试）
    let gen_res = Command::new("openssl")
        .args(["genrsa", "-out", &key_path.to_string_lossy(), "2048"])
        .output();
    if gen_res.map(|o| o.status.success()).unwrap_or(false) {
        let _ = Command::new("openssl")
            .args([
                "rsa",
                "-in",
                &key_path.to_string_lossy(),
                "-pubout",
                "-out",
                &pub_path.to_string_lossy(),
            ])
            .output();
        let cfg = dir.join("svc.toml");
        std::fs::write(&cfg, "service_name = \"s\"\n").unwrap();
        // 签名函数读 exe 旁固定名密钥——此处直接构造等效校验路径: 用 rsa 库手工签名/校验
        use rsa::pkcs1v15::{Signature, SigningKey, VerifyingKey};
        use rsa::pkcs8::{DecodePrivateKey, DecodePublicKey};
        use rsa::sha2::Sha256;
        use rsa::signature::{RandomizedSigner, SignatureEncoding, Verifier};
        let key = rsa::RsaPrivateKey::from_pkcs8_pem(&std::fs::read_to_string(&key_path).unwrap())
            .unwrap();
        let pub_key =
            rsa::RsaPublicKey::from_public_key_pem(&std::fs::read_to_string(&pub_path).unwrap())
                .unwrap();
        let data = b"payload-bytes";
        let mut rng = rsa::rand_core::OsRng;
        let sig = SigningKey::<Sha256>::new(key).sign_with_rng(&mut rng, data);
        let sig_bytes = sig.to_bytes();
        // 正确签名 → 校验通过
        let ok_sig = Signature::try_from(sig_bytes.as_ref()).unwrap();
        let verifying = VerifyingKey::<Sha256>::new(pub_key);
        assert!(verifying.verify(data, &ok_sig).is_ok(), "正确签名应通过");
        // 篡改内容 → 校验失败
        assert!(
            verifying.verify(b"tampered", &ok_sig).is_err(),
            "篡改内容应被拒绝"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn process_count_clamps_to_range() {
    // process_count 钳制: <=0 → 1；>64 → 64（配置失控防护）
    let dir = unique_temp_dir("pcount");
    let f = dir.join("pc.toml");
    std::fs::write(&f, "service_name = \"pc\"\nservice_display_name = \"pc\"\nservice_description = \"pc\"\nservice_executable_path = 'C:\\x.exe'\nprocess_count = 100\n").unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.process_count, 100);
    let mut host = crate::service_host::ServiceHost::new();
    host.apply_runtime_fields_probe(&cfg);
    assert_eq!(host.process_count_probe(), 64, "超上限应钳制到 64");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hook_timeout_fields_parse_and_default() {
    // 钩子/停止命令超时参数化: 缺省回退常量；显式配置生效
    let dir = unique_temp_dir("hookt");
    let f = dir.join("ht.toml");
    std::fs::write(&f, "service_name = \"ht\"\nservice_display_name = \"ht\"\nservice_description = \"ht\"\nservice_executable_path = 'C:\\x.exe'\nhook_prestart_timeout_secs = 7\nhook_poststop_timeout_secs = 9\nstop_cmd_timeout_secs = 12\n").unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.hook_prestart_timeout_secs, 7);
    assert_eq!(cfg.hook_poststop_timeout_secs, 9);
    assert_eq!(cfg.stop_cmd_timeout_secs, 12);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn metrics_format_parses_prometheus() {
    // metrics_format: prometheus 生效，其他值回退 json
    let dir = unique_temp_dir("mf");
    let f = dir.join("mf.toml");
    std::fs::write(&f, "service_name = \"mf\"\nservice_display_name = \"mf\"\nservice_description = \"mf\"\nservice_executable_path = 'C:\\x.exe'\nmetrics_format = \"PROMETHEUS\"\n").unwrap();
    let cfg = load_config(&f);
    let mut host = crate::service_host::ServiceHost::new();
    host.apply_runtime_fields_probe(&cfg);
    assert_eq!(
        host.metrics_format_probe(),
        "prometheus",
        "大小写不敏感生效"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_config_extended_prechecks() {
    // --check 扩展预检: 非法 SDDL / 坏 schedules / 坏 tcp 目标 / 插件引用缺失 应报错
    let dir = unique_temp_dir("chk2");
    let mk = |content: &str| {
        let f = dir.join(format!(
            "c-{}.toml",
            std::process::id() + content.len() as u32 % 1000
        ));
        std::fs::write(&f, content).unwrap();
        f
    };
    // 坏 SDDL（无效字符）
    let f1 = mk(
        "service_name = \"a\"\nservice_display_name = \"a\"\nservice_description = \"a\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nsecurity_descriptor = \"D:(NOTVALID\"\n",
    );
    assert!(validate_config(&f1).is_err(), "非法 SDDL 应报错");
    // 坏 schedules（daily_at 无法解析 + every_secs 非正）
    let f2 = mk(
        "service_name = \"b\"\nservice_display_name = \"b\"\nservice_description = \"b\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nschedules = [{ daily_at = \"25:99\" }, { every_secs = -3 }]\n",
    );
    assert!(validate_config(&f2).is_err(), "坏 schedules 应报错");
    // 坏 tcp 健康检查目标
    let f3 = mk(
        "service_name = \"c\"\nservice_display_name = \"c\"\nservice_description = \"c\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nhealth_check_url = \"tcp://:8080\"\n",
    );
    assert!(validate_config(&f3).is_err(), "坏 tcp 目标应报错");
    // 引用插件但本机无插件 → 报错（测试进程目录通常无 .osx）
    let f4 = mk(
        "service_name = \"d\"\nservice_display_name = \"d\"\nservice_description = \"d\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nplugins = [{ kit = \"ping\", phase = \"start\" }]\n",
    );
    crate::service_host::clear_plugin_cache();
    let plugins = crate::service_host::discover_plugins();
    if plugins.is_empty() {
        assert!(validate_config(&f4).is_err(), "引用插件但无插件可用应报错");
    }
    // 内置告警通道校验: notify_url 非法 / smtp 缺 from/to 应报错
    let f6 = mk(
        "service_name = \"f\"\nservice_display_name = \"f\"\nservice_description = \"f\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nnotify_url = \"not-a-url\"\n",
    );
    assert!(validate_config(&f6).is_err(), "非法 notify_url 应报错");
    let f7 = mk(
        "service_name = \"g\"\nservice_display_name = \"g\"\nservice_description = \"g\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nsmtp_host = \"mail.example.com:25\"\n",
    );
    assert!(
        validate_config(&f7).is_err(),
        "smtp 缺 smtp_from/smtp_to 应报错"
    );
    // 正常配置仍通过（不因新增预检误伤）
    let f5 = mk(
        "service_name = \"e\"\nservice_display_name = \"e\"\nservice_description = \"e\"\nservice_executable_path = 'C:\\Windows\\System32\\ping.exe'\nschedules = [{ every_secs = 60, action = \"hook\", command = \"echo x\" }]\nhealth_check_url = \"tcp://127.0.0.1:8080\"\n",
    );
    assert!(validate_config(&f5).is_ok(), "合法配置应通过预检");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn chunk_already_done_detects_complete_chunks() {
    // 断点续传: 已写满且非零的块判定完成；未写/短块判定未完成
    use std::io::Write;
    let dir = unique_temp_dir("chunk");
    let f = dir.join("f.bin");
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(true)
        .open(&f)
        .unwrap();
    file.write_all(&[1u8; 1024 * 1024]).unwrap(); // 恰好 1MiB
    // 第一块完整（非零）→ 完成
    assert!(crate::service_core::chunk_already_done(
        &file,
        0,
        1024 * 1024 - 1
    ));
    // 尾部越界块（文件长度不足）→ 未完成
    assert!(!crate::service_core::chunk_already_done(
        &file,
        1024 * 1024,
        2 * 1024 * 1024 - 1
    ));
    // 全零区间（未写）→ 未完成
    file.set_len(2 * 1024 * 1024).unwrap();
    assert!(
        !crate::service_core::chunk_already_done(&file, 1024 * 1024, 2 * 1024 * 1024 - 1),
        "全零块视为未完成"
    );
    // 部分写入的块（前 512KB 非零、后 512KB 零）→ 未完成（残缺块必须重下）
    let half = vec![1u8; 512 * 1024];
    use std::os::windows::fs::FileExt;
    file.seek_write(&half, 1024 * 1024).unwrap();
    assert!(
        !crate::service_core::chunk_already_done(&file, 1024 * 1024, 2 * 1024 * 1024 - 1),
        "半写块视为未完成"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn multi_process_starts_requested_instances() {
    // 多子进程: start_child_process 启动 process_count 个实例，主实例标记正确
    let dir = unique_temp_dir("mp");
    let config_path = dir.join("mp.toml");
    std::fs::write(&config_path,
        "service_name = \"mp\"\nservice_display_name = \"mp\"\nservice_description = \"mp\"\n\
         service_executable_path = 'C:\\Windows\\System32\\WindowsPowerShell\\v1.0\\powershell.exe'\n\
         service_executable_args = '-NoProfile -Command Start-Sleep 5'\nprocess_count = 2\nlog_dir = ''\n"
    ).unwrap();
    let mut host = crate::service_host::ServiceHost::new();
    assert!(host.on_start_from(&config_path), "宿主应启动成功");
    assert_eq!(host.child.len(), 2, "应启动 2 个实例");
    let pids: Vec<u32> = host.child.iter().map(|c| c.id()).collect();
    assert_eq!(
        host.last_child_pid_probe(),
        pids[0],
        "主实例 PID 应记录第一个实例"
    );
    // 清理
    for mut c in std::mem::take(&mut host.child) {
        let _ = c.kill();
        let _ = c.wait();
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn writable_cache_consistent_per_path() {
    // 可写性判定缓存: 同一路径重复查询结果一致（且不重复 spawn PowerShell）
    crate::service_core::clear_writable_cache();
    let dir = unique_temp_dir("wcache");
    let d = dir.to_string_lossy().to_string();
    let r1 = is_user_writable(&d);
    let r2 = is_user_writable(&d);
    assert_eq!(r1, r2, "缓存应返回一致结果");
    crate::service_core::clear_writable_cache();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_rate_limit_parses() {
    // 下载限速字段: 缺省 0（不限速），显式配置生效
    let dir = unique_temp_dir("ratel");
    let f = dir.join("rl.toml");
    std::fs::write(&f, "service_name = \"rl\"\nservice_display_name = \"rl\"\nservice_description = \"rl\"\nservice_executable_path = 'C:\\x.exe'\ndownload_rate_limit_kbps = 512\n").unwrap();
    let cfg = load_config(&f);
    assert_eq!(cfg.download_rate_limit_kbps, 512);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn parse_osx_probe_spec_parses_kit_and_payload() {
    // osx:// 健康探针规格解析: kit 提取 + 表单参数解码为 payload JSON
    let (kit, payload) =
        crate::service_host::parse_osx_probe_spec("probe?url=127.0.0.1%3A3306&probe_type=mysql")
            .unwrap();
    assert_eq!(kit, "probe");
    assert_eq!(payload["url"], "127.0.0.1:3306", "表单编码应解码");
    assert_eq!(payload["probe_type"], "mysql");
    // 无参数
    let (kit2, payload2) = crate::service_host::parse_osx_probe_spec("probe").unwrap();
    assert_eq!(kit2, "probe");
    assert!(payload2.as_object().unwrap().is_empty());
    // 空 kit 拒绝
    assert!(crate::service_host::parse_osx_probe_spec("").is_none());
    assert!(crate::service_host::parse_osx_probe_spec("?a=1").is_none());
}

// ==================== 审计修复回归（percent_decode 边界 / 截断下载 / 跨源凭据 / quick 残留 / 启动类型预检） ====================

#[test]
fn parse_osx_probe_spec_truncated_escape_no_panic() {
    // B1 回归: 以 "%X" 结尾的残缺转义不得越界 panic（旧边界 i+2<=len 允许 bytes[i+2] 越界）
    let (kit, payload) = crate::service_host::parse_osx_probe_spec("probe?url=host%A").unwrap();
    assert_eq!(kit, "probe");
    assert_eq!(payload["url"], "host%A", "残缺转义应原样保留");
    let (_, p2) = crate::service_host::parse_osx_probe_spec("probe?url=%").unwrap();
    assert_eq!(p2["url"], "%");
}

#[test]
fn download_core_truncated_body_fails_without_sha() {
    // B8 回归: 响应体比 Content-Length 短（连接提前干净关闭）→ 必须报错，不得静默成功
    let body = b"short".to_vec();
    let b2 = body.clone();
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), "1048576".to_string())],
            b2.clone(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let dir = unique_temp_dir("trunc");
    let tmp = dir.join("f.bin");
    let result = download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        0,
        None,
        0,
    );
    stop.store(true, Ordering::Relaxed);
    assert!(result.is_err(), "截断响应必须失败");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_core_cross_origin_redirect_strips_basic_auth() {
    // S1 回归: Basic 凭据仅同源发送——302 跳转到另一主机时 Authorization 不得跟随
    let seen_auth = Arc::new(std::sync::Mutex::new(String::new()));
    let seen = seen_auth.clone();
    let data = b"final".to_vec();
    let d2 = data.clone();
    let (addr_b, stop_b, _cb) = spawn_http_server(move |method, lines| {
        if method == "HEAD" {
            return ("200 OK".to_string(), vec![], vec![]);
        }
        let auth = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .cloned()
            .unwrap_or_default();
        *seen.lock().unwrap() = auth;
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), d2.len().to_string())],
            d2.clone(),
        )
    });
    let loc = format!("http://{}:{}/file", addr_b.ip(), addr_b.port());
    let (addr_a, stop_a, _ca) = spawn_http_server(move |_method, _lines| {
        (
            "302 Found".to_string(),
            vec![("Location".into(), loc.clone())],
            vec![],
        )
    });
    let url = format!("http://{}:{}/start", addr_a.ip(), addr_a.port());
    let dir = unique_temp_dir("redirauth");
    let tmp = dir.join("f.bin");
    let result = download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::Basic("user", "pass"),
        None,
        0,
        None,
        0,
    );
    stop_a.store(true, Ordering::Relaxed);
    stop_b.store(true, Ordering::Relaxed);
    result.unwrap();
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let auth_seen = seen_auth.lock().unwrap();
    assert!(
        auth_seen.is_empty(),
        "跨源重定向后不得携带 Authorization，实际: {auth_seen}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sweep_stale_quick_configs_removes_only_old_files() {
    // B10 回归: 快速安装残留的临时配置按超期清理；新文件与不匹配名称保留
    let dir = unique_temp_dir("quick");
    std::fs::create_dir_all(&dir).unwrap();
    let old = dir.join("osmium-quick-1-old.toml");
    std::fs::write(&old, "x").unwrap();
    // mtime 回拨 2 小时，越过 1 小时清理阈值
    let f = std::fs::OpenOptions::new().write(true).open(&old).unwrap();
    f.set_times(
        std::fs::FileTimes::new()
            .set_modified(std::time::SystemTime::now() - Duration::from_secs(7200)),
    )
    .unwrap();
    let fresh = dir.join("osmium-quick-2-fresh.toml");
    std::fs::write(&fresh, "y").unwrap();
    let other = dir.join("unrelated.txt");
    std::fs::write(&other, "z").unwrap();
    crate::service_core::sweep_stale_quick_configs(&dir);
    assert!(!old.exists(), "超期残留应被删除");
    assert!(fresh.exists(), "未超期文件保留");
    assert!(other.exists(), "无关文件保留");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_config_flags_unknown_start_mode() {
    // F3 回归: service_start_mode 拼错值（旧实现静默落 automatic）→ 预检显式报错
    let dir = unique_temp_dir("smode");
    let bad = dir.join("bad.toml");
    std::fs::write(
        &bad,
        "service_name = \"sm\"\nservice_display_name = \"sm\"\nservice_description = \"sm\"\nservice_executable_path = 'C:\\x.exe'\nservice_start_mode = \"automatik\"\n",
    )
    .unwrap();
    let errs = validate_config(&bad).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("unknown value 'automatik'")),
        "未知启动类型应被标记: {errs:?}"
    );
    // 合法值不产生启动类型错误项（其他校验项的错误不影响该断言）
    let good = dir.join("good.toml");
    std::fs::write(
        &good,
        "service_name = \"sm\"\nservice_display_name = \"sm\"\nservice_description = \"sm\"\nservice_executable_path = 'C:\\x.exe'\nservice_start_mode = \"delayed_auto\"\n",
    )
    .unwrap();
    let errs2 = validate_config(&good).unwrap_err();
    assert!(
        !errs2.iter().any(|e| e.contains("service_start_mode")),
        "合法启动类型不应报错: {errs2:?}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn validate_config_flags_insecure_download_and_bad_numbers() {
    // F1: --check 预检必须包含不安全下载检查（http 无 sha / basic 走明文 http）——
    // 与宿主启动时同源判定，安装前就能发现
    let dir = unique_temp_dir("insecure");
    let f = dir.join("bad.toml");
    std::fs::write(
        &f,
        "service_name = \"sm\"\nservice_display_name = \"sm\"\nservice_description = \"sm\"\nservice_executable_path = 'C:\\x.exe'\ndownload_url = 'http://example.com/app.exe'\n",
    )
    .unwrap();
    let errs = validate_config(&f).unwrap_err();
    assert!(
        errs.iter().any(|e| e.contains("plain HTTP without sha256")),
        "http 无 sha 下载必须被预检拦截: {errs:?}"
    );
    // F3: 数值字段负值/越界显式报错（0 = 未配置合法，负值非法）
    std::fs::write(
        &f,
        "service_name = \"sm\"\nservice_display_name = \"sm\"\nservice_description = \"sm\"\nservice_executable_path = 'C:\\x.exe'\ndownload_threads = -1\nprocess_count = 65\ndownload_rate_limit_kbps = -5\n",
    )
    .unwrap();
    let errs = validate_config(&f).unwrap_err();
    for needle in [
        "download_threads",
        "process_count",
        "download_rate_limit_kbps",
    ] {
        assert!(
            errs.iter().any(|e| e.contains(needle)),
            "{needle} 越界应被预检标记: {errs:?}"
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn readonly_commands_skip_admin_gate() {
    // 只读/本地命令集合: 免管理员（帮助/查询/插件列表/预检/前台调试/签名）
    for t in [
        "help",
        "-h",
        "--help",
        "--list",
        "--lst",
        "--status",
        "--sts",
        "--status-all",
        "--stsa",
        "--extend",
        "--ext",
        "--check",
        "--chk",
        "--test",
        "--tst",
        "--sign-config",
        "--sigc",
    ] {
        assert!(
            crate::service_cli::is_readonly_command(t),
            "'{t}' should be treated as read-only"
        );
    }
    // SCM 写操作与内部入口仍要求管理员
    for t in [
        "--install",
        "--ins",
        "--import",
        "--export",
        "--uninstall",
        "--uin",
        "--start",
        "--str",
        "--stop",
        "--stp",
        "--restart",
        "--rst",
        "--delete",
        "--del",
        "--kill",
        "--kil",
        "--refresh",
        "--rfs",
        "--reload",
        "--rld",
        "-internal",
        "-m",
    ] {
        assert!(
            !crate::service_cli::is_readonly_command(t),
            "'{t}' should require administrator"
        );
    }
}

#[test]
fn expand_env_value_keeps_url_percent_escapes() {
    // 回归: URL 百分号转义不得被当环境变量吞掉（%20/%2F/%E4 数字开头序列原样保留）
    assert_eq!(
        expand_env_value("https://cdn.example.com/app%20v2%2Fbuild/f.zip", "C:\\base"),
        "https://cdn.example.com/app%20v2%2Fbuild/f.zip"
    );
    assert_eq!(
        expand_env_value("https://h/p?token=a%3Db%26c", "C:\\base"),
        "https://h/p?token=a%3Db%26c"
    );
    assert_eq!(
        expand_env_value("https://h/%E4%B8%AD%E6%96%87.zip", "C:\\base"),
        "https://h/%E4%B8%AD%E6%96%87.zip"
    );
    // 合法变量名展开语义不回归（含 %BASE% 与普通变量）
    unsafe {
        std::env::set_var("OSMIUM_TV", "vv");
    }
    assert_eq!(
        expand_env_value("%BASE%\\%OSMIUM_TV%", "C:\\b"),
        "C:\\b\\vv"
    );
    assert_eq!(expand_env_value("%PID%", "C:\\b"), "%PID%");
    // 未闭合单个 % 与数字开头的伪变量均按字面保留
    assert_eq!(expand_env_value("50% off", "C:\\b"), "50% off");
    assert_eq!(expand_env_value("%20name%tail", "C:\\b"), "%20name%tail");
}

#[test]
fn escapes_deploy_dir_boundary_cases() {
    use crate::service_host::escapes_deploy_dir;
    let base = "C:\\ProgramData\\Osmium\\svcs\\svc";
    // 部署目录自身与其子路径放行
    assert!(!escapes_deploy_dir(base, base));
    assert!(!escapes_deploy_dir(
        "C:\\ProgramData\\Osmium\\svcs\\svc\\logs\\a.log",
        base
    ));
    // ..\ 折叠后越出部署目录 → 拒绝
    assert!(escapes_deploy_dir(
        "C:\\ProgramData\\Osmium\\svcs\\svc\\..\\other\\x",
        base
    ));
    // 前缀相同但为兄弟目录（svc2）不得因字符串前缀误判放行
    assert!(escapes_deploy_dir(
        "C:\\ProgramData\\Osmium\\svcs\\svc2\\x",
        base
    ));
}

#[test]
fn download_entry_rejects_relative_target_escape() {
    // 数组条目 to 相对路径折叠后越出部署目录 → 配置错误，网络请求前即失败
    let dir = unique_temp_dir("escape");
    std::fs::create_dir_all(&dir).unwrap();
    let cfg: ServiceConfig = toml::from_str(
        "service_name = 's'\nservice_display_name = 's'\nservice_description = 'd'\n\
         service_executable_path = 'C:\\\\x\\\\e.exe'\n\
         [[downloads]]\nfrom = 'https://example.invalid/a.bin'\nto = '..\\\\evil.bin'\n",
    )
    .unwrap();
    let entry = &cfg.downloads.as_ref().unwrap()[0];
    let result = crate::service_host::run_download_entry(
        &cfg,
        entry,
        &dir.to_string_lossy(),
        "",
        &LogOptions::default(),
    );
    let err = format!("{:?}", result.err());
    assert!(
        err.contains("escapes the deployment directory"),
        "实际: {err}"
    );
    // 旧单条模式 download_to 同款拦截
    let cfg2: ServiceConfig = toml::from_str(
        "service_name = 's'\nservice_display_name = 's'\nservice_description = 'd'\n\
         service_executable_path = 'C:\\\\x\\\\e.exe'\ndownload_url = 'https://example.invalid/a.bin'\n\
         download_to = '..\\\\evil.bin'\n",
    )
    .unwrap();
    let entry2 = &download_entries(&cfg2)[0];
    let result2 = crate::service_host::run_download_entry(
        &cfg2,
        entry2,
        &dir.to_string_lossy(),
        "",
        &LogOptions::default(),
    );
    let err2 = format!("{:?}", result2.err());
    assert!(
        err2.contains("download_to") && err2.contains("escapes"),
        "实际: {err2}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_resume_rejects_query_only_source_change() {
    // 回归: 归属标记按完整 URL 哈希判定——仅换查询串的换源不得复用旧断点
    //（旧脱敏串方案剥离 query 后会误判同源，旧块混入新内容）
    let data: Vec<u8> = (0..1024 * 1024 + 11).map(|i| (i % 253) as u8).collect();
    let d2 = data.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        let common = vec![
            ("Content-Length".into(), d2.len().to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
        ];
        if method == "HEAD" {
            return ("200 OK".to_string(), common, vec![]);
        }
        let ranged = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()))
            .and_then(|spec| {
                spec.split_once('-')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)))
            .filter(|(_, e)| *e < d2.len());
        if let Some((s, e)) = ranged {
            let mut h = vec![
                ("Content-Length".into(), (e - s + 1).to_string()),
                (
                    "Content-Range".into(),
                    format!("bytes {}-{}/{}", s, e, d2.len()),
                ),
            ];
            h.extend(common);
            return ("206 Partial Content".to_string(), h, d2[s..=e].to_vec());
        }
        ("200 OK".to_string(), common, d2.clone())
    });
    let dir = unique_temp_dir("tmp");
    let tmp = dir.join("query-change.tmp");
    let _ = std::fs::remove_file(&tmp);
    // 预置"另一来源"的合法格式断点标记（不同 URL 的哈希 + 当前远端长度）:
    // 标记与本次 URL 不匹配 → 必须清零整体重下
    let other_url = format!("http://{}:{}/other.bin?v=999", addr.ip(), addr.port());
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(other_url.as_bytes());
    let other_hash: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&tmp, vec![9u8; 512 * 1024]).unwrap();
    std::fs::write(
        format!("{}.resume", tmp.display()),
        format!("{other_hash}\n{}", data.len()),
    )
    .unwrap();
    let url = format!("http://{}:{}/big.bin?v=1", addr.ip(), addr.port());
    download_core(
        &url,
        tmp.to_str().unwrap(),
        60,
        DownloadAuth::None,
        None,
        8,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        std::fs::read(&tmp).unwrap(),
        data,
        "查询串不同的换源必须整体重下，不得复用旧断点数据块"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn download_chunk_rejects_mismatched_content_range() {
    // S6 回归: 服务器回错位 Content-Range（声明区间与请求不符）时必须拒绝该块——
    // 分块路径拒绝错位片段后整体回退单线程全量重下，最终文件内容必须纯净
    //（修复前错位片段会被静默拼入文件，无 sha 配置时直接损坏）
    let data: Vec<u8> = (0..3 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
    let d2 = data.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        let common = vec![
            ("Content-Length".into(), d2.len().to_string()),
            ("Accept-Ranges".into(), "bytes".into()),
        ];
        if method == "HEAD" {
            return ("200 OK".to_string(), common, vec![]);
        }
        // 恶意服务器: 请求区间 a-b，但声明 Content-Range 从 a+1024 开始（错位）
        let ranged = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("range:"))
            .and_then(|l| l.split_once(':').map(|(_, v)| v.trim().to_string()))
            .and_then(|v| v.strip_prefix("bytes=").map(|s| s.to_string()))
            .and_then(|spec| {
                spec.split_once('-')
                    .map(|(a, b)| (a.to_string(), b.to_string()))
            })
            .and_then(|(a, b)| Some((a.parse::<usize>().ok()?, b.parse::<usize>().ok()?)));
        if let Some((s, e)) = ranged {
            let wrong = (s + 1024).min(d2.len() - 1);
            let mut h = vec![
                ("Content-Length".into(), (e - s + 1).to_string()),
                (
                    "Content-Range".into(),
                    format!("bytes {wrong}-{e}/{}", d2.len()),
                ),
            ];
            h.extend(common);
            return ("206 Partial Content".to_string(), h, d2[wrong..=e].to_vec());
        }
        ("200 OK".to_string(), common, d2.clone())
    });
    let dir = unique_temp_dir("cr");
    let tmp = dir.join("f.bin");
    let url = format!("http://{}:{}/bad.bin", addr.ip(), addr.port());
    // 分块路径必须拒绝错位块并回退（结果 Ok，但内容必须与源完全一致，不得混入错位片段）
    download_core(
        &url,
        tmp.to_str().unwrap(),
        30,
        DownloadAuth::None,
        None,
        8,
        None,
        0,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(
        std::fs::read(&tmp).unwrap(),
        data,
        "错位 Content-Range 后回退重下，文件内容必须纯净"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
