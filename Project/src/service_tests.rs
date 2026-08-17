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
use crate::service_core::{DownloadAuth,
                          build_dependency_string, can_overwrite_source, compare_versions,
                          decrypt_sensitive, delete_dir_tree, delete_old_logs, deployed_config_path, download_core, dpapi_decrypt, dpapi_encrypt, get_file_version, get_own_path,
                          green_dot, has_download, is_updater_reserved_name, is_user_writable, is_valid_service_name, load_config,
                          parse_start_mode, red, red_dot,
                          safe_delete_dir, scm_sleep_time_ms, scm_status_params, scm_wait_hint_ms, sddl_dacl_grants_non_admin_write, sddl_owner_is_administrative, secure_directory,
                          security_descriptor_from_sddl,
                          set_preshutdown_enabled, set_scm_sleep_time_ms, set_scm_wait_hint_ms,
                          sha256_matches, strip_verbatim_prefix,
                          write_deployed_config, write_quick_config,
};
use crate::service_host::{LogOptions,
                          apply_log_mode, auto_roll_logs, build_child_command, collect_descendants, current_log_name,
                          download_auth_from_entry, download_entries, download_entry_stage,
                          download_stage_is, escape_invisible, expand_env_value, expand_stop_pid, ext_phase_matches, failure_action_chain, http_date_from_mtime,
                          log_pattern_safe, process_alive, process_cpu_100ns,
                          process_env_var, process_working_set_mb, redact_url, reset_auto_roll_state, reset_current_logs,
                          resolve_download_target, roll_by_time_if_due, roll_if_needed, roll_logs_to_old, run_hook,
                          run_stop_command,
                          runaway_cleanup_pid_file, runaway_exceeded, set_process_priority, warn_if_insecure_download,
                          write_log_entry, zip_backup_file,
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
                        .lines().map(|s| s.to_string()).collect();
                    let method = lines.first()
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
    // 服务操作命令与全部简化别名（含 --test/--tst、--extend/--ext）均可省略 -m 直接使用
    for tag in [
        "--install", "--uninstall", "--start", "--stop", "--restart",
        "--status", "--delete", "--list", "--test", "--tst",
        "--extend", "--ext",
        "--ins", "--uin", "--str", "--stp", "--rst", "--sts", "--del", "--lst",
    ] {
        assert!(crate::service_cli::is_cli_command(tag), "{tag} should be recognized as a CLI command");
    }
    // 非命令参数不应误判为 CLI 命令
    assert!(!crate::service_cli::is_cli_command("-m"));
    assert!(!crate::service_cli::is_cli_command("--help"));
    assert!(!crate::service_cli::is_cli_command("--updater"));
    assert!(!crate::service_cli::is_cli_command("my-service"));
}

// ==================== 共享宿主 ImagePath 解析（-internal --run） ====================

#[test]
fn parse_run_service_name_extracts_from_image_path() {
    // 新格式: 引号包裹的宿主路径 + -internal --run <name>
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\Program Files\Osmium\os.exe" -internal --run my-service"#),
        Some("my-service".to_string())
    );
    // 服务名含空格（install 时引号包裹，解析时还原）
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\Osmium\os.exe" -internal --run "my service""#),
        Some("my service".to_string())
    );
    // --run 大小写不敏感
    assert_eq!(
        crate::service_core::parse_run_service_name(r#""C:\x.exe" -internal --RUN foo"#),
        Some("foo".to_string())
    );
}

