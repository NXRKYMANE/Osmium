# Osmium 插件开发与使用指南

Osmium 支持万物皆插件：官方的高级功能、第三方的扩展能力，都是一个独立的可执行程序（`.osx`），放到 `exts\` 目录里，由宿主按需拉起。插件的用法、协议和开发方式都在下面了。

## 插件是什么

- 插件就是一个普通程序，把扩展名改成 `.osx` 就行（比如 `osmium-kit.exe` → `osmium-okits.osx`）
- 插件放在宿主 exe 同级的 `exts\` 目录（平台安装为 `%ProgramFiles%\Osmium\exts\`）
- 宿主启动时递归扫描 `exts\` 下所有 `.osx`，按请求里的 `kit` 字段分发调用
- **插件不常驻**：每次调用临时拉起，处理完一个请求就退出

### 文件名叫什么无所谓（改名不影响调用）

宿主调用插件**不认文件名**，只认三样东西：`kit` 能力名、`.osx` 扩展名、`exts\` 目录。所以官方插件（`osmium-okits.osx`）改成任意名字（比如 `my-tools.osx`、`随便什么.osx`），只要满足上面三点，所有功能照常：

- 宿主内置配置字段照常：`download_auth = "sspi"`、`download_unzip = true`、`shared_directory_mappers`、`failure_action = "reboot"` —— 它们调的是 kit 名（`sspi`/`unzip`/`netmap`/`reboot`），跟文件名无关
- 配置里 `[[plugins]]` 声明的 `kit` 照常命中
- `--extend` 照常列出（只是显示的名字变成新文件名）

调用链是这样的：

```
run_plugin("sspi", ...)       # 宿主只关心 kit 名
  → discover_plugins()        # 扫描 exts\*.osx —— 不看名字，全量收集
  → 广播 {"kit":"sspi", ...}  # 请求里只有能力名，没有文件名
  → 插件自己认领              # 内部按 kit 字段分发，认得就干
  → 首个 ok 即成功
```

正因为认能力不认文件，才有的这些特性：

- **改名自由**：插件换名字、换版本、升级替换，宿主和配置一行不用动
- **多插件共存**：`exts\` 下可以同时放官方插件和任意多个第三方插件，互不干扰
- **同名能力多实现**：多个插件都响应同一个 kit 时，宿主按发现顺序取第一个成功的
- **一个文件多能力**：官方插件一个文件同时响应 `ping`/`sspi`/`netmap`/`unzip`/`reboot` 五个 kit

唯一要注意的：

1. 扩展名必须是 `.osx`（改成 `.exe` 之类 `discover_plugins` 就找不到了）
2. 必须放在宿主 exe 同级的 `exts\` 目录
3. 插件内部的 kit 分发逻辑不能改（比如把 `sspi` 分发改成了别的名字，配置里写 `sspi` 就命中不了了——这种情况才需要同步改配置）

### 检查插件是否可用

```powershell
os --extend
# 或简写
os --ext
```

输出每个插件的状态：**绿点 ●** = 可用，**红点 ●** = 不可用（ACL 不可信 / 协议不响应 / 已损坏）。

## 官方插件 osmium-okits.osx

官方插件随安装包分发（组件页"官方扩展包"默认不勾选，勾上才有），内置这些能力：

| kit      | 功能                                                         | 宿主内置配置字段（更省事）  |
| --- | --- | --- |
| `ping`   | 可用性探测（宿主 `--extend` 自检用）                         | 不用配                      |
| `sspi`   | Windows 集成认证下载（Negotiate/NTLM/Kerberos 401 挑战循环） | `download_auth = "sspi"`    |
| `netmap` | 网络共享目录映射 / 断开                                      | `shared_directory_mappers`  |
| `unzip`  | zip 解压（防 zip-slip 穿越）                                 | `download_unzip = true`     |
| `reboot` | 系统重启（崩溃恢复动作）                                     | `failure_action = "reboot"` |

### 官方功能怎么用

1. **宿主内置字段**（最省事）：解压、共享映射、重启、sspi 下载都有现成配置字段，宿主自动调对应插件：

```toml
# sspi 认证下载（经 osmium-kit-sspi 插件完成）
download_url = "https://server/app.exe"
download_auth = "sspi"

# 下载 zip 后自动解压（经 unzip 插件）
download_unzip = true

# 启动时映射共享、停止时断开（经 netmap 插件）
[[shared_directory_mappers]]
local_path = "Z:"
remote_path = '\\server\share'

