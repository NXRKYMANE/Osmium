# Osmium 一键构建: Rust 构建与测试 + 官方插件 + 安装包
# 用法: .\.release.ps1 [-SkipTests] [-Upx] [-SkipSign]

param(
    [switch]$SkipTests,
    [switch]$Upx,
    [switch]$SkipSign
)

$ErrorActionPreference = "Continue"
$ProjectRoot = $PSScriptRoot
$ISCC = "C:\Program Files\Inno Setup 7\ISCC.exe"

# 代码签名工具链: 本机 SDK 自带 signtool.exe（找不到则跳过签名）
$signtool = Get-ChildItem "$env:ProgramFiles(x86)\Windows Kits\10\bin","F:\DevTools\Windows11 SDK\bin" -Recurse -Filter "signtool.exe" -ErrorAction SilentlyContinue | Where-Object { $_.FullName -match "\\x64\\" } | Sort-Object FullName -Descending | Select-Object -First 1
if (-not $signtool) {
    $signtool = Get-ChildItem "$env:ProgramFiles(x86)\Windows Kits\10\bin" -Recurse -Filter "signtool.exe" -ErrorAction SilentlyContinue | Sort-Object FullName -Descending | Select-Object -First 1
}

# 代码签名证书来源（优先级）:
#   1. 环境变量 OSMIUM_CERT_PFX（路径）+ OSMIUM_CERT_PASSWORD（可选）
#   2. 仓库 Misc\codesign.pfx（自签名开发证书，无需密码）
# 找不到证书或 signtool 时自动跳过签名
function Get-SignCert {
    if ($env:OSMIUM_CERT_PFX -and (Test-Path $env:OSMIUM_CERT_PFX)) {
        return @{ Pfx = $env:OSMIUM_CERT_PFX; Password = $env:OSMIUM_CERT_PASSWORD }
    }
    $devPfx = Join-Path $ProjectRoot "Misc\codesign.pfx"
    if (Test-Path $devPfx) {
        # 自签名开发证书密码: 优先取环境变量 OSMIUM_DEV_CERT_PASSWORD（密码不应随仓库分发）；
        # 未设置时回退仓库内固定密码并告警（仅本地开发可用）
        $devPass = $env:OSMIUM_DEV_CERT_PASSWORD
        if (-not $devPass) {
            $devPass = "OsmiumDevSign2026!"
            Write-Warning "OSMIUM_DEV_CERT_PASSWORD not set, using the repo-default dev certificate password."
        }
        return @{ Pfx = $devPfx; Password = $devPass }
    }
    return $null
}

# 签名单个文件（带时间戳; 时间戳服务器不可达时回退无时间戳并告警）
function Sign-File([string]$file, $cert) {
    if (-not $signtool) { Write-Warning "signtool not found, skipping signature: $file"; return }
    if (-not (Test-Path $file)) { Write-Warning "File not found, skipping signature: $file"; return }
    $stamps = @("http://timestamp.digicert.com", "http://timestamp.sectigo.com", "http://timestamp.comodoca.com")
    $stamped = $false
    foreach ($ts in $stamps) {
        $args = @("sign", "/f", $cert.Pfx, "/fd", "SHA256", "/tr", $ts, "/td", "SHA256")
        if ($cert.Password) { $args += @("/p", $cert.Password) }
        $args += $file
        & $signtool.FullName @args | Out-Null
        if ($LASTEXITCODE -eq 0) { $stamped = $true; break }
    }
    if (-not $stamped) {
        $args = @("sign", "/f", $cert.Pfx, "/fd", "SHA256")
        if ($cert.Password) { $args += @("/p", $cert.Password) }
        $args += $file
        & $signtool.FullName @args | Out-Null
        if ($LASTEXITCODE -eq 0) {
            Write-Warning "Signed WITHOUT timestamp (timestamp servers unreachable): $file"
        } else {
            Write-Warning "Signing failed: $file (exit $LASTEXITCODE)"
        }
    } else {
        Write-Host "Signed: $file" -ForegroundColor Green
    }
}

# UPX 压缩复制: 临时副本压缩后落到目标（不动源文件），压缩参数用 --lzma（实测与 --ultra-brute 体积几乎一致, 快 60 倍）
# 签名由调用方在 4.5 段统一完成（此时证书才确定）
function Compress-Upx([string]$src, [string]$dst, [string]$label) {
    if (-not (Test-Path $upxPath)) { throw "UPX not found at $upxPath" }
    $tmp = Join-Path $env:TEMP ("os-upx-" + [guid]::NewGuid().ToString("N") + ".exe")
    Copy-Item $src $tmp -Force
    & $upxPath --lzma $tmp | Out-Null
    if ($LASTEXITCODE -ne 0) { Remove-Item $tmp -Force -ErrorAction SilentlyContinue; throw "UPX compression failed: $src" }
    Copy-Item $tmp $dst -Force
    Remove-Item $tmp -Force
    Write-Host "Done: $dst ($label)" -ForegroundColor Green
}

