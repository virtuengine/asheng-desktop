# 阿盛语音模块安装脚本
#
# 由安装器在主体安装完成后调用，也可以手动重跑：
#   powershell -ExecutionPolicy Bypass -File voice-install.ps1
#
# 职责：拉清单 → 断点续传下载 → SHA256 校验 → 解压部署 → 写完成标记
# 退出码：0 成功；1 清单获取失败；2 下载失败；3 校验失败；4 解压失败

[CmdletBinding()]
param(
    [string]$Dest = (Join-Path $env:LOCALAPPDATA 'Asheng\asr'),
    [string]$Manifest = 'https://43.142.55.26:8668/dl/desktop/voice-pack.json',
    [int]$Retries = 3,
    [switch]$Silent
)

$ErrorActionPreference = 'Stop'
try {
    [Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
} catch {}

$script:Form = $null
$script:Bar = $null
$script:Status = $null
$script:Percent = $null
$script:LastUi = [DateTime]::MinValue
# 用户点了「跳过」：停止下载，安装继续
$script:Cancel = $false

function Show-Ui {
    Add-Type -AssemblyName System.Windows.Forms
    Add-Type -AssemblyName System.Drawing
    $f = New-Object Windows.Forms.Form
    $f.Text = '阿盛 · 安装语音模块'
    $f.ClientSize = New-Object Drawing.Size(430, 176)
    $f.StartPosition = 'CenterScreen'
    $f.FormBorderStyle = 'FixedDialog'
    $f.MaximizeBox = $false
    $f.MinimizeBox = $false
    $f.ControlBox = $false
    $f.TopMost = $true
    $f.BackColor = [Drawing.Color]::White
    $f.Font = New-Object Drawing.Font('Microsoft YaHei UI', 9)
    $f.ShowInTaskbar = $false

    $title = New-Object Windows.Forms.Label
    $title.Text = '正在安装语音模块'
    $title.Location = New-Object Drawing.Point(20, 18)
    $title.AutoSize = $true
    $title.Font = New-Object Drawing.Font('Microsoft YaHei UI', 11, [Drawing.FontStyle]::Bold)
    $title.ForeColor = [Drawing.Color]::FromArgb(21, 34, 44)

    $status = New-Object Windows.Forms.Label
    $status.Location = New-Object Drawing.Point(20, 50)
    $status.Size = New-Object Drawing.Size(390, 20)
    $status.Text = '正在连接服务器…'
    $status.ForeColor = [Drawing.Color]::FromArgb(60, 80, 96)

    $bar = New-Object Windows.Forms.ProgressBar
    $bar.Location = New-Object Drawing.Point(20, 78)
    $bar.Size = New-Object Drawing.Size(390, 16)
    $bar.Minimum = 0
    $bar.Maximum = 1000

    $pct = New-Object Windows.Forms.Label
    $pct.Location = New-Object Drawing.Point(20, 104)
    $pct.Size = New-Object Drawing.Size(390, 18)
    $pct.Text = ''
    $pct.ForeColor = [Drawing.Color]::FromArgb(107, 127, 144)

    $note = New-Object Windows.Forms.Label
    $note.Location = New-Object Drawing.Point(20, 136)
    $note.Size = New-Object Drawing.Size(300, 20)
    $note.Text = '装完即可说「阿胜阿胜」或按住右 Ctrl 提问'
    $note.ForeColor = [Drawing.Color]::FromArgb(150, 162, 174)

    $skip = New-Object Windows.Forms.Button
    $skip.Text = '跳过'
    $skip.Location = New-Object Drawing.Point(342, 130)
    $skip.Size = New-Object Drawing.Size(68, 26)
    $skip.FlatStyle = 'Flat'
    $skip.FlatAppearance.BorderSize = 0
    $skip.ForeColor = [Drawing.Color]::FromArgb(107, 127, 144)
    $skip.Cursor = 'Hand'
    $skip.Add_Click({
        $script:Cancel = $true
        Set-Ui '正在跳过语音模块…' -1
    })

    $f.Controls.AddRange(@($title, $status, $bar, $pct, $note, $skip))
    $f.Show()
    [Windows.Forms.Application]::DoEvents()

    $script:Form = $f
    $script:Bar = $bar
    $script:Status = $status
    $script:Percent = $pct
}

function Set-Ui([string]$text, [int]$value, [string]$detail = '') {
    if (-not $script:Form -or $script:Form.IsDisposed) { return }
    $now = [DateTime]::Now
    if (($now - $script:LastUi).TotalMilliseconds -lt 110 -and $value -lt 1000) { return }
    $script:LastUi = $now
    $script:Status.Text = $text
    if ($value -lt 0) { $value = 0 }
    if ($value -gt 1000) { $value = 1000 }
    $script:Bar.Value = $value
    $script:Percent.Text = $detail
    [Windows.Forms.Application]::DoEvents()
}

function Close-Ui {
    if (-not $script:Form) { return }
    try { $script:Form.Close() } catch {}
    $script:Form = $null
}

function Get-Manifest([string]$url) {
    for ($i = 1; $i -le $Retries; $i++) {
        try {
            $req = [Net.HttpWebRequest]::Create($url)
            $req.Timeout = 20000
            $req.ReadWriteTimeout = 20000
            $req.UserAgent = 'AshengSetup/1.1'
            $resp = $req.GetResponse()
            $sr = New-Object IO.StreamReader($resp.GetResponseStream(), [Text.Encoding]::UTF8)
            $raw = $sr.ReadToEnd()
            $sr.Close()
            $resp.Close()
            return $raw | ConvertFrom-Json
        } catch {
            if ($i -eq $Retries) { throw }
            Set-Ui '连接服务器失败，正在重试…' -1
            Start-Sleep -Seconds $i
        }
    }
}

function Get-Pack([string]$url, [string]$part, [long]$total) {
    $have = [long]0
    if (Test-Path -LiteralPath $part) {
        $have = (Get-Item -LiteralPath $part).Length
        if ($total -gt 0 -and $have -gt $total) {
            Remove-Item -LiteralPath $part -Force
            $have = 0
        }
        if ($total -gt 0 -and $have -eq $total) { return }
    }

    $req = [Net.HttpWebRequest]::Create($url)
    $req.Timeout = 30000
    $req.ReadWriteTimeout = 60000
    $req.UserAgent = 'AshengSetup/1.1'
    if ($have -gt 0) { $req.AddRange($have) }

    $resp = $req.GetResponse()
    $code = [int]$resp.StatusCode
    $stream = $resp.GetResponseStream()
    # 服务器不支持断点续传时只能整包重来
    if ($have -gt 0 -and $code -ne 206) {
        $have = 0
        Set-Ui '服务器不支持续传，重新下载…' 30
    }

    $mode = 'Create'
    if ($have -gt 0) { $mode = 'Append' }
    $fs = [IO.File]::Open($part, [IO.FileMode]$mode, [IO.FileAccess]::Write)
    $buf = New-Object byte[] 262144
    $got = $have
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $last = $sw.Elapsed.TotalSeconds
    try {
        while (($n = $stream.Read($buf, 0, $buf.Length)) -gt 0) {
            if ($script:Cancel) { throw '用户跳过语音模块' }
            $fs.Write($buf, 0, $n)
            $got += $n
            if ($total -le 0) { continue }
            $sec = $sw.Elapsed.TotalSeconds
            if (($sec - $last) -lt 0.25) { continue }
            $last = $sec
            $speed = $got / [math]::Max($sec, 0.001) / 1MB
            $left = [math]::Max($total - $got, 0) / 1MB / [math]::Max($speed, 0.001)
            if ($left -ge 60) {
                $eta = '{0:N0} 分 {1:N0} 秒' -f [math]::Floor($left / 60), ($left % 60)
            } else {
                $eta = '{0:N0} 秒' -f $left
            }
            $done = $got / 1MB
            $all = $total / 1MB
            Set-Ui ('正在下载语音模块  {0:N0} / {1:N0} MB' -f $done, $all) `
                ([int]($got * 1000 / $total)) `
                ('{0:N1} MB/s    已用 {1} / 预计剩余 {2}' -f $speed, (Format-Duration $sec), $eta)
        }
    } finally {
        $fs.Close()
        $stream.Close()
        $resp.Close()
    }
}

function Format-Duration([double]$seconds) {
    if ($seconds -ge 60) {
        return ('{0:N0} 分 {1:N0} 秒' -f [math]::Floor($seconds / 60), ($seconds % 60))
    }
    return ('{0:N0} 秒' -f $seconds)
}

function Expand-Pack([string]$zip, [string]$dest) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    if (Test-Path -LiteralPath $dest) { Remove-Item -LiteralPath $dest -Recurse -Force }
    New-Item -ItemType Directory -Path $dest -Force | Out-Null

    $z = [IO.Compression.ZipFile]::OpenRead($zip)
    try {
        $entries = $z.Entries
        $count = $entries.Count
        $i = 0
        foreach ($e in $entries) {
            if ($script:Cancel) { throw '用户跳过语音模块' }
            $i++
            $rel = $e.FullName.Replace('/', '\')
            $target = Join-Path $dest $rel
            $dir = Split-Path -Parent $target
            if ($dir -and -not (Test-Path -LiteralPath $dir)) {
                New-Item -ItemType Directory -Path $dir -Force | Out-Null
            }
            if ($e.Name -ne '') {
                [IO.Compression.ZipFileExtensions]::ExtractToFile($e, $target, $true)
            }
            Set-Ui '正在解压语音模块…' (600 + [int]($i * 390 / [math]::Max($count, 1))) ('{0} / {1} 个文件' -f $i, $count)
        }
    } finally {
        $z.Dispose()
    }
}

$code = 0
$part = $Dest + '.part'
$zip = $Dest + '.zip'
try {
    if (-not $Silent) { Show-Ui }

    Set-Ui '正在获取安装信息…' 10
    $m = Get-Manifest $Manifest
    if (-not $m.url -or -not $m.sha256) { throw '清单内容不完整' }
    $total = [long]$m.size

    # 已装且内容一致：升级安装包但语音模块没变时不重复下载
    $stampPath = Join-Path $Dest '.ok'
    $exePath = Join-Path $Dest 'asr-host.exe'
    if ((Test-Path -LiteralPath $stampPath) -and (Test-Path -LiteralPath $exePath)) {
        try {
            $old = Get-Content -LiteralPath $stampPath -Raw -Encoding UTF8 | ConvertFrom-Json
            if ($old.sha256 -and ([string]$old.sha256).ToLower() -eq ([string]$m.sha256).ToLower()) {
                Close-Ui
                exit 0
            }
        } catch {}
    }

    # 下载文件放在 Dest 同级，先保证父目录存在
    $parent = Split-Path -Parent $Dest
    if ($parent -and -not (Test-Path -LiteralPath $parent)) {
        New-Item -ItemType Directory -Path $parent -Force | Out-Null
    }

    $ok = $false
    for ($try = 1; $try -le $Retries; $try++) {
        try {
            Get-Pack $m.url $part $total
            Set-Ui '正在校验文件完整性…' 580 '校验失败会自动重新下载'
            $h = (Get-FileHash -LiteralPath $part -Algorithm SHA256).Hash.ToLower()
            if ($h -ne ([string]$m.sha256).ToLower()) {
                Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
                throw '文件校验不通过'
            }
            $ok = $true
            break
        } catch {
            if ($script:Cancel) { throw }
            if ($try -eq $Retries) { throw }
            $wait = [int][math]::Pow(2, $try)
            Set-Ui ('下载中断，{0} 秒后重试（第 {1}/{2} 次）' -f $wait, $try, $Retries) 30
            Start-Sleep -Seconds $wait
        }
    }
    if (-not $ok) { throw '下载失败' }

    Move-Item -LiteralPath $part -Destination $zip -Force
    Set-Ui '正在部署语音模块…' 600
    Expand-Pack $zip $Dest

    $stamp = [ordered]@{
        version = [string]$m.version
        sha256  = [string]$m.sha256
        size    = $total
        at      = (Get-Date).ToString('s')
    }
    $stamp | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $Dest '.ok') -Encoding UTF8
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue

    Set-Ui '语音模块安装完成' 1000
    Start-Sleep -Milliseconds 500
} catch {
    # 用户主动跳过：清掉半成品，按成功返回给安装器（安装流程继续）
    if ($script:Cancel) {
        Remove-Item -LiteralPath $part -Force -ErrorAction SilentlyContinue
        Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
        Close-Ui
        exit 7
    }

    $msg = $_.Exception.Message
    $log = Join-Path $env:TEMP 'asheng-voice-install.log'
    try {
        ('[{0}] {1}{2}{3}' -f (Get-Date -Format 's'), $msg, [Environment]::NewLine, $_.ScriptStackTrace) |
            Add-Content -LiteralPath $log -Encoding UTF8
    } catch {}

    if (-not $Silent) {
        try {
            [Windows.Forms.MessageBox]::Show(
                ('语音模块安装失败：{0}{1}{1}可以先跳过，不影响其他功能；之后重新运行安装包即可再装。' -f $msg, [Environment]::NewLine),
                '阿盛 · 语音模块',
                [Windows.Forms.MessageBoxButtons]::OK,
                [Windows.Forms.MessageBoxIcon]::Warning) | Out-Null
        } catch {}
    }
    $code = 2
}

Close-Ui
exit $code
