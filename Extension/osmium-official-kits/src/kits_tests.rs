// ==================== 单元 + 协议集成测试（覆盖 kits_core 全部功能与协议分发） ====================

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

use crate::kits_core::{
    MapperSpec, map_shared_directories, send_email_smtp, send_syslog_udp, split_credential,
    sspi_download_to_file, sspi_spn, unmap_shared_directories, unzip_to_dir,
};

/// 定位插件二进制: 集成测试环境用 CARGO_BIN_EXE 编译期路径；
/// 单元测试（deps 目录）向上一级取 target`<profile>`\ 下的同名产物
fn kit_bin() -> PathBuf {
    if let Some(p) = option_env!("CARGO_BIN_EXE_osmium-kit") {
        return PathBuf::from(p);
    }
    let exe = std::env::current_exe().expect("current exe 应可获取");
    exe.parent()
        .and_then(|p| p.parent())
        .expect("deps 目录向上一级应为 target 配置目录")
        .join("osmium-kit.exe")
}

/// 调用插件: 传入 stdin JSON（空串表示无输入），返回 (退出码, stdout, stderr)
fn invoke(json: &str) -> (i32, String, String) {
    let bin = kit_bin();
    if !bin.exists() {
        // 测试目标不会自动构建普通 bin（workspace 根 target），先构建当前包；
        // workspace 根 = 本包 Cargo.toml 目录上溯两级
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ws_root = manifest.parent().and_then(|p| p.parent()).unwrap();
        let status = Command::new("cargo")
            .current_dir(ws_root)
            .args(["build", "-p", "osmium-official-kits"])
            .status()
            .expect("cargo 应可执行");
        assert!(status.success(), "插件二进制构建失败");
    }
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("插件应可启动");
    if !json.is_empty() {
        child
            .stdin
            .take()
            .unwrap()
            .write_all(json.as_bytes())
            .unwrap();
    }
    drop(child.stdin.take());
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let code = child.wait().unwrap().code().unwrap_or(-1);
    (code, out.trim().to_string(), err.trim().to_string())
}