# 崩溃后重启系统（经 reboot 插件）
failure_action = "reboot"
```

2. **`plugins` 配置驱动**（通用通道，第三方插件也走这个）：在服务配置里声明生命周期调用：

```toml
[[plugins]]
kit = "backup"              # 插件能力标识（对应插件请求 JSON 的 kit 字段）
phase = "start_after"       # start / start_after / stop_before / stop
payload = { mode = "full" } # 可选参数，合并进请求 JSON 透传给插件
fail_on_error = false       # 可选；true 时插件在 start 阶段失败会阻断启动
```

## 插件协议

所有插件共用一套协议，跟语言无关（Rust / C / Go / Python 打包都行）：

| 项     | 规则                                                                |
| --- | --- |
| 调用   | 宿主 spawn 插件进程（不带命令行参数，`CREATE_NO_WINDOW`）           |
| 输入   | stdin 一行 JSON，含 `kit` 字段（宿主注入）+ 业务字段                |
| 输出   | stdout 一行 JSON：`{"ok": true}` 或 `{"ok": false, "error": "..."}` |
| 退出码 | 0 = 成功，非 0 = 失败（和 ok 字段双重判定）                         |
| stderr | 人类能读的错误信息（不污染协议，宿主调用时丢弃）                    |
| 空输入 | 静默退出（双击运行场景不产生输出）                                  |
| 限制   | stdin 上限 1MB；宿主 5 秒超时强杀（防插件挂死宿主）                 |

## 第三方插件开发

写一个插件其实很简单：实现协议、放进 `exts\`、配置里声明、`--extend` 看绿点。下面给出 5 种语言的完整示例，都是同一个 backup 能力，逻辑完全一致，挑你顺手的抄。

### Rust 示例

```rust
use std::io::Read;
use serde_json::Value;

