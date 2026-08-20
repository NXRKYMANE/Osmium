<#
  Osmium Release 资产镜像同步（GitHub → Gitee / Gitea 风格镜像）

  策略: 以 GitHub Release 为唯一事实源, 镜像侧完全对齐——
    0. 顺序校正: 镜像按 created_at 倒序展示（最新创建在最前）, 若最新创建的 release 不是
       GitHub 最新版本（历史创建顺序错乱）→ 删除全部重建; 同步创建顺序从旧到新保证最新版排最前
    1. 镜像上 tag 已不在 GitHub → 删除对应 release（上游删减同步）
    2. GitHub 每个 release: 镜像缺 → 等待仓库镜像同步 tag 后创建；已有 → 编辑元数据（name/body/prerelease）
    3. 资产: 镜像侧先删除"多余/同名不同大小/Force"的旧资产, 再从 GitHub 下载重传（删减改动全覆盖）
  依赖: gh CLI（GitHub 认证读 GH_TOKEN 环境变量）; 镜像认证用 -Token
  用法示例见 .github/workflows/release-sync.yml
#>
param(
    [Parameter(Mandatory = $true)][string]$RepoOwner,   # GitHub 侧仓库所有者
    [Parameter(Mandatory = $true)][string]$Repo,        # GitHub 侧仓库名
    [Parameter(Mandatory = $true)][string]$TargetOwner, # 镜像仓库所有者
    [Parameter(Mandatory = $true)][string]$TargetRepo,  # 镜像仓库名
    [Parameter(Mandatory = $true)][string]$TargetApi,   # 镜像 API 基址（Gitee: https://gitee.com/api/v5；Gitea 风格: https://<host>/api/v1）
    [ValidateSet("gitee", "gitea", "github")][string]$Style = "gitee",
    [string]$Token = $env:TARGET_TOKEN,
    [string]$GitHubToken = $env:GH_TOKEN,
    [switch]$Force,   # 同名资产一律删旧重传（默认按 name+size 比对）
    [string]$OnlyTag  # 只同步指定 tag（本地验证用）
)

$ErrorActionPreference = "Stop"
Set-StrictMode -Version 2.0
# 防御: secret 可能带尾随 CRLF（PS 5.1 管道写 gh secret set 会追加换行）, 头/URL 注入前必须清理
if ($Token) { $Token = $Token.Trim() }

function Write-Step([string]$msg) { Write-Host "[sync] $msg" -ForegroundColor Cyan }
function Write-Ok([string]$msg) { Write-Host "[sync] $msg" -ForegroundColor Green }
function Write-Warn([string]$msg) { Write-Warning "[sync] $msg" }

# 镜像鉴权头: gitea 用 "token <PAT>", github(GitHub 兼容 v5, 如 AtomGit) 用 "Bearer <token>"
$AuthHeader = $null
if ($Style -ne "gitee") {
    $AuthHeader = if ($Style -eq "github") { "Bearer $Token" } else { "token $Token" }
}
# Gitee WAF 拦截非浏览器 UA（pwsh/PS 默认 UA 对部分路径返回 404 页面）, 所有请求统一带浏览器 UA
$BrowserUA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0 Safari/537.36"

# ==================== GitHub 侧（gh CLI） ====================

function Get-GhReleases {
    # 2>$null 丢弃 gh 的 stderr 进度行（PS 5.1 的 2>&1 混入后 ConvertFrom-Json 逐行转换会错乱）
    # --order asc 显式声明按创建时间升序（旧→新）: 同步时按此顺序创建, 使镜像最新版排最前
    $raw = & gh release list --repo "$RepoOwner/$Repo" --limit 300 --order asc --json "tagName,name,isDraft,isPrerelease" 2>$null
    if ($LASTEXITCODE -ne 0) { throw "gh release list failed (exit $LASTEXITCODE)" }
    $out = @()
    # PS 5.1 坑: @(x | ConvertFrom-Json) 会把数组包成单元素嵌套, 须直接管道赋值再遍历
    $releases = $raw | ConvertFrom-Json
    foreach ($r in $releases) {
        $view = & gh release view $r.tagName --repo "$RepoOwner/$Repo" --json "tagName,name,body,isDraft,isPrerelease,assets" 2>$null
        if ($LASTEXITCODE -ne 0) { throw "gh release view failed for '$($r.tagName)' (exit $LASTEXITCODE)" }
        $out += ($view | ConvertFrom-Json)
    }
    return $out
}

