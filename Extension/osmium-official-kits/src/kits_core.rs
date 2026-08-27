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

/// 构造 HTTP Agent（全局超时覆盖整个请求；4xx/5xx 不转错误，调用方按状态码处理）。
/// max_redirects=0: 重定向手动跟随——sspi 的 Negotiate/NTLM 令牌不随自动跟随中继到
/// 重定向目标（凭据中继面，与宿主下载器策略对齐）；notify 无凭据可传非零值允许自动跟随。
/// proxy 解析失败必须 fail-closed 返回 Err: 管理员配代理往往就是做出网管控，
/// 静默放弃代理直连目标等于旁路管控且无任何告警
fn build_agent(
    timeout_secs: u64,
    proxy: Option<&str>,
    max_redirects: u32,
) -> Result<ureq::Agent, String> {
    let mut builder = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .max_redirects(max_redirects);
    if let Some(proxy_url) = proxy {
        let p =
            ureq::Proxy::new(proxy_url).map_err(|e| format!("invalid proxy '{proxy_url}': {e}"))?;
        builder = builder.proxy(Some(p));
    }
    Ok(ureq::Agent::new_with_config(builder.build()))
}

/// 执行一次带 SSPI 认证的完整下载（URL → 目标文件）: 401 挑战-响应循环（最多 3 轮），
/// 重定向手动跟随（拒绝 https→http 降级，跨源重新协商）；凭据缺省用当前进程身份；
/// 目标文件以 .download.tmp 原子写入（CreateNew 防 TOCTOU）后改名，截断响应按失败处理
pub fn sspi_download_to_file(
    url: &str,
    to: &str,
    username: Option<&str>,
    password: Option<&str>,
    proxy: Option<&str>,
    timeout_secs: u64,
) -> Result<(), String> {
    use base64::Engine as _;

    let client = build_agent(timeout_secs, proxy, 0)?;

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

    // 当前请求 URL（重定向手动跟随会更新）；token 为上一轮 ISC 输出（跨源时作废）
    let mut current = url.to_string();
    let mut token: Option<Vec<u8>> = None;
    let mut challenges = 0u32;
    for _ in 0..12 {
        let uri = url::Url::parse(&current).map_err(|e| format!("invalid URL: {e}"))?;
        let host = uri.host_str().unwrap_or("localhost").to_string();
        // SPN 按当前源计算（重定向换主机后须匹配新源的注册名）
        let spn = sspi_spn(&host, uri.scheme(), uri.port());
        let spn_wide = to_wide(&spn);

        let mut req = client.get(&current);
        if let Some(t) = &token {
            let b64 = base64::engine::general_purpose::STANDARD.encode(t);
            req = req.header("authorization", format!("Negotiate {b64}"));
        }
        let resp = req
            .call()
            .map_err(|e| format!("request failed for {0}: {e}", redact_webhook_url(&current)))?;
        // 手动跟随重定向（max_redirects=0 时 3xx 原样返回）: 拒绝 https→http 降级；
        // 跨源后旧协商上下文/令牌作废，对新源从零开始挑战（令牌不带给第三方）
        if (300..400).contains(&resp.status().as_u16()) {
            let status = resp.status().as_u16();
            let loc = resp
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
                .ok_or_else(|| format!("redirect without Location header (HTTP {status})"))?;
            let next = resolve_redirect_url(&current, &loc);
            // scheme 大小写不敏感: current 首跳为调用方原始字符串（未归一化），
            // 大写 HTTPS:// 会让前缀匹配漏判——统一小写后判定
            if current.to_ascii_lowercase().starts_with("https://")
                && next.to_ascii_lowercase().starts_with("http://")
            {
                return Err(format!(
                    "insecure redirect refused: {0} -> {1}",
                    redact_webhook_url(&current),
                    redact_webhook_url(&next)
                ));
            }
            let _ = std::io::copy(&mut resp.into_body().into_reader(), &mut std::io::sink());
            if let Some(old) = guard.ctx.take() {
                unsafe {
                    let _ = DeleteSecurityContext(&old);
                }
            }
            token = None;
            current = next;
            continue;
        }
        if resp.status().is_success() {
            let expected_len = resp
                .headers()
                .get("content-length")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let mut reader = resp.into_body().into_reader();
            let copied = match std::io::copy(&mut reader, &mut file) {
                Ok(n) => n,
                // 对端提前断开（Peer disconnected 等）: 同属截断失败，清理残留 tmp
                Err(e) => {
                    let _ = std::fs::remove_file(&tmp);
                    return Err(format!("failed to write '{to}': {e}"));
                }
            };
            // 截断对照: 连接干净关闭的短响应按失败处理（无 sha 配置时防静默损坏被执行）
            if let Some(expect) = expected_len
                && copied != expect
            {
                let _ = std::fs::remove_file(&tmp);
                return Err(format!(
                    "truncated download: got {copied} of {expect} bytes"
                ));
            }
            drop(file);
            std::fs::rename(&tmp, to).map_err(|e| {
                // rename 失败（目标被占用等）: 清理 tmp 防残留累积
                let _ = std::fs::remove_file(&tmp);
                format!("rename failed for '{to}': {e}")
            })?;
            return Ok(());
        }
        if resp.status().as_u16() != 401 {
            return Err(format!(
                "server returned HTTP {} for {}",
                resp.status().as_u16(),
                redact_webhook_url(&current)
            ));
        }
        challenges += 1;
        if challenges > 3 {
            return Err("authentication exceeded 3 challenge rounds — server likely rejected the credentials or the negotiation is unsupported".into());
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
    Err("download failed: too many redirects or unresolved authentication".into())
}

/// 解析重定向 Location（相对/绝对，RFC 3986 join）；解析失败原样返回
fn resolve_redirect_url(current: &str, location: &str) -> String {
    url::Url::parse(current)
        .ok()
        .and_then(|base| base.join(location).ok())
        .map(|u| u.to_string())
        .unwrap_or_else(|| location.to_string())
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
    CONNECT_TEMPORARY, CONNECT_UPDATE_PROFILE, NET_CONNECT_FLAGS, NET_RESOURCE_SCOPE, NETRESOURCEW,
    RESOURCETYPE_DISK, WNetAddConnection2W, WNetCancelConnection2W,
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
                // CONNECT_TEMPORARY: 映射仅服务生命周期内有效——不写账户 profile 持久化记录，
                // 防服务停止后凭据背书的共享在下次登录时被系统自动重建
                NET_CONNECT_FLAGS(CONNECT_TEMPORARY.0),
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
                // CONNECT_UPDATE_PROFILE: 同时清除 profile 中残留的持久化映射记录
                NET_CONNECT_FLAGS(CONNECT_UPDATE_PROFILE.0),
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
/// 单 zip 条目数上限: 海量零字节条目不触发体积上限但同样可耗尽 inode/拖慢循环（纵深防御）
const UNZIP_ENTRY_LIMIT: usize = 100_000;

/// 路径是否为 reparse 点（symlink/junction）: Win32 属性查询（std 的 is_symlink 覆盖不了 junction）
fn is_reparse_path(p: &Path) -> bool {
    use windows::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, GetFileAttributesW, INVALID_FILE_ATTRIBUTES,
    };
    let wide = to_wide(&p.to_string_lossy());
    unsafe {
        let attrs = GetFileAttributesW(PCWSTR::from_raw(wide.as_ptr()));
        attrs != INVALID_FILE_ATTRIBUTES && attrs & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
    }
}

/// 解压 zip 到目标目录（防 zip-slip: 条目路径规范化后必须仍位于目标目录内）。
/// 中途失败清理本次已解压的文件（防新旧产物混杂与炸弹场景的垃圾残留——与 sspi
/// 路径失败清 tmp 对称; 目录保持不动，仅删本次创建的文件）
pub fn unzip_to_dir(zip_path: &str, target_dir: &str) -> Result<(), String> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| format!("cannot open zip '{0}': {e}", zip_path))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| format!("invalid zip '{0}': {e}", zip_path))?;
    // canonicalize 可能带 \\?\ 前缀，比较时必须统一基准（直接用其拼接出绝对目标路径）
    let canon_base =
        std::fs::canonicalize(target_dir).unwrap_or_else(|_| PathBuf::from(target_dir));
    // 本次已解压的文件清单（失败时逆序清理）: 条目顺序中父目录先于子文件的情况
    // 无需处理——只删文件，不删目录（目录可能预存在或含其他内容）
    let mut written_files: Vec<PathBuf> = Vec::new();
    let mut total_written: u64 = 0;
    let mut result: Result<(), String> = Ok(());
    if archive.len() > UNZIP_ENTRY_LIMIT {
        return Err(format!(
            "zip '{0}' contains {1} entries, exceeding the {2} safety limit",
            zip_path,
            archive.len(),
            UNZIP_ENTRY_LIMIT
        ));
    }
    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(e) => {
                result = Err(format!(
                    "cannot read zip entry #{0} in '{1}': {e}",
                    i, zip_path
                ));
                break;
            }
        };
        let name = entry.name().replace('\\', "/");
        if name.ends_with('/') {
            continue;
        }
        // 拒绝 NTFS 备用数据流条目（如 "legit.dll:evil"）: File::create 会静默创建 ADS
        // 而非报错，可污染目标目录内既有文件（写入面畸形输入未拒绝）
        if name.contains(':') {
            result = Err(format!(
                "zip entry '{0}' contains ':' (NTFS alternate data stream not allowed)",
                name
            ));
            break;
        }
        // 规范化解压目标: 消除 . / .. 组件后再做前缀校验，杜绝 "..\evil" 类穿越绕过
        let raw = if Path::new(&name).is_absolute() {
            PathBuf::from(&name)
        } else {
            canon_base.join(&name)
        };
        let out_path = normalize_zip_path(&raw);
        if !out_path.starts_with(&canon_base) {
            result = Err(format!(
                "zip entry '{0}' escapes target directory '{1}'",
                name, target_dir
            ));
            break;
        }
        if let Some(parent) = out_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // reparse 写穿防护（与宿主落点同款）: 解压目标自身是 symlink/junction 时拒绝——
        // 目标目录内预存的同名链接会让 File::create 穿过链接写到指向位置
        if is_reparse_path(&out_path) {
            result = Err(format!(
                "zip entry '{0}' target '{1}' is a reparse point, refusing to extract",
                name,
                out_path.display()
            ));
            break;
        }
        let mut out = match std::fs::File::create(&out_path) {
            Ok(f) => f,
            Err(e) => {
                result = Err(format!(
                    "cannot create '{0}' while extracting '{1}': {e}",
                    out_path.display(),
                    name
                ));
                break;
            }
        };
        let copied = match std::io::copy(&mut entry, &mut out) {
            Ok(n) => n,
            Err(e) => {
                result = Err(format!(
                    "failed writing '{0}' while extracting '{1}': {e}",
                    out_path.display(),
                    name
                ));
                break;
            }
        };
        written_files.push(out_path);
        total_written = total_written.saturating_add(copied);
        if total_written > UNZIP_TOTAL_LIMIT {
            result = Err(format!(
                "zip '{0}' expands beyond the {1} GiB safety limit (possible zip bomb)",
                zip_path,
                UNZIP_TOTAL_LIMIT / 1024 / 1024 / 1024
            ));
            break;
        }
    }
    if result.is_err() {
        // 失败清理本次已解压文件（逆序: 深层路径先删; 尽力而为，忽略删除错误）
        for p in written_files.iter().rev() {
            let _ = std::fs::remove_file(p);
        }
    }
    result
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

