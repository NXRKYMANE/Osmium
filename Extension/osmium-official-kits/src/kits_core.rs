// ==================== osmium-official-kits 核心共享实现 ====================
// 与主程序 service_core.rs 同构: 共享逻辑集中单文件，main.rs 只做协议分发入口

// ==================== SSPI（Negotiate/NTLM）认证下载 ====================
// 自 service_core.rs 搬迁: 独立进程内完成完整 401 挑战-响应循环（凭据上下文进程内自持）

use std::io::{BufRead, Read, Write};
use std::time::Duration;

use windows::Win32::Security::Authentication::Identity::{
    AcquireCredentialsHandleW, DeleteSecurityContext, FreeContextBuffer, FreeCredentialsHandle,
    ISC_REQ_ALLOCATE_MEMORY, ISC_REQ_CONFIDENTIALITY, ISC_REQ_INTEGRITY, ISC_REQ_MUTUAL_AUTH,
    InitializeSecurityContextW, SEC_WINNT_AUTH_IDENTITY_EXW, SEC_WINNT_AUTH_IDENTITY_VERSION,
    SECBUFFER_TOKEN, SECPKG_CRED_OUTBOUND, SecBuffer, SecBufferDesc,
};
use windows::Win32::Security::Credentials::SecHandle;
use windows::core::PCWSTR;

/// 构造下载 Agent（全局超时覆盖整个下载；4xx/5xx 不转错误，
/// 401/200 均需读取原始响应）
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

