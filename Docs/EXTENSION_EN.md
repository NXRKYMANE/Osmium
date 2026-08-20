# Osmium Plugin Development & Usage Guide

Osmium is plugin-everything: official advanced features and third-party extensions are all standalone executables (`.osx`) placed under the executable's directory (the platform installer uses `exts\`), launched by the host on demand. How plugins work, the protocol, and how to write one — it's all here.

## What a Plugin Is

- A plugin is just an ordinary program with its extension renamed to `.osx` (e.g. `osmium-kit.exe` → `osmium64-official-kits.osx`)
- Plugins live anywhere under the host exe's directory — the host recursively discovers every `.osx` (skipping dot-hidden folders), so standalone deployments can put plugins directly next to the exe; the platform installer still ships the official kit to `%ProgramFiles%\Osmium\exts\`
- At startup the host recursively scans every `.osx` under the executable's directory and dispatches requests by the `kit` field
- **Plugins are not resident**: each call launches a fresh process, which handles one request and exits

### The File Name Doesn't Matter (Renaming Doesn't Break Calls)

The host does **not** identify plugins by file name — it only cares about three things: the `kit` capability name, the `.osx` extension, and discoverability under the executable's directory. So renaming the official plugin (`osmium64-official-kits.osx`) to any other name (e.g. `my-tools.osx`, `whatever.osx`) keeps every feature working, as long as those three hold:

- Host built-in config fields keep working: `download_auth = "sspi"`, `download_unzip = true`, `shared_directory_mappers`, `failure_action = "reboot"` — they call the kit names (`sspi`/`unzip`/`netmap`/`reboot`), which have nothing to do with file names
- A `kit` declared in a `[[plugins]]` block still matches
- `--extend` still lists it (just showing the new file name)

The call chain looks like this:

```
run_plugin("sspi", ...)        # the host only cares about the kit name
  → discover_plugins()         # scans *.osx under the executable — no name matching, collects all
  → broadcast {"kit":"sspi"}   # the request carries the capability name, not a file name
  → the plugin claims it       # internal dispatch by the kit field — recognize and run
  → first ok wins
```

Because it resolves capabilities instead of files, you get:

- **Free renaming**: swap plugin names, versions, or upgrades — the host and config need zero changes
- **Multiple plugins coexist**: the official plugin and any number of third-party plugins can live in `exts\` side by side without interference
- **Multiple implementations of one capability**: when several plugins respond to the same kit, the host takes the first success in discovery order
- **One file, many capabilities**: the official plugin responds to five kits (`ping`/`sspi`/`netmap`/`unzip`/`reboot`) from a single file

The only things to watch:

1. The extension must stay `.osx` (renaming to `.exe` or similar means `discover_plugins` can't find it)
2. It must be discoverable under the host exe's directory (any depth; dot-hidden folders are skipped)
3. The plugin's internal kit dispatch must not change (e.g. if you rename the `sspi` dispatch inside the plugin, a config writing `sspi` can't hit it anymore — only in that case do you need to update the config too)

### Checking Whether a Plugin Is Usable

```powershell
os --extend
# or the short alias
os --ext
```

Prints each plugin's status: **green dot ●** = usable, **red dot ●** = unusable (untrusted ACL / protocol not responding / broken).

## Official Plugin osmium64-official-kits.osx

The official plugin ships with the installer (component page "Official extension kit", unchecked by default — tick it to get it), with these built-in capabilities:

| kit      | Feature                                                                       | Host built-in config field (easier) |
| --- | --- | --- |
| `ping`   | Availability probe (used by the host's `--extend` self-check)                 | nothing to configure                |
| `sspi`   | Windows integrated-auth download (Negotiate/NTLM/Kerberos 401 challenge loop) | `download_auth = "sspi"`            |
| `netmap` | Network share mapping / disconnecting                                         | `shared_directory_mappers`          |
| `unzip`  | zip extraction (zip-slip traversal blocked)                                   | `download_unzip = true`             |
| `reboot` | System reboot (failure-recovery action)                                       | `failure_action = "reboot"`         |

### Two Ways to Use Official Features

1. **Host built-in fields** (easiest): unzip, share mapping, reboot and sspi download all have ready-made config fields — the host calls the matching plugin automatically:

```toml
# sspi-authenticated download (done via the osmium-kit-sspi plugin)
download_url = "https://server/app.exe"
download_auth = "sspi"

# auto-extract downloaded zip (via the unzip plugin)
download_unzip = true

# map shares at start, disconnect at stop (via the netmap plugin)
[[shared_directory_mappers]]
local_path = "Z:"
remote_path = '\\server\share'