/// 本地 HTTP 测试服务器: handler 接收 (方法, 请求行列表)，返回 (状态行, 头部, 响应体)；
/// 返回 (地址, 停止标志, 已处理请求计数)
fn spawn_http_server<F>(handler: F) -> (std::net::SocketAddr, Arc<AtomicBool>, Arc<AtomicUsize>)
where
    F: Fn(&str, &[String]) -> (String, Vec<(String, String)>, Vec<u8>) + Send + Sync + 'static,
{
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
                    let mut body_len: usize = 0;
                    let mut head_end: usize = 0;
                    loop {
                        match stream.read(&mut tmp) {
                            Ok(0) => break,
                            Ok(n) => {
                                buf.extend_from_slice(&tmp[..n]);
                                // 请求头完整后解析 Content-Length，把 body 读满再处理
                                //（仅读头就 break 的话 body 是否到达取决于 TCP 分段的竞态）
                                if head_end == 0
                                    && let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n")
                                {
                                    head_end = pos + 4; // body 起始偏移 = 头结束位置
                                    let head = String::from_utf8_lossy(&buf);
                                    body_len = head
                                        .lines()
                                        .filter_map(|l| {
                                            l.strip_prefix("Content-Length:")
                                                .or_else(|| l.strip_prefix("content-length:"))
                                        })
                                        .filter_map(|v| v.trim().parse::<usize>().ok())
                                        .next()
                                        .unwrap_or(0);
                                }
                                if body_len > 0 && buf.len() - head_end >= body_len {
                                    break;
                                }
                                if body_len == 0 && head_end > 0 {
                                    break; // 无 body 的请求（GET 等）
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

#[test]
fn split_credential_parses_domain_user() {
    assert_eq!(
        split_credential("DOMAIN\\alice"),
        ("DOMAIN".to_string(), "alice".to_string())
    );
    assert_eq!(split_credential("bob"), ("".to_string(), "bob".to_string()));
    assert_eq!(
        split_credential("CORP\\a\\b"),
        ("CORP".to_string(), "a\\b".to_string())
    );
    assert_eq!(
        split_credential("user@example.com"),
        ("".to_string(), "user@example.com".to_string())
    );
}

#[test]
fn split_credential_bruteforce_no_panic() {
    let chars = ['a', '\\', ' ', '中', '\0', '\n'];
    let mut input = String::new();
    for _ in 0..32 {
        input.push(chars[(input.len() * 7) % chars.len()]);
    }
    let (domain, user) = split_credential(&input);
    assert!(domain.len() + user.len() <= input.len() + 1);
    assert_eq!(split_credential(""), ("".to_string(), "".to_string()));
    assert_eq!(split_credential("\\"), ("".to_string(), "".to_string()));
}

#[test]
fn sspi_spn_default_port_omits_and_custom_port_appends() {
    assert_eq!(sspi_spn("files.corp", "http", Some(80)), "HTTP/files.corp");
    assert_eq!(
        sspi_spn("files.corp", "https", Some(443)),
        "HTTP/files.corp"
    );
    assert_eq!(sspi_spn("files.corp", "http", None), "HTTP/files.corp");
    assert_eq!(
        sspi_spn("files.corp", "http", Some(8080)),
        "HTTP/files.corp:8080"
    );
    assert_eq!(
        sspi_spn("files.corp", "https", Some(8443)),
        "HTTP/files.corp:8443"
    );
}

#[test]
fn sspi_download_rejects_401_without_negotiate() {
    // 服务器始终 401 且无 WWW-Authenticate: Negotiate → SSPI 流程必须明确报错而非无限循环
    let (addr, stop, _count) =
        spawn_http_server(move |_method, _lines| ("401 Unauthorized".to_string(), vec![], vec![]));
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let tmp = std::env::temp_dir().join("osmium-kit-sspi-401.tmp");
    let _ = std::fs::remove_file(&tmp);
    let result = sspi_download_to_file(&url, tmp.to_str().unwrap(), None, None, None, 5);
    stop.store(true, Ordering::Relaxed);
    assert!(
        matches!(&result, Err(msg) if msg.contains("without Negotiate")),
        "401 无 Negotiate 挑战必须快速报错，实际 {:?}",
        result
    );
    let _ = std::fs::remove_file(&tmp);
}

#[test]
fn sspi_download_succeeds_without_auth_challenge() {
    // 服务器直接 200（无需认证）: SSPI 流程第一轮即成功，文件内容应完整落地
    let body = b"hello from sspi kit".to_vec();
    let body2 = body.clone();
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), body2.len().to_string())],
            body2.clone(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let target = std::env::temp_dir().join("osmium-kit-sspi-ok.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(&url, target.to_str().unwrap(), None, None, None, 5);
    stop.store(true, Ordering::Relaxed);
    assert!(result.is_ok(), "直接 200 应下载成功，实际 {:?}", result);
    assert_eq!(
        std::fs::read(&target).unwrap(),
        body,
        "文件内容应与服务器一致"
    );
    let _ = std::fs::remove_file(&target);
}

#[test]
fn sspi_download_writes_tmp_atomically_and_renames() {
    // 验证目标文件旁不残留 .download.tmp（原子改名语义）
    let body = vec![0x42u8; 4096];
    let body2 = body.clone();
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), body2.len().to_string())],
            body2.clone(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let target = std::env::temp_dir().join("osmium-kit-sspi-atomic.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(&url, target.to_str().unwrap(), None, None, None, 5);
    stop.store(true, Ordering::Relaxed);
    assert!(result.is_ok());
    let tmp_path = format!("{}.download.tmp", target.display());
    assert!(
        !std::path::Path::new(&tmp_path).exists(),
        "下载完成后不得残留 tmp 文件"
    );
    assert_eq!(std::fs::read(&target).unwrap(), body);
    let _ = std::fs::remove_file(&target);
}

#[test]
#[ignore = "环境依赖: 显式假凭据触发 AcquireCredentialsHandleW 0x8009030E（LSA 会话身份上下文）, 本机验证后保留"]
fn sspi_download_with_explicit_credentials_ok_on_anonymous_server() {
    // 显式凭据路径（DOMAIN\User + password 构造 SEC_WINNT_AUTH_IDENTITY_EXW）:
    // 服务器无需认证直接 200 时，凭据构造分支被完整执行且下载成功
    let body = b"credential branch payload".to_vec();
    let body2 = body.clone();
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), body2.len().to_string())],
            body2.clone(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let target = std::env::temp_dir().join("osmium-kit-sspi-cred.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(
        &url,
        target.to_str().unwrap(),
        Some("CORP\\alice"),
        Some("s3cret"),
        None,
        5,
    );
    stop.store(true, Ordering::Relaxed);
    assert!(
        result.is_ok(),
        "显式凭据 + 匿名 200 应成功，实际 {:?}",
        result
    );
    assert_eq!(std::fs::read(&target).unwrap(), body);
    let _ = std::fs::remove_file(&target);
}

#[test]
fn sspi_download_fails_when_proxy_unreachable() {
    // 代理参数生效验证: 指向本机无人监听的端口，连接必然失败（proxy 分支被真实执行）
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        ("200 OK".to_string(), vec![], b"unused".to_vec())
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let proxy = "http://127.0.0.1:9"; // DISCARD 端口: 无人监听
    let target = std::env::temp_dir().join("osmium-kit-sspi-proxy.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(&url, target.to_str().unwrap(), None, None, Some(proxy), 5);
    stop.store(true, Ordering::Relaxed);
    assert!(result.is_err(), "不可达代理必须报错，实际 {:?}", result);
    let _ = std::fs::remove_file(&target);
}

#[test]
fn unzip_to_dir_extracts_and_blocks_traversal() {
    // 正常 zip: 文件解压到目标目录，内容一致
    let dir = std::env::temp_dir().join(format!("osmium-kit-unzip-ok-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let zip_path = dir.join("ok.zip");
    let payload = b"unzip payload".to_vec();
    {
        use std::io::Write as _;
        let f = std::fs::File::create(&zip_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("sub/file.txt", opts).unwrap();
        zw.write_all(&payload).unwrap();
        zw.finish().unwrap();
    }
    let target = dir.join("out");
    std::fs::create_dir_all(&target).unwrap();
    unzip_to_dir(zip_path.to_str().unwrap(), target.to_str().unwrap()).unwrap();
    let extracted = std::fs::read(target.join("sub/file.txt")).unwrap();
    assert_eq!(extracted, payload);

    // 恶意 zip（..\ 穿越条目）: 必须拒绝且不落盘
    let evil_path = dir.join("evil.zip");
    {
        use std::io::Write as _;
        let f = std::fs::File::create(&evil_path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        zw.start_file("../evil.txt", opts).unwrap();
        zw.write_all(b"evil").unwrap();
        zw.finish().unwrap();
    }
    assert!(unzip_to_dir(evil_path.to_str().unwrap(), target.to_str().unwrap()).is_err());
    assert!(
        !dir.join("evil.txt").exists(),
        "zip-slip 条目不得写出目标目录"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn netmap_empty_input_succeeds_and_bad_share_reports_error() {
    // 空条目: 无操作即成功
    assert!(map_shared_directories(&[]).is_empty());
    assert!(unmap_shared_directories(&[]).is_empty());
    // 不存在的共享: 映射必然失败并返回可读错误（不 panic）
    let spec = MapperSpec {
        local_path: "Z:".into(),
        remote_path: r"\\nonexistent-host-zz\share".into(),
        username: None,
        password: None,
    };
    let errors = map_shared_directories(&[spec]);
    assert!(!errors.is_empty(), "映射不存在的共享必须报错");
    assert!(
        errors[0].contains("error"),
        "错误信息应含错误码: {}",
        errors[0]
    );
    let _ = unmap_shared_directories(&[MapperSpec {
        local_path: "Z:".into(),
        remote_path: String::new(),
        username: None,
        password: None,
    }]);
}

// ==================== 协议层集成测试（真实调用 osmium-kit 二进制） ====================

#[test]
fn ping_returns_ok() {
    let (code, out, _err) = invoke(r#"{"kit":"ping"}"#);
    assert_eq!(code, 0, "ping 应退出码 0");
    assert_eq!(out, r#"{"ok":true}"#);
}

#[test]
fn invalid_json_fails_with_stderr_and_ok_false() {
    let (code, out, err) = invoke("this is not json");
    assert_ne!(code, 0, "非法 JSON 应非零退出");
    assert!(out.contains(r#""ok":false"#), "stdout 应带 ok:false: {out}");
    assert!(err.contains("osmium-kit error"), "stderr 应抛出详情: {err}");
}

#[test]
fn unknown_kit_fails() {
    let (code, out, err) = invoke(r#"{"kit":"nonsense"}"#);
    assert_ne!(code, 0);
    assert!(
        out.contains("unknown kit"),
        "stdout 应含 unknown kit: {out}"
    );
    assert!(err.contains("osmium-kit error"));
}

#[test]
fn sspi_missing_to_field_fails() {
    let (code, out, err) = invoke(r#"{"kit":"sspi","url":"http://x"}"#);
    assert_ne!(code, 0);
    assert!(out.contains("missing 'to'"), "{out}");
    assert!(err.contains("osmium-kit error"));
}

#[test]
fn sspi_missing_url_field_fails() {
    let (code, out, err) = invoke(r#"{"kit":"sspi","to":"C:\\x"}"#);
    assert_ne!(code, 0);
    assert!(out.contains("missing 'url'"), "{out}");
    assert!(err.contains("osmium-kit error"));
}

#[test]
fn empty_input_silently_exits_without_output() {
    // 双击/无调用方场景: 必须静默退出（无 stdout/stderr）
    let (code, out, err) = invoke("");
    assert_eq!(code, 0, "空输入应退出码 0");
    assert_eq!(out, "", "不得输出任何内容");
    assert_eq!(err, "", "不得输出任何内容");
}

/// 喂超大输入: 子进程只读 1MB 即截断，剩余数据无人消费，写入可能报管道错误——
/// 循环分块写入并容忍 broken pipe（子进程读满后退出关闭管道）
fn invoke_large(data: &[u8]) -> (i32, String, String) {
    let mut child = Command::new(kit_bin())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("插件应可启动");
    if !data.is_empty() {
        let mut stdin = child.stdin.take().unwrap();
        for chunk in data.chunks(64 * 1024) {
            if stdin.write_all(chunk).is_err() {
                break; // 子进程已读满上限退出（管道关闭）
            }
        }
    }
    drop(child.stdin.take());
    let mut out = String::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_string(&mut out)
        .unwrap();
    let mut err = String::new();
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut err)
        .unwrap();
    let code = child.wait().unwrap().code().unwrap_or(-1);
    (code, out.trim().to_string(), err.trim().to_string())
}

#[test]
fn stdin_at_limit_parses_fine() {
    // 恰好 1MB（上限含边界）: 完整读取并正常处理
    let mut json = r#"{"kit":"ping","pad":""#.to_string();
    json.push_str(&"a".repeat(1024 * 1024 - 23)); // 前缀 21 字符 + 后缀 2 字符 → 总长恰好 1MB
    json.push_str("\"}");
    assert_eq!(json.len(), 1024 * 1024, "JSON 长度必须精确 1MB");
    let (code, out, _err) = invoke(&json);
    assert_eq!(code, 0, "上限内输入应成功");
    assert!(out.contains(r#""ok":true"#), "{out}");
}

#[test]
fn stdin_over_limit_truncates_and_fails_fast() {
    // 超过 1MB: 截断后 JSON 不完整 → 快速失败（不得卡死/撑爆内存）
    let mut data = b"{\"kit\":\"ping\",\"pad\":\"".to_vec();
    data.extend(std::iter::repeat_n(b'a', 1024 * 1024 + 64 * 1024));
    let (code, out, err) = invoke_large(&data);
    assert_ne!(code, 0, "超限输入应非零退出");
    assert!(out.contains("invalid request"), "应报解析失败: {out}");
    assert!(err.contains("osmium-kit error"), "stderr 应抛详情: {err}");
}

#[test]
fn netmap_bad_share_reports_error() {
    // 映射不存在的共享: 返回 ok:false + 失败明细
    let req = r#"{"kit":"netmap","action":"map","mappers":[{"local_path":"Z:","remote_path":"\\\\nonexistent-host-zz\\share"}]}"#;
    let (code, out, _err) = invoke(req);
    assert_ne!(code, 0, "映射失败应非零退出: {out}");
    assert!(out.contains(r#""ok":false"#));
    assert!(out.contains("details"), "应返回失败明细: {out}");
}

#[test]
fn netmap_empty_mappers_fails_with_clear_message() {
    // 空 mappers 列表无操作可言，必须明确报错（防宿主误以为映射成功）
    let (code, out, err) = invoke(r#"{"kit":"netmap","action":"unmap","mappers":[]}"#);
    assert_ne!(code, 0, "空 mappers 应失败: {out}");
    assert!(out.contains("no mappers"), "{out}");
    assert!(err.contains("osmium-kit error"));
}

#[test]
fn netmap_unknown_action_fails() {
    let (code, out, err) = invoke(
        r#"{"kit":"netmap","action":"frobnicate","mappers":[{"local_path":"Z:","remote_path":"\\\\srv\\share"}]}"#,
    );
    assert_ne!(code, 0);
    assert!(out.contains("unknown action"), "{out}");
    assert!(err.contains("osmium-kit error"));
}

#[test]
fn unzip_extracts_and_blocks_traversal() {
    // 构造正常 zip + 恶意 zip（zip-slip），协议层验证解压与拦截
    let dir = std::env::temp_dir().join(format!("osmium-kit-protocol-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    let ok_zip = dir.join("ok.zip");
    {
        let f = std::fs::File::create(&ok_zip).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file("sub/data.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"protocol payload").unwrap();
        zw.finish().unwrap();
    }
    let out_dir = dir.join("out");
    std::fs::create_dir_all(&out_dir).unwrap();
    let req = serde_json::json!({
        "kit": "unzip",
        "src": ok_zip.to_string_lossy(),
        "dest": out_dir.to_string_lossy(),
    })
    .to_string();
    let (code, out, _err) = invoke(&req);
    assert_eq!(code, 0, "正常解压应成功: {out}");
    assert_eq!(
        std::fs::read_to_string(out_dir.join("sub/data.txt")).unwrap(),
        "protocol payload"
    );

    let evil_zip = dir.join("evil.zip");
    {
        let f = std::fs::File::create(&evil_zip).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        zw.start_file("../evil.txt", zip::write::SimpleFileOptions::default())
            .unwrap();
        zw.write_all(b"evil").unwrap();
        zw.finish().unwrap();
    }
    let req = serde_json::json!({
        "kit": "unzip",
        "src": evil_zip.to_string_lossy(),
        "dest": out_dir.to_string_lossy(),
    })
    .to_string();
    let (code, out, err) = invoke(&req);
    assert_ne!(code, 0, "zip-slip 必须拒绝: {out}");
    assert!(out.contains(r#""ok":false"#));
    assert!(
        err.contains("osmium-kit error"),
        "stderr 应抛出错误详情: {err}"
    );
    assert!(!dir.join("evil.txt").exists(), "穿越文件不得写出");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sspi_download_succeeds_via_local_server() {
    // 本地服务器直接 200: SSPI 插件第一轮即成功，文件内容完整落地
    let body = b"sspi protocol payload".to_vec();
    let body2 = body.clone();
    let (addr, stop, _count) = spawn_http_server(move |_method, _lines| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), body2.len().to_string())],
            body2.clone(),
        )
    });
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let dir = std::env::temp_dir().join(format!("osmium-sspi-proto-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.bin");
    let req = serde_json::json!({
        "kit": "sspi",
        "url": url,
        "to": target.to_string_lossy(),
        "timeout_secs": 10,
    })
    .to_string();
    let (code, out, _err) = invoke(&req);
    stop.store(true, Ordering::Relaxed);
    assert_eq!(code, 0, "SSPI 下载应成功: {out}");
    assert_eq!(
        std::fs::read(&target).unwrap(),
        body,
        "文件内容应与服务器一致"
    );
    assert!(
        !target.with_extension("download.tmp").exists(),
        "成功下载后临时文件必须清理"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn sspi_401_without_challenge_fails_with_details() {
    // 服务器 401 且无 Negotiate 挑战: 必须快速失败（ok:false + stderr 详情）
    let (addr, stop, _count) =
        spawn_http_server(move |_method, _lines| ("401 Unauthorized".to_string(), vec![], vec![]));
    let url = format!("http://{}:{}", addr.ip(), addr.port());
    let dir = std::env::temp_dir().join(format!("osmium-sspi-401-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.bin");
    let req = serde_json::json!({
        "kit": "sspi",
        "url": url,
        "to": target.to_string_lossy(),
        "timeout_secs": 10,
    })
    .to_string();
    let (code, out, err) = invoke(&req);
    stop.store(true, Ordering::Relaxed);
    assert_ne!(code, 0, "401 无挑战应失败: {out}");
    assert!(out.contains("without Negotiate"), "{out}");
    assert!(err.contains("osmium-kit error"));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
#[ignore = "需要本机 IIS Windows 认证站点（匿名关、Negotiate/NTLM）"]
fn sspi_download_authenticates_against_real_iis() {
    // 真机回归: IIS 站点 Windows 身份验证 → 完整 Negotiate/NTLM 挑战循环 → 200 下载落地。
    // 站点要求: 匿名关闭 + Windows 认证启用，绑定 8808 端口， 根目录放 test.txt（内容须与下方断言一致）
    let url = "http://localhost:8808/test.txt";
    let dir = std::env::temp_dir().join(format!("osmium-sspi-iis-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let target = dir.join("out.txt");
    let req = serde_json::json!({
        "kit": "sspi",
        "url": url,
        "to": target.to_string_lossy(),
        "timeout_secs": 15,
    })
    .to_string();
    let (code, out, err) = invoke(&req);
    assert_eq!(code, 0, "IIS 认证下载应成功: {out} {err}");
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "IIS SSPI test payload from default app pool.",
        "内容应与站点文件一致"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn notify_webhook_posts_json_text() {
    // notify kit: 本地 HTTP 服务器收到 POST application/json，body 含 {"text": ...}
    let received = Arc::new(std::sync::Mutex::new(String::new()));
    let r2 = received.clone();
    let (addr, stop, _count) = spawn_http_server(move |method, lines| {
        if method == "POST" {
            let req = lines.join("\n");
            *r2.lock().unwrap() = req;
        }
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), "2".into())],
            b"ok".to_vec(),
        )
    });
    crate::kits_core::notify_webhook(
        &format!("http://{}:{}/hook", addr.ip(), addr.port()),
        "service crash: code 1",
        10,
        None,
    )
    .unwrap();
    stop.store(true, Ordering::Relaxed);
    let req = received.lock().unwrap().clone();
    assert!(req.starts_with("POST /hook"), "应为 POST: {req}");
    assert!(
        req.contains("content-type: application/json"),
        "应带 JSON 头: {req}"
    );
    assert!(
        req.contains("service crash: code 1"),
        "body 应含通知文本: {req}"
    );
}

#[test]
fn notify_webhook_reports_http_error() {
    // notify kit: 服务器回 500 → 返回错误（不 panic）
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).unwrap();
        let body = "err";
        let resp = format!(
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        let _ = stream.write_all(resp.as_bytes());
        // 等客户端读完响应再关闭，避免 RST 被误报为 IO 错误（10053）
        thread::sleep(Duration::from_millis(300));
    });
    let err = crate::kits_core::notify_webhook(&format!("http://{addr}/hook"), "boom", 10, None)
        .expect_err("500 应报错");
    assert!(err.contains("500"), "错误应含状态码: {err}");
    handle.join().unwrap();
}

#[test]
fn notify_error_redacts_userinfo() {
    // notify 失败时错误消息不得含 URL 内嵌凭据（防泄漏）
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = stream.read(&mut buf).unwrap();
        let resp =
            "HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = stream.write_all(resp.as_bytes());
        // 等客户端读完响应再关闭，避免 RST 被误报为 IO 错误（10053）
        thread::sleep(Duration::from_millis(300));
    });
    let err = crate::kits_core::notify_webhook(
        &format!("http://user:secret@{}:{}/hook", addr.ip(), addr.port()),
        "boom",
        5,
        None,
    )
    .expect_err("500 应报错");
    assert!(!err.contains("secret"), "错误消息不得含密码: {err}");
    handle.join().unwrap();
}

#[test]
fn smtp_sends_full_session_to_local_server() {
    // smtp kit: 本地 SMTP 会话服务器完整应答，断言命令序列与邮件内容
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 65536];
        let mut sent = String::new();
        let mut read_cmd = |stream: &mut std::net::TcpStream, sent: &mut String| {
            let n = stream.read(&mut buf).unwrap();
            if n == 0 {
                return false;
            }
            sent.push_str(&String::from_utf8_lossy(&buf[..n]));
            true
        };
        stream.write_all(b"220 localhost ESMTP\r\n").unwrap();
        // EHLO → 250（多行） / MAIL → 250 / RCPT → 250 / DATA → 354 / 邮件体+点行 → 250 / QUIT → 221
        let replies: [&[u8]; 6] = [
            b"250-localhost\r\n250-AUTH PLAIN\r\n250 OK\r\n",
            b"250 OK\r\n",
            b"250 OK\r\n",
            b"354 End data with <CR><LF>.<CR><LF>\r\n",
            b"250 OK\r\n",
            b"221 Bye\r\n",
        ];
        for reply in replies {
            if !read_cmd(&mut stream, &mut sent) {
                break;
            }
            stream.write_all(reply).unwrap();
        }
        sent
    });
    send_email_smtp(
        &format!("127.0.0.1:{}", addr.port()),
        "alerts@example.com",
        "ops@example.com",
        "Test Alert",
        "service crashed line1\nline2",
        None,
        None,
        10,
    )
    .expect("smtp 会话应成功");
    let sent = handle.join().unwrap();
    assert!(sent.contains("EHLO osmium"), "应发 EHLO: {sent}");
    assert!(
        sent.contains("MAIL FROM:<alerts@example.com>"),
        "应发 MAIL FROM: {sent}"
    );
    assert!(
        sent.contains("RCPT TO:<ops@example.com>"),
        "应发 RCPT TO: {sent}"
    );
    assert!(sent.contains("Subject: Test Alert"), "应含主题: {sent}");
    assert!(
        sent.contains("service crashed line1\r\nline2"),
        "body 换行应规范化为 CRLF: {sent}"
    );
}

#[test]
fn smtp_reports_server_error() {
    // smtp kit: 服务器拒绝 MAIL FROM → 返回明确错误
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        stream.write_all(b"220 localhost ESMTP\r\n").unwrap();
        let _ = stream.read(&mut buf).unwrap();
        stream.write_all(b"250 OK\r\n").unwrap();
        let _ = stream.read(&mut buf).unwrap();
        stream.write_all(b"550 relay denied\r\n").unwrap();
    });
    let err = send_email_smtp(
        &format!("127.0.0.1:{}", addr.port()),
        "a@b.c",
        "d@e.f",
        "t",
        "m",
        None,
        None,
        5,
    )
    .expect_err("550 应报错");
    assert!(err.contains("550"), "错误应含状态码: {err}");
    handle.join().unwrap();
}

#[test]
fn syslog_sends_rfc5424_udp_frame() {
    // syslog kit: 本地 UDP 收包，断言 PRI/TAG/内容与 RFC 5424 结构
    use std::net::UdpSocket;
    let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let mut buf = [0u8; 8192];
        let (n, _) = socket.recv_from(&mut buf).unwrap();
        String::from_utf8_lossy(&buf[..n]).into_owned()
    });
    send_syslog_udp(
        &format!("127.0.0.1:{}", addr.port()),
        "service crashed",
        3,
        2,
        "Osmium",
        5,
    )
    .expect("syslog 发送应成功");
    let frame = handle.join().unwrap();
    // PRI = facility(3)*8 + severity(2) = 26
    assert!(frame.starts_with("<26>1 "), "PRI+版本应正确: {frame}");
    assert!(
        frame.contains("Z ") || frame.contains("Z"),
        "时间戳应带 Z: {frame}"
    );
    assert!(frame.contains("Osmium"), "应含 TAG: {frame}");
    assert!(frame.contains("service crashed"), "应含消息: {frame}");
}

#[test]
fn syslog_rejects_invalid_port() {
    // syslog kit: 非法端口格式应快速失败（host:abc）
    let err = send_syslog_udp("127.0.0.1:abc", "x", 3, 2, "t", 2).unwrap_err();
    assert!(err.contains("invalid port"), "错误应含端口解析信息: {err}");
}

#[test]
fn notify_payload_formats() {
    // notify 平台格式: generic/teams/discord/feishu 各自 JSON 结构
    use crate::kits_core::notify_payload;
    let g: serde_json::Value = serde_json::from_str(&notify_payload("hi", "generic")).unwrap();
    assert_eq!(g["text"], "hi");
    let d: serde_json::Value = serde_json::from_str(&notify_payload("hi", "DISCORD")).unwrap();
    assert_eq!(d["content"], "hi", "discord 大小写不敏感");
    let f: serde_json::Value = serde_json::from_str(&notify_payload("hi", "feishu")).unwrap();
    assert_eq!(f["msg_type"], "text");
    assert_eq!(f["content"]["text"], "hi");
    let t: serde_json::Value = serde_json::from_str(&notify_payload("hi", "teams")).unwrap();
    assert_eq!(t["@type"], "MessageCard");
    assert_eq!(t["text"], "hi");
}

#[test]
fn probe_redis_and_tcp_via_local_server() {
    // probe kit: 本地 TCP 服务器回 +PONG → redis 探针成功；纯 tcp 连接成功即健康
    use crate::kits_core::probe_target;
    use std::net::TcpListener;
    use std::thread;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut buf = [0u8; 64];
        let _ = stream.read(&mut buf).unwrap();
        let _ = stream.write_all(b"+PONG\r\n");
    });
    assert!(
        probe_target("redis", "127.0.0.1", addr.port(), 5).is_ok(),
        "redis PING 应成功"
    );
    handle.join().unwrap();
    // 关闭端口 → 连接失败
    assert!(
        probe_target("tcp", "127.0.0.1", 1, 2).is_err(),
        "关闭端口应失败"
    );
}

#[test]
fn probe_mysql_honors_packet_header_offset() {
    // B6 回归: MySQL 初始握书包 payload 前有 4 字节包头（3 长度 + 1 序号），
    // 协议版本 0x0a 在 buf[4]——旧实现检查 buf[0]（长度字节低位）恒误报
    use crate::kits_core::probe_target;
    use std::net::TcpListener;
    use std::thread;
    // 标准握手包: 长度=0x0a+负载（此处简化），序号 0，payload[0]=0x0a
    let handshake = [0x14, 0x00, 0x00, 0x00, 0x0a, 0x38, 0x2e, 0x30];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.write_all(&handshake);
    });
    assert!(
        probe_target("mysql", "127.0.0.1", addr.port(), 5).is_ok(),
        "带包头的握手包应识别协议版本"
    );
    handle.join().unwrap();
    // 错误包: payload[0]=0xff（buf[4]），错误消息从 buf[5] 开始
    let err_pkt = [0x03, 0x00, 0x00, 0x00, 0xff, 0x64];
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let _ = stream.write_all(&err_pkt);
    });
    let result = probe_target("mysql", "127.0.0.1", addr.port(), 5);
    assert!(result.is_err(), "0xff 错误包应失败");
    assert!(
        result.unwrap_err().contains("mysql server error"),
        "错误消息应取自包头之后"
    );
    handle.join().unwrap();
}

#[test]
fn sspi_download_follows_redirect_manually_to_final_source() {
    // max_redirects=0 后由插件手动跟随: 首响应 302（相对 Location）→ 最终 200 内容落地；
    // 未实现手动跟随会直接报 "server returned HTTP 302"
    let body = b"redirected payload".to_vec();
    let body2 = body.clone();
    let (addr, stop, count) = spawn_http_server(move |_method, lines| {
        let path = lines
            .iter()
            .find(|l| l.to_ascii_lowercase().starts_with("get "))
            .map(|l| l.split_whitespace().nth(1).unwrap_or("").to_string())
            .unwrap_or_default();
        if path.ends_with("/final") {
            return (
                "200 OK".to_string(),
                vec![("Content-Length".into(), body2.len().to_string())],
                body2.clone(),
            );
        }
        // 相对 Location: 验证 RFC 3986 join 解析
        (
            "302 Found".to_string(),
            vec![("Location".into(), "/final".into())],
            vec![],
        )
    });
    let url = format!("http://{}:{}/start", addr.ip(), addr.port());
    let target = std::env::temp_dir().join("osmium-kit-sspi-redirect.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(&url, target.to_str().unwrap(), None, None, None, 5);
    stop.store(true, Ordering::Relaxed);
    assert!(
        result.is_ok(),
        "302 应被手动跟随到最终源，实际 {:?}",
        result
    );
    assert_eq!(std::fs::read(&target).unwrap(), body);
    assert!(
        count.load(Ordering::Relaxed) >= 2,
        "重定向链应产生至少两次请求"
    );
    let _ = std::fs::remove_file(&target);
}

#[test]
fn sspi_download_truncated_body_fails_and_leaves_no_target() {
    // 截断对照: 声明 Content-Length 大于实际响应体（连接干净关闭）→ 按失败处理，
    // 目标文件不得落地、tmp 不得残留（无 sha 配置时防静默损坏被执行）
    let body = b"short".to_vec();
    let (addr, stop, _count) = spawn_http_server(move |_m, _l| {
        (
            "200 OK".to_string(),
            vec![("Content-Length".into(), "1024".to_string())],
            body.clone(),
        )
    });
    let url = format!("http://{}:{}/trunc.bin", addr.ip(), addr.port());
    let target = std::env::temp_dir().join("osmium-kit-sspi-trunc.bin");
    let _ = std::fs::remove_file(&target);
    let result = sspi_download_to_file(&url, target.to_str().unwrap(), None, None, None, 5);
    stop.store(true, Ordering::Relaxed);
    let err = result.unwrap_err();
    // 截断的两种表现: 长度对照命中（truncated）或对端断开导致写失败（Peer disconnected）——均拒绝落盘
    assert!(
        err.contains("truncated") || err.contains("failed to write"),
        "应按截断失败处理，实际: {err}"
    );
    assert!(!target.exists(), "截断下载不得改名落盘");
    let tmp_path = format!("{}.download.tmp", target.display());
    assert!(!std::path::Path::new(&tmp_path).exists(), "tmp 应被清理");
}
