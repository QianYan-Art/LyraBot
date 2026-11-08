<#
.SYNOPSIS
    Hermes 抖音作品交互式打包与同步入库工具（上限 2 GiB）

.DESCRIPTION
    存放位置：任意目录（默认配置 %USERPROFILE%\Videos\OpenFilm\Tools\tangyuge-publish.json）
    （位于公共 Tools 目录，不在单个项目内，不会被 OpenFilm 自动清理，兼容 PowerShell 5.1 与 PowerShell 7）

    功能特性：
      1. 编码与显示格式化：自动设置 UTF-8 (65001) 输入输出编码 + CJK 宽字符等宽对齐计算，中英文混排表格严丝合缝。
      2. 视频选择：自动扫描 Projects 下已导出的 MP4 供序号选择，也支持直接输入/粘贴/拖入任意视频路径。
      3. 可选项交互菜单：支持随时按编号 [1-5] 修改【视频路径】【作品编号】【工作标题】【工作简介】【来源说明】，并实时校验字数与格式。
      4. 自动打包入库：调用本机 stage_release.py prepare 生成 manifest.json，通过带实时进度条的 SCP 上传，并调用服务器 stage_release.py install 原子入库到 /var/lib/tangyuge-douyin/releases/<release_id>/{manifest.json, video.mp4}。

.EXAMPLE
    # 交互式运行（推荐，可选择视频、查看或修改标题/路径/简介）：
    pwsh .\tangyuge-upload.ps1

.EXAMPLE
    # 命令行传参直接运行（加 -Yes 跳过交互确认）：
    pwsh .\tangyuge-upload.ps1 -Video "C:\...\xxx.mp4" -ReleaseId "ep01" -Title "第一栞 · 标题" -Yes
#>
[CmdletBinding()]
param(
    [string]$Video = "",
    [string]$ReleaseId = "",
    [string]$Title = "",
    [string]$Description = "",
    [string]$Source = "OpenFilm Studio，字幕已烧录",
    [string]$ConfigPath = (Join-Path $env:USERPROFILE "Videos\OpenFilm\Tools\tangyuge-publish.json"),
    [switch]$Yes
)

$ErrorActionPreference = "Stop"

# ==================== 1. 字符编码与 CJK 终端等宽格式化工具 ====================
try {
    $null = & chcp 65001 2>$null
    $utf8NoBom = New-Object System.Text.UTF8Encoding($false)
    [Console]::InputEncoding  = $utf8NoBom
    [Console]::OutputEncoding = $utf8NoBom
    $OutputEncoding           = $utf8NoBom
} catch {}

function Get-DisplayWidth([string]$Text) {
    if (-not $Text) { return 0 }
    $w = 0
    foreach ($ch in $Text.ToCharArray()) {
        $code = [int][char]$ch
        # CJK 统一表意文字、全角标点、常见宽字符占 2 列宽度
        if (($code -ge 0x1100 -and $code -le 0x115F) -or
            ($code -ge 0x2E80 -and $code -le 0xA4CF -and $code -ne 0x303F) -or
            ($code -ge 0xAC00 -and $code -le 0xD7A3) -or
            ($code -ge 0xF900 -and $code -le 0xFAFF) -or
            ($code -ge 0xFE10 -and $code -le 0xFE6F) -or
            ($code -ge 0xFF00 -and $code -le 0xFF60) -or
            ($code -ge 0xFFE0 -and $code -le 0xFFE6)) {
            $w += 2
        } else {
            $w += 1
        }
    }
    return $w
}

function Pad-DisplayRight([string]$Text, [int]$TotalWidth) {
    $w = Get-DisplayWidth $Text
    if ($w -ge $TotalWidth) { return $Text }
    return $Text + (" " * ($TotalWidth - $w))
}

function Truncate-Display([string]$Text, [int]$MaxWidth) {
    if ((Get-DisplayWidth $Text) -le $MaxWidth) { return $Text }
    $out = ""
    $w = 0
    foreach ($ch in $Text.ToCharArray()) {
        $cw = Get-DisplayWidth ([string]$ch)
        if (($w + $cw + 3) -gt $MaxWidth) {
            return $out + "..."
        }
        $out += $ch
        $w += $cw
    }
    return $out
}

