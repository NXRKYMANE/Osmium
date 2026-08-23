//! osmium-kit — 官方工具包插件入口（构建后改名为 .osx）
//! 协议: stdin 单行 JSON（含 kit 字段分发到各功能）→ stdout 单行 JSON 响应，退出码 0/非0

mod kits_core;
#[cfg(test)]
mod kits_tests;

use serde::{Deserialize, Serialize};

use kits_core::{
    MapperSpec, map_shared_directories, notify_payload, notify_webhook, parse_host_port,
    probe_target, reboot_system, send_email_smtp, send_syslog_udp, sspi_download_to_file,
    unmap_shared_directories, unzip_to_dir,
};

/// 插件请求: 按 kit 字段分发到 SSPI / netmap / unzip / reboot / notify / smtp / syslog 功能
#[derive(Deserialize)]
struct Request {
    /// 功能标识: sspi | netmap | unzip | reboot | notify | smtp | syslog
    kit: String,
    /// sspi: 下载源 URL
    url: Option<String>,
    /// sspi: 目标文件路径（以 .download.tmp 原子写入后改名）
    to: Option<String>,
    /// sspi: 可选凭据（DOMAIN\User 或 user）；缺省用当前进程身份
    username: Option<String>,
    /// sspi: 可选凭据密码
    password: Option<String>,
    /// sspi: 可选代理（http/https）
    proxy: Option<String>,
    /// sspi/notify: 超时（秒）
    timeout_secs: Option<u64>,
    /// netmap: "map"（连接）| "unmap"（断开）
    action: Option<String>,
    /// netmap: 映射条目列表
    mappers: Option<Vec<MapperSpec>>,
    /// unzip: 待解压的 zip 文件路径
    src: Option<String>,
    /// unzip: 解压目标目录
    dest: Option<String>,
    /// notify/smtp/syslog: 通知文本（notify 为 POST JSON 的 text 字段）；缺省用注入的崩溃上下文组装
    text: Option<String>,
    /// notify: webhook 平台格式（generic 默认 | teams | discord | feishu）
    format: Option<String>,
    /// probe: 探针类型（tcp 默认 | redis | mysql）
    probe_type: Option<String>,
    /// probe: 目标端口（缺省按类型: redis 6379 / mysql 3306 / tcp 80）
    port: Option<u16>,
    /// crash 阶段宿主自动注入: 服务名
    service_name: Option<String>,
    /// crash 阶段宿主自动注入: 子进程退出码
    exit_code: Option<i32>,
    /// crash 阶段宿主自动注入: 连续故障次数
    failures: Option<i64>,
    /// smtp: 服务器地址（host:port，缺省端口 25）
    host: Option<String>,
    /// smtp: 发件人（From 头）
    from: Option<String>,
    /// smtp: 收件人列表（逗号分隔）
    to_addr: Option<String>,
    /// smtp: 邮件主题
    subject: Option<String>,
    /// syslog: 服务器地址（host:port，缺省 514）
    syslog_host: Option<String>,
    /// syslog: facility 号（0-23，默认 3 daemon）
    facility: Option<u8>,
    /// syslog: severity 号（0-7，默认 5 notice）
    severity: Option<u8>,
    /// syslog: 程序名 TAG
    tag: Option<String>,
}

/// 插件响应; 其余输出一律走 stderr（避免污染协议）
#[derive(Serialize)]
struct Response {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    /// 部分失败时的逐条原因（netmap）
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Vec<String>>,
}

fn main() {
    use std::io::Read;
    let mut input = Vec::new();
    // 限制输入大小: 协议调用方为宿主（受信任），防异常调用方喂超大输入撑爆内存
    let _ = std::io::stdin().take(1024 * 1024).read_to_end(&mut input);
    let input = String::from_utf8_lossy(&input).into_owned();
    // 无调用方（双击运行/无人喂输入）: 静默退出，不输出任何内容
    if input.trim().is_empty() {
        std::process::exit(0);
    }
    let req: Request = match serde_json::from_str(&input) {
        Ok(r) => r,
        Err(e) => fail(&format!("invalid request: {e}")),
    };

    match req.kit.to_lowercase().as_str() {
        "ping" => ok(), // 可用性探测: 立即返回 ok，不做任何事
        "sspi" => dispatch_sspi(&req),
        "netmap" => dispatch_netmap(&req),
        "unzip" => dispatch_unzip(&req),
        "reboot" => dispatch_reboot(),
        "notify" => dispatch_notify(&req),
        "probe" => dispatch_probe(&req),
        "smtp" => dispatch_smtp(&req),
        "syslog" => dispatch_syslog(&req),
        other => fail(&format!("unknown kit: {other}")),
    }
}