# 切到 x86 交叉工具链（无 vswhere 的手动环境）: 返回当前环境快照供 Restore-Env 恢复
# 标准 VS 环境（vswhere 存在）由 rustc/cc-rs 自动按 target 选择工具链, 无需切换
function Save-X86Env {
    $saved = @{ Lib = $env:LIB; Include = $env:INCLUDE; Path = $env:PATH; Cc = $env:CC_i686_pc_windows_msvc }
    if (-not (Test-Path $vswhere)) {
        $env:LIB = "$($msvc.FullName)\lib\x86;$sdkBase\Lib\$($sdkVer.Name)\ucrt\x86;$sdkBase\Lib\$($sdkVer.Name)\um\x86"
        $env:INCLUDE = "$($msvc.FullName)\include;$sdkBase\Include\$($sdkVer.Name)\ucrt;$sdkBase\Include\$($sdkVer.Name)\um;$sdkBase\Include\$($sdkVer.Name)\shared"
        $env:PATH = "$($msvc.FullName)\bin\Hostx64\x86;$env:PATH"
        # cc-rs（ring 汇编）必须显式指定 x86 编译器, 否则按 PATH 误选 x64 机器类型导致 LNK1112
        $env:CC_i686_pc_windows_msvc = "$($msvc.FullName)\bin\Hostx64\x86\cl.exe"
    }
    return $saved
}

function Restore-Env($saved) {
    $env:LIB = $saved.Lib
    $env:INCLUDE = $saved.Include
    $env:PATH = $saved.Path
    if ($null -eq $saved.Cc) { Remove-Item Env:CC_i686_pc_windows_msvc -ErrorAction SilentlyContinue }
    else { $env:CC_i686_pc_windows_msvc = $saved.Cc }
}

# 工具链：无 VS（vswhere）时使用本机 F:\DevTools 的 MSVC + SDK（自动取最新版本），跳过 vcvarsall 查找
# MSVC 版本目录以数字开头（如 14.51.36231）; "Tools" 等非版本目录需排除
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    $msvc = Get-ChildItem "F:\DevTools\MSVC" -Directory -ErrorAction SilentlyContinue | Where-Object { $_.Name -match '^\d' } | Sort-Object Name -Descending | Select-Object -First 1
    $sdkVer = Get-ChildItem "F:\DevTools\Windows11 SDK\Lib" -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
    $sdkBase = "F:\DevTools\Windows11 SDK"
    if ($msvc -and $sdkVer -and (Test-Path "$($msvc.FullName)\bin\Hostx64\x64\link.exe")) {
        $env:PATH = "$($msvc.FullName)\bin\Hostx64\x64;$env:PATH"
        $env:LIB = "$($msvc.FullName)\lib\x64;$sdkBase\Lib\$($sdkVer.Name)\ucrt\x64;$sdkBase\Lib\$($sdkVer.Name)\um\x64"
        $env:INCLUDE = "$($msvc.FullName)\include;$sdkBase\Include\$($sdkVer.Name)\ucrt;$sdkBase\Include\$($sdkVer.Name)\um;$sdkBase\Include\$($sdkVer.Name)\shared"
    }
}

# 1. 读取版本号 (Cargo.toml) —— 主程序与插件各自独立版本（插件文件名带它自己的版本）
$cargoToml = Get-Content "$ProjectRoot\Project\Cargo.toml" -Raw
$rsVersion = [regex]::Match($cargoToml, '^version = "([^"]+)"', 'Multiline').Groups[1].Value.Trim()
Write-Host "Version (Rust): $rsVersion" -ForegroundColor Cyan
$kitsToml = Get-Content "$ProjectRoot\Extension\osmium-official-kits\Cargo.toml" -Raw
$kitsVersion = [regex]::Match($kitsToml, '^version = "([^"]+)"', 'Multiline').Groups[1].Value.Trim()
Write-Host "Version (kits): $kitsVersion" -ForegroundColor Cyan

# UPX 路径（插件发行版构建时直接压缩；不可用时回退未压缩并告警）
$upxPath = "F:\DevTools\UPX\upx.exe"

# 2. 一次 workspace 构建: 主程序 (opt-3) + 官方插件 (per-package opt-level=z)
# 合并构建让依赖树只编译一次（分开构建时共享依赖会被重复编译）
Write-Host "Building workspace (osmium + kits, release)..." -ForegroundColor Yellow
Push-Location "$ProjectRoot"
try {
    cargo build --release --workspace
    if ($LASTEXITCODE -ne 0) { throw "Rust build failed" }
} finally {
    Pop-Location
}