/// 重启系统（InitiateSystemShutdownExW）: 调用前启用 SeShutdownPrivilege——
/// 该特权在多数令牌中"存在但禁用"，不启用时 ERROR_PRIVILEGE_NOT_HELD（LocalSystem
/// 通常可用，但插件可能以虚拟账户/自定义账户上下文执行，reboot 动作会静默失效）
pub fn reboot_system() -> Result<(), String> {
    use windows::Win32::System::Shutdown::{
        InitiateSystemShutdownExW, SHTDN_REASON_FLAG_PLANNED, SHTDN_REASON_MAJOR_APPLICATION,
    };
    unsafe {
        // 启用 SeShutdownPrivilege（失败即报错: LocalSystem 下该特权已启用时调用直接成功）
        enable_shutdown_privilege()?;
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

/// 启用 SeShutdownPrivilege（LookupPrivilegeValueW + AdjustTokenPrivileges，宿主
/// enable_debug_privilege 同款模板）; 失败返回错误由调用方报告
fn enable_shutdown_privilege() -> Result<(), String> {
    use windows::Win32::Foundation::{CloseHandle, LUID};
    use windows::Win32::Security::{
        AdjustTokenPrivileges, LUID_AND_ATTRIBUTES, LookupPrivilegeValueW, SE_PRIVILEGE_ENABLED,
        TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    unsafe {
        let mut token = windows::Win32::Foundation::HANDLE::default();
        OpenProcessToken(
            GetCurrentProcess(),
            TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
            &mut token,
        )
        .map_err(|e| format!("OpenProcessToken failed: {e}"))?;
        let mut luid = LUID::default();
        let result = LookupPrivilegeValueW(
            PCWSTR::null(),
            PCWSTR::from_raw(
                "SeShutdownPrivilege\0"
                    .encode_utf16()
                    .collect::<Vec<u16>>()
                    .as_ptr(),
            ),
            &mut luid,
        );
        if let Err(e) = result {
            let _ = CloseHandle(token);
            return Err(format!("LookupPrivilegeValueW failed: {e}"));
        }
        let tp = TOKEN_PRIVILEGES {
            PrivilegeCount: 1,
            Privileges: [LUID_AND_ATTRIBUTES {
                Luid: luid,
                Attributes: SE_PRIVILEGE_ENABLED,
            }],
        };
        let r = AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None);
        let _ = CloseHandle(token);
        r.map_err(|e| format!("AdjustTokenPrivileges failed: {e}"))?;
        Ok(())
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
    // notify 无凭据（凭据在 payload 里），允许跟随重定向——build_agent 的 max_redirects=0
    // 是给 sspi 的（防 Negotiate/NTLM 令牌中继），此处传 10 恢复 ureq 默认跟随上限
    let agent = build_agent(timeout_secs.max(1), proxy, 10)?;
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
            // query/fragment 常带 token/signature 等凭据，与宿主 redact_url 口径对齐一并剥离
            u.set_query(None);
            u.set_fragment(None);
            u.to_string()
        }
        // 解析失败（畸形 URL）也可能携带 user:pass@: 剥掉 scheme:// 与首 '/' 之间的
        // userinfo 段再回退原文——错误消息路径仍可能把含凭据的原文放进输出
        Err(_) => {
            let mut s = url.to_string();
            if let Some(scheme_end) = s.find("://") {
                let after = &s[scheme_end + 3..];
                if let Some(at) = after.find('@') {
                    let host_part = &after[at + 1..];
                    s = format!("{}{}", &s[..scheme_end + 3], host_part);
                }
            }
            s
        }
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
    // 遍历全部解析地址（双栈主机 IPv4/IPv6 任一可达即成功）: 只试第一个
    // 地址会在服务仅监听另一协议族时误报失败（健康检查误判 crash 触发无谓强杀重启）
    let addrs = (host, port)
        .to_socket_addrs()
        .map_err(|e| format!("probe: cannot resolve {host}:{port}: {e}"))?;
    let mut stream = None;
    let mut last_err = String::new();
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = format!("{e}"),
        }
    }
    let mut stream = stream.ok_or_else(|| {
        format!(
            "probe: connect to {host}:{port} failed: {}",
            if last_err.is_empty() {
                "no address resolved".to_string()
            } else {
                last_err
            }
        )
    })?;
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
            // MySQL 客户端握手: 初始握书包为 [3 字节长度][1 字节序号][payload]，
            // payload 首字节（buf[4]）是协议版本（0x0a）或错误包标志（0xff）。
            // 必须循环读满至少 5 字节: 单次 read 在 TCP 分段（代理/隧道/MTU 边界）
            // 下可能只到 4 字节包头，此时 buf[0] 是长度字段的任意值 → 健康检查
            // 假阴性触发无谓强杀
            let mut buf = [0u8; 128];
            let mut n = 0usize;
            while n < 5 {
                let r = stream
                    .read(&mut buf[n..])
                    .map_err(|e| format!("probe: mysql read failed: {e}"))?;
                if r == 0 {
                    return Err("probe: mysql closed connection".into());
                }
                n += r;
            }
            let first = buf[4];
            if first == 0x0a || first == 0x00 {
                Ok(()) // 协议版本 10 握手包 / 0x00 也视为握手开始
            } else if first == 0xff {
                let msg_at = 5;
                Err(format!(
                    "probe: mysql server error: {}",
                    String::from_utf8_lossy(&buf[msg_at..n]).trim()
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
    // 连接阶段同样受超时约束（connect 无超时版本在 SYN 被丢弃的地址上会走
    // 系统默认重试序列，最长约 20 秒+，拖慢 crash 告警链路）
    let addrs = (addr, port)
        .to_socket_addrs()
        .map_err(|e| format!("smtp: cannot resolve {host}:{port}: {e}"))?;
    let mut stream = None;
    let mut last_err = String::new();
    for a in addrs {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last_err = format!("{e}"),
        }
    }
    let stream = stream.ok_or_else(|| {
        format!(
            "smtp: connect to {host} failed: {}",
            if last_err.is_empty() {
                "no address resolved".to_string()
            } else {
                last_err
            }
        )
    })?;
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));
    // 读/写各持一份句柄（clone），避免闭包捕获与后续写入的借用冲突
    let mut reader = BufReader::new(stream.try_clone().map_err(|e| e.to_string())?);
    let mut writer = stream.try_clone().map_err(|e| e.to_string())?;

    // 读欢迎行（220）: 带 8KB 行上限（与 smtp_expect 一致，防恶意服务器无界输出）
    let mut line = String::new();
    reader
        .by_ref()
        .take(8192)
        .read_line(&mut line)
        .map_err(|e| format!("smtp: read greeting failed: {e}"))?;
    if !line.starts_with("220") {
        return Err(format!("smtp: unexpected greeting: {line}"));
    }
    smtp_expect(
        &mut writer,
        &mut reader,
        "EHLO osmium\r\n",
        "EHLO",
        &["250"],
    )?;
    if username.is_some() {
        smtp_expect(
            &mut writer,
            &mut reader,
            "AUTH PLAIN\r\n",
            "AUTH PLAIN",
            &["334"],
        )?;
        // AUTH PLAIN 凭据: base64("\0user\0pass")
        let user = username.unwrap_or("");
        let pass = password.unwrap_or("");
        let auth = format!("\0{user}\0{pass}");
        let auth_b64 =
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, auth.as_bytes());
        // display 必须脱敏: 错误消息会拼入 command 原文并回传宿主落日志，
        // 认证失败（密码过期等最常见故障）时 base64 凭据会明文进日志（一行解码即还原）
        smtp_expect(
            &mut writer,
            &mut reader,
            &format!("{auth_b64}\r\n"),
            "AUTH PLAIN credentials (redacted)",
            &["235"],
        )?;
    }
    smtp_expect(
        &mut writer,
        &mut reader,
        &format!("MAIL FROM:<{from}>\r\n"),
        "MAIL FROM",
        &["250"],
    )?;
    // RCPT TO 必须逐条发送（RFC 5321: 每个收件人一条命令）——
    // 配置允许多个逗号分隔地址，拼成单条 RCPT 会被多数 MTA 以 501/553 拒绝
    for rcp in to.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        smtp_expect(
            &mut writer,
            &mut reader,
            &format!("RCPT TO:<{rcp}>\r\n"),
            "RCPT TO",
            &["250", "251"],
        )?;
    }
    smtp_expect(&mut writer, &mut reader, "DATA\r\n", "DATA", &["354"])?;
    // body 规范化换行 + 裸 CR 清洗（RFC 5321 §2.3.8 禁止孤立 \r，部分服务器会按宽松
    // 策略错切行）+ dot-stuffing: 行首 "." 前补 "."，否则正文以 "." 开头的行会被
    // 服务器误判为 DATA 结束（RFC 5321 §4.5.2）
    let body_escaped = body.replace('\r', "").replace('\n', "\r\n");
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
        "message body (redacted)",
        &["250"],
    )?;
    // QUIT 结果忽略: DATA 收到 250 后邮件已被服务器接收——qmail 类 MTA 及部分反垃圾
    // 网关会在 250 后立即断连不等 QUIT，此时读/写侧报连接关闭属正常，不能误报发信失败
    let _ = smtp_expect(&mut writer, &mut reader, "QUIT\r\n", "QUIT", &["221"]);
    Ok(())
}