# reboot the system after a crash (via the reboot plugin)
failure_action = "reboot"
```

2. **`plugins` config-driven calls** (generic channel — third-party plugins use this too): declare lifecycle calls in the service config:

```toml
[[plugins]]
kit = "backup"              # plugin capability id (the kit field of the plugin request JSON)
phase = "start_after"       # start / start_after / stop_before / stop
payload = { mode = "full" } # optional args, merged into the request JSON and passed to the plugin
fail_on_error = false       # optional; true blocks startup when the plugin fails in the start phase
```

## Plugin Protocol

All plugins share one protocol, independent of language (Rust / C / Go / Python packaging all work):

| Item        | Rule                                                                                                        |
| --- | --- |
| Invocation  | the host spawns the plugin process (no command-line args, `CREATE_NO_WINDOW`)                               |
| Input       | one line of JSON on stdin, with the `kit` field (injected by the host) + business fields                    |
| Output      | one line of JSON on stdout: `{"ok": true}` or `{"ok": false, "error": "..."}`                               |
| Exit code   | 0 = success, non-zero = failure (double-checked with the ok field)                                          |
| stderr      | human-readable error info (does not pollute the protocol; discarded by the host)                            |
| Empty input | exits silently (double-click scenario produces no output)                                                   |
| Limits      | stdin capped at 1MB; the host force-kills after a 5-second timeout (so a stuck plugin cannot hang the host) |

## Third-Party Plugin Development

Writing a plugin is actually simple: implement the protocol, drop it into `exts\`, declare it in the config, and check the green dot with `--extend`. Below are complete examples in 5 languages — all implementing the same `backup` capability with identical logic; pick whichever you're comfortable with.

### Rust Example

```rust
use std::io::Read;
use serde_json::Value;

fn main() {
    let mut input = String::new();
    // cap input size: protect against an abnormal caller feeding oversized input
    let _ = std::io::stdin().take(1024 * 1024).read_to_string(&mut input);
    if input.trim().is_empty() {
        std::process::exit(0); // no caller (double-click): exit silently
    }
    let req: Value = match serde_json::from_str(&input) {
        Ok(v) => v,
        Err(e) => fail(&format!("invalid request: {e}")),
    };
    // dispatch by kit field: if it's not your capability, fail with a clear message
    match req["kit"].as_str().unwrap_or("") {
        "backup" => { /* do the business */ println!(r#"{{"ok":true}}"#); }
        other => fail(&format!("unknown kit: {other}")),
    }
}

fn fail(msg: &str) -> ! {
    eprintln!("osmium-kit error: {msg}");          // stderr: for humans
    println!(r#"{{"ok":false,"error":"{msg}"}}"#); // stdout: protocol response
    std::process::exit(1);
}
```

### C++ Example (nlohmann/json)

Needs the single-header library [nlohmann/json](https://github.com/nlohmann/json); compiles with VS or MinGW.

```cpp
#include <iostream>
#include <string>
#include <nlohmann/json.hpp>

using json = nlohmann::json;

int fail(const std::string& msg) {
    std::cerr << "osmium-kit error: " << msg << std::endl;         // stderr: for humans
    std::cout << "{\"ok\":false,\"error\":\"" << msg << "\"}" << std::endl; // stdout: protocol response
    return 1;
}

int main() {
    // read all of stdin (the host only feeds up to 1MB; truncate yourself if you want to be safe)
    std::string input((std::istreambuf_iterator<char>(std::cin)), std::istreambuf_iterator<char>());
    if (input.empty()) {
        return 0; // no caller (double-click): exit silently
    }
    json req;
    try {
        req = json::parse(input);
    } catch (...) {
        return fail("invalid request");
    }
    std::string kit = req.value("kit", "");
    if (kit == "backup") {
        // do the business
        std::cout << R"({"ok":true})" << std::endl;
        return 0;
    }
    return fail("unknown kit: " + kit);
}
```

### Python Example

The standard library is enough — no third-party packages.

```python
import json
import sys


def fail(msg):
    print(f"osmium-kit error: {msg}", file=sys.stderr)      # stderr: for humans
    print(json.dumps({"ok": False, "error": msg}))          # stdout: protocol response
    sys.exit(1)


def main():
    # cap input size: read only the first 1MB
    data = sys.stdin.buffer.read(1024 * 1024)
    if not data.strip():
        sys.exit(0)  # no caller (double-click): exit silently
    try:
        req = json.loads(data)
    except ValueError as e:
        fail(f"invalid request: {e}")
    kit = req.get("kit", "")
    if kit == "backup":
        # do the business
        print(json.dumps({"ok": True}))
    else:
        fail(f"unknown kit: {kit}")


if __name__ == "__main__":
    main()
```

### Java Example (no third-party dependencies)

The JDK has no built-in JSON parser, so here's a dependency-free minimal `kit` extraction; for production, switch to Jackson / Gson.

```java
import java.io.IOException;
import java.nio.charset.StandardCharsets;

public class Plugin {

    public static void main(String[] args) throws IOException {
        // cap input size: read only the first 1MB
        byte[] buf = new byte[1024 * 1024];
        int n = System.in.read(buf);
        String input = new String(buf, 0, Math.max(n, 0), StandardCharsets.UTF_8).trim();
        if (input.isEmpty()) {
            return; // no caller (double-click): exit silently
        }
        String kit = extractKit(input);
        if ("backup".equals(kit)) {
            // do the business
            System.out.println("{\"ok\":true}");
        } else {
            fail("unknown kit: " + kit);
        }
    }

    // minimal extraction of "kit":"xxx" (no full JSON parse; field order doesn't matter)
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
        System.err.println("osmium-kit error: " + msg);                 // stderr: for humans
        System.out.println("{\"ok\":false,\"error\":\"" + msg + "\"}"); // stdout: protocol response
        System.exit(1);
    }
}
```

### Node.js Example

The standard library is enough — `JSON.parse` is built in.

```js
// cap input size: read only the first 1MB
let input = '';
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
    input += chunk;
    if (input.length > 1024 * 1024) {
        process.exit(1); // fail fast when over the limit
    }
});
process.stdin.on('end', () => {
    if (!input.trim()) {
        process.exit(0); // no caller (double-click): exit silently
    }
    let req;
    try {
        req = JSON.parse(input);
    } catch (e) {
        return fail('invalid request: ' + e.message);
    }
    const kit = req.kit || '';
    if (kit === 'backup') {
        // do the business
        console.log(JSON.stringify({ ok: true }));
    } else {
        fail('unknown kit: ' + kit);
    }
});