function Write-Banner([string]$Text) {
    $line = "═" * 76
    Write-Host ""
    Write-Host $line -ForegroundColor Cyan
    Write-Host ("  " + $Text) -ForegroundColor Cyan
    Write-Host $line -ForegroundColor Cyan
}

function Write-Section([string]$Text) {
    $w = Get-DisplayWidth $Text
    $pad = [Math]::Max(2, 71 - $w)
    Write-Host ""
    Write-Host ("── " + $Text + " " + ("─" * $pad)) -ForegroundColor DarkCyan
}

function Read-Prompt([string]$PromptText) {
    Write-Host ("  > " + $PromptText + ": ") -ForegroundColor Yellow -NoNewline
    $line = [Console]::In.ReadLine()
    if ($null -eq $line) {
        Write-Host ""
        return ""
    }
    if ([Console]::IsInputRedirected) {
        Write-Host $line -ForegroundColor Cyan
    }
    return $line
}

function Clean-PathInput([string]$Raw) {
    if (-not $Raw) { return "" }
    $s = $Raw.Trim()
    if (($s.StartsWith('"') -and $s.EndsWith('"')) -or ($s.StartsWith("'") -and $s.EndsWith("'"))) {
        $s = $s.Substring(1, $s.Length - 2).Trim()
    }
    return $s
}