/// 执行一次带 SSPI 认证的完整下载（URL → 目标文件）: 401 挑战-响应循环（最多 3 轮），
/// 凭据缺省用当前进程身份；目标文件以 .download.tmp 原子写入（CreateNew 防 TOCTOU）后改名
pub fn sspi_download_to_file(
    url: &str,
    to: &str,
    username: Option<&str>,
    password: Option<&str>,
    proxy: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    use base64::Engine as _;

    let client = build_agent(timeout_secs, proxy);

    // tmp 原子创建: 拒绝预创建文件替换；残留同名文件清理后重试一次
    let tmp = format!("{}.download.tmp", to);
    let create = || {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
    };
    let mut file = match create() {
        Ok(f) => f,
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            create().map_err(|e| format!("cannot create temporary file '{0}': {e}", tmp))?
        }
    };

    // SPN: HTTP/<host>，非默认端口拼 :port（Kerberos 匹配服务注册名）
    let uri = url::Url::parse(url).map_err(|e| format!("invalid URL: {e}"))?;
    let host = uri.host_str().unwrap_or("localhost").to_string();
    let spn = sspi_spn(&host, uri.scheme(), uri.port());
    let spn_wide = to_wide(&spn);

    // 凭据: 缺省用当前会话；提供 user/pass 时构造身份结构（DOMAIN\User 或 user）。
    // 身份缓冲（宽字符串 + 结构）必须存活到 AcquireCredentialsHandleW 调用结束，故在闭包内构造
    let package = to_wide("Negotiate");
    let mut cred = SecHandle::default();
    let acquire_result = unsafe {
        let mut expiry: i64 = 0;
        let auth_ptr: Option<*const core::ffi::c_void> = match (username, password) {
            (Some(u), Some(p)) => {
                let (domain, user) = split_credential(u);
                let mut user_w = to_wide(&user);
                let mut domain_w = to_wide(&domain);
                let mut pass_w = to_wide(p);
                let mut identity = SEC_WINNT_AUTH_IDENTITY_EXW {
                    Version: SEC_WINNT_AUTH_IDENTITY_VERSION,
                    Length: size_of::<SEC_WINNT_AUTH_IDENTITY_EXW>() as u32,
                    User: user_w.as_mut_ptr(),
                    UserLength: user_w.len().saturating_sub(1) as u32,
                    Domain: domain_w.as_mut_ptr(),
                    DomainLength: domain_w.len().saturating_sub(1) as u32,
                    Password: pass_w.as_mut_ptr(),
                    PasswordLength: pass_w.len().saturating_sub(1) as u32,
                    // SEC_WINNT_AUTH_IDENTITY_UNICODE = 0x2
                    Flags: 0x2,
                    PackageList: std::ptr::null_mut(),
                    PackageListLength: 0,
                };
                Some(&mut identity as *mut _ as *const core::ffi::c_void)
            }
            _ => None,
        };
        AcquireCredentialsHandleW(
            PCWSTR::null(),
            PCWSTR::from_raw(package.as_ptr()),
            SECPKG_CRED_OUTBOUND,
            None,
            auth_ptr,
            None,
            None,
            &mut cred,
            Some(&mut expiry),
        )
    };
    if let Err(e) = acquire_result {
        return Err(format!(
            "AcquireCredentialsHandleW failed (0x{:08X}). The current process may lack a usable Windows identity.",
            e.code().0 as u32
        ));
    }
    // 句柄守卫: 所有退出路径（成功/报错/? 提前返回）自动释放凭据句柄与最终安全上下文
    let mut guard = SspiGuard { cred, ctx: None };

    // 挑战-响应循环: 无头 → 401+challenge → 送 token → 200 读 body
    let mut token: Option<Vec<u8>> = None;
    for _ in 0..3 {
        let mut req = client.get(url);
        if let Some(t) = &token {
            let b64 = base64::engine::general_purpose::STANDARD.encode(t);
            req = req.header("authorization", format!("Negotiate {b64}"));
        }
        let resp = req
            .call()
            .map_err(|e| format!("request failed for {url}: {e}"))?;
        if resp.status().is_success() {
            let mut reader = resp.into_body().into_reader();
            std::io::copy(&mut reader, &mut file)
                .map_err(|e| format!("failed to write '{to}': {e}"))?;
            drop(file);
            std::fs::rename(&tmp, to).map_err(|e| format!("rename failed for '{to}': {e}"))?;
            return Ok(());
        }
        if resp.status().as_u16() != 401 {
            return Err(format!(
                "server returned HTTP {} for {url}",
                resp.status().as_u16()
            ));
        }
        // 401: 取 WWW-Authenticate: Negotiate/NTLM [challenge]
        let challenge = resp
            .headers()
            .get_all("www-authenticate")
            .iter()
            .filter_map(|v| v.to_str().ok())
            .find(|v| {
                v.to_lowercase().starts_with("negotiate") || v.to_lowercase().starts_with("ntlm")
            })
            .map(|s| s.to_string());
        let Some(challenge) = challenge else {
            return Err("server returned 401 without Negotiate/NTLM challenge".into());
        };
        // 读完 401 响应体后连接才归还池: NTLM 认证状态绑定 TCP 连接，
        // 不读 body 每次都是新连接，IIS 无法关联 Type1/Type2/Type3 序列
        let mut sink = std::io::sink();
        let _ = std::io::copy(&mut resp.into_body().into_reader(), &mut sink);
        let input = challenge
            .split_whitespace()
            .nth(1)
            .and_then(|b| base64::engine::general_purpose::STANDARD.decode(b).ok())
            .unwrap_or_default();

        // InitializeSecurityContext: 输入挑战 token，输出响应 token（SSPI 分配内存）
        let (input_desc, mut input_buffer);
        let input_ptr: Option<*const SecBufferDesc> = if input.is_empty() && guard.ctx.is_none() {
            None
        } else {
            input_buffer = SecBuffer {
                cbBuffer: input.len() as u32,
                BufferType: SECBUFFER_TOKEN,
                pvBuffer: input.as_ptr() as *mut core::ffi::c_void,
            };
            input_desc = SecBufferDesc {
                ulVersion: 0,
                cBuffers: 1,
                pBuffers: &mut input_buffer,
            };
            Some(&input_desc as *const _)
        };
        let mut out_buffer = SecBuffer {
            cbBuffer: 0,
            BufferType: SECBUFFER_TOKEN,
            pvBuffer: std::ptr::null_mut(),
        };
        let mut out_desc = SecBufferDesc {
            ulVersion: 0,
            cBuffers: 1,
            pBuffers: &mut out_buffer,
        };
        let mut attrs: u32 = 0;
        let mut new_ctx = SecHandle::default();
        let status = unsafe {
            InitializeSecurityContextW(
                Some(&guard.cred),
                guard.ctx.as_ref().map(|c| c as *const SecHandle),
                Some(spn_wide.as_ptr()),
                ISC_REQ_ALLOCATE_MEMORY
                    | ISC_REQ_CONFIDENTIALITY
                    | ISC_REQ_MUTUAL_AUTH
                    | ISC_REQ_INTEGRITY,
                0,
                0,
                input_ptr,
                0,
                Some(&mut new_ctx),
                Some(&mut out_desc),
                &mut attrs,
                None,
            )
        };
        // 拷贝输出 token 后释放 SSPI 分配的内存
        let out_token = if out_buffer.pvBuffer.is_null() {
            None
        } else {
            Some(unsafe {
                std::slice::from_raw_parts(
                    out_buffer.pvBuffer as *const u8,
                    out_buffer.cbBuffer as usize,
                )
                .to_vec()
            })
        };
        if !out_buffer.pvBuffer.is_null() {
            unsafe {
                let _ = FreeContextBuffer(out_buffer.pvBuffer);
            }
        }
        // 上下文轮换: 替换出的旧句柄立即显式释放；new_ctx 无论成败都交守卫在退出时统一释放，
        // 失败路径（SSPI 可能已分配内存）也能保证 DeleteSecurityContext
        if let Some(old) = guard.ctx.replace(new_ctx) {
            unsafe {
                let _ = DeleteSecurityContext(&old);
            }
        }
        let code = status.0 as u32;
        if !matches!(code, 0 | 0x0009_0312) {
            return Err(format!("InitializeSecurityContextW failed 0x{code:08X}"));
        }
        token = out_token;
    }
    Err("authentication exceeded 3 challenge rounds — server likely rejected the credentials or the negotiation is unsupported".into())
}

