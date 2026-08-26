#requires -Version 7.4

<#
.SYNOPSIS
使用真实游戏和确定性本地 Provider 比较两个 ATT Release 制品。

.DESCRIPTION
主样本按 AB/BA 顺序运行两轮；结果贴线或冲突时最多补第三轮。其他样本各运行一对。
每个场景都建立新的发行根、项目数据库、冻结来源和输出目录，并核对 Manual 导出、完整
输出树、任务消息与结构化终态。原始记录和报告写入仓库 tmp/performance。

.PARAMETER BaselineExe
冻结基线的 Release/MSVC att.exe。

.PARAMETER CandidateExe
待验证的 Release/MSVC att.exe。

.PARAMETER TestRoot
本机测试集根目录。默认使用仓库的 测试集 目录。

.PARAMETER PrimarySample
承担两至三轮决策的样本 ID。默认是高文本量 MZ 样本 boku-to-succubus。

.PARAMETER AdditionalSamples
主样本通过后各运行一对的回归样本 ID。

.PARAMETER FocusStage
本次候选声称改善的阶段；相对收益按该阶段判断。

.PARAMETER GenericCopies
Generic 基础 fixture 的确定性扩展份数。每份包含五个 Group。

.PARAMETER TargetTaskCharacters
性能配置中每个模型任务的目标原文字符数。基线与候选始终使用同一值。

.PARAMETER MaximumPrimaryRounds
主样本最多轮数。只能是 2 或 3；第三轮只在前两轮贴线或冲突时启动。

.PARAMETER BudgetMinutes
本次运行预算。预算只在成对轮次之间检查，不中断已经开始的 ATT 命令。

.PARAMETER RunRoot
本次原始记录、证据和报告目录。必须位于仓库 tmp/performance 下且尚不存在。

.PARAMETER PreflightOnly
只验证制品、Node、样本、资产和环境身份，不运行性能场景。

.PARAMETER KeepWorkspaces
保留成功场景的完整运行目录。默认只保留原始记录、项目日志、哈希和报告。

.PARAMETER AllowIdenticalExecutables
只用于验证测试框架本身。允许两个制品哈希相同，但最终结论固定为 validation_only。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [string]$BaselineExe,

    [Parameter(Mandatory)]
    [string]$CandidateExe,

    [string]$TestRoot,
    [string]$PrimarySample = 'boku-to-succubus',
    [string[]]$AdditionalSamples = @(
        'succubus-academia',
        'princess-honey-trap',
        'succubus-academia-generic'
    ),

    [ValidateSet('Extract', 'Translate', 'WriteBack')]
    [string]$FocusStage = 'Translate',

    [ValidateRange(1, 50000)]
    [int]$GenericCopies = 2000,

    [ValidateRange(2, 3)]
    [int]$MaximumPrimaryRounds = 3,

    [ValidateRange(1, 45)]
    [int]$BudgetMinutes = 20,

    [ValidateRange(1, 1000000)]
    [int]$TargetTaskCharacters = 24000,

    [string]$RunRoot,
    [switch]$PreflightOnly,
    [switch]$KeepWorkspaces,
    [switch]$AllowIdenticalExecutables
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false
$Utf8NoBom = [Text.UTF8Encoding]::new($false)

$RepositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$MaintenanceRoot = Join-Path $RepositoryRoot 'maintenance\performance'
$ManifestPath = Join-Path $MaintenanceRoot 'samples.json'
$ProviderScript = Join-Path $MaintenanceRoot 'local-provider.mjs'
$PerformanceRoot = [IO.Path]::GetFullPath((Join-Path $RepositoryRoot 'tmp\performance'))

if ([string]::IsNullOrWhiteSpace($TestRoot)) {
    $TestRoot = Join-Path $RepositoryRoot '测试集'
}
$TestRoot = [IO.Path]::GetFullPath($TestRoot)
$BaselineExe = [IO.Path]::GetFullPath($BaselineExe)
$CandidateExe = [IO.Path]::GetFullPath($CandidateExe)