function Download-GhAssets($ghRelease, [string]$dir) {
    # 缓存命中（同名同大小）跳过下载; 未命中并行 curl 下载（带超时重试, 防国内网络慢/挂起）
    foreach ($ga in $ghRelease.assets) {
        $local = Join-Path $dir $ga.name
        if ((Test-Path $local) -and ((Get-Item $local).Length -eq $ga.size)) {
            Write-Step "cached '$($ga.name)', skipping download"
            continue
        }
        # --ssl-no-revoke: 国内网络访问 CRL 吊销服务器不可达时 schannel 报 CRYPT_E_NO_REVOCATION_CHECK
        # 逐文件 -o 显式落盘（curl 多 URL + -O 组合在 PS 5.1 下会把二进制泄到 stdout, 弃用并行）
        Write-Step "downloading '$($ga.name)' ($($ga.size) bytes)"
        & curl.exe -sS -L -A $BrowserUA --ssl-no-revoke --connect-timeout 30 --max-time 900 --retry 3 --retry-delay 5 -o $local $ga.url
        if ($LASTEXITCODE -ne 0) { throw "asset download failed (curl exit $LASTEXITCODE)" }
    }
}

# ==================== 镜像 API 基础调用 ====================

function Get-MirrorPage([string]$path, [int]$page) {
    if ($Style -eq "gitee") {
        return @(Invoke-RestMethod -Method Get -Uri "$TargetApi${path}?per_page=50&page=$page&access_token=$Token" -UserAgent $BrowserUA)
    }
    return @(Invoke-RestMethod -Method Get -Uri "$TargetApi${path}?per_page=50&page=$page" -Headers @{ Authorization = $AuthHeader } -UserAgent $BrowserUA)
}

