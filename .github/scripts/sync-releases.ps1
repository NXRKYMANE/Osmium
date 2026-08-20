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
            # Gitee 写接口要求 JSON body; 用 curl --data-binary 发临时文件（字节原样,
            # 规避 PS 5.1 Latin-1 与旧版 pwsh byte[] 重编码导致 emoji 损坏/Gitee 400）
            $uri = "$TargetApi${path}?access_token=$Token"
            return Invoke-MirrorJsonCurl $method $uri $form
        }
        return Invoke-RestMethod -Method $method -Uri "$TargetApi${path}?access_token=$Token" -UserAgent $BrowserUA
    }
    $params = @{ Method = $method; Uri = "$TargetApi$path"; Headers = @{ Authorization = $AuthHeader } }
    # AtomGit(GitHub 兼容) 的 PATCH 端点要求 PRIVATE-TOKEN 头（Bearer 会 401 token not found）
    if ($Style -eq "github" -and $method -eq "PATCH") {
        $params.Headers = @{ "PRIVATE-TOKEN" = $Token }
    }
    $params.UserAgent = $BrowserUA
    if ($null -ne $form) {
        # 写请求同样走 curl 二进制链路: 规避旧版 pwsh 的 byte[] 重编码导致 JSON 解析失败
        return Invoke-MirrorJsonCurl $method "$TargetApi$path" $form $params.Headers
    }
    return Invoke-RestMethod @params
}

# 把对象写入 UTF-8 临时 JSON 文件, curl --data-binary 原样上传（与资产上传同一套稳妥链路）;
# 内联参数 + -o 落盘 + HTTP 码检查（PS 5.1 的 splatting/管道会破坏参数, 且 curl 无 -f 时 400 不报错）
function Invoke-MirrorJsonCurl([string]$method, [string]$uri, $form, $headers) {
    $jsonFile = Join-Path ([System.IO.Path]::GetTempPath()) ("mirror-json-" + [guid]::NewGuid().ToString("N") + ".json")
    $respBody = Join-Path ([System.IO.Path]::GetTempPath()) ("mirror-json-resp-" + [guid]::NewGuid().ToString("N") + ".txt")
    [System.IO.File]::WriteAllBytes($jsonFile, (utf8_json $form))
    try {
        if ($null -ne $headers -and $headers.Count -gt 0) {
            $h = $headers.GetEnumerator() | Select-Object -First 1
            $code = & curl.exe -sS -A $BrowserUA -m 120 --retry 3 --retry-delay 5 -X $method `
                -H "Content-Type: application/json; charset=utf-8" -H "$($h.Key): $($h.Value)" `
                --data-binary "@$jsonFile" $uri -o $respBody -w "%{http_code}" 2>$null
        } else {
            $code = & curl.exe -sS -A $BrowserUA -m 120 --retry 3 --retry-delay 5 -X $method `
                -H "Content-Type: application/json; charset=utf-8" `
                --data-binary "@$jsonFile" $uri -o $respBody -w "%{http_code}" 2>$null
        }
        if (-not "$code".StartsWith("2")) { throw "mirror JSON $method failed HTTP ${code}" }
        if (Test-Path $respBody) { return Get-Content $respBody -Raw | ConvertFrom-Json }
        return $null
    } finally {
        Remove-Item $jsonFile, $respBody -Force -ErrorAction SilentlyContinue
    }
}

# PS 5.1 下把对象转 UTF-8 JSON 字节（Invoke-RestMethod 字符串 Body 默认 Latin-1, 中文/emoji 会乱码）
function utf8_json($obj) { [System.Text.Encoding]::UTF8.GetBytes(($obj | ConvertTo-Json -Depth 10 -Compress)) }