/// SSPI 句柄守卫: 退出时统一释放凭据句柄与安全上下文，
/// 覆盖成功/报错/`?` 提前返回等全部退出路径，防止句柄泄漏
struct SspiGuard {
    cred: SecHandle,
    ctx: Option<SecHandle>,
}
impl Drop for SspiGuard {
    fn drop(&mut self) {
        unsafe {
            if let Some(ctx) = self.ctx.take() {
                let _ = DeleteSecurityContext(&ctx);
            }
            let _ = FreeCredentialsHandle(&self.cred);
        }
    }
}

/// 构造 Negotiate SPN: HTTP/`<host>`；非默认端口拼入 :port（Kerberos 需匹配服务注册的 SPN）
pub fn sspi_spn(host: &str, scheme: &str, port: Option<u16>) -> String {
    match (scheme, port) {
        // 默认端口是服务注册 SPN 的省略形式，显式拼上反而匹配不上
        ("http", Some(80)) | ("https", Some(443)) => format!("HTTP/{}", host),
        (_, Some(p)) => format!("HTTP/{}:{}", host, p),
        _ => format!("HTTP/{}", host),
    }
}

/// 凭据拆分为 (domain, user): "DOMAIN\User" 拆分反斜杠；否则 user 原样、domain 空（UPN 交给 SSPI）
pub fn split_credential(value: &str) -> (String, String) {
    if let Some(idx) = value.find('\\') {
        (value[..idx].to_string(), value[idx + 1..].to_string())
    } else {
        (String::new(), value.to_string())
    }
}