function Invoke-MirrorForm([string]$method, [string]$path, $form) {
    if ($Style -eq "gitee") {
        if ($null -ne $form) {
            # Gitee 写接口要求 JSON body（form-urlencoded 的 PATCH 会报 content-type 不支持）
            return Invoke-RestMethod -Method $method -Uri "$TargetApi${path}?access_token=$Token" `
                -ContentType "application/json" -Body ($form | ConvertTo-Json -Depth 10 -Compress) -UserAgent $BrowserUA
        }
        return Invoke-RestMethod -Method $method -Uri "$TargetApi${path}?access_token=$Token" -UserAgent $BrowserUA
    }
    $params = @{ Method = $method; Uri = "$TargetApi$path"; Headers = @{ Authorization = $AuthHeader } }
    $params.UserAgent = $BrowserUA
    if ($null -ne $form) {
        $params.ContentType = "application/json"
        $params.Body = ($form | ConvertTo-Json -Depth 10 -Compress)
    }
    return Invoke-RestMethod @params
}

function Get-TargetReleaseList {
    # 翻页拉全量（Gitee/Gitea 每页上限分别为 100/50, 统一按 50 翻页）
    $all = @()
    $page = 1
    while ($true) {
        $items = @(Get-MirrorPage "/repos/$TargetOwner/$TargetRepo/releases" $page)
        if (-not $items -or $items.Count -eq 0) { break }
        # 逐元素累加: 杜绝任何版本的数组嵌套（嵌套会让属性访问枚举出多个值）
        foreach ($item in $items) { $all += $item }
        if ($items.Count -lt 50) { break }
        $page++
    }
    return $all
}

function Get-TargetTags {
    $all = @()
    $page = 1
    while ($true) {
        $items = @(Get-MirrorPage "/repos/$TargetOwner/$TargetRepo/tags" $page)
        if (-not $items -or $items.Count -eq 0) { break }
        foreach ($item in $items) { $all += $item }
        if ($items.Count -lt 50) { break }
        $page++
    }
    return $all
}

function Get-TargetDefaultBranch {
    # Gitee 创建 release 必须带 target_commitish（GitHub 兼容 API 不需要）
    if ($Style -eq "gitee") {
        $repo = Invoke-RestMethod -Method Get -Uri "$TargetApi/repos/$TargetOwner/${TargetRepo}?access_token=$Token" -UserAgent $BrowserUA
        return $repo.default_branch
    }
    return $null
}

function Remove-TargetRelease($rid) {
    # AtomGit(GitHub 兼容 v5) 的 release 对象无 id 字段, 调用方按风格传 tag_name 或数字 id
    Invoke-MirrorForm "DELETE" "/repos/$TargetOwner/$TargetRepo/releases/$rid" $null | Out-Null
}

function New-TargetRelease($release) {
    if ($Style -eq "gitee") {
        return Invoke-MirrorForm "POST" "/repos/$TargetOwner/$TargetRepo/releases" @{
            tag_name = $release.tagName; name = $release.name; body = $release.body
            prerelease = $release.isPrerelease; target_commitish = $TargetDefaultBranch
        }
    }
    return Invoke-MirrorForm "POST" "/repos/$TargetOwner/$TargetRepo/releases" @{
        tag_name = $release.tagName; name = $release.name; body = $release.body
        prerelease = $release.isPrerelease; draft = $false
    }
}

function Update-TargetRelease([int]$id, $release) {
    # 元数据总是覆盖为 GitHub 现值（幂等, 保证 edited 事件后镜像一致）
    if ($Style -eq "gitee") {
        Invoke-MirrorForm "PATCH" "/repos/$TargetOwner/$TargetRepo/releases/$id" @{
            tag_name = $release.tagName; name = $release.name; body = $release.body; prerelease = $release.isPrerelease
        } | Out-Null
        return
    }
    Invoke-MirrorForm "PATCH" "/repos/$TargetOwner/$TargetRepo/releases/$id" @{
        name = $release.name; body = $release.body; prerelease = $release.isPrerelease; draft = $false
    } | Out-Null
}

function Wait-TargetTag([string]$tag) {
    # 创建 release 前 tag 需已由仓库镜像同步到镜像（异步, 最多等 5 分钟）
    for ($i = 0; $i -lt 10; $i++) {
        $matched = Get-TargetTags | Where-Object { $_.name -eq $tag }
        if ($null -ne $matched) { return $true }
        Start-Sleep -Seconds 30
    }
    return $false
}

function Remove-TargetAsset([int]$releaseId, [int]$assetId) {
    if ($Style -eq "gitee") {
        Invoke-MirrorForm "DELETE" "/repos/$TargetOwner/$TargetRepo/releases/$releaseId/attach_files/$assetId" $null | Out-Null
        return
    }
    Invoke-MirrorForm "DELETE" "/repos/$TargetOwner/$TargetRepo/releases/$releaseId/assets/$assetId" $null | Out-Null
}

function Upload-TargetAsset([int]$releaseId, [string]$filePath, [string]$fileName) {
    if ($Style -eq "gitee") {
        # Gitee: multipart/form-data 上传（curl.exe, 带超时重试; PS 5.1 的 Invoke-RestMethod 无 -Form）
        $uri = "$TargetApi/repos/$TargetOwner/$TargetRepo/releases/$releaseId/attach_files?access_token=$Token"
        $respBody = Join-Path ([System.IO.Path]::GetTempPath()) ("gitee-upload-" + [guid]::NewGuid().ToString("N") + ".json")
        $code = & curl.exe -sS -A $BrowserUA -m 600 --retry 3 --retry-delay 5 -X POST "$uri" -F "file=@$filePath;filename=$fileName" -o $respBody -w "%{http_code}" 2>$null
        if (-not "$code".StartsWith("2")) {
            $detail = if (Test-Path $respBody) { Get-Content $respBody -Raw } else { "" }
            throw "Gitee upload failed HTTP ${code}: $detail"
        }
        Remove-Item $respBody -Force -ErrorAction SilentlyContinue
        return
    }
    # Gitea: 单二进制 body 上传
    $uri = "$TargetApi/repos/$TargetOwner/$TargetRepo/releases/$releaseId/assets?name=$([uri]::EscapeDataString($fileName))"
    Invoke-RestMethod -Method Post -Uri $uri -Headers @{ Authorization = $AuthHeader } -InFile $filePath -ContentType "application/octet-stream" -UserAgent $BrowserUA | Out-Null
}

# ==================== 资产对齐 ====================

function Sync-Assets($ghRelease, $mirrorRelease) {
    $ghAssets = @()
    if ($null -ne $ghRelease.assets) { $ghAssets = @($ghRelease.assets) }
    $mirrorAssets = @()
    if ($null -ne $mirrorRelease.assets) { $mirrorAssets = @($mirrorRelease.assets) }
    # 下载缓存目录: 按 tag 分目录跨会话保留, 重跑/多镜像同步跳过重复慢速下载
    $tmpDir = Join-Path (Join-Path ([System.IO.Path]::GetTempPath()) "osmium-sync-cache") $ghRelease.tagName
    New-Item -ItemType Directory -Force -Path $tmpDir | Out-Null
    if ($Style -eq "github") {
        # AtomGit 平台 API 限制: 仅支持创建 release（更新 400 / 删除 405 / 资产接口 404）, 资产无法同步
        Write-Step "AtomGit: assets cannot be synced (platform API limitation); release was created with metadata by the main flow"
        return
    }
    # Gitee 平台限制: 自动生成的源码归档(<tag>.zip/.tar.gz)无 id 无法删除, 资产不一致时重建 release; gitea 走先删后传
    if ($Style -eq "gitee") {
        Sync-RebuildAssets $ghRelease $mirrorRelease $ghAssets $mirrorAssets $tmpDir
        return
    }
    Download-GhAssets $ghRelease $tmpDir
    # 先删: 镜像多余 / 同名不同大小 / Force
    foreach ($ma in $mirrorAssets) {
        $gh = $ghAssets | Where-Object { $_.name -eq $ma.name } | Select-Object -First 1
        if (-not $gh) {
            Write-Warn "asset '$($ma.name)' no longer exists upstream, deleting"
            Remove-TargetAsset $mirrorRelease.id $ma.id
        } elseif ($Force -or $gh.size -ne $ma.size) {
            Write-Step "asset '$($ma.name)' changed (or Force), replacing"
            Remove-TargetAsset $mirrorRelease.id $ma.id
        }
    }
    # 后传: 缺失 / 不同大小 / Force（先删后传, 避免同名重复上传）
    foreach ($ga in $ghAssets) {
        $need = $Force
        if (-not $need) {
            $ma = $mirrorAssets | Where-Object { $_.name -eq $ga.name } | Select-Object -First 1
            $need = (-not $ma) -or ($ma.size -ne $ga.size)
        }
        if ($need) {
            $local = Join-Path $tmpDir $ga.name
            if (-not (Test-Path $local)) { throw "downloaded asset missing: $($ga.name)" }
            Upload-TargetAsset $mirrorRelease.id $local $ga.name
            Write-Ok "asset '$($ga.name)' uploaded"
        }
    }
}

function Sync-RebuildAssets($ghRelease, $mirrorRelease, $ghAssets, $mirrorAssets, $tmpDir) {
    # 策略: 自动归档（<tag>.zip/.tar.gz, 按名识别）跳过比对（平台特性, 不可删）;
    # 其余手动资产集合与 GitHub 不一致 → 删除整个 release 重建再全量上传; 一致则跳过
    $autoNames = @("$($ghRelease.tagName).zip", "$($ghRelease.tagName).tar.gz")
    $manual = @()
    foreach ($ma in $mirrorAssets) {
        if ($autoNames -contains $ma.name) { continue }
        $manual += $ma
    }
    $needRebuild = $false
    if ($manual.Count -gt 0) {
        foreach ($ma in $manual) {
            $gh = $ghAssets | Where-Object { $_.name -eq $ma.name } | Select-Object -First 1
            # Gitee 列表接口资产无 size, 仅按名字集合比对（不一致即重建重传）
            if (-not $gh -or $Force) { $needRebuild = $true; break }
        }
        if (-not $needRebuild) {
            foreach ($ga in $ghAssets) {
                $found = $manual | Where-Object { $_.name -eq $ga.name } | Select-Object -First 1
                if (-not $found) { $needRebuild = $true; break }
            }
        }
    }
    if (-not $needRebuild -and $manual.Count -eq $ghAssets.Count -and $ghAssets.Count -gt 0) {
        Write-Step "assets in sync, skipping"
        return
    }
    Download-GhAssets $ghRelease $tmpDir
    if ($needRebuild) {
        Write-Warn "assets differ from upstream, rebuilding release '$($ghRelease.tagName)'"
        Remove-TargetRelease $mirrorRelease.id
        $mirrorRelease = New-TargetRelease $ghRelease
    }
    foreach ($ga in $ghAssets) {
        $exists = $manual | Where-Object { $_.name -eq $ga.name } | Select-Object -First 1
        if ($exists) { continue }
        $local = Join-Path $tmpDir $ga.name
        if (-not (Test-Path $local)) { throw "downloaded asset missing: $($ga.name)" }
        Upload-TargetAsset $mirrorRelease.id $local $ga.name
        Write-Ok "asset '$($ga.name)' uploaded"
    }
}

# ==================== 主流程 ====================

if (-not $GitHubToken) { throw "GH_TOKEN 未设置" }
if (-not $Token) { throw "镜像 Token 未设置" }
if (-not (Get-Command gh -ErrorAction SilentlyContinue)) { throw "需要 gh CLI（GitHub Actions windows runner 自带）" }

Write-Step "Fetching upstream releases from $RepoOwner/$Repo ..."
$ghReleases = @(Get-GhReleases | Where-Object { -not $_.isDraft })
# gh list 按创建时间升序（旧→新）: 直接按此顺序创建即可,
# 最新版本最后创建 → 镜像（created_at 倒序展示）最新版本排最前, 与 GitHub 展示顺序一致

Write-Step "Fetching mirror releases from $TargetOwner/$TargetRepo ..."
$targetReleases = @(Get-TargetReleaseList)
$TargetDefaultBranch = Get-TargetDefaultBranch

# 0. 顺序校正: 镜像按 created_at 倒序展示, 若最新创建的 release 不是 GitHub 最新版本,
# 说明历史创建顺序错乱（展示反了）→ gitee 删除全部重建; github 风格平台不支持删除, 告警提示手动
$sortedTr = @($targetReleases | Sort-Object created_at -Descending)
if ($sortedTr.Count -gt 1 -and $sortedTr[0].tag_name -ne $ghReleases[$ghReleases.Count - 1].tagName) {
    if ($Style -eq "github") {
        Write-Warn "release order corrupted (latest-created '$($sortedTr[0].tag_name)' != upstream latest '$($ghReleases[$ghReleases.Count - 1].tagName)');"
        Write-Warn "platform cannot DELETE releases - delete them manually in the web UI, then re-run this script to rebuild"
    } else {
        Write-Warn "release order corrupted (latest-created '$($sortedTr[0].tag_name)' != upstream latest), deleting all mirror releases to rebuild"
        foreach ($tr in $targetReleases) { Remove-TargetRelease $tr.id }
        $targetReleases = @()
    }
}

# 1. 删除镜像侧已不存在的 release（上游删减同步）
foreach ($tr in $targetReleases) {
    if ($Style -eq "github") { continue }  # AtomGit 不支持删除 release（DELETE 405）
    if (-not ($ghReleases | Where-Object { $_.tagName -eq $tr.tag_name })) {
        Write-Warn "release '$($tr.tag_name)' no longer exists upstream, deleting mirror release"
        Remove-TargetRelease $tr.id
    }
}

# 2. 逐 release 对齐（创建/编辑 + 资产全覆盖）
foreach ($r in $ghReleases) {
    if ($OnlyTag -and $r.tagName -ne $OnlyTag) { continue }
    $mirror = $targetReleases | Where-Object { $_.tag_name -eq $r.tagName } | Select-Object -First 1
    if (-not $mirror) {
        if (-not (Wait-TargetTag $r.tagName)) {
            Write-Warn "tag '$($r.tagName)' not synced to mirror yet, skipping (next run will pick it up)"
            continue
        }
        Write-Ok "creating mirror release '$($r.tagName)'"
        $mirror = New-TargetRelease $r
    } else {
        Write-Step "updating mirror release metadata '$($r.tagName)'"
        if ($Style -eq "github") {
            # AtomGit 不支持更新元数据（PATCH 400）, 创建时已带最新元数据, 跳过
            Write-Step "AtomGit: metadata update skipped (platform API read-only)"
        } else {
            Update-TargetRelease $mirror.id $r
        }
    }
    Sync-Assets $r $mirror
}

Write-Ok "Sync complete."
