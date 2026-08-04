<#
.SYNOPSIS
验证当前 dist 是否可以作为公开 Windows x64 发行物。

.DESCRIPTION
检查发行资源、目录边界、空项目目录、Markdown 相对链接、PE 动态依赖，以及从仓库外
运行 Version、Generic Init、Extract 和零工作量 Translate 的真实结果。

.PARAMETER ExpectedVersion
不带 v 前缀的三段版本号，例如 1.0.0。

.PARAMETER TargetRoot
可选的发行根；省略时验证仓库固定的 dist。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidatePattern('^\d+\.\d+\.\d+$')]
    [string]$ExpectedVersion,
    [string]$TargetRoot
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$repositoryRoot = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..')).Path
$distributionRoot = if ([string]::IsNullOrWhiteSpace($TargetRoot)) {
    Join-Path $repositoryRoot 'dist'
}
else {
    [System.IO.Path]::GetFullPath($TargetRoot)
}

function Assert-NoReparsePoint {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [switch]$Recurse
    )

    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push((Get-Item -LiteralPath $Path -Force).FullName)
    while ($pending.Count -gt 0) {
        $current = Get-Item -LiteralPath $pending.Pop() -Force
        if (($current.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "发行物包含 reparse point：$($current.FullName)"
        }
        if ($Recurse -and $current.PSIsContainer) {
            foreach ($child in Get-ChildItem -LiteralPath $current.FullName -Force) {
                $pending.Push($child.FullName)
            }
        }
    }
}

function Invoke-CapturedProcess {
    param(
        [Parameter(Mandatory)]
        [string]$FilePath,
        [Parameter(Mandatory)]
        [string[]]$Arguments,
        [Parameter(Mandatory)]
        [string]$WorkingDirectory
    )

    $startInfo = [System.Diagnostics.ProcessStartInfo]::new()
    $startInfo.FileName = $FilePath
    $startInfo.WorkingDirectory = $WorkingDirectory
    $startInfo.UseShellExecute = $false
    $startInfo.RedirectStandardOutput = $true
    $startInfo.RedirectStandardError = $true
    foreach ($argument in $Arguments) {
        $startInfo.ArgumentList.Add($argument)
    }

    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $startInfo
    if (-not $process.Start()) {
        throw "无法启动发行程序：$FilePath"
    }
    $stdout = $process.StandardOutput.ReadToEnd()
    $stderr = $process.StandardError.ReadToEnd()
    $process.WaitForExit()
    [pscustomobject]@{
        ExitCode = $process.ExitCode
        Stdout = $stdout
        Stderr = $stderr
    }
}

function Assert-SuccessfulCommand {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [Parameter(Mandatory)]
        [pscustomobject]$Result
    )

    if ($Result.ExitCode -ne 0) {
        throw "$Name 失败，退出码 $($Result.ExitCode)：`nstdout:`n$($Result.Stdout)`nstderr:`n$($Result.Stderr)"
    }
}

function Get-PeDependencies {
    param(
        [Parameter(Mandatory)]
        [string]$Executable
    )

    $tool = Get-Command llvm-objdump -ErrorAction SilentlyContinue
    if ($null -ne $tool) {
        $output = & $tool.Source -p $Executable 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "llvm-objdump 无法读取 PE 依赖：`n$($output -join "`n")"
        }
        return @(
            $output |
                Select-String -Pattern '^\s*DLL Name:\s*(?<name>\S+)\s*$' |
                ForEach-Object { $_.Matches[0].Groups['name'].Value }
        )
    }

    $tool = Get-Command dumpbin -ErrorAction SilentlyContinue
    if ($null -ne $tool) {
        $output = & $tool.Source /DEPENDENTS $Executable 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "dumpbin 无法读取 PE 依赖：`n$($output -join "`n")"
        }
        return @(
            $output |
                Select-String -Pattern '^\s*(?<name>[A-Za-z0-9._-]+\.dll)\s*$' |
                ForEach-Object { $_.Matches[0].Groups['name'].Value }
        )
    }

    throw '完整发行检查需要 llvm-objdump 或 dumpbin。'
}