# 通用翻页拉全量（Gitee/Gitea 每页上限分别为 100/50, 统一按 50 翻页）
function Get-TargetAll([string]$path) {
    $all = @()
    $page = 1
    while ($true) {
        $items = @(Get-MirrorPage $path $page)
        if (-not $items -or $items.Count -eq 0) { break }
        # 逐元素累加: 杜绝任何版本的数组嵌套（嵌套会让属性访问枚举出多个值）
        foreach ($item in $items) { $all += $item }
        if ($items.Count -lt 50) { break }
        $page++
    }
    return $all
}

function Get-TargetReleaseList { Get-TargetAll "/repos/$TargetOwner/$TargetRepo/releases" }

function Get-TargetTags { Get-TargetAll "/repos/$TargetOwner/$TargetRepo/tags" }

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
    if ($Style -eq "github") {
        # AtomGit: PATCH 必须用 form-urlencoded（JSON body 会被服务器按 Latin-1 误解码, em-dash/中文变乱码）;
        # 字段从 UTF-8 文件读取, 内联参数 + -o 落盘（PS 5.1 的 splatting/管道会破坏 --data-urlencode 参数）
        $tmp = Join-Path ([System.IO.Path]::GetTempPath()) ("atomgit-patch-" + [guid]::NewGuid().ToString("N"))
        $respBody = Join-Path ([System.IO.Path]::GetTempPath()) ("atomgit-patch-resp-" + [guid]::NewGuid().ToString("N") + ".json")
        $utf8 = New-Object System.Text.UTF8Encoding($false)
        [System.IO.File]::WriteAllText("$tmp.name.txt", $release.name, $utf8)
        [System.IO.File]::WriteAllText("$tmp.body.txt", $release.body, $utf8)
        & curl.exe -sS -A $BrowserUA -m 120 -X PATCH -H "PRIVATE-TOKEN: $Token" `
            --data-urlencode "name@$tmp.name.txt" --data-urlencode "body@$tmp.body.txt" `
            --data-urlencode "prerelease=$($release.isPrerelease)" `
            "$TargetApi/repos/$TargetOwner/$TargetRepo/releases/$($release.tagName)" -o $respBody 2>$null
        $code = $LASTEXITCODE
        Remove-Item "$tmp.name.txt", "$tmp.body.txt", $respBody -Force -ErrorAction SilentlyContinue
        if ($code -ne 0) {
            Write-Warn "AtomGit metadata update failed for '$($release.tagName)' (curl exit $code)"
        }
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

function Remove-TargetAsset($releaseId, [int]$assetId) {
    if ($Style -eq "gitee") {
        Invoke-MirrorForm "DELETE" "/repos/$TargetOwner/$TargetRepo/releases/$releaseId/attach_files/$assetId" $null | Out-Null
        return
    }
    if ($Style -eq "github") {
        # AtomGit: DELETE 附件需 PRIVATE-TOKEN 头（curl, PS 的 Invoke-RestMethod 兼容性问题）
        & curl.exe -sS -A $BrowserUA -X DELETE -H "PRIVATE-TOKEN: $Token" `
            "$TargetApi/repos/$TargetOwner/$TargetRepo/releases/$releaseId/attach_files/$assetId" 2>$null | Out-Null
        if ($LASTEXITCODE -ne 0) { Write-Warn "AtomGit asset delete failed (curl exit $LASTEXITCODE)" }
        return
    }
    Invoke-MirrorForm "DELETE" "/repos/$TargetOwner/$TargetRepo/releases/$releaseId/assets/$assetId" $null | Out-Null
}

