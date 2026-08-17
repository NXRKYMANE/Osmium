# Osmium 一键构建: Rust 构建与测试 + 官方插件 + 安装包
# 用法: .\BUILD.ps1 [-SkipTests] [-Upx] [-SkipSign]

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
        # 自签名开发证书固定密码（仅仓库内开发用；正式证书请用 OSMIUM_CERT_PFX/PASSWORD）
        return @{ Pfx = $devPfx; Password = "OsmiumDevSign2026!" }
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

# 工具链：无 VS（vswhere）时使用本机 F:\DevTools 的 MSVC + SDK（自动取最新版本），跳过 vcvarsall 查找
$vswhere = Join-Path ${env:ProgramFiles(x86)} "Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) {
    $msvc = Get-ChildItem "F:\DevTools\MSVC" -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
    $sdkVer = Get-ChildItem "F:\DevTools\Windows11 SDK\Lib" -Directory -ErrorAction SilentlyContinue | Sort-Object Name -Descending | Select-Object -First 1
    $sdkBase = "F:\DevTools\Windows11 SDK"
    if ($msvc -and $sdkVer -and (Test-Path "$($msvc.FullName)\bin\Hostx64\x64\link.exe")) {
        $env:PATH = "$($msvc.FullName)\bin\Hostx64\x64;$env:PATH"
        $env:LIB = "$($msvc.FullName)\lib\x64;$sdkBase\Lib\$($sdkVer.Name)\ucrt\x64;$sdkBase\Lib\$($sdkVer.Name)\um\x64"
        $env:INCLUDE = "$($msvc.FullName)\include;$sdkBase\Include\$($sdkVer.Name)\ucrt;$sdkBase\Include\$($sdkVer.Name)\um;$sdkBase\Include\$($sdkVer.Name)\shared"
    }
}

# 1. 读取版本号 (Cargo.toml)
$cargoToml = Get-Content "$ProjectRoot\Project\Cargo.toml" -Raw
$rsVersion = [regex]::Match($cargoToml, '^version = "([^"]+)"', 'Multiline').Groups[1].Value.Trim()
Write-Host "Version (Rust): $rsVersion" -ForegroundColor Cyan

# 2. 构建主程序 (release) + 测试
Write-Host "Building Osmium (release)..." -ForegroundColor Yellow
Push-Location "$ProjectRoot\Project"
try {
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "Rust build failed" }
    if (-not $SkipTests) {
        Write-Host "Running Osmium unit tests..." -ForegroundColor Yellow
        cargo test --release
        if ($LASTEXITCODE -ne 0) { throw "Rust tests failed" }
    }
} finally {
    Pop-Location
}

# 3. 构建官方插件 (osmium-official-kits, opt-level=z) + 测试
Write-Host "Building osmium-official-kits (release, size-first)..." -ForegroundColor Yellow
# --config 传内联字符串会被 PowerShell 5.1 剥离双引号（opt-level=z 解析失败），改用临时配置文件
$kitsProfile = Join-Path $env:TEMP "osmium-kits-profile.toml"
[System.IO.File]::WriteAllText($kitsProfile, 'profile.release.opt-level = "z"', [System.Text.UTF8Encoding]::new($false))
Push-Location "$ProjectRoot\Extension\osmium-official-kits"
try {
    cargo build --release --config $kitsProfile
    if ($LASTEXITCODE -ne 0) { throw "Kits build failed" }
    if (-not $SkipTests) {
        Write-Host "Running kits unit tests..." -ForegroundColor Yellow
        cargo test
        if ($LASTEXITCODE -ne 0) { throw "Kits tests failed" }
    }
} finally {
    Pop-Location
    Remove-Item $kitsProfile -Force -ErrorAction SilentlyContinue
}

# 4. 整理 Publish 产物（先清空, 确保目录里只有最终产物）
$publishDir = Join-Path $ProjectRoot "Publish"
New-Item -ItemType Directory -Force -Path $publishDir | Out-Null
Get-ChildItem $publishDir -Force | Remove-Item -Recurse -Force
# 主程序: osmium64.exe → os64.exe（安装时改名为 os.exe）
Copy-Item (Join-Path $ProjectRoot "target\release\osmium64.exe") (Join-Path $publishDir "os64.exe") -Force
# 官方插件: osmium-kit.exe → exts\osmium-okits.osx
$extDir = Join-Path $publishDir "exts"
New-Item -ItemType Directory -Force -Path $extDir | Out-Null
Copy-Item (Join-Path $ProjectRoot "target\release\osmium-kit.exe") (Join-Path $extDir "osmium-okits.osx") -Force