/// 宽字符串转换（UTF-16 + null 结尾）
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ==================== 共享目录映射（SharedDirectoryMapper） ====================
// 自 service_host.rs 搬迁: 服务启动时映射网络共享、停止时断开

use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::NetworkManagement::WNet::{
    NET_CONNECT_FLAGS, NET_RESOURCE_SCOPE, NETRESOURCEW, RESOURCETYPE_DISK, WNetAddConnection2W,
    WNetCancelConnection2W,
};

/// 单条映射配置（与主程序 SharedMapperConfig 字段一致）
#[derive(serde::Deserialize)]
pub struct MapperSpec {
    pub local_path: String,
    pub remote_path: String,
    pub username: Option<String>,
    pub password: Option<String>,
}

/// 映射网络共享目录到本地挂载点；返回失败列表（空 = 全部成功）
pub fn map_shared_directories(mappers: &[MapperSpec]) -> Vec<String> {
    unsafe {
        let mut errors = Vec::new();
        for m in mappers {
            let local = to_wide(&m.local_path);
            let remote = to_wide(&m.remote_path);
            let user = m.username.as_ref().map(|s| to_wide(s));
            let pass = m.password.as_ref().map(|s| to_wide(s));
            let resource = NETRESOURCEW {
                dwScope: NET_RESOURCE_SCOPE(0),
                dwType: RESOURCETYPE_DISK,
                dwDisplayType: 0,
                dwUsage: 0,
                lpLocalName: windows::core::PWSTR::from_raw(local.as_ptr() as *mut u16),
                lpRemoteName: windows::core::PWSTR::from_raw(remote.as_ptr() as *mut u16),
                lpComment: windows::core::PWSTR::null(),
                lpProvider: windows::core::PWSTR::null(),
            };
            let result = WNetAddConnection2W(
                &resource,
                pass.as_ref()
                    .map(|w| PCWSTR::from_raw(w.as_ptr()))
                    .unwrap_or(PCWSTR::null()),
                user.as_ref()
                    .map(|w| PCWSTR::from_raw(w.as_ptr()))
                    .unwrap_or(PCWSTR::null()),
                NET_CONNECT_FLAGS(0),
            );
            if result != ERROR_SUCCESS {
                errors.push(format!(
                    "Shared directory map failed: {0} -> {1} (error {2})",
                    m.local_path, m.remote_path, result.0
                ));
            }
        }
        errors
    }
}

/// 断开全部网络共享映射（强制断开）；返回失败列表（空 = 全部成功）
pub fn unmap_shared_directories(mappers: &[MapperSpec]) -> Vec<String> {
    unsafe {
        let mut errors = Vec::new();
        for m in mappers {
            let local = to_wide(&m.local_path);
            let result = WNetCancelConnection2W(
                PCWSTR::from_raw(local.as_ptr()),
                NET_CONNECT_FLAGS(0),
                true,
            );
            if result != ERROR_SUCCESS {
                errors.push(format!(
                    "Shared directory unmap failed: {0} (error {1})",
                    m.local_path, result.0
                ));
            }
        }
        errors
    }
}

// ==================== zip 解压（下载资源解压） ====================
// 自 service_host.rs 搬迁: 解压到目标目录并防 zip-slip 穿越

use std::path::{Path, PathBuf};

/// 解压总大小上限（8 GiB）: 防恶意 zip 炸弹填满磁盘（下载资源为管理员配置，仅作兜底）
const UNZIP_TOTAL_LIMIT: u64 = 8 * 1024 * 1024 * 1024;