function Upload-TargetAsset($releaseId, [string]$filePath, [string]$fileName) {
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
    if ($Style -eq "github") {
        # AtomGit: 先取 OBS 预签名上传地址（PRIVATE-TOKEN 头）再 PUT 文件, 回调自动挂载资产
        $respBody = Join-Path ([System.IO.Path]::GetTempPath()) ("atomgit-upload-" + [guid]::NewGuid().ToString("N") + ".json")
        $url = "$TargetApi/repos/$TargetOwner/$TargetRepo/releases/$releaseId/upload_url?file_name=$([uri]::EscapeDataString($fileName))"
        $respJson = & curl.exe -sS -A $BrowserUA -m 60 -H "PRIVATE-TOKEN: $Token" $url 2>$null
        if ($LASTEXITCODE -ne 0) { throw "AtomGit upload_url failed (curl exit $LASTEXITCODE)" }
        $resp = $respJson | ConvertFrom-Json
        $code = & curl.exe -sS -m 600 --retry 3 --retry-delay 5 -X PUT $resp.url `
            -H "x-obs-meta-project-id: $($resp.headers.'x-obs-meta-project-id')" `
            -H "x-obs-acl: $($resp.headers.'x-obs-acl')" `
            -H "x-obs-callback: $($resp.headers.'x-obs-callback')" `
            -H "Content-Type: $($resp.headers.'Content-Type')" `
            --data-binary "@$filePath" -o $respBody -w "%{http_code}" 2>$null
        if (-not "$code".StartsWith("2")) {
            $detail = if (Test-Path $respBody) { Get-Content $respBody -Raw } else { "" }
            throw "AtomGit upload failed HTTP ${code}: $detail"
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
        # AtomGit: 资产走 OBS 预签名上传（upload_url + PUT）；自动源码归档与手动附件按 type 区分
        Download-GhAssets $ghRelease $tmpDir
        $autoNames = @("$($ghRelease.tagName).zip", "$($ghRelease.tagName).tar.gz", "$($ghRelease.tagName).tar.bz2", "$($ghRelease.tagName).tar")
        foreach ($ma in $mirrorAssets) {
            if ($ma.type -eq "source" -or $autoNames -contains $ma.name) { continue }
            $gh = $ghAssets | Where-Object { $_.name -eq $ma.name } | Select-Object -First 1
            if (-not $gh) {
                Write-Warn "asset '$($ma.name)' no longer exists upstream, deleting"
                Remove-TargetAsset $mirrorRelease.tag_name $ma.id
            } elseif ($Force) {
                Write-Step "asset '$($ma.name)' Force, replacing"
                Remove-TargetAsset $mirrorRelease.tag_name $ma.id
            }
        }
        foreach ($ga in $ghAssets) {
            $need = $Force
            if (-not $need) {
                $ma = $mirrorAssets | Where-Object { $_.name -eq $ga.name } | Select-Object -First 1
                $need = (-not $ma)
            }
            if ($need) {
                $local = Join-Path $tmpDir $ga.name
                if (-not (Test-Path $local)) { throw "downloaded asset missing: $($ga.name)" }
                Upload-TargetAsset $mirrorRelease.tag_name $local $ga.name
                Write-Ok "asset '$($ga.name)' uploaded"
            }
        }
        return
    }
    # Gitee 平台限制: 自动生成的源码归档(<tag>.zip/.tar.gz)无 id 无法删除, 资产不一致时重建 release; gitea 走先删后传
    if ($Style -eq "gitee") {
        Sync-RebuildAssets $ghRelease $mirrorRelease $ghAssets $mirrorAssets $tmpDir
        return
    }
    Download-GhAssets $ghRelease $tmpDir
    # 先删: 镜像多余 / 同名不同大小 / Force（自动源码归档 <tag>.zip/.tar.gz 跳过, 平台特性不可删）
    $autoNames = @("$($ghRelease.tagName).zip", "$($ghRelease.tagName).tar.gz")
    foreach ($ma in $mirrorAssets) {
        if ($autoNames -contains $ma.name) { continue }
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
if ($ghReleases.Count -eq 0) {
    Write-Warn "no releases upstream, nothing to sync"
    return
}
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
            Update-TargetRelease 0 $r   # AtomGit release 对象无 id 字段, 内部按 tag 路径
        } else {
            Update-TargetRelease $mirror.id $r
        }
    }
    Sync-Assets $r $mirror
}

Write-Ok "Sync complete."