# 清除当前会话中可能拖慢 SSH/SCP 的代理环境变量
foreach ($proxyVar in @("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "SOCKS_PROXY", "http_proxy", "https_proxy", "all_proxy", "socks_proxy")) {
    if (Test-Path "Env:$proxyVar") {
        Remove-Item "Env:$proxyVar" -ErrorAction SilentlyContinue
    }
}

# ==================== 2. 读取基础环境与配置 ====================
function Resolve-WorkingPython {
    $candidates = @()
    if ($rawCfg.python) { $candidates += [string]$rawCfg.python }
    $candidates += (Join-Path $env:USERPROFILE "Videos\OpenFilm\Tools\.venv-audio\Scripts\python.exe")
    $cmdPy = Get-Command python -ErrorAction SilentlyContinue
    if ($cmdPy -and $cmdPy.Source) { $candidates += $cmdPy.Source }
    foreach ($cand in $candidates) {
        if ($cand -and (Test-Path -LiteralPath $cand)) {
            try {
                $null = & $cand -V 2>$null
                if ($LASTEXITCODE -eq 0) { return $cand }
            } catch {}
        }
    }
    throw "未找到可用的 Python 3 解释器（已检查配置 python、OpenFilm\Tools\.venv-audio 与 PATH）。"
}
if (-not (Test-Path -LiteralPath $ConfigPath)) {
    throw "找不到配置文件: $ConfigPath"
}
# 本机专属路径（ops_dir、python、cleanup_tool）只放在配置文件里，脚本本身不含个人路径。
$rawCfg = Get-Content -LiteralPath $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
$Python = Resolve-WorkingPython
$OpsDir = if ($rawCfg.ops_dir) { [string]$rawCfg.ops_dir } elseif (Test-Path -LiteralPath (Join-Path $PSScriptRoot "stage_release.py")) { $PSScriptRoot } else { throw "配置缺少 ops_dir（指向仓库 ops/douyin 目录）" }
$StageScript  = Join-Path $OpsDir "stage_release.py"

if (-not (Test-Path -LiteralPath $ConfigPath)) {
    throw "找不到配置文件: $ConfigPath"
}
if (-not (Test-Path -LiteralPath $Python)) {
    throw "找不到 Python 环境: $Python"
}

# 复用 Python 入库入口的配置校验，防止主机、用户和远端路径进入 shell 前未经约束。
$configCode = @'
import json, sys
sys.path.insert(0, sys.argv[1])
from sync_openfilm import load_config
c = load_config(sys.argv[2])
print(json.dumps(dict(host=c.host, user=c.user, port=c.port,
    identity_file=str(c.identity_file), projects_root=str(c.projects_root), server_source=c.server_source)))
'@
$configOut = & $Python -B -X utf8 -c $configCode $OpsDir $ConfigPath
if ($LASTEXITCODE -ne 0) { throw '发布配置校验失败；未连接服务器。' }
$cfg          = $configOut | ConvertFrom-Json
$ProjectsRoot = (Resolve-Path -LiteralPath $cfg.projects_root).Path
$IdentityFile = (Resolve-Path -LiteralPath $cfg.identity_file).Path
$ServerSource = if ($cfg.server_source) { $cfg.server_source } else { "/opt/tangyuge-douyin" }
$Destination  = "$($cfg.user)@$($cfg.host)"
$SshPort      = [int]$cfg.port

# 根据视频路径自动推断 ReleaseId / Title / Description 的辅助函数
function Get-VideoDefaults([string]$TargetVideoPath) {
    $item = Get-Item -LiteralPath $TargetVideoPath
    $projDir = $item.Directory.FullName

    # 1) ReleaseId
    $defId = ""
    if ($projDir -match 'yu-(ep\d{2})') {
        $defId = $Matches[1].ToLower()
    } elseif ($item.Name -match '(ep\d{2})') {
        $defId = $Matches[1].ToLower()
    } else {
        $defId = "ep01"
    }

    # 2) Title
    $defTitle = ""
    $readmePath = Join-Path $projDir "README.md"
    if (Test-Path -LiteralPath $readmePath) {
        $readmeText = Get-Content -LiteralPath $readmePath -Raw -Encoding UTF8
        if ($readmeText -match '\*\*(第[^*\r\n]+?)\*\*') {
            $defTitle = $Matches[1].Trim()
        }
    }
    if (-not $defTitle) {
        if ($item.BaseName -match 'ep00') {
            $defTitle = "序栞 · 星栞书库"
        } else {
            $defTitle = $defId
        }
    }
    if ($defTitle.Length -gt 30) {
        $defTitle = $defTitle.Substring(0, 30)
    }

    # 3) Description
    $defDesc = ""
    $briefPath = Join-Path $projDir "brief.md"
    if ($defId -eq "ep01") {
        $defDesc = "一次失败的实验：让 Gemma 4 12B 原生开口说话。四条路线逐一检验——逆映射、线性探针、编解码器上限、学习型解码器——讲清楚怎样证明模型真的学到了东西：留出集、负对照、预注册阈值。损失在降，不等于输出可用。"
    } elseif (Test-Path -LiteralPath $briefPath) {
        $briefLines = Get-Content -LiteralPath $briefPath -Encoding UTF8
        $idx = [Array]::FindIndex($briefLines, [Predicate[string]]{ param($s) $s -match '^## One-sentence idea' })
        if ($idx -ge 0 -and ($idx + 1) -lt $briefLines.Count) {
            $defDesc = $briefLines[$idx + 1].Trim()
        }
    }
    if (-not $defDesc) {
        $defDesc = "$defTitle（OpenFilm Studio 导出，字幕已烧录）"
    }

    return [PSCustomObject]@{
        ReleaseId   = $defId
        Title       = $defTitle
        Description = $defDesc
    }
}

# 扫描 Projects 下所有候选 MP4
function Get-CandidateVideos {
    $list = @()
    $projDirs = Get-ChildItem -LiteralPath $ProjectsRoot -Directory -ErrorAction SilentlyContinue
    foreach ($d in $projDirs) {
        $mp4s = Get-ChildItem -LiteralPath $d.FullName -Filter "*.mp4" -File -ErrorAction SilentlyContinue
        foreach ($m in $mp4s) {
            $list += $m
        }
    }
    return @($list | Sort-Object LastWriteTime -Descending)
}

function Resolve-AllowedVideo([string]$CandidatePath) {
    $item = Get-Item -LiteralPath $CandidatePath -Force -ErrorAction Stop
    if ($item -isnot [IO.FileInfo] -or $item.Extension -ine '.mp4') {
        throw '视频必须是普通 MP4 文件。'
    }
    $root = [IO.Path]::GetFullPath($ProjectsRoot).TrimEnd('\', '/')
    $prefix = $root + [IO.Path]::DirectorySeparatorChar
    if (-not $item.FullName.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)) {
        throw "安全限制：视频必须位于 OpenFilm Projects 根目录 ($ProjectsRoot) 内。"
    }
    # 同时检查父目录，阻止通过目录联接或符号链接绕出受限根目录。
    $cursor = $item
    while ($null -ne $cursor) {
        if ($cursor.Attributes -band [IO.FileAttributes]::ReparsePoint) {
            throw '视频及其父目录不能是符号链接或目录联接。'
        }
        if ($cursor.FullName.TrimEnd('\', '/') -ieq $root) { break }
        $cursor = if ($cursor -is [IO.FileInfo]) { $cursor.Directory } else { $cursor.Parent }
    }
    if ($null -eq $cursor -or $item.Length -lt 1 -or $item.Length -gt 2GB) {
        throw '视频必须处于受限目录内，大小在 1 字节到 2 GiB 之间。'
    }
    return $item.FullName
}

function Prompt-SelectVideo([string]$CurrentPath) {
    $candidates = Get-CandidateVideos
    Write-Section "选择或输入视频文件 (.mp4)"
    if ($candidates.Count -gt 0) {
        $maxRelW = 40
        $relNames = @()
        foreach ($c in $candidates) {
            $rel = $c.FullName
            if ($rel.StartsWith($ProjectsRoot, [System.StringComparison]::OrdinalIgnoreCase)) {
                $rel = $rel.Substring($ProjectsRoot.Length).TrimStart('\', '/')
            }
            $relNames += $rel
            $rw = Get-DisplayWidth $rel
            if ($rw -gt $maxRelW) { $maxRelW = $rw }
        }
        for ($i = 0; $i -lt $candidates.Count; $i++) {
            $c = $candidates[$i]
            $relPadded = Pad-DisplayRight $relNames[$i] ($maxRelW + 2)
            $mb = "{0,6:N1} MiB" -f ($c.Length / 1MB)
            $dt = $c.LastWriteTime.ToString("yyyy-MM-dd HH:mm")
            $tag = if ($i -eq 0 -and (-not $CurrentPath)) { " [默认]" } elseif ($c.FullName -eq $CurrentPath) { " [当前]" } else { "" }
            Write-Host ("  [{0}] {1} ({2} | {3}){4}" -f ($i + 1), $relPadded, $mb, $dt, $tag) -ForegroundColor $(if ($tag) { "Green" } else { "Gray" })
        }
    }
    Write-Host ""
    Write-Host "  说明: 输入序号（如 1），或直接粘贴/拖入 .mp4 文件完整路径（需位于 $ProjectsRoot 内）" -ForegroundColor DarkGray

    while ($true) {
        $defaultHint = if ($CurrentPath) { "直接回车保持当前，Q 返回" } elseif ($candidates.Count -gt 0) { "直接回车选 [1]，Q 退出" } else { "请输入完整路径，Q 退出" }
        $raw = Read-Prompt "请选择序号或输入视频路径 ($defaultHint)"
        $clean = Clean-PathInput $raw

        if ($clean -and $clean.ToLower() -in @("q", "quit", "exit")) {
            if ($CurrentPath) { return $CurrentPath }
            Write-Host "  已取消操作。" -ForegroundColor Yellow
            exit 0
        }

        if (-not $clean) {
            if ($CurrentPath) { return $CurrentPath }
            if ($candidates.Count -gt 0) { return $candidates[0].FullName }
            Write-Host "  [!] 请输入有效的视频路径。" -ForegroundColor Red
            continue
        }

        if ($clean -match '^\d+$') {
            $idx = [int]$clean - 1
            if ($idx -ge 0 -and $idx -lt $candidates.Count) {
                return $candidates[$idx].FullName
            }
            Write-Host "  [!] 序号超出范围 (1 ~ $($candidates.Count))，请重新输入。" -ForegroundColor Red
            continue
        }

        if (-not (Test-Path -LiteralPath $clean -PathType Leaf)) {
            Write-Host "  [!] 找不到文件: $clean" -ForegroundColor Red
            continue
        }
        try { return Resolve-AllowedVideo $clean }
        catch { Write-Host "  [!] $($_.Exception.Message)" -ForegroundColor Red }
    }
}

# ==================== 3. 初始化参数与交互式可选项菜单 ====================
Write-Banner "Hermes 抖音作品同步入库工具 (上限 2 GiB)"

if ($Video) {
    $Video = Clean-PathInput $Video
    if (-not (Test-Path -LiteralPath $Video -PathType Leaf)) {
        throw "指定的视频文件不存在: $Video"
    }
    $Video = Resolve-AllowedVideo $Video
} elseif ($Yes) {
    $cands = Get-CandidateVideos
    if ($cands.Count -eq 0) { throw "未找到任何候选 .mp4 文件，请通过 -Video 指定。" }
    $Video = $cands[0].FullName
} else {
    $Video = Prompt-SelectVideo -CurrentPath ""
}

$Video = Resolve-AllowedVideo $Video
$defaults = Get-VideoDefaults -TargetVideoPath $Video
if (-not $ReleaseId)   { $ReleaseId   = $defaults.ReleaseId }
if (-not $Title)       { $Title       = $defaults.Title }
if (-not $Description) { $Description = $defaults.Description }

# 交互式配置确认与修改循环
if (-not $Yes) {
    while ($true) {
        $vItem = Get-Item -LiteralPath $Video
        $sizeMiB = [Math]::Round($vItem.Length / 1MB, 1)

        $lbl1 = Pad-DisplayRight "  [1] 视频路径 (Video)"       28
        $lbl2 = Pad-DisplayRight "  [2] 作品编号 (ReleaseId)"   28
        $lbl3 = Pad-DisplayRight "  [3] 视频标题 (Title)"       28
        $lbl4 = Pad-DisplayRight "  [4] 视频简介 (Description)" 28
        $lbl5 = Pad-DisplayRight "  [5] 来源标记 (Source)"      28
        $lblS = Pad-DisplayRight "      服务器入库目录"         28

        Write-Section "当前上传配置（输入数字 1~5 可修改对应项）"
        Write-Host ("{0} : {1} ({2} MiB)" -f $lbl1, $Video, $sizeMiB) -ForegroundColor White
        Write-Host ("{0} : {1}" -f $lbl2, $ReleaseId) -ForegroundColor White
        Write-Host ("{0} : {1}  [{2}/30 字]" -f $lbl3, $Title, $Title.Length) -ForegroundColor Green
        $descShort = Truncate-Display $Description 66
        Write-Host ("{0} : {1}  [{2}/1000 字]" -f $lbl4, $descShort, $Description.Length) -ForegroundColor White
        Write-Host ("{0} : {1}" -f $lbl5, $Source) -ForegroundColor White
        Write-Host ("{0} : {1}:{2} -> /var/lib/tangyuge-douyin/releases/{3}/" -f $lblS, $Destination, $SshPort, $ReleaseId) -ForegroundColor DarkGray
        Write-Host ""
        Write-Host "  提示：若开启了全局/TUN 网络代理，请先在代理软件中关闭或切为直连，否则 SCP 会被限速。" -ForegroundColor Yellow
        Write-Host "  菜单：[Enter / Y] 确认开始上传  |  [1] 改路径  [2] 改编号  [3] 改标题  [4] 改简介  [5] 改来源  |  [Q] 退出" -ForegroundColor Cyan

        $choice = (Read-Prompt "请输入操作选项 (直接回车开始上传)").Trim()
        if ($choice -eq "" -or $choice.ToLower() -in @("y", "yes")) {
            break
        }
        elseif ($choice.ToLower() -in @("q", "quit", "n", "no", "exit")) {
            Write-Host "  已取消上传。" -ForegroundColor Yellow
            exit 0
        }
        elseif ($choice -eq "1") {
            $newVideo = Prompt-SelectVideo -CurrentPath $Video
            if ($newVideo -ne $Video) {
                $Video = $newVideo
                $refreshAns = (Read-Prompt "视频已更换，是否根据新视频刷新默认的【编号/标题/简介】？[Y/n]").Trim().ToLower()
                if ($refreshAns -eq "" -or $refreshAns -in @("y", "yes")) {
                    $newDef = Get-VideoDefaults -TargetVideoPath $Video
                    $ReleaseId   = $newDef.ReleaseId
                    $Title       = $newDef.Title
                    $Description = $newDef.Description
                }
            }
        }
        elseif ($choice -eq "2") {
            Write-Host "  当前作品编号: $ReleaseId （仅允许小写字母、数字、连字符，如 ep01 或 ep01-r2）" -ForegroundColor Gray
            while ($true) {
                $inId = (Read-Prompt "请输入新作品编号 (直接回车保持不变)").Trim()
                if (-not $inId) { break }
                if ($inId -cmatch '^[a-z0-9][a-z0-9-]{0,63}$') {
                    $ReleaseId = $inId
                    break
                }
                Write-Host "  [!] 格式无效：仅允许小写英文字母、数字和连字符 (-)，长度 1~64。" -ForegroundColor Red
            }
        }
        elseif ($choice -eq "3") {
            Write-Host "  当前视频标题: $Title （1 ~ 30 个字符）" -ForegroundColor Gray
            while ($true) {
                $inTitle = (Read-Prompt "请输入新视频标题 (直接回车保持不变)").Trim()
                if (-not $inTitle) { break }
                if ($inTitle.Length -ge 1 -and $inTitle.Length -le 30) {
                    $Title = $inTitle
                    break
                }
                Write-Host "  [!] 标题长度必须在 1 到 30 个字符之间（当前输入 $($inTitle.Length) 字）。" -ForegroundColor Red
            }
        }
        elseif ($choice -eq "4") {
            Write-Host "  当前视频简介: $Description" -ForegroundColor Gray
            while ($true) {
                $inDesc = (Read-Prompt "请输入新视频简介 (1~1000 字，直接回车保持不变)").Trim()
                if (-not $inDesc) { break }
                if ($inDesc.Length -le 1000) {
                    $Description = $inDesc
                    break
                }
                Write-Host "  [!] 简介不能超过 1000 个字符（当前输入 $($inDesc.Length) 字）。" -ForegroundColor Red
            }
        }
        elseif ($choice -eq "5") {
            Write-Host "  当前来源标记: $Source" -ForegroundColor Gray
            $inSrc = (Read-Prompt "请输入新来源标记 (直接回车保持不变)").Trim()
            if ($inSrc) { $Source = $inSrc }
        }
        else {
            Write-Host "  [!] 无效选项 '$choice'，请输入 1~5、Y（回车）或 Q。" -ForegroundColor Red
        }
    }
}

# ==================== 4. 本机校验与上传入库执行 ====================
$Video = Resolve-AllowedVideo $Video
$VideoItem  = Get-Item -LiteralPath $Video
$VideoPath  = $VideoItem.FullName
$ProjectDir = $VideoItem.Directory.FullName
$SizeMiB    = [Math]::Round($VideoItem.Length / 1MB, 1)

$LocalTempDir = Join-Path $ProjectDir (".tmp-tangyuge-sync-" + [Guid]::NewGuid().ToString("N").Substring(0, 10))
New-Item -ItemType Directory -Path $LocalTempDir -Force | Out-Null
$ManifestPath = Join-Path $LocalTempDir "manifest.json"
$RemoteTempDir = ""

try {
    Write-Host "`n[1/4] 正在本机核验视频流 (H.264/AAC) 并计算 SHA-256..." -ForegroundColor Green
    $prepOut = & $Python -X utf8 $StageScript prepare `
        --video $VideoPath `
        --release-id $ReleaseId `
        --title $Title `
        --description $Description `
        --source $Source `
        --output $ManifestPath
    if ($LASTEXITCODE -ne 0) {
        throw "本机 stage_release.py prepare 校验失败。"
    }
    $manifest = Get-Content -LiteralPath $ManifestPath -Raw -Encoding UTF8 | ConvertFrom-Json
    Write-Host "      SHA-256  : $($manifest.video_sha256)"
    Write-Host "      规格时长 : $($manifest.width)x$($manifest.height) @ $($manifest.fps) fps, $([Math]::Round($manifest.duration, 2)) s"

    Write-Host "`n[2/4] 正在连接服务器 $($cfg.host) 创建私有暂存目录..." -ForegroundColor Green
    $SshCommon = @(
        "-p", "$SshPort",
        "-i", $IdentityFile,
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=yes",
        "-o", "IdentitiesOnly=yes",
        "-o", "ConnectTimeout=15"
    )
    $ScpCommon = @(
        "-P", "$SshPort",
        "-i", $IdentityFile,
        "-o", "BatchMode=yes",
        "-o", "StrictHostKeyChecking=yes",
        "-o", "IdentitiesOnly=yes",
        "-o", "IPQoS=throughput",
        "-o", "Compression=no"
    )

    $mkOut = & ssh @SshCommon $Destination "umask 077 && mktemp -d /tmp/tangyuge-douyin-upload.XXXXXXXXXX"
    if ($LASTEXITCODE -ne 0 -or (-not $mkOut)) {
        throw "无法在服务器创建临时目录，请检查 SSH 连接。"
    }
    $RemoteTempDir = ($mkOut | Select-Object -Last 1).Trim()
    if ($RemoteTempDir -notmatch '^/tmp/tangyuge-douyin-upload\.[A-Za-z0-9]{10,64}$') {
        throw "服务器返回的临时目录路径异常: $RemoteTempDir"
    }

    & scp @ScpCommon $ManifestPath "${Destination}:${RemoteTempDir}/manifest.json"
    if ($LASTEXITCODE -ne 0) {
        throw "上传 manifest.json 失败。"
    }

    $inspectCmd = "python3 -c `"import json, os, sys; from pathlib import Path; sys.path.insert(0, '$ServerSource'); from releases import ReleaseStore, manifests_have_same_content, validate_manifest; expected = validate_manifest(json.loads(Path('$RemoteTempDir/manifest.json').read_text(encoding='utf-8'))); store = ReleaseStore('/var/lib/tangyuge-douyin'); pkg = store.releases_dir / '$ReleaseId'; print(json.dumps({'state': 'missing'}) if not os.path.lexists(pkg) else (json.dumps({'state': 'conflict'}) if not manifests_have_same_content({k:v for k,v in store.load('$ReleaseId').items() if k != 'video_path'}, expected) else json.dumps({'state': 'same', 'hash': store.load('$ReleaseId')['video_sha256'], 'bytes': store.load('$ReleaseId')['video_bytes']})))`""
    $inspectRaw = & ssh @SshCommon $Destination $inspectCmd
    if ($LASTEXITCODE -ne 0) {
        throw "远端作品状态检查失败。"
    }
    $inspect = ($inspectRaw | Select-Object -Last 1) | ConvertFrom-Json
    if ($inspect.state -eq "conflict") {
        throw "服务器上已存在编号为 '$ReleaseId' 但内容不同的作品，拒绝覆盖！若为修订版请修改 [2] 作品编号（例如 ${ReleaseId}-r2）。"
    }
    if ($inspect.state -eq "same" -and $inspect.hash -eq $manifest.video_sha256 -and $inspect.bytes -eq $manifest.video_bytes) {
        Write-Host "`n[3/4] 服务器上已存在完全相同的 '$ReleaseId' 作品包（SHA-256 一致），无需重复上传视频！" -ForegroundColor Cyan
        Write-Host "[4/4] 校验完成：/var/lib/tangyuge-douyin/releases/$ReleaseId/{manifest.json, video.mp4}" -ForegroundColor Green
        $yuTool = [string]$rawCfg.cleanup_tool
        if ($yuTool -and (Test-Path -LiteralPath $yuTool)) {
            Write-Host "`n[清理] 正在自动清理 OpenFilm 已完结对话的底层调试抓包 (.film/logs) ..." -ForegroundColor DarkCyan
            & $Python -I $yuTool clean-logs --all
        }
        exit 0
    }

    Write-Host "`n[3/4] 正在上传视频到 ${Destination}:${RemoteTempDir}/video.mp4 ..." -ForegroundColor Green
    $sw = [System.Diagnostics.Stopwatch]::StartNew()
    & scp @ScpCommon $VideoPath "${Destination}:${RemoteTempDir}/video.mp4"
    $sw.Stop()
    if ($LASTEXITCODE -ne 0) {
        throw "视频 SCP 上传失败（退出码 $LASTEXITCODE）。"
    }
    $avgSpeed = [Math]::Round($SizeMiB / [Math]::Max($sw.Elapsed.TotalSeconds, 0.1), 2)
    Write-Host "      上传完成，耗时 $([Math]::Round($sw.Elapsed.TotalSeconds, 1)) 秒（平均 $avgSpeed MiB/s）。" -ForegroundColor Green

    Write-Host "`n[4/4] 正在服务器上校验 SHA-256 并原子安装到 /var/lib/tangyuge-douyin/releases/$ReleaseId/ ..." -ForegroundColor Green
    $installCmd = "python3 '$ServerSource/stage_release.py' install --source-dir '$RemoteTempDir'"
    $installOut = & ssh @SshCommon $Destination $installCmd
    if ($LASTEXITCODE -ne 0) {
        throw "远端 stage_release.py install 安装失败。"
    }

    $verifyRaw = & ssh @SshCommon $Destination $inspectCmd
    if ($LASTEXITCODE -ne 0) {
        throw "安装后远端核验失败。"
    }
    $verify = ($verifyRaw | Select-Object -Last 1) | ConvertFrom-Json
    if ($verify.state -ne "same" -or $verify.hash -ne $manifest.video_sha256 -or $verify.bytes -ne $manifest.video_bytes) {
        throw "远端安装后的作品摘要与本机不一致！"
    }

    Write-Banner "✅ 同步入库成功！（已入库，尚未公开发布）"
    Write-Host "  作品编号 : $ReleaseId"
    Write-Host "  视频标题 : $Title"
    Write-Host "  远端路径 : /var/lib/tangyuge-douyin/releases/$ReleaseId/{manifest.json, video.mp4}"
    Write-Host "  视频大小 : $($manifest.video_bytes) bytes ($SizeMiB MiB)"
    Write-Host "  SHA-256  : $($manifest.video_sha256)"
    Write-Host "  下一步   : 在 QQ 私聊对唐语歌说「准备发布第 1 栞」（或对应期号）" -ForegroundColor Cyan
    $yuTool = [string]$rawCfg.cleanup_tool
    if ($yuTool -and (Test-Path -LiteralPath $yuTool)) {
        Write-Host "`n[清理] 正在自动清理 OpenFilm 已完结对话的底层调试抓包 (.film/logs) ..." -ForegroundColor DarkCyan
        & $Python -I $yuTool clean-logs --all
    }
    Write-Host ("═" * 76) -ForegroundColor Cyan
}
finally {
    if ($RemoteTempDir -match '^/tmp/tangyuge-douyin-upload\.[A-Za-z0-9]{10,64}$') {
        Write-Host "正在清理服务器临时目录 $RemoteTempDir ..." -ForegroundColor DarkGray
        & ssh -p "$SshPort" -i $IdentityFile -o BatchMode=yes -o StrictHostKeyChecking=yes -o IdentitiesOnly=yes $Destination "rm -rf -- '$RemoteTempDir'" 2>$null
    }
    if (Test-Path -LiteralPath $LocalTempDir) {
        Remove-Item -LiteralPath $LocalTempDir -Recurse -Force -ErrorAction SilentlyContinue
    }
}
