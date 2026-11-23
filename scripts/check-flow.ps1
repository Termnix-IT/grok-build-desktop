<#
.SYNOPSIS
起動中の Grok Build Desktop に、Grok Bot と同じく gbd.exe でジョブを送り、登録した各エージェントが
動くかと、gbd で監視できるかを確かめます。

.DESCRIPTION
本物の CLI（Claude Code、Codex、Grok、Antigravity など）を動かすので、それぞれの使用量を使います。
確認用のリポジトリ（既定は %LOCALAPPDATA%\GrokBuildDesktop-flow-check）を作り、各エージェントに
「flow-check-<id>.txt を作って 1 行目に OK と書く」ジョブを 1 つずつ、同じタスクで順に送ります。

前もって次を済ませておきます。
  1. アプリを起動する（npm run tauri dev でもインストールした版でもよい）。
  2. 設定の「連携」で受付をオンにし、確認用のリポジトリのフォルダを許可に足す。
     フォルダはこのスクリプトが初回に作るので、最初の実行で許可がないと言われたら、足してから流し直す。

各エージェントについて次を確かめ、最後に表で出します。
  - gbd status で実行中になったのが見えたか
  - gbd log でエージェントの出力が読めたか
  - gbd wait が成功で終わったか
  - 変更したファイルにそのファイルが 1 件だけ入っていて、中身が OK か
  - 要約（エージェントの答え）が空でないか

途中で Ctrl+C を押すと、実行中のジョブを gbd cancel で止めてから終わります。

.PARAMETER Gbd
gbd.exe のパス。省略すると、このリポジトリの src-tauri\target\debug と release、PATH の順に探します。
インストールした版の場所は、設定の「連携」に出ます。

.PARAMETER Project
確認用のリポジトリのフォルダ。このスクリプトが作ったフォルダ（目印のファイルがあるもの）だけを使い、
流すたびに git reset --hard と git clean で最初の状態に戻します。

.PARAMETER Agent
確かめるエージェントの id。省略すると、有効で、見つかったものをすべて確かめます。

.PARAMETER TimeoutSecs
1 つのジョブを打ち切る秒数（10 以上）。アプリの設定の上限より長くはできません。

.EXAMPLE
pwsh -File scripts\check-flow.ps1

.EXAMPLE
pwsh -File scripts\check-flow.ps1 -Agent claude,codex -TimeoutSecs 300
#>
[CmdletBinding()]
param(
    [string]$Gbd,
    [string]$Project = (Join-Path $env:LOCALAPPDATA 'GrokBuildDesktop-flow-check'),
    [string[]]$Agent,
    [ValidateRange(10, 1800)]
    [int]$TimeoutSecs = 600
)

$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$marker = 'flow-check-marker.txt'

function Write-Step([string]$Text) {
    Write-Host ''
    Write-Host "== $Text" -ForegroundColor Cyan
}

function Stop-Check([string]$Text, [int]$Code = 2) {
    Write-Host $Text -ForegroundColor Red
    exit $Code
}

function Find-Gbd {
    if ($Gbd) {
        if (-not (Test-Path -LiteralPath $Gbd -PathType Leaf)) {
            Stop-Check "gbd.exe が見つかりません: $Gbd"
        }
        return (Resolve-Path -LiteralPath $Gbd).Path
    }
    $root = Split-Path -Parent $PSScriptRoot
    foreach ($profileName in 'debug', 'release') {
        $candidate = Join-Path $root "src-tauri\target\$profileName\gbd.exe"
        if (Test-Path -LiteralPath $candidate -PathType Leaf) {
            return $candidate
        }
    }
    $command = Get-Command gbd.exe -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($command) {
        return $command.Source
    }
    Stop-Check ('gbd.exe が見つかりません。cargo build --manifest-path src-tauri/Cargo.toml --bin gbd で作るか、' +
        '-Gbd でパスを渡してください')
}

# Runs one gbd command. --ascii keeps the output ASCII, so no console code page can garble it.
function Invoke-Gbd([string[]]$Arguments) {
    $lines = & $script:GbdPath --ascii @Arguments
    $code = $LASTEXITCODE
    $text = ($lines | Out-String).Trim()
    try {
        $json = $text | ConvertFrom-Json
    } catch {
        $json = $null
    }
    [pscustomobject]@{ Code = $code; Json = $json; Text = $text }
}