function Test-AllowedSystemDependency {
    param(
        [Parameter(Mandatory)]
        [string]$Name
    )

    if ($Name -match '^(?i:api-ms-win-|ext-ms-win-)') {
        return $true
    }
    $allowed = [System.Collections.Generic.HashSet[string]]::new(
        [System.StringComparer]::OrdinalIgnoreCase
    )
    foreach ($systemDll in @(
            'advapi32.dll',
            'bcrypt.dll',
            'bcryptprimitives.dll',
            'crypt32.dll',
            'kernel32.dll',
            'ntdll.dll',
            'secur32.dll',
            'userenv.dll',
            'ws2_32.dll'
        )) {
        [void]$allowed.Add($systemDll)
    }
    $allowed.Contains($Name)
}

function Get-MarkdownRelativeTargets {
    param(
        [Parameter(Mandatory)]
        [string]$Content
    )

    $targets = [System.Collections.Generic.List[string]]::new()
    foreach ($match in [regex]::Matches(
            $Content,
            '(?m)!?\[[^\]\r\n]*\]\((?<target><[^>\r\n]+>|[^)\r\n]+)\)'
        )) {
        $targets.Add($match.Groups['target'].Value)
    }
    foreach ($match in [regex]::Matches(
            $Content,
            '(?m)^\s*\[[^\]\r\n]+\]:\s*(?<target><[^>\r\n]+>|\S+)'
        )) {
        $targets.Add($match.Groups['target'].Value)
    }
    $targets
}

