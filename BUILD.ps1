# Osmium 一键构建: Rust 构建与测试 + 官方插件 + 安装包
# 用法: .\BUILD.ps1 [-SkipTests] [-Upx]

param(
    [switch]$SkipTests,
    [switch]$Upx
)

$ErrorActionPreference = "Continue"
$ProjectRoot = $PSScriptRoot
$ISCC = "C:\Program Files\Inno Setup 7\ISCC.exe"

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