fn main() {
    let mut input = String::new();
    // 限制输入大小: 防异常调用方喂超大输入
    let _ = std::io::stdin().take(1024 * 1024).read_to_string(&mut input);
    if input.trim().is_empty() {
        std::process::exit(0); // 无调用方（双击）: 静默退出
    }
    let req: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => fail(&format!("invalid request: {e}")),
    };
    // 按 kit 字段分发: 不是你的能力就明确报错
    match req["kit"].as_str().unwrap_or("") {
        "backup" => { /* 执行业务 */ println!(r#"{{"ok":true}}"#); }
        other => fail(&format!("unknown kit: {other}")),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("osmium-kit error: {msg}");          // stderr: 给人看的
    println!(r#"{{"ok":false,"error":"{msg}"}}"#); // stdout: 协议响应
    std::process::exit(1);
}
```

### C++ 示例（nlohmann/json）

需要单头库 [nlohmann/json](https://github.com/nlohmann/json)，VS 或 MinGW 编译都行。

```cpp
#include <iostream>
#include <string>
#include <nlohmann/json.hpp>

using json = nlohmann::json;

int fail(const std::string& msg) {
    std::cerr << "osmium-kit error: " << msg << std::endl;         // stderr: 给人看的
    std::cout << "{\"ok\":false,\"error\":\"" << msg << "\"}" << std::endl; // stdout: 协议响应
    return 1;
}

int main() {
    // 读完整 stdin（宿主只喂 1MB 以内，不放心可以自己截断）
    std::string input((std::istreambuf_iterator<char>(std::cin)), std::istreambuf_iterator<char>());
    if (input.empty()) {
        return 0; // 无调用方（双击）: 静默退出
    }
    json req;
    try {
        req = json::parse(input);
    } catch (...) {
        return fail("invalid request");
    }
    std::string kit = req.value("kit", "");
    if (kit == "backup") {
        // 执行业务
        std::cout << R"({"ok":true})" << std::endl;
        return 0;
    }
    return fail("unknown kit: " + kit);
}
```

### Python 示例

标准库就够，不需要任何第三方包。

```python
import json
import sys


def fail(msg):
    print(f"osmium-kit error: {msg}", file=sys.stderr)      # stderr: 给人看的
    print(json.dumps({"ok": False, "error": msg}))          # stdout: 协议响应
    sys.exit(1)


def main():
    # 限制输入大小: 只读前 1MB
    data = sys.stdin.buffer.read(1024 * 1024)
    if not data.strip():
        sys.exit(0)  # 无调用方（双击）: 静默退出
    try:
        req = json.loads(data)
    except ValueError as e:
        fail(f"invalid request: {e}")
    kit = req.get("kit", "")
    if kit == "backup":
        # 执行业务
        print(json.dumps({"ok": True}))
    else:
        fail(f"unknown kit: {kit}")


if __name__ == "__main__":
    main()
```

### Java 示例（无第三方依赖）

JDK 标准库没有 JSON 解析，这里给一个不依赖任何库的极简 `kit` 字段提取；生产环境建议换 Jackson / Gson。

```java
import java.io.IOException;
import java.nio.charset.StandardCharsets;

public class Plugin {

    public static void main(String[] args) throws IOException {
        // 限制输入大小: 只读前 1MB
        byte[] buf = new byte[1024 * 1024];
        int n = System.in.read(buf);
        String input = new String(buf, 0, Math.max(n, 0), StandardCharsets.UTF_8).trim();
        if (input.isEmpty()) {
            return; // 无调用方（双击）: 静默退出
        }
        String kit = extractKit(input);
        if ("backup".equals(kit)) {
            // 执行业务
            System.out.println("{\"ok\":true}");
        } else {
            fail("unknown kit: " + kit);
        }
    }

    // 极简提取 "kit":"xxx"（不解析完整 JSON，字段顺序无所谓）
    private static String extractKit(String json) {
        int i = json.indexOf("\"kit\"");
        if (i < 0) return "";
        int c = json.indexOf(':', i);
        if (c < 0) return "";
        int q1 = json.indexOf('"', c + 1);
        if (q1 < 0) return "";
        int q2 = json.indexOf('"', q1 + 1);
        return q2 < 0 ? "" : json.substring(q1 + 1, q2);
    }

    private static void fail(String msg) {
        System.err.println("osmium-kit error: " + msg);                 // stderr: 给人看的
        System.out.println("{\"ok\":false,\"error\":\"" + msg + "\"}"); // stdout: 协议响应
        System.exit(1);
    }
}
```

### Node.js 示例

标准库就够，`JSON.parse` 内置。

```js
// 限制输入大小: 只读前 1MB
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
    input += chunk;
    if (input.length > 1024 * 1024) {
        process.exit(1); // 超限快速失败
    }
});
process.stdin.on('end', () => {
    if (!input.trim()) {
        process.exit(0); // 无调用方（双击）: 静默退出
    }
    let req;
    try {
        req = JSON.parse(input);
    } catch (e) {
        return fail('invalid request: ' + e.message);
    }
    const kit = req.kit || '';
    if (kit === 'backup') {
        // 执行业务
        console.log(JSON.stringify({ ok: true }));
    } else {
        fail('unknown kit: ' + kit);
    }
});

function fail(msg) {
    console.error('osmium-kit error: ' + msg);               // stderr: 给人看的
    console.log(JSON.stringify({ ok: false, error: msg }));  // stdout: 协议响应
    process.exit(1);
}
```

### 接入步骤

1. 把编译好的程序改名为 `xxx.osx`
2. 放进宿主 exe 同级的 `exts\` 目录（或安装目录 `%ProgramFiles%\Osmium\exts\`）
3. 目录和插件文件要满足信任要求（见下面"几个要注意的点"）
4. 在服务配置里声明调用：

```toml
[[plugins]]
kit = "backup"            # 必须和插件内分发的 kit 名一致
phase = "start_after"
payload = { mode = "full" }
```

5. 跑 `os --extend` 确认绿点，重启服务生效

### 多插件与执行顺序

- 同一 phase 按配置数组声明顺序逐个执行
- 每个调用独立拉起插件进程，互不干扰、没有状态共享
- 单个插件失败不影响其他插件（`fail_on_error` 只在 start 阶段能阻断）
- 同一 kit 可以被多个插件声明，宿主按 `exts\` 发现顺序取第一个成功的

## 几个要注意的点

- **ACL 信任校验**：信任锚点是宿主 exe 自身位置——exe 装在受保护位置（如 `%ProgramFiles%\Osmium\`）时，插件目录和文件必须放在仅 SYSTEM / Administrators 可写的地方（防止有人偷偷换掉 `.osx` 提权成 SYSTEM），不符合的插件会被拒绝执行、标红；**inplace 集成部署**（exe 放在你自己的项目目录）时插件与 exe 同级，攻击面跟宿主一致，自动放行（能替换插件的攻击者同样能替换 exe，不额外增加风险）
- **执行隔离**：插件是独立进程，5 秒超时强杀，崩了不影响宿主
- **输入限制**：stdin 1MB 上限；官方 unzip 插件还有总解压 8GiB 上限（zip bomb 兜底）
- **凭据安全**：插件请求里的密码由宿主从配置解密后传入，日志只记去敏后的 URL

## 常见问题

**插件显示红点 / 日志报 "writable by unprivileged users"**：`exts\` 目录或插件文件被非管理员账户可写（比如解压到了用户目录）。把插件放到管理员安装的 `%ProgramFiles%\Osmium\exts\` 就行。

**日志报 "plugin 'xxx' not found (exts\*.osx missing)"**：`exts\` 下没有 `.osx`，或者插件扩展名不是 `.osx`。

**插件改名后配置失效了吗**：不会。配置只认 `kit` 能力名，不认文件名；只要扩展名还是 `.osx` 且在 `exts\` 下就行。

**想让插件常驻运行**：插件协议是一次性调用（拉起 → 处理 → 退出）。要常驻服务就用 Osmium 宿主管目标进程，别写成插件。