function Assert-MarkdownLinks {
    $root = [System.IO.Path]::GetFullPath($distributionRoot).TrimEnd('\', '/')
    $prefix = $root + [System.IO.Path]::DirectorySeparatorChar
    $failures = [System.Collections.Generic.List[string]]::new()
    $markdownFiles = @(
        Get-Item -LiteralPath (Join-Path $distributionRoot 'README.md')
        Get-ChildItem -LiteralPath (Join-Path $distributionRoot 'docs') -Recurse -File -Filter '*.md'
        Get-ChildItem -LiteralPath (Join-Path $distributionRoot 'skills') -Recurse -File -Filter '*.md'
        Get-ChildItem -LiteralPath (Join-Path $distributionRoot 'licenses') -Recurse -File -Filter '*.md'
    )

    foreach ($file in $markdownFiles) {
        $content = Get-Content -Raw -LiteralPath $file.FullName
        foreach ($rawTarget in Get-MarkdownRelativeTargets -Content $content) {
            $target = $rawTarget.Trim()
            if ($target.StartsWith('<') -and $target.EndsWith('>')) {
                $target = $target.Substring(1, $target.Length - 2)
            }
            elseif ($target -match '^\S+\s+["'']') {
                $target = ($target -split '\s+', 2)[0]
            }
            if ([string]::IsNullOrWhiteSpace($target) -or $target.StartsWith('#')) {
                continue
            }
            $absoluteUri = $null
            if ([System.Uri]::TryCreate($target, [System.UriKind]::Absolute, [ref]$absoluteUri)) {
                continue
            }
            $pathPart = ($target -split '[?#]', 2)[0]
            if ([string]::IsNullOrWhiteSpace($pathPart)) {
                continue
            }
            $pathPart = [System.Uri]::UnescapeDataString($pathPart).Replace('/', '\')
            $candidate = [System.IO.Path]::GetFullPath((Join-Path $file.DirectoryName $pathPart))
            if (-not $candidate.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
                $failures.Add("链接越出发行根：$($file.FullName) -> $target")
            }
            elseif (-not (Test-Path -LiteralPath $candidate)) {
                $failures.Add("链接目标不存在：$($file.FullName) -> $target")
            }
        }
    }
    if ($failures.Count -gt 0) {
        throw "发行包相对链接检查失败：`n$($failures -join "`n")"
    }
}

if (-not (Test-Path -LiteralPath $distributionRoot -PathType Container)) {
    throw "发行目录不存在：$distributionRoot"
}
Assert-NoReparsePoint -Path $distributionRoot -Recurse

& (Join-Path $PSScriptRoot 'sync-dist-resources.ps1') -Check -TargetRoot $distributionRoot

foreach ($requiredFile in @('att.exe', 'config.toml', 'LICENSE', 'README.md')) {
    $path = Join-Path $distributionRoot $requiredFile
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "发行文件缺失：$path"
    }
}
foreach ($requiredDirectory in @('docs', 'licenses', 'projects', 'prompts', 'skills')) {
    $path = Join-Path $distributionRoot $requiredDirectory
    if (-not (Test-Path -LiteralPath $path -PathType Container)) {
        throw "发行目录缺失：$path"
    }
}

$allowedTopLevel = [System.Collections.Generic.HashSet[string]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($name in @(
        'att.exe',
        'config.toml',
        'LICENSE',
        'README.md',
        'docs',
        'licenses',
        'projects',
        'prompts',
        'skills'
    )) {
    [void]$allowedTopLevel.Add($name)
}
foreach ($item in Get-ChildItem -LiteralPath $distributionRoot -Force) {
    if (-not $allowedTopLevel.Contains($item.Name)) {
        throw "发行根包含未声明内容：$($item.FullName)"
    }
}

$projects = Join-Path $distributionRoot 'projects'
if (Get-ChildItem -LiteralPath $projects -Force | Select-Object -First 1) {
    throw "公开发行的 projects 目录必须为空：$projects"
}
if (Get-ChildItem -LiteralPath $distributionRoot -File -Filter '*.dll') {
    throw '静态 Release 不应在发行根携带 DLL。'
}

$executable = Join-Path $distributionRoot 'att.exe'
$dependencies = Get-PeDependencies -Executable $executable
$unexpectedDependencies = @($dependencies | Where-Object { -not (Test-AllowedSystemDependency $_) })
if ($unexpectedDependencies.Count -gt 0) {
    throw "att.exe 含有未声明的非系统动态依赖：$($unexpectedDependencies -join ', ')"
}

Assert-MarkdownLinks

$temporaryRoot = [System.IO.Path]::GetFullPath([System.IO.Path]::GetTempPath()).TrimEnd('\', '/')
$smokeRoot = Join-Path $temporaryRoot ('att-release-smoke-' + [System.Guid]::NewGuid().ToString('N'))
$smokeCwd = Join-Path $temporaryRoot ('att-release-cwd-' + [System.Guid]::NewGuid().ToString('N'))
$smokeInput = Join-Path $temporaryRoot ('att-release-input-' + [System.Guid]::NewGuid().ToString('N'))
$cleanupPaths = @($smokeRoot, $smokeCwd, $smokeInput)
try {
    New-Item -ItemType Directory -Path $smokeRoot, $smokeCwd, $smokeInput | Out-Null
    foreach ($item in Get-ChildItem -LiteralPath $distributionRoot -Force) {
        Copy-Item -LiteralPath $item.FullName -Destination $smokeRoot -Recurse -Force
    }
    $smokeExecutable = Join-Path $smokeRoot 'att.exe'

    $version = Invoke-CapturedProcess -FilePath $smokeExecutable -Arguments @('--version') `
        -WorkingDirectory $smokeCwd
    Assert-SuccessfulCommand -Name '仓库外 Version' -Result $version
    if ($version.Stdout.Trim() -cne "att $ExpectedVersion") {
        throw "Version 输出不符：expected=att $ExpectedVersion actual=$($version.Stdout.Trim())"
    }

    $init = Invoke-CapturedProcess -FilePath $smokeExecutable -WorkingDirectory $smokeCwd `
        -Arguments @(
            'generic', 'init', '--name', 'release-smoke', '--path', $smokeInput,
            '--source-language', 'ja', '--target-language', 'zh-Hans'
        )
    Assert-SuccessfulCommand -Name '仓库外 Generic Init' -Result $init

    $extract = Invoke-CapturedProcess -FilePath $smokeExecutable -WorkingDirectory $smokeCwd `
        -Arguments @('generic', 'extract', '--name', 'release-smoke')
    Assert-SuccessfulCommand -Name '仓库外 Generic Extract' -Result $extract

    $translate = Invoke-CapturedProcess -FilePath $smokeExecutable -WorkingDirectory $smokeCwd `
        -Arguments @('generic', 'translate', '--name', 'release-smoke', 'primary')
    Assert-SuccessfulCommand -Name '仓库外零工作量 Generic Translate' -Result $translate
}
finally {
    $prefix = $temporaryRoot + [System.IO.Path]::DirectorySeparatorChar
    foreach ($path in $cleanupPaths) {
        $candidate = [System.IO.Path]::GetFullPath($path)
        if (-not $candidate.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
            throw "拒绝清理临时目录之外的路径：$candidate"
        }
        if (Test-Path -LiteralPath $candidate) {
            Assert-NoReparsePoint -Path $candidate -Recurse
            Remove-Item -LiteralPath $candidate -Recurse -Force
        }
    }
}

Write-Output "ATT $ExpectedVersion 发行包通过完整检查。"