/// 解压 zip 到目标目录（防 zip-slip: 条目路径规范化后必须仍位于目标目录内）
pub fn unzip_to_dir(zip_path: &str, target_dir: &str) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("cannot open zip '{0}': {e}", zip_path))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid zip '{0}': {e}", zip_path))?;
    // canonicalize 可能带 \\?\ 前缀，比较时必须统一基准（直接用其拼接出绝对目标路径）
    let canon_base =
        std::fs::canonicalize(target_dir).unwrap_or_else(|_| PathBuf::from(target_dir));
    let mut total_written: u64 = 0;
    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| format!("cannot read zip entry #{0} in '{1}': {e}", i, zip_path))?;
        let name = entry.name().replace('\\', "/");
        if name.ends_with('/') {
            continue;
        }
        // 规范化解压目标: 消除 . / .. 组件后再做前缀校验，杜绝 "..\evil" 类穿越绕过
        let raw = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            canon_base.join(&name)
        };
        let out_path = normalize_zip_path(&raw);
        if !out_path.starts_with(&canon_base) {
            return Err(format!(
                "zip entry '{0}' escapes target directory '{1}'",
                name, target_dir
            ));
        }
        if let Some(parent) = out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let mut out = std::fs::File::create(&out_path).map_err(|e| {
            format!(
                "cannot create '{0}' while extracting '{1}': {e}",
                out_path.display(),
                name
            )
        })?;
        let written = std::io::copy(&mut entry, &mut out).map_err(|e| {
            format!(
                "failed writing '{0}' while extracting '{1}': {e}",
                out_path.display(),
                name
            )
        })?;
        total_written = total_written.saturating_add(written);
        if total_written > UNZIP_TOTAL_LIMIT {
            return Err(format!(
                "zip '{0}' expands beyond the {1} GiB safety limit (possible zip bomb)",
                zip_path,
                UNZIP_TOTAL_LIMIT / 1024 / 1024 / 1024
            ));
        }
    }
    Ok(())
}

/// 词法规范化路径: 移除 "." 组件、折叠 ".." 组件（不访问文件系统）
fn normalize_zip_path(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

// ==================== 系统重启（故障恢复 reboot 动作） ====================
// 自 service_host.rs 搬迁: 按故障恢复策略重启系统

/// 重启系统（InitiateSystemShutdownExW，LocalSystem 默认具备关机特权）
pub fn reboot_system() -> Result<(), String> {
    use windows::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_APPLICATION,
    };
    unsafe {
        InitiateSystemShutdownExW(
            PCWSTR::null(),
            PCWSTR::null(),
            0,
            true,
            true,
            SHTDN_REASON_MAJOR_APPLICATION | SHTDN_REASON_FLAG_PLANNED,
        )
        .map_err(|e| format!("InitiateSystemShutdownExW failed: {e}"))
    }
}

// ==================== Webhook 通知（服务事件推送） ====================
// 宿主在 start/stop/crash 生命周期阶段调用 notify kit: POST JSON 到配置的 URL

/// 推送通知: 向 webhook URL POST application/json（body 为调用方按平台格式构造的 JSON 文本），
/// 2xx 视为成功；超时/网络错误返回错误详情（宿主按 fail_on_error 决定是否阻断）； 错误消息中的 URL 去除 userinfo（防内嵌凭据经日志泄漏）
pub fn notify_webhook(
    url: &str,
    body: &str,
    timeout_secs: u64,
    proxy: Option<&str>,
) -> Result<(), String> {
    let agent = build_agent(timeout_secs, proxy);
    let resp = agent
        .post(url)
        .header("content-type", "application/json")
        .send(body.to_string())
        .map_err(|e| {
            format!(
                "notify request failed for '{0}': {e}",
                redact_webhook_url(url)
            )
        })?;
    if resp.status().is_success() {
        Ok(())
    } else {
        Err(format!(
            "notify webhook returned HTTP {}",
            resp.status().as_u16()
        ))
    }
}

/// 按平台格式构造 webhook 通知 JSON 体（generic 为通用 {"text": ...}）:
/// teams / discord / feishu 使用各自消息卡片结构，便于各 IM 正确渲染
pub fn notify_payload(text: &str, format: &str) -> String {
    match format.to_ascii_lowercase().as_str() {
        "teams" => serde_json::json!({
            "@type": "MessageCard",
            "@context": "http://schema.org/extensions",
            "summary": text,
            "text": text,
        }),
        "discord" => serde_json::json!({ "content": text }),
        "feishu" | "lark" => serde_json::json!({
            "msg_type": "text",
            "content": { "text": text },
        }),
        _ => serde_json::json!({ "text": text }),
    }
    .to_string()
}