function fail(msg) {
    console.error('osmium-kit error: ' + msg);               // stderr: for humans
    console.log(JSON.stringify({ ok: false, error: msg }));  // stdout: protocol response
    process.exit(1);
}
```

### Getting Started

1. Rename the compiled program to `xxx.osx`
2. Drop it anywhere under the host exe's directory (standalone: next to the exe; platform install: `%ProgramFiles%\Osmium\exts\`)
3. The directory and plugin file must satisfy the trust requirement (see "Things to Keep in Mind" below)
4. Declare the call in the service config:

```toml
[[plugins]]
kit = "backup"            # must match the kit name dispatched inside the plugin
phase = "start_after"
payload = { mode = "full" }
```

5. Run `os --extend` to confirm the green dot, then restart the service

### Multiple Plugins & Execution Order

- Same phase runs in declaration order of the config array
- Each call launches a separate plugin process — no interference, no shared state
- A single plugin failure does not affect the others (`fail_on_error` can only block in the start phase)
- The same kit can be declared by multiple plugins; the host takes the first success in discovery order

## Things to Keep in Mind

- **ACL trust check**: the trust anchor is the host exe's own location — when the exe lives in a protected location (e.g. `%ProgramFiles%\Osmium\`), the plugin directory and file must sit somewhere only SYSTEM / Administrators can write (so nobody can swap a `.osx` to escalate to SYSTEM); untrusted plugins are refused and marked red. **Inplace embedded deployment** (exe inside your own project directory) is trusted automatically: the plugin sits next to the exe, sharing the same attack surface — an attacker who could replace the plugin could equally replace the exe, so no extra risk is added
- **Execution isolation**: plugins are separate processes, force-killed after 5 seconds; a crash does not affect the host
- **Input limits**: stdin capped at 1MB; the official unzip plugin also has a total 8GiB extraction cap (zip-bomb protection)
- **Credential safety**: passwords in plugin requests are decrypted by the host from the config before being passed in; logs only record redacted URLs

## FAQ

**Plugin shows a red dot / log says "writable by unprivileged users"**: the `exts\` directory or the plugin file is writable by a non-admin account (e.g. you extracted it to a user directory). Put the plugin into the admin-installed `%ProgramFiles%\Osmium\exts\` and you're done.

**Log says "plugin 'xxx' not found (exts\*.osx missing)"**: there's no `.osx` under the executable's directory, or the plugin extension isn't `.osx`.

**Does renaming the plugin break my config?** No. The config only knows the `kit` capability name, not the file name; as long as the extension stays `.osx` and it sits under the executable's directory, it works.

**Want a resident plugin?** The plugin protocol is one-shot (launch → handle → exit). If you need a resident service, use the Osmium host to manage the target process — don't write it as a plugin.