#[test]
fn parse_run_service_name_rejects_non_run_formats() {
    // 无 --run 参数（inplace 旧格式 / 外部服务）
    assert_eq!(crate::service_core::parse_run_service_name(r#""C:\ProgramData\Osmium\svcs\a\a.exe""#), None);
    assert_eq!(crate::service_core::parse_run_service_name(r#""C:\foo.exe""#), None);
    assert_eq!(crate::service_core::parse_run_service_name(""), None);
    // --run 后无参数
    assert_eq!(crate::service_core::parse_run_service_name(r#""C:\x.exe" -internal --run"#), None);
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
fn deployed_config_path_builds_svcs_layout() {
    // 平台部署配置路径: ProgramData\Osmium\svcs\<name>\<name>.osiml
    let p = deployed_config_path("my-svc");
    let s = p.to_string_lossy();
    assert!(s.ends_with("ProgramData\\Osmium\\svcs\\my-svc\\my-svc.osiml"), "路径布局错误: {s}");
    // 服务名含空格/特殊字符时原样拼接（不做清理，防穿越依赖服务名校验）
    let p2 = deployed_config_path("svc with space");
    assert!(p2.to_string_lossy().ends_with("svc with space\\svc with space.osiml"));
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
fn is_updater_reserved_name_case_insensitive() {
    assert!(is_updater_reserved_name("Osmium Service Checker"));
    assert!(is_updater_reserved_name("osmium service checker")); // 大小写不敏感
    assert!(is_updater_reserved_name("OSMIUM SERVICE CHECKER"));
    assert!(!is_updater_reserved_name("checker"));
    assert!(!is_updater_reserved_name("Osmium"));
    assert!(!is_updater_reserved_name(""));
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

// ==================== 启动模式解析 ====================

#[test]
fn parse_start_mode_rules() {
    // 与 WinSW 启动模式语义一致
    assert_eq!(parse_start_mode(None), (SERVICE_AUTO_START, false));
    assert_eq!(parse_start_mode(Some("")), (SERVICE_AUTO_START, false));
    assert_eq!(parse_start_mode(Some("automatic")), (SERVICE_AUTO_START, false));
    assert_eq!(parse_start_mode(Some("delayed_auto")), (SERVICE_AUTO_START, true));
    assert_eq!(parse_start_mode(Some("delayed-auto")), (SERVICE_AUTO_START, true));
    assert_eq!(parse_start_mode(Some("delayedauto")), (SERVICE_AUTO_START, true));
    assert_eq!(parse_start_mode(Some("DELAYED_AUTO")), (SERVICE_AUTO_START, true)); // 大小写不敏感
    assert_eq!(parse_start_mode(Some("manual")), (SERVICE_DEMAND_START, false));
    assert_eq!(parse_start_mode(Some("disabled")), (SERVICE_DISABLED, false));
    assert_eq!(parse_start_mode(Some("unknown")), (SERVICE_AUTO_START, false)); // 未知回退自动
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
    assert_eq!(
        build_dependency_string(Some("A:B")),
        Some("A\0B\0\0".to_string())
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
        "2020-01-01.log",     // 主日志（旧）
        "2020-01-01.err.log", // err 分流（旧）
        "2020-01-01.log.1",   // 滚动备份（旧）
        "2020-01-01.err.log.2", // err 滚动备份（旧）
        "2020-01-01.log.3.zip", // zip 归档（超半年，删除）
        "2099-01-01.log",     // 未来日志（保留）
        "2099-01-01.log.1.zip", // 未来 zip 归档（保留，日期未过期）
        "notes.txt",          // 非日志（保留）
    ];
    for n in &names {
        std::fs::write(dir.join(n), "x").unwrap();
    }
    let cutoff = chrono::Local::now().date_naive();
    // 90 天前的 zip 归档：未超半年保留期（180 天），必须保留
    let recent_zip = format!("{}.log.2.zip", (cutoff - chrono::Duration::days(90)).format("%Y-%m-%d"));
    std::fs::write(dir.join(&recent_zip), "x").unwrap();
    let deleted = delete_old_logs(&dir, cutoff, false);
    assert_eq!(deleted, 5, "应清理 5 个过期日志（含超半年 zip 归档）");
    let remaining: Vec<String> = std::fs::read_dir(&dir).unwrap()
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
    let old_log = format!("{}.log", (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d"));
    let old_backup = format!("{}.log.1", (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d"));
    std::fs::write(dir.join(&old_log), "expired-content").unwrap();
    std::fs::write(dir.join(&old_backup), "expired-backup").unwrap();

    // 开启归档: 过期日志先压成 .zip 再删原文件
    let deleted = delete_old_logs(&dir, cutoff, true);
    assert_eq!(deleted, 2, "两个过期日志都应清理");
    assert!(!dir.join(&old_log).exists(), "原日志应被删除");
    assert!(dir.join(format!("{old_log}.zip")).exists(), "应先生成 zip 归档");
    assert!(!dir.join(&old_backup).exists(), "原滚动备份应被删除");
    assert!(dir.join(format!("{old_backup}.zip")).exists(), "滚动备份也应先生成 zip 归档");

    // 关闭归档: 直接删除，不产生 zip
    let recent_log = format!("{}.log", (cutoff - chrono::Duration::days(40)).format("%Y-%m-%d"));
    let _ = std::fs::remove_file(dir.join(format!("{recent_log}.zip")));
    std::fs::write(dir.join(&recent_log), "no-archive").unwrap();
    let deleted = delete_old_logs(&dir, cutoff, false);
    assert_eq!(deleted, 1);
    assert!(!dir.join(&recent_log).exists());
    assert!(!dir.join(format!("{recent_log}.zip")).exists(), "未开启归档时不应产生 zip");
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
    assert_eq!(deleted, 3, "应清理旧 mtime 自定义文件、紧凑日期文件与 .old: {deleted}");
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
    for _ in 0..50 {
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
    for name in ["CON", "con", "PRN", "AUX", "NUL", "COM1", "com3", "LPT9", "CON.txt", "nul.log"] {
        assert!(!is_valid_service_name(name), "should reject DOS device name: {}", name);
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
    assert_eq!(redact_url("https://example.com/app.exe"), "https://example.com/app.exe");
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
    assert_eq!(redact_url("https://user@example.com/a"), "https://example.com/a");
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
    let chars: Vec<char> = "abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789 .\\/:\t-_\u{1}\u{7f}中文"
        .chars().collect();
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
    assert!(is_user_writable(&d), "Everyone 可写目录必须判可写（拦截安装）");
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
    assert!(!is_user_writable(&d), "仅 SYSTEM/Administrators 的目录必须放行");
    let _ = std::fs::remove_dir_all(&dir);
}

// ==================== SDDL 解析（纯函数，直接验证解析器） ====================

#[test]
fn sddl_parse_detects_low_priv_write_aces() {
    // 攻击方 ACE: Everyone(WD)/Users(BU)/Authenticated Users(AU)/交互式(IU) 写
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;0x1301bf;;;WD)(A;;FA;;;SY)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;M;;;BU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FA;;;AU)"));
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FW;;;IU)"));
    // 攻击方显式账户 SID（非 RID 500/512）
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;FA;;;S-1-5-21-1111-2222-3333-1001)"));
    // 仅 SYSTEM/Administrators → 无低权限写
    assert!(!sddl_dacl_grants_non_admin_write("D:PAI(A;;FA;;;SY)(A;;FA;;;BA)"));
    assert!(!sddl_dacl_grants_non_admin_write("D:PAI(A;;FR;;;WD)(A;;FA;;;SY)"));
}

#[test]
fn sddl_parse_ignores_inherit_only_creator_owner_ace() {
    // 回归: Program Files 等标准 ACL 含 CREATOR OWNER 的 InheritOnly(IO) 全控 ACE，
    // 它只传播给子对象、不影响当前对象可写性，修复前会误判为"非管理员可写"导致 inplace 安装被拒
    assert!(!sddl_dacl_grants_non_admin_write("D:PAI(A;ID;FA;;;SY)(A;ID;FA;;;BA)(A;OICIIOID;GA;;;CO)(A;ID;0x1200a9;;;BU)"));
    // 非 InheritOnly 的 CREATOR OWNER 全控 ACE（当前对象生效）仍必须判可写
    assert!(sddl_dacl_grants_non_admin_write("D:PAI(A;;GA;;;CO)"));
}

#[test]
fn sddl_parse_owner_rules() {
    assert!(sddl_owner_is_administrative("O:BA"));
    assert!(sddl_owner_is_administrative("O:SY"));
    assert!(sddl_owner_is_administrative("O:S-1-5-80-956008885-3418522649-1831038044-1853292631-2271478464")); // TrustedInstaller
    assert!(!sddl_owner_is_administrative("O:WD"));
    assert!(!sddl_owner_is_administrative("O:BU"));
    assert!(!sddl_owner_is_administrative("O:S-1-5-21-1111-2222-3333-1001"));
}

// ==================== P0-2/P1-2/P1-4/P2-1/P2-2 安全修复回归 ====================

#[test]
fn secure_directory_removes_attacker_aces() {
    // 模拟攻击者预创建目录并留下 Everyone/Users 写 ACE: 加固后不得再允许低权限主体改写（P0-2）；
    // 非管理员环境无法加固（takeown 需要管理员），跳过
    let dir = unique_temp_dir("harden");
    let d = dir.to_string_lossy().to_string();
    assert!(icacls_ok(&[&d, "/grant", "*S-1-1-0:(OI)(CI)M", "/grant", "*S-1-5-32-545:(OI)(CI)M"]));
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
    use windows::Win32::System::Services::{SERVICE_ACCEPT_SHUTDOWN,
                                           SERVICE_ACCEPT_STOP, SERVICE_RUNNING,
                                           SERVICE_START_PENDING, SERVICE_STOPPED, SERVICE_STOP_PENDING,
    };
    // PENDING/STOPPED 阶段不得接受停止/关机控制码，PENDING 阶段 checkpoint 非零（P2-1）
    assert_eq!(scm_status_params(SERVICE_START_PENDING.0), (0, 1));
    assert_eq!(scm_status_params(SERVICE_STOP_PENDING.0), (0, 1));
    assert_eq!(scm_status_params(SERVICE_STOPPED.0), (0, 0));
    assert_eq!(scm_status_params(SERVICE_RUNNING.0), (SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0));
}

#[test]
fn is_valid_service_name_rejects_windows_reserved_chars() {
    // Windows 文件名保留字符: 服务名兼作 svcs 目录名（P2-2）
    for c in ['<', '>', ':', '"', '|', '?', '*'] {
        assert!(!is_valid_service_name(&format!("my-svc{}1", c)), "应拒绝字符: {c}");
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
    assert_eq!(cfg.service_executable_path, std::fs::canonicalize(&exe).unwrap().to_string_lossy().to_string());
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
    assert!(r.is_err(), "损坏的 toml 必须 panic（调用方捕获后按失效服务清理）");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn can_overwrite_source_same_and_different() {
    let dir = unique_temp_dir("overwrite");
    let a = dir.join("a.toml");
    let b = dir.join("b.toml");
    let c = dir.join("c.toml");
    let base = "service_name = \"x\"\nservice_display_name = \"X\"\nservice_description = \"d\"\nservice_executable_path = ";
    std::fs::write(&a, format!("{base}\"C:\\\\app.exe\"\nservice_executable_args = \"--a\"\n")).unwrap();
    std::fs::write(&b, format!("{base}\"C:\\\\app.exe\"\nservice_executable_args = \"--a\"\n")).unwrap();
    std::fs::write(&c, format!("{base}\"C:\\\\other.exe\"\n")).unwrap();
    let (sa, sb, sc) = (a.to_string_lossy(), b.to_string_lossy(), c.to_string_lossy());
    assert!(can_overwrite_source(&sa, &sb, "x")); // 同源 → 允许覆盖更新
    assert!(!can_overwrite_source(&sa, &sc, "x")); // 不同 exe → 拒绝
    // 已部署 toml 缺失 → 退回 ImagePath 归属判定；未注册服务名 → 不可覆盖
    let missing_path = dir.join("missing.toml");
    let missing = missing_path.to_string_lossy();
    assert!(!can_overwrite_source(&missing, &sa, "definitely-not-a-service"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sha256_matches_validates_file() {
    use sha2::{Digest, Sha256};
    let dir = unique_temp_dir("sha");
    let f = dir.join("payload.bin");
    std::fs::write(&f, b"hello osmium").unwrap();
    let hex: String = Sha256::digest(std::fs::read(&f).unwrap())
        .iter().map(|b| format!("{:02x}", b)).collect();
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
    assert_eq!(resolve_download_target(&rel, "C:\\deploy"), "C:\\deploy\\sub\\app.exe");

    let abs = ServiceConfig {
        download_url: Some("http://x/app.exe".into()),
        download_to: Some("C:\\abs\\app.exe".into()),
        service_executable_path: "C:\\ignored.exe".into(),
        ..Default::default()
    };
    assert_eq!(resolve_download_target(&abs, "C:\\deploy"), "C:\\abs\\app.exe");

    let name = ServiceConfig {
        download_url: Some("http://x/app.exe".into()),
        service_executable_path: "C:\\prog\\target.exe".into(),
        ..Default::default()
    };
    assert_eq!(resolve_download_target(&name, "C:\\deploy"), "C:\\deploy\\target.exe");
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

    assert_eq!(std::fs::read_to_string(dir.join("2026-08-02.log.3")).unwrap(), "backup-2");
    assert_eq!(std::fs::read_to_string(dir.join("2026-08-02.log.2")).unwrap(), "backup-1");
    assert!(std::fs::metadata(dir.join("2026-08-02.log.1")).unwrap().len() >= 1_000_000);
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
fn run_hook_executes_injects_env_and_logs() {
    let dir = unique_temp_dir("hook");
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false, pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
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
                    if stream.read(&mut buf).is_err() { continue; }
                    let req = String::from_utf8_lossy(&buf);
                    let len = server_data.len();
                    let (status, headers, body): (&str, String, &[u8]) = if req.starts_with("HEAD") {
                        ("200 OK", format!("Content-Length: {}\r\nAccept-Ranges: bytes\r\n", len), &[])
                    } else if let Some(range) = req.lines().find(|l| l.starts_with("Range: bytes=")) {
                        let spec = range.trim_start_matches("Range: bytes=");
                        let (a, b) = spec.split_once('-').unwrap();
                        let start: usize = a.parse().unwrap();
                        let end: usize = if b.is_empty() { len - 1 } else { b.parse().unwrap() };
                        (
                            "206 Partial Content",
                            format!("Content-Range: bytes {}-{}/{}\r\nContent-Length: {}\r\nAccept-Ranges: bytes\r\n",
                                start, end, len, end - start + 1),
                            &server_data[start..=end],
                        )
                    } else {
                        ("200 OK", format!("Content-Length: {}\r\nAccept-Ranges: bytes\r\n", len), server_data.as_slice())
                    };
                    let head = format!("HTTP/1.1 {}\r\n{}\r\n", status, headers);
                    if stream.write_all(head.as_bytes()).is_err() { continue; }
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
    let tmp = std::env::temp_dir().join("osmium-chunk-test.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16, None);
    stop.store(true, Ordering::Relaxed);
    let handled = server.join().unwrap();

    result.unwrap();
    let got = std::fs::read(&tmp).unwrap();
    assert_eq!(got, *data);
    // HEAD 探测 + 3 个分块请求；少于 4 说明分块路径未生效（回退单线程）
    assert!(handled >= 4, "expected HEAD + chunk requests, got {}", handled);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn expand_env_value_resolves_base_and_vars() {
    // %BASE% 特指部署目录
    assert_eq!(expand_env_value("D:/data/%BASE%/log", "C:\\deploy"), "D:/data/C:\\deploy/log");
    // 已定义环境变量正常展开（PATH 必存在）
    let path = std::env::var("PATH").unwrap_or_default();
    assert_eq!(expand_env_value("x%PATH%y", "base"), format!("x{path}y"));
    // 未定义变量展开为空串
    assert_eq!(expand_env_value("%OSMIUM_UNDEFINED_XYZ%", "base"), "");
    // 普通文本与中文原样保留
    assert_eq!(expand_env_value("C:\\程序\\run.exe", "base"), "C:\\程序\\run.exe");
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
    assert_eq!(std::fs::read_to_string(dir.join("2026-08-03.log.3")).unwrap(), "backup-2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn unzip_missing_plugin_reports_error() {
    // zip 解压经 osmium-kit-unzip 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin("unzip",
        &serde_json::json!({ "src": "C:\\x.zip", "dest": "C:\\out" }));
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
            ("200 OK".to_string(), vec![("Content-Length".into(), d2.len().to_string())], vec![])
        } else {
            ("200 OK".to_string(), vec![("Content-Length".into(), d2.len().to_string())], d2.clone())
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-norange.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16, None).unwrap();
    stop.store(true, Ordering::Relaxed);
    // HEAD 探测 + 1 次单线程 GET = 2 请求；分块路径会更多
    assert_eq!(count.load(Ordering::Relaxed), 2, "应走单线程回退（仅 HEAD+GET）");
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
            return ("200 OK".to_string(), vec![("Accept-Ranges".into(), "none".into())], vec![]);
        }
        let auth = lines.iter()
            .find(|l| l.to_ascii_lowercase().starts_with("authorization:"))
            .cloned().unwrap_or_default();
        if auth.contains("Basic dXNlcjpwYXNz") { // base64("user:pass")
            got.store(true, Ordering::Relaxed);
            ("200 OK".to_string(), vec![("Content-Length".into(), d2.len().to_string())], d2.clone())
        } else {
            ("401 Unauthorized".to_string(), vec![], b"denied".to_vec())
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-auth.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::Basic("user", "pass"), None, 16, None).unwrap();
    stop.store(true, Ordering::Relaxed);
    assert!(got_auth.load(Ordering::Relaxed), "服务器必须收到 Basic 认证头");
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn download_core_404_returns_err() {
    let (addr, stop, _count) = spawn_http_server(|_m, _l| {
        ("404 Not Found".to_string(), vec![("Content-Length".into(), "4".into())], b"nope".to_vec())
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-404.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16, None);
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
    let tmp = std::env::temp_dir().join("osmium-timeout.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = download_core(&url, tmp.to_str().unwrap(), 2, DownloadAuth::None, None, 16, None);
    stop.store(true, Ordering::Relaxed);
    assert!(matches!(result, Err((true, _))), "超时必须返回 (true, 消息)，实际 {:?}", result);
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
    let tmp = std::env::temp_dir().join("osmium-fallback.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16, None).unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(std::fs::read(&tmp).unwrap(), data, "回退后数据必须完整一致");
    let _ = std::fs::remove_file(&tmp);
}

// ==================== 日志底层: 分流 / 转义 / 空目录 / 归档失败 / 滚动空操作 ====================

#[test]
fn write_log_entry_splits_err_and_escapes() {
    let dir = unique_temp_dir("wlog");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions { split_out_err: true, max_size_mb: 0, backup_count: 0, zip_backup: false, pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
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
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false, pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
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
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false, pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    let start = Instant::now();
    // ping -t 永不退出，验证超时强杀后 run_hook 尽快返回
    run_hook(Some("ping -t 127.0.0.1"), "prestart", 800, dir.to_string_lossy().to_string(), None, &opts, None, None);
    let elapsed = start.elapsed();
    assert!(elapsed.as_secs() < 5, "超时钩子必须被强杀，实际耗时 {:?}", elapsed);
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let content = std::fs::read_to_string(dir.join(format!("{date}.log"))).unwrap();
    assert!(content.contains("timed out"), "日志应记录超时强杀: {content}");
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
    let tokens = ["0", "1", "2", "10", "999", "abc", "", "1.2.3", "0.0.1", "99999999999999999999"];
    for _ in 0..50_000 {
        let a = (0..(next() % 5) as usize).map(|_| tokens[(next() as usize) % tokens.len()]).collect::<Vec<_>>().join(".");
        let b = (0..(next() % 5) as usize).map(|_| tokens[(next() as usize) % tokens.len()]).collect::<Vec<_>>().join(".");
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
    assert_eq!(expand_env_value("%BASE%\\中文\\%PATH%", "D:\\d"), format!("D:\\d\\中文\\{path}"));
    // %PID% 占位符保留原样（停止命令执行时才替换，对应 WinSW #217）
    assert_eq!(expand_env_value("--pid %PID%", "C:\\base"), "--pid %PID%");
    assert_eq!(expand_env_value("%pid% %BASE%", "C:\\base"), "%pid% C:\\base");
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
    assert_eq!(resolve_download_target(&c, "C:\\deploy"), "C:\\deploy\\t.exe");
    // 无文件名（exe 路径以 \ 结尾的目录）→ Windows file_name 取最后一段目录名
    c.download_to = None;
    c.service_executable_path = "C:\\prog\\".into();
    assert_eq!(resolve_download_target(&c, "C:\\deploy"), "C:\\deploy\\prog");
    // UNC / 以 \ 开头的相对路径视为绝对
    c.download_to = Some("\\\\server\\share\\f.exe".into());
    assert_eq!(resolve_download_target(&c, "C:\\deploy"), "\\\\server\\share\\f.exe");
}

#[test]
fn redact_url_edge_cases() {
    // 内嵌凭据（user:pass@host）一并去除（防凭据进日志）
    assert_eq!(
        redact_url("https://user:pass@example.com/a?x=1#f"),
        "https://example.com/a"
    );
    assert_eq!(redact_url("http://example.com?only=query"), "http://example.com/");
    assert_eq!(redact_url("http://example.com#onlyfrag"), "http://example.com/");
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
    std::fs::write(&a, format!("{base}\"C:\\\\App.Exe\"\nservice_executable_args = \"--X\"\n")).unwrap();
    std::fs::write(&b, format!("{base}\"c:\\\\app.exe\"\nservice_executable_args = \"--x\"\n")).unwrap();
    // 路径与参数均忽略大小写 → 视为同源允许覆盖
    assert!(can_overwrite_source(&a.to_string_lossy(), &b.to_string_lossy(), "x"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn load_config_full_fields_roundtrip() {
    let dir = unique_temp_dir("cfgfull");
    let f = dir.join("full.toml");
    std::fs::write(&f, r#"
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
"#).unwrap();
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
    assert_eq!(scm_status_params(999), (SERVICE_ACCEPT_STOP | SERVICE_ACCEPT_SHUTDOWN, 0));
}

#[test]
fn collect_descendants_invalid_pid_empty() {
    assert!(collect_descendants(u32::MAX).is_empty(), "无效 pid 必须返回空且不 panic");
}

#[test]
fn sddl_malformed_inputs_no_panic() {
    for s in ["", "garbage", "D:", "D:PAI(", "D:PAI(A;;FA;;;SY)", "O:", "D:P(A;;GA;;;WD)", "(A;;FA;;;WD)"] {
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
    assert!(sha256_matches(&dir.join("nope.bin").to_string_lossy(), None));
    // 配置了校验值但文件缺失 → false
    assert!(!sha256_matches(&dir.join("nope.bin").to_string_lossy(), Some(&"0".repeat(64))));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn write_deployed_config_missing_source_false() {
    let dir = unique_temp_dir("cfgmiss");
    assert!(!write_deployed_config(&dir.join("nope.toml").to_string_lossy(), &dir.join("out.toml")));
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
    assert_eq!(dpapi_decrypt("enc:OSMIUM1:!!!not-base64!!!"), "enc:OSMIUM1:!!!not-base64!!!");
}

#[test]
fn decrypt_sensitive_covers_all_fields() {
    // 三个敏感字段逐一加密后统一解密还原；明文/无前缀值原样透传
    let enc_svc = dpapi_encrypt("svc-pass").unwrap();
    let enc_dl = dpapi_encrypt("dl-pass").unwrap();
    let enc_map = dpapi_encrypt("map-pass").unwrap();
    let mut config = ServiceConfig {
        service_password: Some(enc_svc.clone()),
        download_password: Some(enc_dl.clone()),
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
    let mappers = config.shared_directory_mappers.as_ref().unwrap();
    assert_eq!(mappers[0].password.as_deref(), Some("map-pass"));
    assert_eq!(mappers[1].password.as_deref(), Some("plain-map-pass"));
}

#[test]
fn write_deployed_config_encrypts_sensitive_fields() {
    let dir = unique_temp_dir("cryptcfg");
    let src = dir.join("src.toml");
    std::fs::write(&src, concat!(
        "service_name = \"crypt-svc\"\n",
        "service_display_name = \"Crypt\"\n",
        "service_description = \"x\"\n",
        "service_executable_path = \"C:\\\\app.exe\"\n",
        "service_password = \"svc-pass-123\"\n",
        "download_password = \"dl-pass-456\"\n",
    )).unwrap();
    let dst = dir.join("deployed.osiml");
    assert!(write_deployed_config(&src.to_string_lossy(), &dst));
    let text = std::fs::read_to_string(&dst).unwrap();
    assert!(!text.contains("svc-pass-123"), "部署文件不得含明文密码");
    assert!(!text.contains("dl-pass-456"));
    assert!(text.contains("enc:OSMIUM1:"), "部署文件应含 DPAPI 密文");
    // load_config 解密还原
    let cfg = load_config(&dst);
    assert_eq!(cfg.service_password.as_deref(), Some("svc-pass-123"));
    assert_eq!(cfg.download_password.as_deref(), Some("dl-pass-456"));
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
    let opts = LogOptions { split_out_err: true, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: "%Y%m%d".into(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    let main = current_log_name(&opts, "host", &now);
    assert_eq!(main, format!("{}.log", now.format("%Y%m%d")));
    let err = current_log_name(&opts, "err", &now);
    assert_eq!(err, format!("{}.err.log", now.format("%Y%m%d")));
}

#[test]
fn write_log_entry_uses_custom_pattern_and_reset() {
    let dir = unique_temp_dir("logpat");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: "%Y%m".into(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    write_log_entry(&d, "host", "custom-pattern-entry", &opts);
    let name = format!("{}.log", chrono::Local::now().format("%Y%m"));
    assert!(std::fs::read_to_string(dir.join(&name)).unwrap().contains("custom-pattern-entry"));
    // reset 清空当日文件
    let reset_opts = LogOptions { reset: true, ..opts };
    reset_current_logs(&d, &reset_opts);
    assert_eq!(std::fs::read_to_string(dir.join(&name)).unwrap(), "", "reset 应清空日志");
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
    assert!(process_working_set_mb(pid).is_some(), "当前进程内存采样应成功");
    // 不存在进程 → None 不 panic
    assert!(process_cpu_100ns(u32::MAX).is_none());
    assert!(process_working_set_mb(u32::MAX).is_none());
}

#[test]
fn netmap_missing_plugin_reports_error() {
    // 共享目录映射经 osmium-kit-netmap 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin("netmap",
        &serde_json::json!({ "action": "map", "mappers": [] }));
    assert!(err.is_err(), "未安装插件时映射必须失败");
    assert!(err.unwrap_err().contains("netmap"), "错误信息应含插件名");
}

#[test]
fn sspi_missing_plugin_reports_error() {
    // sspi 认证下载经 osmium-kit-sspi 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin("sspi",
        &serde_json::json!({ "url": "http://x", "to": "C:\\x" }));
    assert!(err.is_err(), "未安装插件时 sspi 下载必须失败");
    assert!(err.unwrap_err().contains("sspi"), "错误信息应含插件名");
}

#[test]
fn reboot_missing_plugin_reports_error() {
    // 系统重启经 osmium-kit-reboot 插件执行: 无插件时 run_plugin 必须明确报错（宿主侧安全降级）
    let err = crate::service_host::run_plugin("reboot", &serde_json::json!({}));
    assert!(err.is_err(), "未安装插件时重启必须失败");
    assert!(err.unwrap_err().contains("reboot"), "错误信息应含插件名");
}

#[test]
fn discover_plugins_returns_osx_entries_only() {
    // 扫描环境: 插件目录（exe 同目录 \exts）随运行环境而定——
    // 未安装插件时为空；若存在插件则逐项校验扩展名与目录跳过规则
    let plugins = crate::service_host::discover_plugins();
    for p in &plugins {
        assert_eq!(p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default(),
                   "osx", "发现的条目必须是 .osx 文件: {}", p.display());
    }
    // 安装环境（Publish\exts 存在真实插件）时不应发现隐藏目录条目
    let names: Vec<String> = plugins.iter().map(|p| p.file_name().unwrap_or_default().to_string_lossy().into_owned()).collect();
    assert!(!names.iter().any(|n| n.starts_with('.')), "不得包含隐藏条目: {names:?}");
}

#[test]
fn plugin_usable_rejects_inert_executable() {
    // 非协议可执行（cmd.exe 无 ping 响应）: 5 秒超时后判定不可用
    let cmd = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string())
        + "\\System32\\cmd.exe";
    assert!(!crate::service_host::plugin_usable(std::path::Path::new(&cmd)),
            "cmd.exe 不响应 ping 协议，必须判定不可用");
}

// ==================== 第二轮 WinSW 对齐: 冒烟 / 暴力 / 边缘测试 ====================

#[test]
fn auto_roll_logs_rolls_once_per_day() {
    reset_auto_roll_state();
    let dir = unique_temp_dir("autoroll");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: Some("00:00:00".into()), out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    // 构造"到达定点时刻后"的固定时间，并预写"到达前"的当日日志
    let date = "2026-08-11";
    let now = chrono::Local.with_ymd_and_hms(2026, 8, 11, 0, 0, 5).single().unwrap();
    std::fs::write(dir.join(format!("{date}.log")), "legacy-before-roll").unwrap();
    // 到达时刻后的首次写入 → 当日日志归档为 {date}.{HHmmss}.log
    auto_roll_logs(&d, &opts, &now);
    let archived = format!("{date}.000005.log");
    assert!(dir.join(&archived).exists(), "到达定点时刻后必须滚动归档");
    assert_eq!(std::fs::read_to_string(dir.join(&archived)).unwrap(), "legacy-before-roll");
    // 同日再次到达 → 防重复滚动（不产生新归档）
    auto_roll_logs(&d, &opts, &now);
    let others = std::fs::read_dir(&dir).unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.starts_with(&format!("{date}.")) && n.ends_with(".log") && n != archived
        })
        .count();
    assert_eq!(others, 0, "同日不得重复滚动");
    // 未到达时刻（早于 auto_roll_at）→ 不滚动
    reset_auto_roll_state();
    let opts_late = LogOptions { auto_roll_at: Some("23:59:59".into()), ..opts };
    let early = chrono::Local.with_ymd_and_hms(2026, 8, 12, 12, 0, 0).single().unwrap();
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
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false, out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    run_hook(Some("echo REDIRECTED-OUTPUT"), "prestart", 5000, d.clone(), None,
        &opts, Some(out_file.to_str().unwrap()), None);
    // 独立文件收到原始输出
    let content = std::fs::read_to_string(&out_file).unwrap();
    assert!(content.contains("REDIRECTED-OUTPUT"), "重定向文件必须含钩子输出");
    // 宿主日志不再有 hook 通道条目（输出已重定向；仅 host 通道的 executing/completed 保留）
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    let host_log = std::fs::read_to_string(dir.join(format!("{date}.log"))).unwrap();
    assert!(!host_log.contains("[hook]"), "重定向后宿主日志不应再有 hook 通道输出");
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
            ("200 OK".to_string(), vec![
                ("Accept-Ranges".into(), "bytes".into()),
                ("Content-Length".into(), d2.len().to_string()),
            ], vec![])
        } else {
            ("200 OK".to_string(), vec![("Content-Length".into(), d2.len().to_string())], d2.clone())
        }
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-t1.tmp");
    let _ = std::fs::remove_file(&tmp);
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 1, None).unwrap();
    stop.store(true, Ordering::Relaxed);
    assert_eq!(count.load(Ordering::Relaxed), 2, "threads=1 必须走单线程（仅 HEAD+GET）");
    assert_eq!(std::fs::read(&tmp).unwrap(), data);
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn process_cpu_sample_is_monotonic() {
    let pid = std::process::id();
    let first = process_cpu_100ns(pid).expect("首次采样应成功");
    thread::sleep(Duration::from_millis(150));
    let second = process_cpu_100ns(pid).expect("二次采样应成功");
    assert!(second >= first, "CPU 时间采样必须单调不减: {first} -> {second}");
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
    for input in ["", "\\", "\\\\", "a\\", "\\b", "domain\\", "a\\b\\c", " ", "\\\u{4e2d}\\\\"] {
        let _ = redact_url(&format!("http://{input}@host/x"));
    }
}

// ==================== WinSW 对齐补全: 启动参数/日志文件名/SDDL/preshutdown/runaway 启动清理 ====================

#[test]
fn build_child_command_injects_env_and_passes_args() {
    use std::collections::HashMap;
    let mut env = HashMap::new();
    env.insert("OSMIUM_TEST_VAR".to_string(), "hello-env".to_string());
    let mut cmd = build_child_command("cmd.exe", Some("/c echo %OSMIUM_TEST_VAR%"), ".", Some(&env), ".", true, true, true, None);
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
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
    std::fs::write(&cfg_path, format!(
        "service_name = \"sspi-rej\"\n\
         service_display_name = \"SSPI Reject\"\n\
         service_description = \"x\"\n\
         service_executable_path = '{}'\n\
         download_url = \"https://x/a.exe\"\n\
         download_to = \"a.exe\"\n\
         download_auth = \"sspi\"\n",
        std::env::current_exe().unwrap().display()
    )).unwrap();
    let mut host = crate::service_host::ServiceHost::new();
    assert!(!host.on_start_from(&cfg_path), "sspi 插件缺失时启动必须失败");
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
    assert!(log_text.contains("sspi"), "日志应含 sspi 失败详情: {log_text}");
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

/// 构造带 plugins 配置的宿主并启动（exe 用 cmd.exe /c exit 快速退出，避免拉起测试 harness）
fn start_host_with_plugins(plugins_toml: &str) -> (bool, String) {
    let dir = unique_temp_dir("plhost");
    let cfg_path = dir.join("svc.toml");
    std::fs::write(&cfg_path, format!(
        "service_name = \"pl-host\"\n\
         service_display_name = \"PL\"\n\
         service_description = \"d\"\n\
         service_executable_path = 'C:\\Windows\\System32\\cmd.exe'\n\
         service_executable_args = \"/c exit\"\n\
         {plugins_toml}"
    )).unwrap();
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
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"start_before\"\n");
    assert!(ok, "fail_on_error=false 时插件失败不得阻断启动");
    assert!(log_text.contains("nonexistent-kit"), "日志应记录失败的插件名: {log_text}");
    assert!(log_text.contains("non-fatal"), "应标记为 non-fatal: {log_text}");
}

#[test]
fn plugin_call_failure_fatal_blocks_start() {
    // 插件缺失 + fail_on_error=true（start 阶段）: 阻断启动
    let (ok, log_text) = start_host_with_plugins(
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"start_before\"\nfail_on_error = true\n");
    assert!(!ok, "fail_on_error=true 时插件失败必须阻断启动");
    assert!(log_text.contains("failed"), "日志应含失败详情: {log_text}");
}

#[test]
fn plugin_call_other_phase_does_not_block_start() {
    // stop 阶段配置的失败插件不得影响启动（phase 过滤生效）
    let (ok, _log) = start_host_with_plugins(
        "[[plugins]]\nkit = \"nonexistent-kit\"\nphase = \"stop_before\"\nfail_on_error = true\n");
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
    assert!(matches!(download_auth_from_entry(&e), DownloadAuth::Basic("DOMAIN\\u", "p")));
    e.auth = Some("Basic".into());
    e.password = None; // 清空密码（用户名保留）→ Basic("DOMAIN\u", "")
    assert!(matches!(download_auth_from_entry(&e), DownloadAuth::Basic("DOMAIN\\u", "")));
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
            DownloadConfig { from: "http://x/a".into(), to: "a.bin".into(), ..Default::default() },
            DownloadConfig { from: "http://x/b".into(), to: "b.bin".into(), sha256: Some("abc".into()), stage: Some("after_start".into()), ..Default::default() },
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
    let (addr, stop, _) = spawn_http_server(|_, _| ("304 Not Modified".into(), Vec::new(), Vec::new()));
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-304-test.tmp");
    let _ = std::fs::remove_file(&tmp);
    // 服务器对 If-Modified-Since 回 304 → download_core 删除 tmp 并视为成功（保留原目标文件）
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16,
        Some("Mon, 01 Jan 2024 00:00:00 GMT".into())).unwrap();
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
    download_core(&url, tmp.to_str().unwrap(), 30, DownloadAuth::None, None, 16,
        Some("Mon, 01 Jan 2024 00:00:00 GMT".into())).unwrap();
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
            from: "http://x/a.bin".into(), to: "a.bin".into(),
            auth: Some("basic".into()), sha256: Some("abc".into()),
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
    let base = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 5, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
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
    let opts = LogOptions { split_out_err: true, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: String::new(), err_filename: String::new(), roll_at_start: true, roll_period_days: 0, zip_date_format: String::new() };
    let date = chrono::Local::now().format("%Y-%m-%d").to_string();
    std::fs::write(dir.join(format!("{date}.log")), "main").unwrap();
    std::fs::write(dir.join(format!("{date}.err.log")), "err").unwrap();
    roll_logs_to_old(&d, &opts);
    assert_eq!(std::fs::read_to_string(dir.join(format!("{date}.log.old"))).unwrap(), "main");
    assert_eq!(std::fs::read_to_string(dir.join(format!("{date}.err.log.old"))).unwrap(), "err");
    // 二次启动 → 覆盖旧 .old
    std::fs::write(dir.join(format!("{date}.log")), "main2").unwrap();
    roll_logs_to_old(&d, &opts);
    assert_eq!(std::fs::read_to_string(dir.join(format!("{date}.log.old"))).unwrap(), "main2");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn roll_by_time_if_due_rolls_stale_log() {
    let dir = unique_temp_dir("rolltime");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 1, zip_date_format: String::new() };
    let now = chrono::Local.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).single().unwrap();
    let path = dir.join("2026-08-11.log");
    std::fs::write(&path, "stale").unwrap();
    // 文件 mtime 为当前 → 未到期不滚动
    roll_by_time_if_due(&d, &opts, &now);
    assert!(path.exists());
    // mtime 改到 3 天前 → 到期滚动为 {date}.{HHmmss}.log
    let old: std::time::SystemTime = (now - chrono::Duration::days(3)).into();
    std::fs::File::options().write(true).open(&path).unwrap().set_modified(old).unwrap();
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
    let expected = format!("2026-08-11.log.{}.zip", chrono::Local::now().format("%Y%m%d"));
    assert!(dir.join(&expected).exists(), "期望 zip 归档名: {}", expected);
    // 空格式 → 保持 {file}.zip
    assert!(zip_backup_file(&log, ""));
    assert!(dir.join("2026-08-11.log.zip").exists());
    let _ = std::fs::remove_dir_all(&dir);
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
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
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
    run_stop_command("powershell.exe", "-NoProfile -Command \"Start-Sleep -Seconds 60\"", 4242, 1, d.clone(), &opts);
    // 超时强杀路径已覆盖（run_stop_command 内部 terminate_pid_tree）
    let _ = child.kill();
    let _ = child.wait();
    // 日志断言: 快速命令有 "exited with code"，常驻命令有 "timed out"
    let now = chrono::Local::now();
    let log = dir.join(current_log_name(&opts, "host", &now));
    let content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(content.contains("exited with code 0"), "日志缺失快速退出记录: {}", content);
    assert!(content.contains("timed out after 1s, killing"), "日志缺失超时强杀记录: {}", content);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn run_stop_command_injects_child_pid() {
    let dir = unique_temp_dir("stoppid");
    let d = dir.to_string_lossy().to_string();
    let opts = LogOptions { split_out_err: false, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: String::new(), err_filename: String::new(), roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    // %PID% 占位符与 WINSGF_CHILD_PID 环境变量同时注入（echo 输出进日志可断言）
    run_stop_command("cmd.exe", "/c echo pid=%PID% env=%WINSGF_CHILD_PID%", 4242, 5, d.clone(), &opts);
    let now = chrono::Local::now();
    let log = dir.join(current_log_name(&opts, "host", &now));
    let content = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(content.contains("pid=4242 env=4242"), "日志缺失 PID 注入输出: {}", content);
    assert!(content.contains("Stop executable: cmd.exe /c echo pid=4242 env=%WINSGF_CHILD_PID%"),
        "日志应展示已展开 %PID% 的停止命令: {}", content);
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
    assert_eq!(expand_stop_pid(&expand_env_value("%PID%", "C:\\base"), 456), "456");
}

#[test]
fn runaway_exceeded_decides_limits() {
    assert!(runaway_exceeded(Some(200), Some(100), None, None));
    assert!(runaway_exceeded(None, None, Some(90.0), Some(50.0)));
    assert!(!runaway_exceeded(Some(50), Some(100), Some(10.0), Some(50.0)));
    assert!(!runaway_exceeded(None, None, None, None));
    assert!(!runaway_exceeded(None, Some(100), None, None)); // 采样缺失不触发
}

#[test]
fn runaway_cleanup_pid_file_terminates_leftover() {
    let dir = unique_temp_dir("runawaypid");
    // 无 pid 文件 → 无操作
    assert_eq!(runaway_cleanup_pid_file(&dir.join("missing.txt").to_string_lossy(), 5000, false, None).unwrap(), None);
    // 非法内容 → 告警
    std::fs::write(dir.join("bad.txt"), "not-a-pid").unwrap();
    assert!(runaway_cleanup_pid_file(&dir.join("bad.txt").to_string_lossy(), 5000, false, None).is_err());
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
    assert_eq!(runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 5000, false, Some("test-svc")).unwrap(), Some(pid));
    assert!(!process_alive(pid), "残留进程应已被终止");
    let _ = child.kill();
    let _ = child.wait();
    // 已退出/0 → 无操作
    std::fs::write(&pid_file, "0").unwrap();
    assert_eq!(runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 5000, false, None).unwrap(), None);
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
    let err = runaway_cleanup_pid_file(&pid_file.to_string_lossy(), 500, false, Some("my-svc")).unwrap_err();
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
    assert_eq!(runaway_cleanup_pid_file(&pid_file2.to_string_lossy(), 500, false, Some("my-svc")).unwrap(), Some(pid2));
    let _ = child2.kill();
    let _ = child2.wait();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn process_env_var_reads_child_environment() {
    // PEB 环境块读取: 读子进程注入的变量与真实 PATH（对齐 WinSW 防误杀校验的数据来源）
    let mut child = Command::new("powershell.exe")
        .args(["-NoProfile", "-Command", "Start-Sleep -Seconds 30"])
        .env("WINSGF_SERVICE_ID", "peb-test-svc")
        .creation_flags(0x08000000)
        .spawn().unwrap();
    let pid = child.id();
    thread::sleep(Duration::from_millis(300));
    assert_eq!(process_env_var(pid, "WINSGF_SERVICE_ID").as_deref(), Some("peb-test-svc"));
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
    let mut cmd = build_child_command("cmd.exe", Some("/c echo %BASE%+%WINSGF_SERVICE_ID%"), ".", None, "C:\\deploy", true, true, true, Some("svc-1"));
    let mut child = cmd.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    let _ = child.wait();
    assert!(out.contains("C:\\deploy+svc-1"), "BASE/WINSGF_SERVICE_ID 注入未生效: {}", out);
    // 用户显式配置 BASE（大小写不敏感）→ 以用户为准
    let mut env = HashMap::new();
    env.insert("base".to_string(), "user-base".to_string());
    let mut cmd2 = build_child_command("cmd.exe", Some("/c echo %BASE%"), ".", Some(&env), "C:\\deploy", true, true, true, None);
    let mut child2 = cmd2.stdout(std::process::Stdio::piped()).spawn().unwrap();
    let mut out2 = String::new();
    child2.stdout.take().unwrap().read_to_string(&mut out2).unwrap();
    let _ = child2.wait();
    assert!(out2.contains("user-base"), "用户 env 应覆盖自动 BASE: {}", out2);
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
    let opts = LogOptions { split_out_err: true, max_size_mb: 0, backup_count: 0, zip_backup: false,
        pattern: String::new(), auto_roll_at: None, out_enabled: true, err_enabled: true, reset: false,
        out_filename: "app.out.log".into(), err_filename: "app.err.log".into(),
        roll_at_start: false, roll_period_days: 0, zip_date_format: String::new() };
    assert_eq!(current_log_name(&opts, "host", &now), "app.out.log");
    assert_eq!(current_log_name(&opts, "out", &now), "app.out.log");
    assert_eq!(current_log_name(&opts, "err", &now), "app.err.log");
    // 未分流时 err 通道仍走主日志名
    let opts_merged = LogOptions { split_out_err: false, ..opts };
    assert_eq!(current_log_name(&opts_merged, "err", &now), "app.out.log");
}

#[test]
fn scm_status_params_honors_preshutdown_flag() {
    use windows::Win32::System::Services::{SERVICE_ACCEPT_PRESHUTDOWN, SERVICE_RUNNING};
    set_preshutdown_enabled(true);
    let (controls, _) = scm_status_params(SERVICE_RUNNING.0);
    assert_ne!(controls & SERVICE_ACCEPT_PRESHUTDOWN, 0, "preshutdown 开启时应上报接受码");
    set_preshutdown_enabled(false);
    let (controls, _) = scm_status_params(SERVICE_RUNNING.0);
    assert_eq!(controls & SERVICE_ACCEPT_PRESHUTDOWN, 0);
}

#[test]
fn security_descriptor_from_sddl_parses_valid_and_rejects_bad() {
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    let sd = security_descriptor_from_sddl("D:(A;;GA;;;SY)(A;;GA;;;BA)").expect("合法 SDDL 应解析成功");
    assert!(!sd.0.is_null());
    unsafe { let _ = LocalFree(Some(HLOCAL(sd.0))); }
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
    unsafe { std::env::set_var("OSMIUM_TEST_EXPAND", "hello"); }
    c.stop_executable = Some("%OSMIUM_TEST_EXPAND%\\stop.exe".into());
    let e = host.expand_config(&c);
    assert_eq!(e.service_executable_path, "C:\\base\\app.exe");
    assert_eq!(e.service_executable_args.as_deref(), Some("--cfg C:\\base\\cfg.ini"));
    assert_eq!(e.working_directory.as_deref(), Some("C:\\base\\work"));
    assert_eq!(e.download_url.as_deref(), Some("http://x/C:\\base/file.bin"));
    assert_eq!(e.download_to.as_deref(), Some("C:\\base\\target.bin"));
    assert_eq!(e.log_dir.as_deref(), Some("C:\\base\\logs"));
    assert_eq!(e.runaway_pid_file.as_deref(), Some("C:\\base\\svc.pid"));
    assert_eq!(e.stop_executable.as_deref(), Some("hello\\stop.exe"));
    unsafe { std::env::remove_var("OSMIUM_TEST_EXPAND"); }
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
        std::fs::write(&config_path, format!(
            "service_name = \"refresh-test\"\n\
             service_display_name = \"refresh-test\"\n\
             service_description = \"refresh-test\"\n\
             service_executable_path = 'C:\\Windows\\System32\\ping.exe'\n\
             service_executable_args = \"{args}\"\n\
             auto_refresh = true\n"
        )).unwrap();
    };
    write_cfg("-n 30 127.0.0.1");
    let mut host = ServiceHost::new();
    assert!(host.on_start_from(&config_path), "宿主应启动成功");
    let pid1 = host.child.as_ref().unwrap().id();
    assert!(process_alive(pid1), "子进程应运行中");

    // 修改配置（args 变化 → mtime 变化）→ 下一次 tick 应检测到并重启子进程
    write_cfg("-n 30 127.0.0.2");
    thread::sleep(Duration::from_millis(20)); // 文件系统 mtime 粒度兜底
    assert!(host.tick(), "tick 应返回 true（子进程仍在运行）");
    let pid2 = host.child.as_ref().unwrap().id();
    assert_ne!(pid1, pid2, "配置变化后子进程应被重启（PID 变化）");

    // 清理: 终止并回收子进程（stop_child_process 私有，直接 kill）
    if let Some(mut c) = host.child.take() {
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
    assert_eq!(crate::service_core::panic_msg(&String::from("boom2"), "fallback"), "boom2");
    assert_eq!(crate::service_core::panic_msg(&42u32, "fallback"), "fallback");
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
    assert_eq!(path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(), "panic.log");
    // 安装路径分支: 模拟 get_own_path 返回安装 exe（直接比对路径派生逻辑）
    let install = get_own_path();
    if install.eq_ignore_ascii_case("C:\\Program Files\\Osmium\\os.exe") {
        assert!(path.to_string_lossy().contains("ProgramData\\Osmium\\svcs"));
    }
    let _ = std::fs::remove_dir_all(&inplace);
}

#[test]
fn write_log_line_appends_dated_entry() {
    // 更新程序日志底层: 写入 yyyy-MM-dd.log，条目含时间戳与通道名
    let dir = unique_temp_dir("wlogline");
    crate::service_core::write_log_line(&dir, "updater", "test-entry");
    let today = chrono::Local::now().format("%Y-%m-%d");
    let content = std::fs::read_to_string(dir.join(format!("{today}.log"))).unwrap();
    assert!(content.contains("[updater]"), "应含通道名: {content}");
    assert!(content.contains("test-entry"), "应含消息: {content}");
    assert!(content.contains(&today.to_string()), "应含日期: {content}");
    let _ = std::fs::remove_dir_all(&dir);
}