/// 去除 URL 的 userinfo 部分（http://user:pass@host → http://host）
fn redact_webhook_url(url: &str) -> String {
    match url::Url::parse(url) {
        Ok(mut u) => {
            let _ = u.set_username("");
            let _ = u.set_password(None);
            u.to_string()
        }
        Err(_) => url.to_string(),
    }
}

// ==================== 数据库/协议健康探针（probe kit） ====================
// 供宿主 health_check_url 以 osx:// 协议调用: 连接目标并验证协议握手（Redis PING / MySQL 握手包 / 纯 TCP）

use std::net::{TcpStream, ToSocketAddrs};

/// 协议健康探针: type = redis | mysql | tcp（缺省 tcp）；host 支持 `host[:port]`（缺省端口按类型）；
/// 连接成功且协议握手符合预期返回 Ok；失败返回错误详情
pub fn probe_target(
    probe_type: &str,
    host: &str,
    port: u16,
    timeout_secs: u64,
) -> Result<(), String> {
    let timeout = Duration::from_secs(timeout_secs.max(2));
    let addr = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("probe: cannot resolve {host}:{port}: {e}"))?
        .next()
        .ok_or_else(|| format!("probe: no address for {host}:{port}"))?;
    let mut stream = TcpStream::connect_timeout(&addr, timeout)
        .map_err(|e| format!("probe: connect to {host}:{port} failed: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    match probe_type.to_ascii_lowercase().as_str() {
        "redis" => {
            // PING → 期望 +PONG（Redis 文本协议）
            stream
                .write_all(b"PING\r\n")
                .map_err(|e| format!("probe: redis write failed: {e}"))?;
            let mut buf = [0u8; 64];
            let n = stream
                .read(&mut buf)
                .map_err(|e| format!("probe: redis read failed: {e}"))?;
            if String::from_utf8_lossy(&buf[..n]).contains("+PONG") {
                Ok(())
            } else {
                Err(format!(
                    "probe: redis unexpected response: {}",
                    String::from_utf8_lossy(&buf[..n]).trim()
                ))
            }
        }
        "mysql" => {
            // MySQL 客户端握手: 服务器首先发握手包（0x0a 协议版本 或 0xff 错误包）
            let mut buf = [0u8; 128];
            let n = stream
                .read(&mut buf)
                .map_err(|e| format!("probe: mysql read failed: {e}"))?;
            if n == 0 {
                return Err("probe: mysql closed connection".into());
            }
            let first = buf[0];
            if first == 0x0a || first == 0x00 {
                Ok(()) // 协议版本 10 握手包 / 0x00 也视为握手开始
            } else if first == 0xff {
                Err(format!(
                    "probe: mysql server error: {}",
                    String::from_utf8_lossy(&buf[1..n]).trim()
                ))
            } else {
                Err(format!(
                    "probe: mysql unexpected handshake byte 0x{first:02x}"
                ))
            }
        }
        _ => Ok(()), // tcp: 连接成功即健康
    }
}