/// SMTP 命令交互: 发送命令行并读取响应，校验期望的状态码前缀（3 位数字）；
/// 多行响应（第 4 字符为 '-'）持续读取（上限 64 行，防恶意/故障服务器无限循环——
/// 单次读取受 read_timeout 约束，但每次都能"成功"读一行即可无限拖长会话）；
/// display 为错误消息回显文本，凭据类命令须传脱敏名（不回显 command 原文）
fn smtp_expect(
    writer: &mut TcpStream,
    reader: &mut std::io::BufReader<TcpStream>,
    command: &str,
    display: &str,
    expect: &[&str],
) -> Result<(), String> {
    writer
        .write_all(command.as_bytes())
        .map_err(|e| format!("smtp: send failed: {e}"))?;
    let mut resp = String::new();
    let mut lines = 0u32;
    loop {
        lines += 1;
        if lines > 64 {
            return Err("smtp: too many response lines".into());
        }
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
            display,
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
    // hostname 清洗: 空格/CR/LF 会破坏 RFC5424 帧的 HOSTNAME 字段（罕见但需防）
    let hostname: String = hostname
        .chars()
        .map(|c| {
            if c.is_whitespace() || c == '\r' || c == '\n' {
                '-'
            } else {
                c
            }
        })
        .collect();
    // TAG 清洗: CR/LF/空格会破坏 RFC5424 帧结构（与 smtp 地址清洗同源的注入防护）
    let tag = tag.replace(['\r', '\n', ' '], "-");
    // MSG 清洗: 同样滤掉 CR（\r 可单独注入帧分段，smtp 同源修复已做，此处补上）
    let msg = message.replace(['\r', '\n'], " ");
    let frame = format!("<{pri}>1 {ts} {hostname} {tag} - - - {msg}");
    // 遍历全部解析地址（与 probe/smtp 同款）: 固定 bind 0.0.0.0 后 send_to 字符串目标
    // 由 std 取首个解析地址——AAAA 优先的双栈主机拿到 IPv6 地址会因地址族不匹配
    // WSAEAFNOSUPPORT，syslog 告警静默丢失；逐地址按协议族建 socket 发送
    let addrs = (addr, port)
        .to_socket_addrs()
        .map_err(|e| format!("syslog: cannot resolve {host}:{port}: {e}"))?;
    let mut last_err = String::new();
    for a in addrs {
        let bind = if a.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" };
        let socket = match UdpSocket::bind(bind) {
            Ok(s) => s,
            Err(e) => {
                last_err = format!("{e}");
                continue;
            }
        };
        let _ = socket.set_write_timeout(Some(Duration::from_secs(timeout_secs.max(2))));
        match socket.send_to(frame.as_bytes(), a) {
            Ok(_) => return Ok(()),
            Err(e) => last_err = format!("{e}"),
        }
    }
    Err(format!(
        "syslog: send to {host} failed: {}",
        if last_err.is_empty() {
            "no address resolved".to_string()
        } else {
            last_err
        }
    ))
}