# 4.5 代码签名: os64.exe + osmium-okits.osx（安装包在第 6 步编译完成后签名）
$signCert = $null
if (-not $SkipSign) {
    $signCert = Get-SignCert
    if ($signCert) {
        Sign-File (Join-Path $publishDir "os64.exe") $signCert
        Sign-File (Join-Path $extDir "osmium-okits.osx") $signCert
    } else {
        Write-Warning "No code-signing certificate found (OSMIUM_CERT_PFX or Misc\codesign.pfx), skipping signature."
    }
}

# 5. 更新 installer.iss 的版本号和版权年份
Write-Host "Updating installer.iss..." -ForegroundColor Yellow
$year = (Get-Date).Year

$rsIss = Get-Content "$ProjectRoot\Project\installer.iss" -Raw -Encoding UTF8
$rsIss = $rsIss -replace '(?m)^#define MyAppVersion ".*"$', "#define MyAppVersion `"$rsVersion`""
$rsIss = $rsIss -replace '(?m)(?<=^#define MyAppPublisher "Copyright \(C\) )\d{4}', $year
[System.IO.File]::WriteAllText("$ProjectRoot\Project\installer.iss", $rsIss, [System.Text.UTF8Encoding]::new($false))

# 6. 编译安装包
Write-Host "Compiling installer..." -ForegroundColor Yellow
& $ISCC "$ProjectRoot\Project\installer.iss"
if ($LASTEXITCODE -ne 0) { throw "Installer build failed" }

$setupName = "osmium-win-x64-setup-v$rsVersion.exe"
# 6.5 代码签名: 安装包编译完成后签名（Inno 的 SignTool 依赖其自带配置，统一在脚本侧完成）
if ($signCert) {
    Sign-File (Join-Path $publishDir $setupName) $signCert
}
Write-Host "Done: Publish\os64.exe" -ForegroundColor Green
Write-Host "Done: Publish\exts\osmium-okits.osx" -ForegroundColor Green
Write-Host "Done: Publish\$setupName" -ForegroundColor Green

# 7. 可选: UPX 压缩版本 (opt-level="z" 体积优先 + UPX --ultra-brute --lzma)
# 仅生成 Publish\os-upx.exe, 不覆盖普通版, 也不生成安装包
$upxPath = "F:\DevTools\UPX\upx.exe"
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
        # workspace 化后 profile 位于根 Cargo.toml: 临时切换 opt-level 为 "z"(体积优先)
        $cargoPath = "$ProjectRoot\Cargo.toml"
        # 临时切换 opt-level 为 "z"(体积优先)
        $cargo = Get-Content $cargoPath -Raw
        $cargo = $cargo -replace '(?m)^opt-level = .*$', 'opt-level = "z"'
        [System.IO.File]::WriteAllText($cargoPath, $cargo, [System.Text.UTF8Encoding]::new($false))
        try {
            Write-Host "Building size-optimized (opt-level=z)..." -ForegroundColor Yellow
            Push-Location "$ProjectRoot\Project"
            try {
                cargo build --release
                if ($LASTEXITCODE -ne 0) { throw "Rust build failed (opt-level=z)" }
            } finally { Pop-Location }

            # 压缩副本, 不动 target 里的原 exe, 保证后续普通构建不受影响
            Write-Host "Compressing with UPX (--ultra-brute --lzma)..." -ForegroundColor Yellow
            $upxTmp = Join-Path $env:TEMP "os-upx-tmp.exe"
            Copy-Item "$ProjectRoot\target\release\osmium64.exe" $upxTmp -Force
            & $upxPath --ultra-brute --lzma $upxTmp
            if ($LASTEXITCODE -ne 0) { throw "UPX compression failed" }
            Copy-Item $upxTmp (Join-Path $publishDir "os-upx.exe") -Force
            Remove-Item $upxTmp -Force
            if ($signCert) { Sign-File (Join-Path $publishDir "os-upx.exe") $signCert }
            Write-Host "Done: Publish\os-upx.exe" -ForegroundColor Green
        } finally {
            # 恢复 opt-level = 3 (速度优先, 供下次普通构建使用)
            $cargo = Get-Content $cargoPath -Raw
            $cargo = $cargo -replace '(?m)^opt-level = .*$', 'opt-level = 3'
            [System.IO.File]::WriteAllText($cargoPath, $cargo, [System.Text.UTF8Encoding]::new($false))
        }
    }
} else {
    Write-Host "UPX not found at $upxPath, skipping optional build." -ForegroundColor DarkYellow
}