/// 解析 `host[:port]`（IPv6 字面量 `[::1]` 或 `[::1]:514` 均支持，括号保留供 connect 解析）；
/// 端口缺省用 default_port；无括号多冒号地址（裸 IPv6）按原样 + 缺省端口处理
pub fn parse_host_port<'a>(
    host: &'a str,
    default_port: u16,
    kind: &str,
) -> Result<(&'a str, u16), String> {
    match host.rsplit_once(':') {
        // 显式端口（host:port / [::1]:port）: 端口须全数字且非空；
        // 带冒号的 host 仅允许带括号的 IPv6 字面量（裸 IPv6 多冒号歧义，按原样处理）
        Some((h, p))
            if !p.is_empty()
                && p.chars().all(|c| c.is_ascii_digit())
                && (!h.contains(':') || h.starts_with('[')) =>
        {
            Ok((
                h,
                p.parse::<u16>()
                    .map_err(|_| format!("{kind}: invalid port"))?,
            ))
        }
        // 有冒号但端口非法: 非 IPv6 字面量（host:abc）→ 报错
        Some((h, _)) if !h.contains(':') => Err(format!("{kind}: invalid port")),
        // 无端口或裸 IPv6（多冒号）→ 原样返回 + 缺省端口
        _ => Ok((host, default_port)),
    }
}

// ==================== SMTP 邮件告警 ====================
// 最小 SMTP 客户端: 会话直连（可选 AUTH PLAIN 认证），仅支持单封邮件（服务事件通知场景）； 不引入 SMTP crate 依赖（与主程序依赖策略一致: 零重依赖）