function Get-ErrorText($Reply) {
    if ($Reply.Json -and $Reply.Json.error) {
        return "$($Reply.Json.error.code): $($Reply.Json.error.message)"
    }
    return "終了コード $($Reply.Code): $($Reply.Text)"
}

function Invoke-Git([string[]]$Arguments) {
    & git -C $script:ProjectPath -c core.autocrlf=false @Arguments
    if ($LASTEXITCODE -ne 0) {
        Stop-Check "git $($Arguments -join ' ') が失敗しました（終了コード $LASTEXITCODE）"
    }
}

function Initialize-Project {
    if (Test-Path -LiteralPath $Project) {
        $script:ProjectPath = (Resolve-Path -LiteralPath $Project).Path
        if (-not (Test-Path -LiteralPath (Join-Path $script:ProjectPath $marker)) -or
            -not (Test-Path -LiteralPath (Join-Path $script:ProjectPath '.git'))) {
            Stop-Check ("$script:ProjectPath はこのスクリプトが作ったリポジトリではありません。" +
                '中身を消さないように止めます。別のフォルダを -Project で渡してください')
        }
        Invoke-Git @('reset', '--hard', '--quiet', 'HEAD')
        Invoke-Git @('clean', '-fdxq')
        return
    }
    New-Item -ItemType Directory -Path $Project | Out-Null
    $script:ProjectPath = (Resolve-Path -LiteralPath $Project).Path
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [IO.File]::WriteAllText((Join-Path $script:ProjectPath $marker),
        "このフォルダは scripts/check-flow.ps1 が使う確認用のリポジトリです。`n", $utf8)
    Invoke-Git @('init', '--quiet')
    Invoke-Git @('add', '--', $marker)
    Invoke-Git @('-c', 'user.name=flow-check', '-c', 'user.email=flow-check@example.invalid',
        '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'flow check')
}

# Prints the lines of `gbd log` not printed yet. `$Seen` collects every line seen so far.
function Write-NewLogLines([string]$JobId, [string]$Label, $Seen) {
    $reply = Invoke-Gbd @('log', $JobId, '--tail', '20')
    if ($reply.Code -ne 0 -or -not $reply.Json) {
        return
    }
    foreach ($line in @($reply.Json.lines)) {
        $key = "$($line.stream)`t$($line.text)"
        if ($Seen.Add($key)) {
            $text = [string]$line.text
            if ($text.Length -gt 160) {
                $text = $text.Substring(0, 160) + '…'
            }
            Write-Host "  [$Label $($line.stream)] $text" -ForegroundColor DarkGray
        }
    }
}

function Test-Agent($Info) {
    $id = [string]$Info.id
    $file = "flow-check-$id.txt"
    $result = [ordered]@{
        'エージェント' = $id
        '結果'         = 'NG'
        '状態'         = ''
        '秒'           = $null
        '実行中を確認' = $false
        'ログの行'     = 0
        'ファイル'     = $false
        '要約'         = ''
        '問題'         = New-Object System.Collections.Generic.List[string]
    }
    Write-Step "$($Info.name)（$id）"
    $job = [ordered]@{
        project      = $script:ProjectPath
        agent        = $id
        title        = "flow check $id"
        prompt       = ("このフォルダに $file というファイルを作り、1 行目に OK とだけ書いてください。" +
            'ほかのファイルは作ったり変えたりしないでください。シェルのコマンドは使わないでください。' +
            '終わったら「完了」とだけ答えてください。')
        timeout_secs = $TimeoutSecs
    }
    if ($script:TaskId) {
        $job.task_id = $script:TaskId
    }
    $jobFile = Join-Path ([IO.Path]::GetTempPath()) "gbd-flow-check-$([guid]::NewGuid()).json"
    $utf8 = New-Object System.Text.UTF8Encoding $false
    [IO.File]::WriteAllText($jobFile, ($job | ConvertTo-Json -Compress), $utf8)
    try {
        $submitted = Invoke-Gbd @('submit', $jobFile)
    } finally {
        Remove-Item -LiteralPath $jobFile -ErrorAction SilentlyContinue
    }
    if ($submitted.Code -ne 0) {
        $result['問題'].Add("submit: $(Get-ErrorText $submitted)")
        $code = if ($submitted.Json -and $submitted.Json.error) { $submitted.Json.error.code } else { '' }
        switch ($code) {
            'project_not_allowed' {
                Stop-Check ("$script:ProjectPath が許可されていません。設定の「連携」で、" +
                    'このフォルダを許可に足してから流し直してください')
            }
            'not_accepting' { Stop-Check '受付が止まっています。設定の「連携」で受付をオンにしてください' }
            'project_paused' {
                Stop-Check ('Grok Bot からの連続実行の上限に達しました。設定の「連携」の拒否の一覧から、' +
                    'このフォルダを再開してください')
            }
            { $_ -in 'task_paused', 'task_limit', 'task_closed' } { $script:TaskId = $null }
        }
        return [pscustomobject]$result
    }
    $jobId = [string]$submitted.Json.job_id
    $script:TaskId = [string]$submitted.Json.task_id
    $script:CurrentJob = $jobId
    Write-Host "  ジョブ $jobId（タスク $script:TaskId）を登録しました"

    $seen = New-Object 'System.Collections.Generic.HashSet[string]'
    $deadline = (Get-Date).AddSeconds($TimeoutSecs + 120)
    $final = $null
    $lastNote = ''
    while ((Get-Date) -lt $deadline) {
        $status = Invoke-Gbd @('status', $jobId)
        if ($status.Code -ne 0) {
            $result['問題'].Add("status: $(Get-ErrorText $status)")
            break
        }
        $state = [string]$status.Json.status
        if ($state -eq 'running') {
            $result['実行中を確認'] = $true
        }
        $note = $state
        if ($state -eq 'queued') {
            $note = if ($null -ne $status.Json.position) { "待ち（前に $($status.Json.position) 件）" } else { '待ち' }
        }
        if ($note -ne $lastNote) {
            Write-Host "  状態: $note"
            $lastNote = $note
        }
        Write-NewLogLines $jobId $id $seen
        $result['ログの行'] = $seen.Count
        if ($state -notin 'queued', 'running') {
            $final = Invoke-Gbd @('result', $jobId)
            break
        }
        # Long-polls like Grok Bot: returns as soon as the job ends, or after 5 seconds.
        $waited = Invoke-Gbd @('wait', $jobId, '--timeout', '5')
        if ($waited.Code -ne 0) {
            $result['問題'].Add("wait: $(Get-ErrorText $waited)")
            break
        }
        if ([string]$waited.Json.status -notin 'queued', 'running') {
            $final = $waited
            Write-NewLogLines $jobId $id $seen
            $result['ログの行'] = $seen.Count
            break
        }
    }
    if (-not $final) {
        $result['問題'].Add('時間内に終わらなかったので取り消しました')
        Invoke-Gbd @('cancel', $jobId) | Out-Null
        $script:CurrentJob = $null
        return [pscustomobject]$result
    }
    $script:CurrentJob = $null
    $view = $final.Json
    $result['状態'] = [string]$view.status
    $result['秒'] = $view.duration_secs
    $summary = [string]$view.summary
    $result['要約'] = if ($summary.Length -gt 40) { $summary.Substring(0, 40) + '…' } else { $summary }

    if ($view.status -ne 'succeeded') {
        $result['問題'].Add("状態が $($view.status)（終了コード $($view.exit_code)）")
    }
    if ($view.error) {
        $result['問題'].Add("エラー: $($view.error)")
    }
    if (-not $summary.Trim()) {
        $result['問題'].Add('要約が空です')
    }
    if (-not $result['実行中を確認']) {
        $result['問題'].Add('gbd status で実行中を一度も見られませんでした')
    }
    if ($result['ログの行'] -eq 0) {
        $result['問題'].Add('gbd log にエージェントの出力がありません')
    }
    $changed = @($view.changed_files)
    $paths = @($changed | ForEach-Object { [string]$_.path })
    if ($paths -notcontains $file) {
        $result['問題'].Add("変更したファイルに $file がありません（$($paths -join ', ')）")
    }
    $others = @($paths | Where-Object { $_ -ne $file })
    if ($others.Count -gt 0) {
        $result['問題'].Add("頼んでいないファイルも変わりました: $($others -join ', ')")
    }
    $path = Join-Path $script:ProjectPath $file
    if (Test-Path -LiteralPath $path -PathType Leaf) {
        $first = [string](Get-Content -LiteralPath $path -TotalCount 1 -Encoding UTF8)
        if ($first.Trim().TrimStart([char]0xFEFF) -eq 'OK') {
            $result['ファイル'] = $true
        } else {
            $result['問題'].Add("$file の 1 行目が OK ではありません: $first")
        }
    } else {
        $result['問題'].Add("$file がありません")
    }
    if (@($view.denied).Count -gt 0) {
        Write-Host "  拒否された操作: $(@($view.denied) -join ' / ')" -ForegroundColor Yellow
    }
    if ($result['問題'].Count -eq 0) {
        $result['結果'] = 'OK'
    }
    foreach ($problem in $result['問題']) {
        Write-Host "  NG: $problem" -ForegroundColor Red
    }
    return [pscustomobject]$result
}

$script:GbdPath = Find-Gbd
$script:TaskId = $null
$script:CurrentJob = $null
Write-Step "gbd: $script:GbdPath"

$version = Invoke-Gbd @('version')
if ($version.Code -ne 0 -or -not $version.Json) {
    Stop-Check "gbd version が失敗しました: $(Get-ErrorText $version)"
}
if (-not $version.Json.app_running) {
    Stop-Check 'アプリが起動していないか、パイプにつながりません。アプリを起動してから流してください'
}
if (-not $version.Json.accepting) {
    Stop-Check '受付が止まっています。設定の「連携」で受付をオンにしてください'
}
Write-Host "  gbd $($version.Json.cli_version) / アプリ $($version.Json.app_version)"
if ($version.Json.cli_version -ne $version.Json.app_version) {
    Write-Host '  gbd とアプリの版が違います' -ForegroundColor Yellow
}

$agents = Invoke-Gbd @('agents')
if ($agents.Code -ne 0) {
    Stop-Check "gbd agents が失敗しました: $(Get-ErrorText $agents)"
}
$all = @($agents.Json.agents)
$targets = New-Object System.Collections.Generic.List[object]
$results = New-Object System.Collections.Generic.List[object]
if ($Agent) {
    foreach ($id in $Agent) {
        $info = $all | Where-Object { $_.id -eq $id } | Select-Object -First 1
        if (-not $info) {
            Stop-Check "エージェント「$id」は登録されていません（$(($all | ForEach-Object id) -join ', ')）"
        }
        $targets.Add($info)
    }
} else {
    foreach ($info in $all) {
        if ($info.enabled -and $info.available -ne $false) {
            $targets.Add($info)
        } else {
            $why = if (-not $info.enabled) { '無効' } else { 'CLI が見つからない' }
            Write-Host "  $($info.id) は確かめません（$why）" -ForegroundColor Yellow
        }
    }
}
if ($targets.Count -eq 0) {
    Stop-Check '確かめられるエージェントがありません'
}

Write-Step "確認用のリポジトリ: $Project"
Initialize-Project
Write-Host "  $script:ProjectPath を最初の状態に戻しました"

try {
    foreach ($info in $targets) {
        if (-not $info.enabled) {
            $results.Add([pscustomobject][ordered]@{ 'エージェント' = $info.id; '結果' = 'NG'; '問題' = '無効になっています' })
            continue
        }
        if ($info.available -eq $false) {
            $results.Add([pscustomobject][ordered]@{ 'エージェント' = $info.id; '結果' = 'NG'; '問題' = 'CLI が見つかりません' })
            continue
        }
        $results.Add((Test-Agent $info))
    }
} finally {
    if ($script:CurrentJob) {
        Write-Host ''
        Write-Host "中断したので、実行中のジョブ $script:CurrentJob を取り消します" -ForegroundColor Yellow
        Invoke-Gbd @('cancel', $script:CurrentJob) | Out-Null
    }
}

Write-Step '結果'
$results | Format-Table -AutoSize -Wrap -Property 'エージェント', '結果', '状態', '秒', '実行中を確認', 'ログの行', 'ファイル', '要約' |
    Out-Host
$failed = @($results | Where-Object { $_.'結果' -ne 'OK' })
if ($failed.Count -gt 0) {
    Write-Host "$($failed.Count) 件のエージェントで問題がありました" -ForegroundColor Red
    exit 1
}
Write-Host 'すべてのエージェントで、送信から監視、結果の受け取りまで通りました' -ForegroundColor Green
exit 0