# 2.5 单元测试（主程序 release 测试; 插件 debug 测试更快）
if (-not $SkipTests) {
    Write-Host "Running Osmium unit tests..." -ForegroundColor Yellow
    Push-Location "$ProjectRoot\Project"
    try {
        cargo test --release
        if ($LASTEXITCODE -ne 0) { throw "Rust tests failed" }
    } finally { Pop-Location }
    Write-Host "Running kits unit tests..." -ForegroundColor Yellow
    Push-Location "$ProjectRoot\Extension\osmium-official-kits"
    try {
        cargo test
        if ($LASTEXITCODE -ne 0) { throw "Kits tests failed" }
    } finally { Pop-Location }
}

# 4. 整理 Publish 产物（先清空, 确保目录里只有最终产物）
$publishDir = Join-Path $ProjectRoot "Publish"
New-Item -ItemType Directory -Force -Path $publishDir | Out-Null
Get-ChildItem $publishDir -Force | Remove-Item -Recurse -Force
# 主程序: 直接以 osmium64.exe 输出（安装时改名为 os.exe）
Copy-Item (Join-Path $ProjectRoot "target\release\osmium.exe") (Join-Path $publishDir "osmium64.exe") -Force
# 官方插件: osmium-kits.exe → UPX 压缩 → exts\osmium64-official-kits-v<KITS_VERSION>.osx（文件名带插件自身版本）
# 插件发行版直接以 opt-level=z + UPX 压缩产物发布（不再区分 UPX/原版；upx 不可用则回退未压缩并告警）
$extDir = Join-Path $publishDir "exts"
New-Item -ItemType Directory -Force -Path $extDir | Out-Null
$kitOsxName = "osmium64-official-kits-v$kitsVersion.osx"
if (Test-Path $upxPath) {
    Compress-Upx (Join-Path $ProjectRoot "target\release\osmium-kits.exe") (Join-Path $extDir $kitOsxName) "kits 64-bit"
} else {
    Write-Warning "UPX not found, kits shipped uncompressed."
    Copy-Item (Join-Path $ProjectRoot "target\release\osmium-kits.exe") (Join-Path $extDir $kitOsxName) -Force
}

# 4.3 32 位构建 (i686): Publish\osmium32.exe + Publish\exts\osmium32-official-kits-v<VERSION>.osx（不生成安装包）
$i686Target = "i686-pc-windows-msvc"
$build32 = $false
$kitOsx32Name = $null
$haveI686 = (& rustup target list --installed 2>$null) -match [regex]::Escape($i686Target)
if (-not $haveI686) {
    Write-Host "Adding rust target $i686Target..." -ForegroundColor Yellow
    & rustup target add $i686Target
    if ($LASTEXITCODE -ne 0) { throw "rustup target add $i686Target failed" }
}
# x86 交叉工具链可用性: 标准 VS 由 rustc/cc-rs 自动处理；手动环境需 Hostx64\x86 链接器
if (Test-Path $vswhere) {
    $build32 = $true
} elseif ($msvc -and (Test-Path "$($msvc.FullName)\bin\Hostx64\x86\cl.exe")) {
    $build32 = $true
} else {
    Write-Warning "x86 MSVC toolchain (Hostx64\x86) not found, skipping 32-bit build."
}
if ($build32) {
    $savedEnv32 = Save-X86Env
    try {
        # 32 位一次 workspace 构建（per-package opt-level=z 已固化在根 Cargo.toml）
        Push-Location "$ProjectRoot"
        try {
            cargo build --release --target $i686Target --workspace
            if ($LASTEXITCODE -ne 0) { throw "32-bit build failed (workspace)" }
        } finally { Pop-Location }
        $kitOsx32Name = "osmium32-official-kits-v$kitsVersion.osx"
        Copy-Item "$ProjectRoot\target\i686-pc-windows-msvc\release\osmium.exe" (Join-Path $publishDir "osmium32.exe") -Force
        # 32 位插件同样 UPX 压缩（发行版即压缩版）
        if (Test-Path $upxPath) {
            Compress-Upx "$ProjectRoot\target\i686-pc-windows-msvc\release\osmium-kits.exe" (Join-Path $extDir $kitOsx32Name) "kits 32-bit"
        } else {
            Write-Warning "UPX not found, 32-bit kits shipped uncompressed."
            Copy-Item "$ProjectRoot\target\i686-pc-windows-msvc\release\osmium-kits.exe" (Join-Path $extDir $kitOsx32Name) -Force
        }
        Write-Host "Done: Publish\osmium32.exe" -ForegroundColor Green
        Write-Host "Done: Publish\exts\$kitOsx32Name" -ForegroundColor Green
    } finally {
        Restore-Env $savedEnv32
    }
}