/// 发送一封 SMTP 邮件（25 或 465/587 需明文/STARTTLS 由服务器协商决定——
/// 本实现仅支持明文端口（25/587 无 TLS），认证走 AUTH PLAIN）； 服务器地址支持 "host:port"（缺省 25）
#[allow(clippy::too_many_arguments)] // 全部为邮件会话所需参数，打包反增调用点负担
pub fn send_email_smtp(
    host: &str,
    from: &str,
    to: &str,
    subject: &str,
    body: &str,
    username: Option<&str>,
    password: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    use std::io::BufReader;
    use std::net::TcpStream;

    // 解析 host[:port]
    let (addr, port) = parse_host_port(host, 25, "smtp")?;
    // 清洗发件/收件地址中的 CR/LF: 地址直接拼入 SMTP 命令与 DATA 头，
    // 含换行的输入会注入命令（RFC 5321 禁止未编码 CRLF 出现在命令中）
    let from = from.replace(['\r', '\n'], "");
    let to = to.replace(['\r', '\n'], "");
    let subject = subject.replace(['\r', '\n'], "");
    let timeout = Duration::from_secs(timeout_secs.max(5));
    let stream = TcpStream::connect((addr, port))
        .map_err(|e| format!("smtp: connect to {host} failed: {e}"))?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    // 读/写各持一份句柄（clone），避免闭包捕获与后续写入的借用冲突
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;

    // 读欢迎行（220）
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .map_err(|e| format!("smtp: read greeting failed: {e}"))?;
    if !line.starts_with("220") {
        return Err(format!("smtp: unexpected greeting: {line}"));
    }
    smtp_expect(&mut writer, &mut reader, "EHLO osmium\r\n", &["250"])?;
    if username.is_some() {
        smtp_expect(&mut writer, &mut reader, "AUTH PLAIN\r\n", &["334"])?;
        // AUTH PLAIN 凭据: base64("\0user\0pass")
        let user = username.unwrap_or("");
        let pass = password.unwrap_or("");
        let auth = format!("\0{user}\0{pass}");
        let auth_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, auth.as_bytes());
        smtp_expect(
            &mut writer,
            &mut reader,
            &format!("{auth_b64}\r\n"),
            &["235"],
        )?;
    }
    smtp_expect(
        &mut writer,
        &mut reader,
        &format!("MAIL FROM:<{from}>\r\n"),
        &["250"],
    )?;
    smtp_expect(
        &mut writer,
        &mut reader,
        &format!("RCPT TO:<{to}>\r\n"),
        &["250", "251"],
    )?;
    smtp_expect(&mut writer, &mut reader, "DATA\r\n", &["354"])?;
    // body 规范化换行（\n → \r\n）+ dot-stuffing: 行首 "." 前补 "."，
    // 否则正文以 "." 开头的行会被服务器误判为 DATA 结束（RFC 5321 §4.5.2）
    let body_escaped = body.replace("\r\n", "\n").replace('\n', "\r\n");
    let body_stuffed: String = body_escaped
        .split("\r\n")
        .map(|l| {
            if l.starts_with('.') {
                format!(".{l}")
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n");
    smtp_expect(
        &mut writer,
        &mut reader,
        &format!("From: {from}\r\nTo: {to}\r\nSubject: {subject}\r\n\r\n{body_stuffed}\r\n.\r\n"),
        &["250"],
    )?;
    smtp_expect(&mut writer, &mut reader, "QUIT\r\n", &["221"])?;
    Ok(())
}

/// SMTP 命令交互: 发送命令行并读取响应，校验期望的状态码前缀（3 位数字）；
/// 多行响应（第 4 字符为 '-'）持续读取
fn smtp_expect(
    writer: &mut TcpStream,
    reader: &mut std::io::BufReader<TcpStream>,
    command: &str,
    expect: &[&str],
) -> Result<(), String> {
    writer
        .write_all(command.as_bytes())
        .map_err(|e| format!("smtp: send failed: {e}"))?;
    let mut resp = String::new();
    loop {
        resp.clear();
        // 响应行长度上限（8KB）: 防恶意/故障服务器持续输出撑爆内存（单封邮件场景的兜底）
        let mut limited = (&mut *reader).take(8192);
        let n = limited
            .read_line(&mut resp)
            .map_err(|e| format!("smtp: read failed: {e}"))?;
        if n == 0 {
            return Err("smtp: connection closed by server".into());
        }
        if resp.len() >= 8192 && !resp.ends_with('\n') {
            return Err("smtp: response line too long".into());
        }
        if resp.len() < 4 {
            return Err(format!("smtp: short response: {resp}"));
        }
        // 多行响应: 第 4 字符为 '-' 时继续读
        if resp.as_bytes().get(3) != Some(&b'-') {
            break;
        }
    }
    let code = &resp[..3];
    if !expect.contains(&code) {
        return Err(format!(
            "smtp: unexpected response '{code}' to {0}: {1}",
            command.trim_end(),
            resp.trim_end()
        ));
    }
    Ok(())
}

// ==================== Syslog 告警（UDP） ====================
// RFC 5424 轻量发送: PRI(facility*8+severity) + 时间戳 + HOSTNAME + APP-NAME + MSG

/// UDP 发送一条 syslog 消息（RFC 5424）; host 支持 "host:port"（缺省 514）
pub fn send_syslog_udp(
    host: &str,
    message: &str,
    facility: u8,
    severity: u8,
    tag: &str,
    timeout_secs: u64,
) -> Result<(), String> {
    use std::net::UdpSocket;

    // 解析 host[:port]
    let (addr, port) = parse_host_port(host, 514, "syslog")?;
    // facility/severity 钳制到合法范围（0-23 / 0-7）
    let pri = ((facility.min(23) as u16) << 3) | (severity.min(7) as u16);
    // RFC 5424 时间戳: UTC（Z 后缀），GetSystemTime 免 chrono 依赖
    let st = unsafe { windows::Win32::System::SystemInformation::GetSystemTime() };
    let ts = format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond, st.wMilliseconds
    );
    let hostname = std::env::var("COMPUTERNAME").unwrap_or_else(|_| "localhost".into());
    let msg = message.replace('\n', " ");
    let frame = format!("<{pri}>1 {ts} {hostname} {tag} - - - {msg}");
    let socket = UdpSocket::bind("0.0.0.0:0").map_err(|e| format!("syslog: bind failed: {e}"))?;
    let _ = socket.set_write_timeout(Some(Duration::from_secs(timeout_secs.max(2))));
    socket
        .send_to(frame.as_bytes(), (addr, port))
        .map_err(|e| format!("syslog: send to {host} failed: {e}"))?;
    Ok(())
}
