//! osmium-kit — 官方工具包插件入口（构建后改名为 .osx）
//! 协议: stdin 单行 JSON（含 kit 字段分发到各功能）→ stdout 单行 JSON 响应，退出码 0/非0

mod kits_core;
#[cfg(test)]
mod kits_tests;

use serde::{Deserialize, Serialize};

use kits_core::{MapperSpec,
                map_shared_directories, reboot_system, sspi_download_to_file, unmap_shared_directories,
                unzip_to_dir,
};

/// 插件请求: 按 kit 字段分发到 SSPI / netmap / unzip / reboot 功能
#[derive(Deserialize)]
struct Request {
    /// 功能标识: sspi | netmap | unzip | reboot
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
    /// sspi: 下载超时（秒），缺省 300
    timeout_secs: Option<u64>,
    /// netmap: "map"（连接）| "unmap"（断开）
    action: Option<String>,
    /// netmap: 映射条目列表
    mappers: Option<Vec<MapperSpec>>,
    /// unzip: 待解压的 zip 文件路径
    src: Option<String>,
    /// unzip: 解压目标目录
    dest: Option<String>,
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
        other => fail(&format!("unknown kit: {other}")),
    }
}

/// SSPI 认证下载: 完成一次完整 401 挑战-响应循环并落盘
fn dispatch_sspi(req: &Request) {
    let Some(url) = req.url.as_deref() else { fail("sspi: missing 'url'") };
    let Some(to) = req.to.as_deref() else { fail("sspi: missing 'to'") };
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
    let Some(action) = req.action.as_deref() else { fail("netmap: missing 'action'") };
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
    let Some(src) = req.src.as_deref() else { fail("unzip: missing 'src'") };
    let Some(dest) = req.dest.as_deref() else { fail("unzip: missing 'dest'") };
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

/// 输出成功响应
fn ok() {
    let resp = Response { ok: true, error: None, details: None };
    println!("{}", serde_json::to_string(&resp).unwrap());
}

/// 输出失败响应并退出（退出码非 0 供宿主判定异常）;
/// 同时向 stderr 抛出错误详情（宿主调用时丢弃、手动运行/调试时可见）
fn fail(msg: &str) -> ! {
    eprintln!("osmium-kit error: {msg}");
    let resp = Response { ok: false, error: Some(msg.to_string()), details: None };
    println!("{}", serde_json::to_string(&resp).unwrap());
    std::process::exit(1);
}