if ([string]::IsNullOrWhiteSpace($RunRoot)) {
    $RunRoot = Join-Path $PerformanceRoot ('run-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
}
$RunRoot = [IO.Path]::GetFullPath($RunRoot)
$WorkRoot = Join-Path $RunRoot 'work'
$EvidenceRoot = Join-Path $RunRoot 'evidence'
$OverallClock = [Diagnostics.Stopwatch]::StartNew()

function Test-PathUnderRoot {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [string]$Root
    )

    $candidate = [IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    $rootPath = [IO.Path]::GetFullPath($Root).TrimEnd('\', '/')
    $prefix = $rootPath + [IO.Path]::DirectorySeparatorChar
    return $candidate.StartsWith($prefix, [StringComparison]::OrdinalIgnoreCase)
}

function Assert-NoReparsePoint {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [switch]$Recurse
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $pending = [Collections.Generic.Stack[string]]::new()
    $pending.Push((Get-Item -LiteralPath $Path -Force).FullName)
    while ($pending.Count -gt 0) {
        $item = Get-Item -LiteralPath $pending.Pop() -Force
        if (($item.Attributes -band [IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "拒绝使用 reparse point：$($item.FullName)"
        }
        if ($Recurse -and $item.PSIsContainer) {
            foreach ($child in Get-ChildItem -LiteralPath $item.FullName -Force) {
                $pending.Push($child.FullName)
            }
        }
    }
}

function Remove-ScenarioWorkspace {
    param([Parameter(Mandatory)] [string]$Path)

    if (-not (Test-PathUnderRoot -Path $Path -Root $WorkRoot)) {
        throw "拒绝清理性能工作区之外的路径：$Path"
    }
    if (Test-Path -LiteralPath $Path) {
        Assert-NoReparsePoint -Path $Path -Recurse
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

function Get-FileSha256 {
    param([Parameter(Mandatory)] [string]$Path)
    return (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-TreeIdentity {
    param([Parameter(Mandatory)] [string]$Root)

    $resolvedRoot = (Resolve-Path -LiteralPath $Root).Path
    Assert-NoReparsePoint -Path $resolvedRoot -Recurse
    $files = @(Get-ChildItem -LiteralPath $resolvedRoot -Recurse -File -Force |
            ForEach-Object { $_.FullName })
    [Array]::Sort($files, [StringComparer]::Ordinal)

    $hasher = [Security.Cryptography.IncrementalHash]::CreateHash(
        [Security.Cryptography.HashAlgorithmName]::SHA256
    )
    $buffer = [byte[]]::new(1MB)
    [long]$totalBytes = 0
    try {
        foreach ($file in $files) {
            $item = Get-Item -LiteralPath $file -Force
            $relative = [IO.Path]::GetRelativePath($resolvedRoot, $item.FullName).Replace('\', '/')
            $header = $Utf8NoBom.GetBytes("$relative`0$($item.Length)`0")
            $hasher.AppendData($header)
            $stream = [IO.File]::OpenRead($item.FullName)
            try {
                while (($count = $stream.Read($buffer, 0, $buffer.Length)) -gt 0) {
                    $hasher.AppendData($buffer, 0, $count)
                }
            }
            finally {
                $stream.Dispose()
            }
            $hasher.AppendData([byte[]](0))
            $totalBytes += $item.Length
        }
        $digest = [Convert]::ToHexString($hasher.GetHashAndReset()).ToLowerInvariant()
    }
    finally {
        $hasher.Dispose()
    }

    return [pscustomobject][ordered]@{
        root = $resolvedRoot
        files = $files.Count
        bytes = $totalBytes
        sha256 = $digest
    }
}

function Invoke-CapturedProcess {
    param(
        [Parameter(Mandatory)] [string]$FilePath,
        [Parameter(Mandatory)] [string[]]$Arguments,
        [Parameter(Mandatory)] [string]$WorkingDirectory,
        [Parameter(Mandatory)] [string]$Name,
        [Parameter(Mandatory)] [string]$EvidenceDirectory
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.Environment['ATT_UI_LANGUAGE'] = 'en'
    foreach ($argument in $Arguments) {
        $startInfo.ArgumentList.Add($argument)
    }

    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    $startedAt = [DateTimeOffset]::UtcNow
    $clock = [Diagnostics.Stopwatch]::StartNew()
    if (-not $process.Start()) {
        throw "无法启动 $Name：$FilePath"
    }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()
    [long]$peakWorkingSet = 0
    while (-not $process.WaitForExit(50)) {
        try {
            $peakWorkingSet = [Math]::Max($peakWorkingSet, [long]$process.WorkingSet64)
        }
        catch {
            # 进程可能恰好在采样时退出；最终退出状态仍由 WaitForExit 确认。
        }
    }
    try {
        $peakWorkingSet = [Math]::Max($peakWorkingSet, [long]$process.PeakWorkingSet64)
    }
    catch {
        # 极短进程可能在首次采样前退出，此时保留已经取得的 0。
    }
    $stdout = $stdoutTask.GetAwaiter().GetResult()
    $stderr = $stderrTask.GetAwaiter().GetResult()
    $clock.Stop()

    $result = [pscustomobject][ordered]@{
        name = $Name
        started_at = $startedAt.ToString('O')
        duration_ms = [Math]::Round($clock.Elapsed.TotalMilliseconds, 3)
        cpu_ms = [Math]::Round($process.TotalProcessorTime.TotalMilliseconds, 3)
        peak_working_set_bytes = $peakWorkingSet
        exit_code = $process.ExitCode
        arguments = $Arguments
    }
    $process.Dispose()

    [IO.Directory]::CreateDirectory($EvidenceDirectory) | Out-Null
    [IO.File]::WriteAllText((Join-Path $EvidenceDirectory "$Name.stdout.log"), $stdout, $Utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $EvidenceDirectory "$Name.stderr.log"), $stderr, $Utf8NoBom)
    if ($result.exit_code -ne 0) {
        throw "$Name 失败，退出码 $($result.exit_code)。证据：$EvidenceDirectory"
    }
    return $result
}

function Get-ExecutableIdentity {
    param([Parameter(Mandatory)] [string]$Executable)

    if (-not (Test-Path -LiteralPath $Executable -PathType Leaf)) {
        throw "ATT 制品不存在：$Executable"
    }
    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $Executable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    $startInfo.ArgumentList.Add('--version')
    $process = [Diagnostics.Process]::Start($startInfo)
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    if ($process.ExitCode -ne 0) {
        throw "无法读取 ATT 版本：$Executable`n$stderr"
    }
    return [pscustomobject][ordered]@{
        path = $Executable
        version = $stdout.Trim()
        bytes = (Get-Item -LiteralPath $Executable).Length
        sha256 = Get-FileSha256 -Path $Executable
    }
}

function Get-ToolchainIdentity {
    try {
        $rustc = Get-Command rustc -CommandType Application -ErrorAction Stop |
            Select-Object -First 1
        $cargo = Get-Command cargo -CommandType Application -ErrorAction Stop |
            Select-Object -First 1
        $rustcOutput = @(& $rustc.Source -Vv)
        if ($LASTEXITCODE -ne 0) {
            throw 'rustc -Vv 失败'
        }
        $cargoOutput = ((& $cargo.Source -V) -join '').Trim()
        if ($LASTEXITCODE -ne 0) {
            throw 'cargo -V 失败'
        }
        $rustup = Get-Command rustup -CommandType Application -ErrorAction SilentlyContinue |
            Select-Object -First 1
        $activeToolchain = $null
        if ($null -ne $rustup) {
            $activeToolchain = ((& $rustup.Source show active-toolchain) -join '').Trim()
            if ($LASTEXITCODE -ne 0) {
                $activeToolchain = $null
            }
        }
        return [pscustomobject][ordered]@{
            available = $true
            rustc_path = $rustc.Source
            rustc = $rustcOutput -join "`n"
            rustc_summary = $rustcOutput[0]
            cargo_path = $cargo.Source
            cargo = $cargoOutput
            active_toolchain = $activeToolchain
            reason = $null
        }
    }
    catch {
        return [pscustomobject][ordered]@{
            available = $false
            rustc_path = $null
            rustc = $null
            rustc_summary = $null
            cargo_path = $null
            cargo = $null
            active_toolchain = $null
            reason = $_.Exception.Message
        }
    }
}

function Get-StorageIdentity {
    param(
        [Parameter(Mandatory)] [string]$Path,
        [Parameter(Mandatory)] [string]$Purpose
    )

    $fullPath = [IO.Path]::GetFullPath($Path)
    $root = [IO.Path]::GetPathRoot($fullPath)
    try {
        if ($root -notmatch '^(?<drive>[A-Za-z]):\\$') {
            throw "不是可查询的本地盘符：$root"
        }
        $partition = Get-Partition -DriveLetter $Matches.drive | Select-Object -First 1
        $disk = $partition | Get-Disk
        $physical = Get-PhysicalDisk -DeviceNumber ([int]$disk.Number) |
            Select-Object -First 1
        return [pscustomobject][ordered]@{
            purpose = $Purpose
            root = $root
            available = $true
            disk_number = [int]$disk.Number
            name = [string]$physical.FriendlyName
            media_type = [string]$physical.MediaType
            bus_type = [string]$physical.BusType
            size_bytes = [long]$physical.Size
            reason = $null
        }
    }
    catch {
        return [pscustomobject][ordered]@{
            purpose = $Purpose
            root = $root
            available = $false
            disk_number = $null
            name = $null
            media_type = $null
            bus_type = $null
            size_bytes = $null
            reason = $_.Exception.Message
        }
    }
}

function Resolve-MaintenanceFile {
    param([Parameter(Mandatory)] [string]$RelativePath)
    $path = [IO.Path]::GetFullPath((Join-Path $MaintenanceRoot $RelativePath))
    if (-not (Test-PathUnderRoot -Path $path -Root $MaintenanceRoot)) {
        throw "样本资产越出维护目录：$RelativePath"
    }
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "样本资产不存在：$path"
    }
    return $path
}

function Resolve-Sample {
    param([Parameter(Mandatory)] $Definition)

    $common = [ordered]@{
        id = [string]$Definition.id
        display_name = [string]$Definition.display_name
        kind = [string]$Definition.kind
        engine = [string]$Definition.engine
        source_language = [string]$Definition.source_language
        purpose = [string]$Definition.purpose
        terms = Resolve-MaintenanceFile -RelativePath ([string]$Definition.terms)
        placeholders = Resolve-MaintenanceFile -RelativePath ([string]$Definition.placeholders)
    }

    if ($common.kind -eq 'rpg_maker') {
        if ($common.engine -notin @('mv', 'mz')) {
            throw "RPG Maker 样本引擎无效：$($common.id)"
        }
        $gameRoot = [IO.Path]::GetFullPath((Join-Path $TestRoot ([string]$Definition.test_set_path)))
        if (-not (Test-Path -LiteralPath $gameRoot -PathType Container)) {
            throw "游戏样本不存在：$gameRoot"
        }
        $common.game_root = $gameRoot
        $common.rules = Resolve-MaintenanceFile -RelativePath ([string]$Definition.rules)
        $dialogueProperty = $Definition.PSObject.Properties['dialogue_rules']
        $common.dialogue_rules = if ($null -eq $dialogueProperty) {
            $null
        }
        else {
            Resolve-MaintenanceFile -RelativePath ([string]$dialogueProperty.Value)
        }
    }
    elseif ($common.kind -eq 'generic') {
        if ($common.engine -ne 'generic') {
            throw "Generic 样本引擎无效：$($common.id)"
        }
        $fixture = [IO.Path]::GetFullPath((Join-Path $MaintenanceRoot ([string]$Definition.fixture)))
        if (-not (Test-PathUnderRoot -Path $fixture -Root $MaintenanceRoot)) {
            throw "Generic fixture 越出维护目录：$fixture"
        }
        if (-not (Test-Path -LiteralPath $fixture -PathType Container)) {
            throw "Generic fixture 不存在：$fixture"
        }
        $common.fixture = $fixture
    }
    else {
        throw "未知样本类型：$($common.kind)"
    }
    return [pscustomobject]$common
}

function New-ScenarioConfig {
    param(
        [Parameter(Mandatory)] [string]$RuntimeRoot,
        [Parameter(Mandatory)] [int]$ProviderPort,
        [Parameter(Mandatory)] [string]$ScenarioLabel
    )

    $content = @"
[prompts]
thinking_output = true
source_echo = false

[llm.clients.performance]
url = "http://127.0.0.1:$ProviderPort/v1/chat/completions"
api_key = "att-performance-$ScenarioLabel"
model = "att-local-performance"
stream = false
max_concurrent_requests = 16
connect_timeout_ms = 5000
read_timeout_ms = 120000
request_timeout_ms = 120000
proxy = false
additional_pem_files = []
retry_delays_ms = []
max_retry_after_ms = 1000
parameters = '''
{}
'''

[[languages]]
type = "japanese"
id = "ja"
minimum_kana_characters = 1
allowed_terms = []

[translation]
record_translation_tasks = true

[[translation.profiles]]
id = "performance"
llm_client = "performance"
target_task_user_message_characters = $TargetTaskCharacters
"@
    [IO.File]::WriteAllText((Join-Path $RuntimeRoot 'config.toml'), $content, $Utf8NoBom)
}

function New-ScenarioRuntime {
    param(
        [Parameter(Mandatory)] [string]$ScenarioRoot,
        [Parameter(Mandatory)] [string]$Executable,
        [Parameter(Mandatory)] [int]$ProviderPort,
        [Parameter(Mandatory)] [string]$ScenarioLabel
    )

    $runtimeRoot = Join-Path $ScenarioRoot 'runtime'
    [IO.Directory]::CreateDirectory($runtimeRoot) | Out-Null
    Copy-Item -LiteralPath $Executable -Destination (Join-Path $runtimeRoot 'att.exe')
    Copy-Item -LiteralPath (Join-Path $RepositoryRoot 'prompts') `
        -Destination (Join-Path $runtimeRoot 'prompts') -Recurse
    New-ScenarioConfig -RuntimeRoot $runtimeRoot -ProviderPort $ProviderPort `
        -ScenarioLabel $ScenarioLabel
    return $runtimeRoot
}

function New-GenericInput {
    param(
        [Parameter(Mandatory)] [string]$FixtureRoot,
        [Parameter(Mandatory)] [string]$TargetRoot,
        [Parameter(Mandatory)] [int]$Copies
    )

    [IO.Directory]::CreateDirectory($TargetRoot) | Out-Null
    $fixtureFiles = @(Get-ChildItem -LiteralPath $FixtureRoot -File -Filter '*.jsonl' |
            Sort-Object Name)
    if ($fixtureFiles.Count -eq 0) {
        throw "Generic fixture 没有 JSONL：$FixtureRoot"
    }

    foreach ($file in $fixtureFiles) {
        $groups = @(Get-Content -LiteralPath $file.FullName |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
                ForEach-Object { $_ | ConvertFrom-Json })
        $target = Join-Path $TargetRoot $file.Name
        $writer = [IO.StreamWriter]::new($target, $false, $Utf8NoBom)
        try {
            for ($copy = 1; $copy -le $Copies; $copy++) {
                $suffix = $copy.ToString('D6', [Globalization.CultureInfo]::InvariantCulture)
                foreach ($sourceGroup in $groups) {
                    $group = ($sourceGroup | ConvertTo-Json -Depth 20 -Compress | ConvertFrom-Json)
                    $group.id = "$($group.id)-case-$suffix"
                    $lastUnit = $group.units[$group.units.Count - 1]
                    $lastUnit.text = "$($lastUnit.text) 性能試験番号$suffix。"
                    $writer.WriteLine(($group | ConvertTo-Json -Depth 20 -Compress))
                }
            }
        }
        finally {
            $writer.Dispose()
        }
    }
}

function Start-LocalProvider {
    param(
        [Parameter(Mandatory)] [string]$NodeExecutable,
        [Parameter(Mandatory)] [string]$MetricsFile,
        [Parameter(Mandatory)] [string]$ReadyFile
    )

    $startInfo = [Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $NodeExecutable
    $startInfo.UseShellExecute = $false
    $startInfo.CreateNoWindow = $true
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in @($ProviderScript, $MetricsFile, $ReadyFile)) {
        $startInfo.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "无法启动本地 Provider：$NodeExecutable"
    }
    $stdoutTask = $process.StandardOutput.ReadToEndAsync()
    $stderrTask = $process.StandardError.ReadToEndAsync()

    $deadline = [DateTimeOffset]::UtcNow.AddSeconds(10)
    while (-not (Test-Path -LiteralPath $ReadyFile -PathType Leaf)) {
        if ($process.HasExited) {
            $stderr = $stderrTask.GetAwaiter().GetResult()
            throw "本地 Provider 在就绪前退出：$stderr"
        }
        if ([DateTimeOffset]::UtcNow -ge $deadline) {
            $process.Kill($true)
            throw '本地 Provider 在 10 秒内没有就绪'
        }
        Start-Sleep -Milliseconds 50
    }
    $ready = Get-Content -Raw -LiteralPath $ReadyFile | ConvertFrom-Json
    return [pscustomobject]@{
        process = $process
        stdout_task = $stdoutTask
        stderr_task = $stderrTask
        port = [int]$ready.port
    }
}

function Stop-LocalProvider {
    param(
        [AllowNull()] $Provider,
        [Parameter(Mandatory)] [string]$OutputDirectory
    )

    if ($null -eq $Provider) {
        return
    }
    try {
        $client = [Net.Http.HttpClient]::new()
        try {
            $content = [Net.Http.ByteArrayContent]::new([byte[]]::new(0))
            $response = $client.PostAsync(
                "http://127.0.0.1:$($Provider.port)/__att_performance__/shutdown",
                $content
            ).GetAwaiter().GetResult()
            $response.Dispose()
        }
        finally {
            $client.Dispose()
        }
    }
    catch {
        Write-Warning "无法通过 HTTP 关闭本地 Provider：$($_.Exception.Message)"
    }

    if (-not $Provider.process.WaitForExit(5000)) {
        $Provider.process.Kill($true)
        $Provider.process.WaitForExit()
    }
    $stdout = $Provider.stdout_task.GetAwaiter().GetResult()
    $stderr = $Provider.stderr_task.GetAwaiter().GetResult()
    [IO.Directory]::CreateDirectory($OutputDirectory) | Out-Null
    [IO.File]::WriteAllText((Join-Path $OutputDirectory 'provider.stdout.log'), $stdout, $Utf8NoBom)
    [IO.File]::WriteAllText((Join-Path $OutputDirectory 'provider.stderr.log'), $stderr, $Utf8NoBom)
    $Provider.process.Dispose()
}

function Get-ProviderSummary {
    param(
        [Parameter(Mandatory)] [string]$MetricsFile,
        [Parameter(Mandatory)] [string]$ScenarioLabel
    )

    $records = @(
        if (Test-Path -LiteralPath $MetricsFile -PathType Leaf) {
            Get-Content -LiteralPath $MetricsFile |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
                ForEach-Object { $_ | ConvertFrom-Json } |
                Where-Object { $_.scenario -ceq $ScenarioLabel } |
                Sort-Object request_index
        }
    )
    if ($records.Count -eq 0) {
        throw "场景没有 Provider 请求：$ScenarioLabel"
    }
    $failed = @($records | Where-Object { $_.status -ne 200 })
    if ($failed.Count -ne 0) {
        throw "场景存在 Provider 失败：$ScenarioLabel"
    }
    return [pscustomobject][ordered]@{
        requests = $records.Count
        units = [long](($records | Measure-Object -Property unit_count -Sum).Sum)
        request_bytes = [long](($records | Measure-Object -Property request_bytes -Sum).Sum)
        response_bytes = [long](($records | Measure-Object -Property response_bytes -Sum).Sum)
        provider_duration_ms = [Math]::Round(
            [double](($records | Measure-Object -Property duration_ms -Sum).Sum),
            3
        )
        request_order_user_message_sha256 = @(
            $records | ForEach-Object { $_.user_message_sha256 }
        )
        user_message_sha256 = @(
            $records | ForEach-Object { $_.user_message_sha256 } | Sort-Object
        )
        system_message_sha256 = @($records | ForEach-Object { $_.system_message_sha256 } |
                Sort-Object -Unique)
        assistant_content_sha256 = @(
            $records | ForEach-Object { $_.assistant_content_sha256 } | Sort-Object
        )
    }
}

function Get-ProjectFacts {
    param(
        [Parameter(Mandatory)] [string]$ProjectRoot,
        [Parameter(Mandatory)] [string]$EvidenceDirectory
    )

    $logRoot = Join-Path $ProjectRoot 'logs'
    $logFiles = @(Get-ChildItem -LiteralPath $logRoot -File -Filter '*.jsonl' | Sort-Object Name)
    $records = @()
    foreach ($file in $logFiles) {
        $fileRecords = @(Get-Content -LiteralPath $file.FullName |
                Where-Object { -not [string]::IsNullOrWhiteSpace($_) } |
                ForEach-Object { $_ | ConvertFrom-Json })
        if ($fileRecords.Count -eq 0 -or $fileRecords[-1].event -cne 'run.finished') {
            throw "项目日志没有以 run.finished 结束：$($file.FullName)"
        }
        $records += $fileRecords
    }

    $translation = @($records | Where-Object { $_.event -ceq 'translation.finished' })
    $publication = @($records | Where-Object { $_.event -ceq 'publication.finished' })
    if ($translation.Count -ne 1 -or $translation[0].payload.result.kind -cne 'complete') {
        throw "Translate 没有唯一 complete 终态：$ProjectRoot"
    }
    if ($publication.Count -ne 1 -or $publication[0].payload.result.kind -cne 'published') {
        throw "WriteBack 没有唯一 published 终态：$ProjectRoot"
    }
    if (@($records | Where-Object { $_.level -ceq 'error' }).Count -ne 0) {
        throw "项目日志存在 error：$ProjectRoot"
    }

    $logEvidence = Join-Path $EvidenceDirectory 'project-logs'
    [IO.Directory]::CreateDirectory($logEvidence) | Out-Null
    foreach ($file in $logFiles) {
        Copy-Item -LiteralPath $file.FullName -Destination (Join-Path $logEvidence $file.Name)
    }

    $translationJson = $translation[0].payload.result | ConvertTo-Json -Depth 20 -Compress
    $publicationJson = $publication[0].payload.result | ConvertTo-Json -Depth 20 -Compress
    $taskRecordsRoot = Join-Path $ProjectRoot 'task-records'
    $taskRecordFiles = @(
        if (Test-Path -LiteralPath $taskRecordsRoot) {
            Get-ChildItem -LiteralPath $taskRecordsRoot -Recurse -File -Filter '*.md'
        }
    )
    return [pscustomobject][ordered]@{
        translation_result_sha256 = Get-StringSha256 -Value $translationJson
        publication_result_sha256 = Get-StringSha256 -Value $publicationJson
        task_finished = @($records | Where-Object { $_.event -ceq 'task.finished' }).Count
        warning_diagnostics = @($records | Where-Object {
                $_.level -ceq 'warn' -and $_.event -like 'diagnostic.*'
            }).Count
        task_record_files = $taskRecordFiles.Count
        task_record_bytes = [long](($taskRecordFiles | Measure-Object -Property Length -Sum).Sum)
    }
}

function Get-StringSha256 {
    param([Parameter(Mandatory)] [AllowEmptyString()] [string]$Value)
    $bytes = $Utf8NoBom.GetBytes($Value)
    return [Convert]::ToHexString([Security.Cryptography.SHA256]::HashData($bytes)).ToLowerInvariant()
}

function Invoke-Scenario {
    param(
        [Parameter(Mandatory)] $Sample,
        [Parameter(Mandatory)] [ValidateSet('baseline', 'candidate')] [string]$Variant,
        [Parameter(Mandatory)] [int]$Round,
        [Parameter(Mandatory)] [string]$Scope,
        [Parameter(Mandatory)] [string]$Executable,
        [Parameter(Mandatory)] [int]$ProviderPort,
        [Parameter(Mandatory)] [string]$ProviderMetrics
    )

    $label = "$Scope-r$($Round.ToString('D2'))-$Variant-$($Sample.id)"
    if ($label -notmatch '^[a-z0-9-]+$') {
        throw "场景标签包含非法字符：$label"
    }
    $scenarioRoot = Join-Path $WorkRoot $label
    $scenarioEvidence = Join-Path $EvidenceRoot $label
    if (Test-Path -LiteralPath $scenarioRoot) {
        throw "场景目录已存在：$scenarioRoot"
    }
    [IO.Directory]::CreateDirectory($scenarioRoot) | Out-Null
    [IO.Directory]::CreateDirectory($scenarioEvidence) | Out-Null

    $runtimeRoot = New-ScenarioRuntime -ScenarioRoot $scenarioRoot -Executable $Executable `
        -ProviderPort $ProviderPort -ScenarioLabel $label
    $att = Join-Path $runtimeRoot 'att.exe'
    $projectName = 'performance'
    $commands = [ordered]@{}

    if ($Sample.kind -eq 'generic') {
        $inputRoot = Join-Path $scenarioRoot 'input'
        New-GenericInput -FixtureRoot $Sample.fixture -TargetRoot $inputRoot -Copies $GenericCopies
        $initArguments = @(
            '--ui-language', 'en', 'generic', 'init', '--name', $projectName,
            '--path', $inputRoot, '--source-language', $Sample.source_language,
            '--target-language', 'zh-Hans'
        )
        $extractArguments = @('--ui-language', 'en', 'generic', 'extract', '--name', $projectName)
    }
    else {
        $initArguments = @(
            '--ui-language', 'en', $Sample.engine, 'init', '--name', $projectName,
            '--path', $Sample.game_root, '--source-language', $Sample.source_language,
            '--target-language', 'zh-Hans', '--dialogue-max-fullwidth-chars', '40',
            '--scrolling-text-max-fullwidth-chars', '40',
            '--help-description-max-fullwidth-chars', '34'
        )
        $extractArguments = @(
            '--ui-language', 'en', $Sample.engine, 'extract', '--name', $projectName,
            '--builtin', '--rules', $Sample.rules
        )
        if ($null -ne $Sample.dialogue_rules) {
            $extractArguments += @('--dialogue-rules', $Sample.dialogue_rules)
        }
    }

    $commands.init = Invoke-CapturedProcess -FilePath $att -Arguments $initArguments `
        -WorkingDirectory $RepositoryRoot -Name 'init' -EvidenceDirectory $scenarioEvidence
    $commands.extract = Invoke-CapturedProcess -FilePath $att -Arguments $extractArguments `
        -WorkingDirectory $RepositoryRoot -Name 'extract' -EvidenceDirectory $scenarioEvidence

    $extractedManualFile = Join-Path $scenarioRoot 'manual-after-extract.toml'
    $commands.manual_after_extract = Invoke-CapturedProcess -FilePath $att -Arguments @(
        '--ui-language', 'en', $Sample.engine, 'manual', 'export', '--name', $projectName,
        $extractedManualFile
    ) -WorkingDirectory $RepositoryRoot -Name 'manual-after-extract' `
        -EvidenceDirectory $scenarioEvidence

    $translateArguments = @(
        '--ui-language', 'en', $Sample.engine, 'translate', '--name', $projectName,
        'performance', '--terms', $Sample.terms, '--placeholders', $Sample.placeholders
    )
    $commands.translate = Invoke-CapturedProcess -FilePath $att -Arguments $translateArguments `
        -WorkingDirectory $RepositoryRoot -Name 'translate' -EvidenceDirectory $scenarioEvidence

    $writeBackArguments = @(
        '--ui-language', 'en', $Sample.engine, 'write-back', '--name', $projectName
    )
    $commands.write_back = Invoke-CapturedProcess -FilePath $att -Arguments $writeBackArguments `
        -WorkingDirectory $RepositoryRoot -Name 'write-back' -EvidenceDirectory $scenarioEvidence

    $manualFile = Join-Path $scenarioRoot 'manual.toml'
    $commands.manual_export = Invoke-CapturedProcess -FilePath $att -Arguments @(
        '--ui-language', 'en', $Sample.engine, 'manual', 'export', '--name', $projectName,
        $manualFile
    ) -WorkingDirectory $RepositoryRoot -Name 'manual-export' -EvidenceDirectory $scenarioEvidence

    $projectRoot = Join-Path $runtimeRoot "projects\$($Sample.engine)\$projectName"
    $writeBackRoot = Join-Path $projectRoot 'write_back'
    $outputIdentity = Get-TreeIdentity -Root $writeBackRoot
    $projectFacts = Get-ProjectFacts -ProjectRoot $projectRoot -EvidenceDirectory $scenarioEvidence
    $provider = Get-ProviderSummary -MetricsFile $ProviderMetrics -ScenarioLabel $label
    if ($projectFacts.task_finished -ne $provider.requests) {
        throw "task.finished 与 Provider 请求数不一致：$label"
    }

    $database = Join-Path $projectRoot 'project.db'
    $result = [pscustomobject][ordered]@{
        label = $label
        sample = $Sample.id
        variant = $Variant
        round = $Round
        scope = $Scope
        workspace = $scenarioRoot
        commands = [pscustomobject]$commands
        provider = $provider
        project = $projectFacts
        extracted_manual_sha256 = Get-FileSha256 -Path $extractedManualFile
        final_manual_sha256 = Get-FileSha256 -Path $manualFile
        database_bytes = (Get-Item -LiteralPath $database).Length
        database_sha256 = Get-FileSha256 -Path $database
        output = $outputIdentity
    }
    return $result
}

function Test-SequenceEqual {
    param([object[]]$Left, [object[]]$Right)
    if ($Left.Count -ne $Right.Count) {
        return $false
    }
    for ($index = 0; $index -lt $Left.Count; $index++) {
        if ([string]$Left[$index] -cne [string]$Right[$index]) {
            return $false
        }
    }
    return $true
}

function Compare-ScenarioPair {
    param(
        [Parameter(Mandatory)] $Baseline,
        [Parameter(Mandatory)] $Candidate
    )

    $checks = [ordered]@{
        extracted_manual_export = $Baseline.extracted_manual_sha256 -ceq `
            $Candidate.extracted_manual_sha256
        final_manual_export = $Baseline.final_manual_sha256 -ceq `
            $Candidate.final_manual_sha256
        output_tree = $Baseline.output.sha256 -ceq $Candidate.output.sha256
        translation_terminal = $Baseline.project.translation_result_sha256 -ceq `
            $Candidate.project.translation_result_sha256
        publication_terminal = $Baseline.project.publication_result_sha256 -ceq `
            $Candidate.project.publication_result_sha256
        task_messages = Test-SequenceEqual -Left $Baseline.provider.user_message_sha256 `
            -Right $Candidate.provider.user_message_sha256
        system_message = Test-SequenceEqual -Left $Baseline.provider.system_message_sha256 `
            -Right $Candidate.provider.system_message_sha256
        assistant_responses = Test-SequenceEqual `
            -Left $Baseline.provider.assistant_content_sha256 `
            -Right $Candidate.provider.assistant_content_sha256
    }
    $failedChecks = @($checks.GetEnumerator() | Where-Object { -not $_.Value } |
            ForEach-Object { $_.Key })
    if ($failedChecks.Count -ne 0) {
        throw "基线与候选结果不等价：$($Baseline.sample)：$($failedChecks -join ', ')"
    }

    $stages = [ordered]@{}
    foreach ($entry in @(
            [pscustomobject]@{ name = 'Extract'; property = 'extract' },
            [pscustomobject]@{ name = 'Translate'; property = 'translate' },
            [pscustomobject]@{ name = 'WriteBack'; property = 'write_back' }
        )) {
        $baselineMs = [double]$Baseline.commands.($entry.property).duration_ms
        $candidateMs = [double]$Candidate.commands.($entry.property).duration_ms
        $improvement = if ($baselineMs -eq 0) {
            0.0
        }
        else {
            (($baselineMs - $candidateMs) / $baselineMs) * 100.0
        }
        $stages[$entry.name] = [pscustomobject][ordered]@{
            baseline_ms = [Math]::Round($baselineMs, 3)
            candidate_ms = [Math]::Round($candidateMs, 3)
            difference_ms = [Math]::Round($candidateMs - $baselineMs, 3)
            improvement_percent = [Math]::Round($improvement, 3)
        }
    }
    return [pscustomobject][ordered]@{
        sample = $Baseline.sample
        round = $Baseline.round
        scope = $Baseline.scope
        correctness = [pscustomobject]$checks
        stages = [pscustomobject]$stages
        assessment = $null
        baseline = $Baseline
        candidate = $Candidate
    }
}

function Invoke-PairedRound {
    param(
        [Parameter(Mandatory)] $Sample,
        [Parameter(Mandatory)] [int]$Round,
        [Parameter(Mandatory)] [string]$Scope,
        [Parameter(Mandatory)] [int]$ProviderPort,
        [Parameter(Mandatory)] [string]$ProviderMetrics
    )

    $order = if (($Round % 2) -eq 1) {
        @('baseline', 'candidate')
    }
    else {
        @('candidate', 'baseline')
    }
    Write-Host "开始 $($Sample.id) 第 $Round 对，顺序：$($order -join ' -> ')"
    $scenarios = @{}
    foreach ($variant in $order) {
        $executable = if ($variant -eq 'baseline') { $BaselineExe } else { $CandidateExe }
        $scenarios[$variant] = Invoke-Scenario -Sample $Sample -Variant $variant -Round $Round `
            -Scope $Scope -Executable $executable -ProviderPort $ProviderPort `
            -ProviderMetrics $ProviderMetrics
    }

    try {
        $pair = Compare-ScenarioPair -Baseline $scenarios.baseline -Candidate $scenarios.candidate
    }
    catch {
        Write-Warning '正确性检查失败，保留两个场景工作区供调查。'
        throw
    }
    if (-not $KeepWorkspaces) {
        Remove-ScenarioWorkspace -Path $scenarios.baseline.workspace
        Remove-ScenarioWorkspace -Path $scenarios.candidate.workspace
    }
    return $pair
}

function Get-Median {
    param([double[]]$Values)
    $sorted = @($Values | Sort-Object)
    if ($sorted.Count -eq 0) {
        return [double]::NaN
    }
    $middle = [int][Math]::Floor($sorted.Count / 2)
    if (($sorted.Count % 2) -eq 1) {
        return [double]$sorted[$middle]
    }
    return ([double]$sorted[$middle - 1] + [double]$sorted[$middle]) / 2.0
}

function Get-PrimaryDecision {
    param([Parameter(Mandatory)] [object[]]$Pairs)

    if ($Pairs.Count -lt 2) {
        return [pscustomobject]@{ kind = 'insufficient'; reason = '主样本不足两轮' }
    }
    $stageNames = @('Extract', 'Translate', 'WriteBack')
    foreach ($stage in $stageNames) {
        $candidate = @($Pairs | ForEach-Object { [double]$_.stages.$stage.candidate_ms })
        if ($candidate | Where-Object { $_ -gt 15000 }) {
            return [pscustomobject]@{
                kind = 'reject'
                reason = "$stage 存在超过 15 秒的候选观测，必须先调查"
            }
        }
    }

    if ($Pairs.Count -eq 2) {
        foreach ($stage in $stageNames) {
            $candidate = @($Pairs | ForEach-Object { [double]$_.stages.$stage.candidate_ms })
            if (@($candidate | Where-Object { $_ -gt 10500 }).Count -eq 2) {
                return [pscustomobject]@{ kind = 'reject'; reason = "$stage 两轮都超过 10.5 秒" }
            }
            if (@($candidate | Where-Object { $_ -ge 9500 -and $_ -le 10500 }).Count -ne 0) {
                return [pscustomobject]@{ kind = 'ambiguous'; reason = "$stage 位于 10 秒保护带" }
            }
            if (@($candidate | Where-Object { $_ -le 9500 }).Count -eq 1 -and
                @($candidate | Where-Object { $_ -gt 10500 }).Count -eq 1) {
                return [pscustomobject]@{ kind = 'ambiguous'; reason = "$stage 两轮结果冲突" }
            }
        }

        $improvements = @($Pairs | ForEach-Object {
                [double]$_.stages.$FocusStage.improvement_percent
            })
        $focusBaseline = @($Pairs | ForEach-Object {
                [double]$_.stages.$FocusStage.baseline_ms
            })
        $focusCandidate = @($Pairs | ForEach-Object {
                [double]$_.stages.$FocusStage.candidate_ms
            })
        $otherRegressions = @()
        foreach ($pair in $Pairs) {
            foreach ($stage in $stageNames | Where-Object { $_ -ne $FocusStage }) {
                $otherRegressions += -[double]$pair.stages.$stage.improvement_percent
            }
        }
        if (@($improvements | Where-Object { $_ -ge 10 }).Count -eq 2 -and
            @($otherRegressions | Where-Object { $_ -gt 3 }).Count -eq 0) {
            return [pscustomobject]@{ kind = 'keep'; reason = "$FocusStage 两轮改善至少 10%" }
        }
        if (@($focusBaseline | Where-Object { $_ -gt 10000 }).Count -eq 2 -and
            @($focusCandidate | Where-Object { $_ -le 9500 }).Count -eq 2) {
            return [pscustomobject]@{
                kind = 'keep'
                reason = "$FocusStage 两轮都从超过 10 秒降到 9.5 秒以内"
            }
        }
        if (@($improvements | Where-Object { $_ -lt 5 }).Count -eq 2 -or
            @($improvements | Where-Object { $_ -le 0 }).Count -eq 2) {
            return [pscustomobject]@{ kind = 'reject'; reason = "$FocusStage 收益不足" }
        }
        return [pscustomobject]@{ kind = 'ambiguous'; reason = "$FocusStage 收益贴线或冲突" }
    }

    foreach ($stage in $stageNames) {
        $candidate = [double[]]@($Pairs | ForEach-Object {
                [double]$_.stages.$stage.candidate_ms
            })
        if ((Get-Median -Values $candidate) -gt 10000) {
            return [pscustomobject]@{ kind = 'reject'; reason = "$stage 三轮中位数超过 10 秒" }
        }
    }
    $improvements = [double[]]@($Pairs | ForEach-Object {
            [double]$_.stages.$FocusStage.improvement_percent
        })
    $positive = @($improvements | Where-Object { $_ -gt 0 }).Count
    $median = Get-Median -Values $improvements
    if ($positive -ge 2 -and $median -ge 10) {
        return [pscustomobject]@{
            kind = 'keep'
            reason = "$FocusStage 至少两轮改善且中位数达到 10%"
        }
    }
    return [pscustomobject]@{ kind = 'reject'; reason = "$FocusStage 三轮证据不足以保留候选" }
}

function Test-AuxiliaryPair {
    param([Parameter(Mandatory)] $Pair)

    $warnings = [Collections.Generic.List[string]]::new()
    foreach ($stage in @('Extract', 'Translate', 'WriteBack')) {
        $value = $Pair.stages.$stage
        if ([double]$value.candidate_ms -gt 10000) {
            return [pscustomobject][ordered]@{
                kind = 'reject'
                reason = "$stage 超过 10 秒"
                noise_band_ms = 0
                noise_evidence = 'absolute_target'
                confidence = 'high'
                warnings = @($warnings)
            }
        }
        $regression = -[double]$value.improvement_percent
        if ($regression -gt 5 -and [double]$value.difference_ms -gt 500) {
            return [pscustomobject]@{
                kind = 'reject'
                reason = "$stage 回退超过 5% 且绝对差超过 0.5 秒"
                noise_band_ms = 500
                noise_evidence = 'none'
                confidence = 'low'
                warnings = @($warnings)
            }
        }
        if ($regression -gt 5) {
            $warnings.Add(
                "$stage 相对回退 $([Math]::Round($regression, 3))%，绝对差 $($value.difference_ms) ms，未超过 0.5 秒保护带"
            )
        }
    }
    return [pscustomobject][ordered]@{
        kind = 'pass'
        reason = if ($warnings.Count -eq 0) {
            '结果等价且未出现明确回退'
        }
        else {
            '存在相对回退警告，但绝对差没有越过 0.5 秒保护带'
        }
        noise_band_ms = 500
        noise_evidence = 'none'
        confidence = 'low'
        warnings = @($warnings)
    }
}

function Format-ReportNumber {
    param([Parameter(Mandatory)] [double]$Value)
    return $Value.ToString('0.###', [Globalization.CultureInfo]::InvariantCulture)
}

function Write-ResultFiles {
    param([Parameter(Mandatory)] $State)

    [IO.Directory]::CreateDirectory($RunRoot) | Out-Null
    $json = $State | ConvertTo-Json -Depth 40
    [IO.File]::WriteAllText((Join-Path $RunRoot 'raw-results.json'), $json, $Utf8NoBom)

    $lines = [Collections.Generic.List[string]]::new()
    $lines.Add('# ATT 性能测量报告')
    $lines.Add('')
    $lines.Add("- 状态：$($State.status)")
    $lines.Add("- 结论：$($State.decision.kind)；$($State.decision.reason)")
    $lines.Add("- 关注阶段：$FocusStage")
    $lines.Add("- 实际耗时：$([Math]::Round($OverallClock.Elapsed.TotalMinutes, 2)) 分钟")
    $lines.Add("- 预算：$BudgetMinutes 分钟")
    if ($State.preflight.configuration.identical_executables) {
        $lines.Add('- 制品关系：SHA-256 相同，本报告只验证测试框架，不评价性能')
    }
    if (-not [string]::IsNullOrWhiteSpace([string]$State.error)) {
        $lines.Add('- 运行失败详情：见 `raw-results.json` 的 `error` 字段')
    }
    $lines.Add('')
    $lines.Add('## 制品与环境')
    $lines.Add('')
    $lines.Add("- 基线：$($State.preflight.baseline.version)，$($State.preflight.baseline.path)，$($State.preflight.baseline.sha256)")
    $lines.Add("- 候选：$($State.preflight.candidate.version)，$($State.preflight.candidate.path)，$($State.preflight.candidate.sha256)")
    $lines.Add("- Node：$($State.preflight.node.version)，$($State.preflight.node.path)")
    $lines.Add("- 系统：$($State.preflight.environment.os)")
    $lines.Add("- CPU：$($State.preflight.environment.cpu)，逻辑核心 $($State.preflight.environment.logical_cores)")
    if ($null -ne $State.preflight.environment.total_memory_bytes) {
        $memoryGiB = [double]$State.preflight.environment.total_memory_bytes / 1GB
        $lines.Add("- 可见内存：$(Format-ReportNumber $memoryGiB) GiB")
    }
    $lines.Add("- PowerShell：$($State.preflight.environment.powershell)")
    if ($State.preflight.toolchain.available) {
        $lines.Add("- Rust：$($State.preflight.toolchain.rustc_summary)；$($State.preflight.toolchain.cargo)；$($State.preflight.toolchain.active_toolchain)")
    }
    else {
        $lines.Add("- Rust：未能记录；$($State.preflight.toolchain.reason)")
    }
    $calibration = if ($State.preflight.environment.calibrated) { '完整' } else { '不完整，不能作为以后同一基线的身份记录复用' }
    $lines.Add("- 环境身份：$calibration")
    $lines.Add("- 仓库提交：$($State.preflight.git_commit)；工作区记录 $(@($State.preflight.git_status).Count) 项差异")
    $lines.Add("- 缓存处理：$($State.preflight.configuration.cache_policy)")
    $lines.Add('')
    $lines.Add('### 存储介质')
    $lines.Add('')
    $lines.Add('| 用途 | 卷 | 磁盘 | 介质 | 总线 | 容量 GiB |')
    $lines.Add('| --- | --- | --- | --- | --- | ---: |')
    foreach ($storage in @($State.preflight.storage)) {
        if ($storage.available) {
            $storageGiB = [double]$storage.size_bytes / 1GB
            $lines.Add("| $($storage.purpose) | $($storage.root) | $($storage.name) | $($storage.media_type) | $($storage.bus_type) | $(Format-ReportNumber $storageGiB) |")
        }
        else {
            $lines.Add("| $($storage.purpose) | $($storage.root) | 无法读取：$($storage.reason) | - | - | - |")
        }
    }
    $lines.Add('')
    $lines.Add('## 样本身份')
    $lines.Add('')
    $lines.Add('| 样本 | 文件数 | 大小 GiB | SHA-256 |')
    $lines.Add('| --- | ---: | ---: | --- |')
    foreach ($property in $State.preflight.source_identities.PSObject.Properties) {
        $identity = $property.Value
        $sizeGiB = [double]$identity.bytes / 1GB
        $lines.Add("| $($property.Name) | $($identity.files) | $(Format-ReportNumber $sizeGiB) | $($identity.sha256) |")
    }
    $lines.Add('')
    $lines.Add('### 样本资产')
    $lines.Add('')
    $lines.Add("- 样本清单：$($State.preflight.manifest_sha256)")
    $lines.Add("- 本地 Provider：$($State.preflight.provider_sha256)")
    $lines.Add('')
    $lines.Add('| 样本 | 资产 | 仓库路径 | SHA-256 |')
    $lines.Add('| --- | --- | --- | --- |')
    foreach ($sampleProperty in $State.preflight.sample_asset_identities.PSObject.Properties) {
        foreach ($assetProperty in $sampleProperty.Value.PSObject.Properties) {
            $asset = $assetProperty.Value
            $relativeAsset = [IO.Path]::GetRelativePath($RepositoryRoot, $asset.path).Replace('\', '/')
            $lines.Add("| $($sampleProperty.Name) | $($assetProperty.Name) | $relativeAsset | $($asset.sha256) |")
        }
    }
    $lines.Add('')
    $lines.Add('## 原始成对结果')
    $lines.Add('')
    $validPairs = @($State.pairs | Where-Object {
            $null -ne $_ -and $null -ne $_.PSObject.Properties['stages']
        })
    if ($validPairs.Count -eq 0) {
        $lines.Add('未运行性能场景。')
    }
    else {
        $lines.Add('| 样本 | 范围 | 轮次 | 阶段 | 基线 ms | 候选 ms | 差值 ms | 改善 |')
        $lines.Add('| --- | --- | ---: | --- | ---: | ---: | ---: | ---: |')
    }
    foreach ($pair in $validPairs) {
        foreach ($stage in @('Extract', 'Translate', 'WriteBack')) {
            $value = $pair.stages.$stage
            $lines.Add("| $($pair.sample) | $($pair.scope) | $($pair.round) | $stage | $(Format-ReportNumber $value.baseline_ms) | $(Format-ReportNumber $value.candidate_ms) | $(Format-ReportNumber $value.difference_ms) | $(Format-ReportNumber $value.improvement_percent)% |")
        }
    }

    if ($validPairs.Count -ne 0) {
        $lines.Add('')
        $lines.Add('每一行对应同机、同样本、同轮次的一对新项目。不同游戏没有合并计算分位数。')
        $lines.Add('')
        $lines.Add('## 同样本统计')
        $lines.Add('')
        $lines.Add('| 样本 | 阶段 | 基线中位数 ms | 基线范围 ms | 候选中位数 ms | 候选范围 ms | 改善中位数 |')
        $lines.Add('| --- | --- | ---: | --- | ---: | --- | ---: |')
        $sampleIds = @($validPairs | ForEach-Object { $_.sample } | Sort-Object -Unique)
        foreach ($sampleId in $sampleIds) {
            $samplePairs = @($validPairs | Where-Object { $_.sample -ceq $sampleId })
            foreach ($stage in @('Extract', 'Translate', 'WriteBack')) {
                $baselineValues = [double[]]@($samplePairs | ForEach-Object {
                        [double]$_.stages.$stage.baseline_ms
                    })
                $candidateValues = [double[]]@($samplePairs | ForEach-Object {
                        [double]$_.stages.$stage.candidate_ms
                    })
                $improvementValues = [double[]]@($samplePairs | ForEach-Object {
                        [double]$_.stages.$stage.improvement_percent
                    })
                $baselineMeasure = $baselineValues | Measure-Object -Minimum -Maximum
                $candidateMeasure = $candidateValues | Measure-Object -Minimum -Maximum
                $baselineRange = "$(Format-ReportNumber $baselineMeasure.Minimum)–$(Format-ReportNumber $baselineMeasure.Maximum)"
                $candidateRange = "$(Format-ReportNumber $candidateMeasure.Minimum)–$(Format-ReportNumber $candidateMeasure.Maximum)"
                $lines.Add("| $sampleId | $stage | $(Format-ReportNumber (Get-Median $baselineValues)) | $baselineRange | $(Format-ReportNumber (Get-Median $candidateValues)) | $candidateRange | $(Format-ReportNumber (Get-Median $improvementValues))% |")
            }
        }

        $lines.Add('')
        $lines.Add('## 正确性检查')
        $lines.Add('')
        $lines.Add('| 样本 | 范围 | 轮次 | 通过项 | 结果 |')
        $lines.Add('| --- | --- | ---: | ---: | --- |')
        foreach ($pair in $validPairs) {
            $checks = @($pair.correctness.PSObject.Properties)
            $passed = @($checks | Where-Object { [bool]$_.Value }).Count
            $failed = @($checks | Where-Object { -not [bool]$_.Value } |
                    ForEach-Object { $_.Name })
            $result = if ($failed.Count -eq 0) { '通过' } else { $failed -join '、' }
            $lines.Add("| $($pair.sample) | $($pair.scope) | $($pair.round) | $passed/$($checks.Count) | $result |")
        }
        $lines.Add('')
        $lines.Add('检查项包括 Extract 后 Manual 导出、最终 Manual 导出、输出树、Translate 终态、WriteBack 终态、TaskBlock 消息、system message 和 Assistant 响应。')

        $auxiliaryPairs = @($validPairs | Where-Object { $_.scope -ceq 'regression' })
        if ($auxiliaryPairs.Count -ne 0) {
            $lines.Add('')
            $lines.Add('## 辅助样本判断')
            $lines.Add('')
            $lines.Add('| 样本 | 判断 | 原因 | 噪声证据 |')
            $lines.Add('| --- | --- | --- | --- |')
            foreach ($pair in $auxiliaryPairs) {
                $assessment = $pair.assessment
                $noise = if ($assessment.noise_evidence -ceq 'absolute_target') {
                    '绝对 10 秒目标，不使用噪声带'
                }
                elseif ($null -eq $assessment.noise_evidence -or
                    $assessment.noise_evidence -ceq 'none') {
                    "$($assessment.noise_band_ms) ms 固定保护带；没有历史 MAD，置信度低"
                }
                else {
                    [string]$assessment.noise_evidence
                }
                $warningText = @($assessment.warnings) -join '；'
                $reason = if ([string]::IsNullOrWhiteSpace($warningText)) {
                    [string]$assessment.reason
                }
                else {
                    "$($assessment.reason)；$warningText"
                }
                $lines.Add("| $($pair.sample) | $($assessment.kind) | $reason | $noise |")
            }
        }
    }
    [IO.File]::WriteAllLines((Join-Path $RunRoot 'report.md'), $lines, $Utf8NoBom)
}

if (-not (Test-PathUnderRoot -Path $RunRoot -Root $PerformanceRoot)) {
    throw "运行目录必须位于 $PerformanceRoot：$RunRoot"
}
if (Test-Path -LiteralPath $RunRoot) {
    throw "运行目录已存在，拒绝覆盖：$RunRoot"
}
foreach ($path in @($ManifestPath, $ProviderScript)) {
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "性能资源不存在：$path"
    }
}
Assert-NoReparsePoint -Path $TestRoot

$nodeCommand = Get-Command node -CommandType Application -ErrorAction Stop |
    Select-Object -First 1
$NodeExecutable = $nodeCommand.Source
$nodeVersion = ((& $NodeExecutable --version) -join '').Trim()
if ($LASTEXITCODE -ne 0) {
    throw '无法读取 Node 版本'
}
& $NodeExecutable --check $ProviderScript
if ($LASTEXITCODE -ne 0) {
    throw '本地 Provider 未通过 node --check'
}

$BaselineIdentity = Get-ExecutableIdentity -Executable $BaselineExe
$CandidateIdentity = Get-ExecutableIdentity -Executable $CandidateExe
$ExecutablesIdentical = $BaselineIdentity.sha256 -ceq $CandidateIdentity.sha256
if ($ExecutablesIdentical -and -not $AllowIdenticalExecutables) {
    throw '基线与候选的 SHA-256 相同，不能进行性能比较；测试框架自检时显式使用 -AllowIdenticalExecutables。'
}
$ToolchainIdentity = Get-ToolchainIdentity
$StorageIdentities = @(
    Get-StorageIdentity -Path $RunRoot -Purpose '运行工作区'
    Get-StorageIdentity -Path $TestRoot -Purpose '真实游戏样本'
    Get-StorageIdentity -Path $BaselineExe -Purpose '基线制品'
    Get-StorageIdentity -Path $CandidateExe -Purpose '候选制品'
)
$EnvironmentCalibrated = $ToolchainIdentity.available -and
    @($StorageIdentities | Where-Object { -not $_.available }).Count -eq 0

$manifest = Get-Content -Raw -LiteralPath $ManifestPath | ConvertFrom-Json
$catalog = @{}
foreach ($definition in $manifest.samples) {
    $sample = Resolve-Sample -Definition $definition
    if ($catalog.ContainsKey($sample.id)) {
        throw "样本 ID 重复：$($sample.id)"
    }
    $catalog[$sample.id] = $sample
}
if (-not $catalog.ContainsKey($PrimarySample)) {
    throw "未知主样本：$PrimarySample"
}
$selectedAdditional = [Collections.Generic.List[object]]::new()
$seenAdditional = [Collections.Generic.HashSet[string]]::new([StringComparer]::OrdinalIgnoreCase)
foreach ($sampleId in $AdditionalSamples) {
    if ($sampleId -ceq $PrimarySample) {
        continue
    }
    if (-not $catalog.ContainsKey($sampleId)) {
        throw "未知回归样本：$sampleId"
    }
    if (-not $seenAdditional.Add($sampleId)) {
        throw "回归样本重复：$sampleId"
    }
    $selectedAdditional.Add($catalog[$sampleId])
}
$primary = $catalog[$PrimarySample]

$selectedSources = @($primary) + @($selectedAdditional)
$sourceIdentities = [ordered]@{}
$sampleAssetIdentities = [ordered]@{}
foreach ($sample in $selectedSources) {
    $sourceRoot = if ($sample.kind -eq 'generic') { $sample.fixture } else { $sample.game_root }
    Write-Output "计算样本身份：$($sample.id)"
    $sourceIdentities[$sample.id] = Get-TreeIdentity -Root $sourceRoot

    $assets = [ordered]@{}
    foreach ($assetName in @('rules', 'dialogue_rules', 'terms', 'placeholders')) {
        $property = $sample.PSObject.Properties[$assetName]
        if ($null -eq $property -or $null -eq $property.Value) {
            continue
        }
        $assetPath = [string]$property.Value
        $assets[$assetName] = [pscustomobject][ordered]@{
            path = $assetPath
            sha256 = Get-FileSha256 -Path $assetPath
        }
    }
    $sampleAssetIdentities[$sample.id] = [pscustomobject]$assets
}

$cpuName = [Runtime.InteropServices.RuntimeInformation]::ProcessArchitecture.ToString()
$osName = [Runtime.InteropServices.RuntimeInformation]::OSDescription
$totalMemory = $null
try {
    $processor = Get-CimInstance Win32_Processor | Select-Object -First 1
    $operatingSystem = Get-CimInstance Win32_OperatingSystem
    $cpuName = $processor.Name.Trim()
    $totalMemory = [long]$operatingSystem.TotalVisibleMemorySize * 1KB
}
catch {
    Write-Warning "无法读取完整 CIM 环境信息：$($_.Exception.Message)"
}

$gitCommit = ((& git -C $RepositoryRoot rev-parse HEAD) -join '').Trim()
$gitStatus = @(& git -C $RepositoryRoot status --porcelain=v1)
$preflight = [pscustomobject][ordered]@{
    recorded_at = [DateTimeOffset]::UtcNow.ToString('O')
    repository = $RepositoryRoot
    git_commit = $gitCommit
    git_status = $gitStatus
    baseline = $BaselineIdentity
    candidate = $CandidateIdentity
    toolchain = $ToolchainIdentity
    storage = $StorageIdentities
    node = [pscustomobject][ordered]@{
        path = $NodeExecutable
        version = $nodeVersion
        sha256 = Get-FileSha256 -Path $NodeExecutable
    }
    environment = [pscustomobject][ordered]@{
        os = $osName
        cpu = $cpuName
        logical_cores = [Environment]::ProcessorCount
        total_memory_bytes = $totalMemory
        powershell = $PSVersionTable.PSVersion.ToString()
        calibrated = $EnvironmentCalibrated
    }
    configuration = [pscustomobject][ordered]@{
        target_language = 'zh-Hans'
        target_task_characters = $TargetTaskCharacters
        generic_copies = $GenericCopies
        translation_profile = 'performance'
        provider = 'deterministic-local-chat-completions'
        max_concurrent_requests = 16
        build_profile = 'release-msvc'
        record_translation_tasks = $true
        identical_executables = $ExecutablesIdentical
        allow_identical_executables = [bool]$AllowIdenticalExecutables
        cache_policy = '每个场景使用新的发行根、项目、数据库和输出；不主动清空 Windows 文件缓存，主样本按 AB/BA 交错。'
    }
    primary_sample = $primary.id
    additional_samples = @($selectedAdditional | ForEach-Object { $_.id })
    source_identities = [pscustomobject]$sourceIdentities
    sample_asset_identities = [pscustomobject]$sampleAssetIdentities
    manifest_sha256 = Get-FileSha256 -Path $ManifestPath
    provider_sha256 = Get-FileSha256 -Path $ProviderScript
}

[IO.Directory]::CreateDirectory($RunRoot) | Out-Null
[IO.Directory]::CreateDirectory($WorkRoot) | Out-Null
[IO.Directory]::CreateDirectory($EvidenceRoot) | Out-Null
$state = [pscustomobject][ordered]@{
    status = 'preflight'
    decision = [pscustomobject]@{ kind = 'not_started'; reason = '尚未开始性能场景' }
    preflight = $preflight
    pairs = [Collections.Generic.List[object]]::new()
    error = $null
}

if ($PreflightOnly) {
    $state.status = 'preflight_complete'
    $state.decision = [pscustomobject]@{ kind = 'not_started'; reason = '调用方选择 PreflightOnly' }
    Write-ResultFiles -State $state
    Write-Output "性能预检完成：$RunRoot"
    return
}

$providerMetrics = Join-Path $RunRoot 'provider-metrics.jsonl'
$providerReady = Join-Path $RunRoot 'provider-ready.json'
$provider = $null
try {
    $provider = Start-LocalProvider -NodeExecutable $NodeExecutable `
        -MetricsFile $providerMetrics -ReadyFile $providerReady
    $state.status = 'running'

    $primaryPairDurations = [Collections.Generic.List[double]]::new()
    for ($round = 1; $round -le 2; $round++) {
        if ($round -gt 1 -and $primaryPairDurations.Count -gt 0) {
            $estimate = Get-Median -Values ([double[]]$primaryPairDurations)
            $remaining = ($BudgetMinutes * 60000) - $OverallClock.Elapsed.TotalMilliseconds
            Write-Output "预计下一对约 $([Math]::Round($estimate / 1000, 1)) 秒；预算剩余 $([Math]::Round($remaining / 1000, 1)) 秒。"
            if ($estimate -gt $remaining) {
                $state.decision = [pscustomobject]@{
                    kind = 'insufficient'
                    reason = '预计下一对会超过预算'
                }
                break
            }
        }
        $pairClock = [Diagnostics.Stopwatch]::StartNew()
        $pair = Invoke-PairedRound -Sample $primary -Round $round -Scope 'primary' `
            -ProviderPort $provider.port -ProviderMetrics $providerMetrics
        $pairClock.Stop()
        $primaryPairDurations.Add($pairClock.Elapsed.TotalMilliseconds)
        $state.pairs.Add($pair)
    }

    $primaryPairs = @($state.pairs | Where-Object { $_.sample -ceq $primary.id })
    $decision = Get-PrimaryDecision -Pairs $primaryPairs
    if ($ExecutablesIdentical) {
        $decision = [pscustomobject]@{
            kind = 'validation_only'
            reason = '基线与候选哈希相同；本次只验证测试框架和结果等价性'
        }
    }
    elseif ($decision.kind -eq 'ambiguous') {
        $twoRoundReason = $decision.reason
        if ($MaximumPrimaryRounds -eq 3) {
            $estimate = Get-Median -Values ([double[]]$primaryPairDurations)
            $remaining = ($BudgetMinutes * 60000) - $OverallClock.Elapsed.TotalMilliseconds
            if ($estimate -le $remaining) {
                Write-Output "前两轮结果不明确，按指南补第三轮。预计 $([Math]::Round($estimate / 1000, 1)) 秒。"
                $pair = Invoke-PairedRound -Sample $primary -Round 3 -Scope 'primary' `
                    -ProviderPort $provider.port -ProviderMetrics $providerMetrics
                $state.pairs.Add($pair)
                $primaryPairs = @($state.pairs | Where-Object { $_.sample -ceq $primary.id })
                $decision = Get-PrimaryDecision -Pairs $primaryPairs
                $decision.reason = "$($decision.reason)；前两轮判断：$twoRoundReason"
            }
            else {
                $decision = [pscustomobject]@{
                    kind = 'insufficient'
                    reason = "需要第三轮，但预计会超过预算；前两轮判断：$twoRoundReason"
                }
            }
        }
        else {
            $decision = [pscustomobject]@{
                kind = 'insufficient'
                reason = "前两轮需要第三轮确认，但 MaximumPrimaryRounds 为 2；前两轮判断：$twoRoundReason"
            }
        }
    }
    $state.decision = $decision

    if ($decision.kind -eq 'keep') {
        $auxiliaryRound = 1
        $auxiliaryEstimate = if ($primaryPairDurations.Count -eq 0) {
            0
        }
        else {
            Get-Median -Values ([double[]]$primaryPairDurations)
        }
        foreach ($sample in $selectedAdditional) {
            $remaining = ($BudgetMinutes * 60000) - $OverallClock.Elapsed.TotalMilliseconds
            Write-Output "预计回归样本 $($sample.id) 一对约 $([Math]::Round($auxiliaryEstimate / 1000, 1)) 秒；预算剩余 $([Math]::Round($remaining / 1000, 1)) 秒。"
            if ($auxiliaryEstimate -gt $remaining) {
                $state.decision = [pscustomobject]@{
                    kind = 'insufficient'
                    reason = "回归样本 $($sample.id) 预计会超过预算"
                }
                break
            }
            $auxiliaryClock = [Diagnostics.Stopwatch]::StartNew()
            $pair = Invoke-PairedRound -Sample $sample -Round $auxiliaryRound `
                -Scope 'regression' -ProviderPort $provider.port -ProviderMetrics $providerMetrics
            $auxiliaryClock.Stop()
            $auxiliaryEstimate = $auxiliaryClock.Elapsed.TotalMilliseconds
            $regression = Test-AuxiliaryPair -Pair $pair
            $pair.assessment = $regression
            $state.pairs.Add($pair)
            if ($regression.kind -eq 'reject') {
                $state.decision = [pscustomobject]@{
                    kind = 'reject'
                    reason = "回归样本 $($sample.id)：$($regression.reason)"
                }
                break
            }
        }
    }

    $state.status = if ($state.decision.kind -in @('keep', 'validation_only')) {
        'complete'
    }
    else {
        'stopped'
    }
}
catch {
    $state.status = 'failed'
    $state.decision = [pscustomobject]@{ kind = 'reject'; reason = '正确性或运行失败' }
    $state.error = $_.Exception.ToString()
    throw
}
finally {
    Stop-LocalProvider -Provider $provider -OutputDirectory $RunRoot
    $OverallClock.Stop()
    try {
        Write-ResultFiles -State $state
    }
    catch {
        if ($state.status -ne 'failed') {
            throw
        }
        Write-Warning "失败报告写入也失败：$($_.Exception.Message)"
    }
}

Write-Output "性能测量完成：$RunRoot"
Write-Output "结论：$($state.decision.kind)；$($state.decision.reason)"