/// SSPI 认证下载: 完成一次完整 401 挑战-响应循环并落盘
fn dispatch_sspi(req: &Request) {
    let Some(url) = req.url.as_deref() else {
        fail("sspi: missing 'url'")
    };
    let Some(to) = req.to.as_deref() else {
        fail("sspi: missing 'to'")
    };
    match sspi_download_to_file(
        url,
        to,
        req.username.as_deref(),
        req.password.as_deref(),
        req.proxy.as_deref(),
        req.timeout_secs.unwrap_or(300),
    ) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// 共享目录映射: 全部条目执行后汇总失败列表
fn dispatch_netmap(req: &Request) {
    let Some(action) = req.action.as_deref() else {
        fail("netmap: missing 'action'")
    };
    let mappers = req.mappers.as_deref().unwrap_or(&[]);
    if mappers.is_empty() {
        fail("netmap: no mappers provided (empty mappers list)");
    }
    let errors = match action.to_lowercase().as_str() {
        "map" => map_shared_directories(mappers),
        "unmap" => unmap_shared_directories(mappers),
        other => fail(&format!("netmap: unknown action: {other}")),
    };
    if errors.is_empty() {
        ok();
    } else {
        let resp = Response {
            ok: false,
            error: Some(format!("{} mapper(s) failed", errors.len())),
            details: Some(errors),
        };
        println!("{}", serde_json::to_string(&resp).unwrap());
        std::process::exit(1);
    }
}

/// zip 解压: 解压 src 到 dest（防 zip-slip 穿越）
fn dispatch_unzip(req: &Request) {
    let Some(src) = req.src.as_deref() else {
        fail("unzip: missing 'src'")
    };
    let Some(dest) = req.dest.as_deref() else {
        fail("unzip: missing 'dest'")
    };
    match unzip_to_dir(src, dest) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// 系统重启: 成功后系统立即重启（无输出返回），失败输出 error
fn dispatch_reboot() {
    match reboot_system() {
        Ok(()) => ok(), // 成功即重启，此分支不可达（保底输出）
        Err(e) => fail(&e),
    }
}

/// Webhook 通知: 向 url POST 按 format 构造的 JSON（服务事件推送，2xx 视为成功）
fn dispatch_notify(req: &Request) {
    let Some(url) = req.url.as_deref() else {
        fail("notify: missing 'url'")
    };
    let body = notify_payload(&alert_text(req), req.format.as_deref().unwrap_or("generic"));
    match notify_webhook(
        url,
        &body,
        req.timeout_secs.unwrap_or(30),
        req.proxy.as_deref(),
    ) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// 协议健康探针: 连接目标并验证协议握手（redis PING / mysql 握手包 / tcp 连接）
fn dispatch_probe(req: &Request) {
    let Some(host) = req.url.as_deref() else {
        fail("probe: missing 'url' (host)")
    };
    let probe_type = req.probe_type.as_deref().unwrap_or("tcp");
    let default_port = match probe_type.to_ascii_lowercase().as_str() {
        "redis" => 6379,
        "mysql" => 3306,
        _ => 80,
    };
    let (addr, port) = match parse_host_port(host, default_port, "probe") {
        Ok(v) => v,
        Err(e) => fail(&e),
    };
    let port = req.port.unwrap_or(port);
    match probe_target(probe_type, addr, port, req.timeout_secs.unwrap_or(5)) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// 告警文本: 显式 text 优先；缺省用宿主 crash 阶段注入的上下文组装（`[服务名]` crashed, exit code X, failure #N）
fn alert_text(req: &Request) -> String {
    if let Some(t) = req.text.as_deref()
        && !t.trim().is_empty()
    {
        return t.to_string();
    }
    let name = req.service_name.as_deref().unwrap_or("service");
    let code = req
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "?".into());
    let failures = req
        .failures
        .map(|n| n.to_string())
        .unwrap_or_else(|| "?".into());
    format!("[{name}] crashed (exit code {code}, failure #{failures})")
}

/// SMTP 邮件告警: 发一封带认证（可选）的邮件（AUTH PLAIN，仅单封，用于服务事件通知）
fn dispatch_smtp(req: &Request) {
    let Some(host) = req.host.as_deref() else {
        fail("smtp: missing 'host'")
    };
    let Some(from) = req.from.as_deref() else {
        fail("smtp: missing 'from'")
    };
    let Some(to_addr) = req.to_addr.as_deref() else {
        fail("smtp: missing 'to'")
    };
    match send_email_smtp(
        host,
        from,
        to_addr,
        req.subject
            .as_deref()
            .unwrap_or("Osmium service notification"),
        &alert_text(req),
        req.username.as_deref(),
        req.password.as_deref(),
        req.timeout_secs.unwrap_or(30),
    ) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// Syslog 告警: UDP 发送 RFC 5424 消息（facility/severity 可配）
fn dispatch_syslog(req: &Request) {
    let Some(host) = req.syslog_host.as_deref() else {
        fail("syslog: missing 'host'")
    };
    match send_syslog_udp(
        host,
        &alert_text(req),
        req.facility.unwrap_or(3), // 默认 daemon
        req.severity.unwrap_or(5), // 默认 notice
        req.tag.as_deref().unwrap_or("Osmium"),
        req.timeout_secs.unwrap_or(5),
    ) {
        Ok(()) => ok(),
        Err(e) => fail(&e),
    }
}

/// 输出成功响应
fn ok() {
    let resp = Response {
        ok: true,
        error: None,
        details: None,
    };
    println!("{}", serde_json::to_string(&resp).unwrap());
}

/// 输出失败响应并退出（退出码非 0 供宿主判定异常）;
/// 同时向 stderr 抛出错误详情（宿主调用时丢弃、手动运行/调试时可见）
fn fail(msg: &str) -> ! {
    eprintln!("osmium-kit error: {msg}");
    let resp = Response {
        ok: false,
        error: Some(msg.to_string()),
        details: None,
    };
    println!("{}", serde_json::to_string(&resp).unwrap());
    std::process::exit(1);
}