# 4.5 代码签名: osmium64.exe + 插件（安装包在第 6 步编译完成后签名）
$signCert = $null
if (-not $SkipSign) {
    $signCert = Get-SignCert
    if ($signCert) {
        Sign-File (Join-Path $publishDir "osmium64.exe") $signCert
        Sign-File (Join-Path $extDir $kitOsxName) $signCert
        if ($build32 -and $kitOsx32Name) {
            Sign-File (Join-Path $publishDir "osmium32.exe") $signCert
            Sign-File (Join-Path $extDir $kitOsx32Name) $signCert
        }
    } else {
        Write-Warning "No code-signing certificate found (OSMIUM_CERT_PFX or Misc\codesign.pfx), skipping signature."
    }
}

# 5. 注入 installer.iss 版本号与版权年份后编译；构建结束还原文件——
#    版本仅注入本次编译产物，避免每次构建都把受跟踪的 installer.iss 弄成脏状态
Write-Host "Updating installer.iss..." -ForegroundColor Yellow
$year = (Get-Date).Year

$issPath = "$ProjectRoot\Project\installer.iss"
$issBackup = Join-Path $env:TEMP ("installer.iss." + [guid]::NewGuid().ToString("N") + ".bak")
Copy-Item $issPath $issBackup -Force
try {
    $rsIss = Get-Content $issPath -Raw -Encoding UTF8
    # CRLF 兼容: installer.iss 为 CRLF 行尾，正则须容忍行尾 \r（否则 $ 锚点匹配不到）
    $rsIss = $rsIss -replace '(?m)^#define MyAppVersion ".*"\r?$', "#define MyAppVersion `"$rsVersion`""
    $rsIss = $rsIss -replace '(?m)^#define KitsVersion ".*"\r?$', "#define KitsVersion `"$kitsVersion`""
    $rsIss = $rsIss -replace '(?m)(?<=^#define MyAppPublisher "Copyright \(C\) )\d{4}\r?$', $year
    [System.IO.File]::WriteAllText($issPath, $rsIss, [System.Text.UTF8Encoding]::new($false))

    # 6. 编译安装包
    Write-Host "Compiling installer..." -ForegroundColor Yellow
    & $ISCC $issPath
    if ($LASTEXITCODE -ne 0) { throw "Installer build failed" }
} finally {
    Copy-Item $issBackup $issPath -Force
    Remove-Item $issBackup -Force -ErrorAction SilentlyContinue
}

$setupName = "osmium-win-x64-setup-v$rsVersion.exe"
# 6.5 代码签名: 安装包编译完成后签名（Inno 的 SignTool 依赖其自带配置，统一在脚本侧完成）
if ($signCert) {
    Sign-File (Join-Path $publishDir $setupName) $signCert
}
Write-Host "Done: Publish\osmium64.exe" -ForegroundColor Green
Write-Host "Done: Publish\exts\$kitOsxName" -ForegroundColor Green
Write-Host "Done: Publish\$setupName" -ForegroundColor Green

# 7. 可选: 主程序 UPX 压缩版本
# 直接用第 2/4.3 步已构建的产物压缩（不再 opt-level=z 重建——切换 opt-level 会触发整个依赖树重编译, 非常慢；
# 且实测普通版 UPX 后 ~1.5 MB, 与 z 版差异很小, 换取大幅提速）
# 仅生成 Publish\osmium64-upx.exe / osmium32-upx.exe, 不覆盖普通版, 也不生成安装包
# 注: 插件已在第 4/4.3 段构建时直接 UPX 压缩, 此处不再处理
if (Test-Path $upxPath) {
    # -Upx 参数强制生成；否则交互询问（非交互终端自动跳过）
    if ($Upx) {
        $yn = "y"
    } else {
        try {
            $yn = Read-Host "Generate UPX-compressed build? (y/n)"
        } catch {
            $yn = "n"
        }
    }
    if ($yn -match '^[yY]') {
        Compress-Upx (Join-Path $publishDir "osmium64.exe") (Join-Path $publishDir "osmium64-upx.exe") "exe 64-bit UPX"
        if ($build32) {
            Compress-Upx (Join-Path $publishDir "osmium32.exe") (Join-Path $publishDir "osmium32-upx.exe") "exe 32-bit UPX"
        }
        if ($signCert) {
            Sign-File (Join-Path $publishDir "osmium64-upx.exe") $signCert
            if ($build32) { Sign-File (Join-Path $publishDir "osmium32-upx.exe") $signCert }
        }
    }
} else {
    Write-Host "UPX not found at $upxPath, skipping optional build." -ForegroundColor DarkYellow
}
