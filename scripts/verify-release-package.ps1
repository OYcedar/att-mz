#requires -Version 7.0

<#
.SYNOPSIS
检查公开 Windows x64 发行包的版本、启动能力、运行依赖和必需资源。

.PARAMETER ExpectedVersion
不带 v 前缀的三段版本号。

.PARAMETER TargetRoot
发行目录；省略时使用仓库的 dist。
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

$distributionRoot = if ([string]::IsNullOrWhiteSpace($TargetRoot)) {
    Join-Path $PSScriptRoot '..\dist'
}
else {
    [System.IO.Path]::GetFullPath($TargetRoot)
}

foreach ($relativePath in @(
        'att.exe', 'config.example.toml', 'config.toml', 'LICENSE', 'README.md',
        'licenses\THIRD-PARTY-LICENSES.html',
        'licenses\FORMIC-THIRD-PARTY-LICENSES.html',
        'tools\formic\formic.exe', 'tools\formic\LICENSE',
        'tools\formic\FORMIC-SOURCE.md', 'tools\formic\README.md',
        'tools\formic\config.example.toml', 'tools\formic\config.toml'
    )) {
    $path = Join-Path $distributionRoot $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Leaf) -or
        (Get-Item -LiteralPath $path).Length -eq 0) {
        throw "发行文件缺失或为空：$path"
    }
}
foreach ($relativePath in @('docs', 'prompts', 'skills', 'tools\formic\docs')) {
    $path = Join-Path $distributionRoot $relativePath
    if (-not (Test-Path -LiteralPath $path -PathType Container)) {
        throw "发行目录缺失：$path"
    }
}

$projects = Join-Path $distributionRoot 'projects'
if (Test-Path -LiteralPath $projects) {
    if (-not (Test-Path -LiteralPath $projects -PathType Container) -or
        (Get-ChildItem -LiteralPath $projects -Force | Select-Object -First 1)) {
        throw "公开发行包不能包含使用者项目：$projects"
    }
}

foreach ($relativePath in @(
        'config.example.toml', 'config.toml',
        'tools\formic\config.example.toml', 'tools\formic\config.toml'
    )) {
    $path = Join-Path $distributionRoot $relativePath
    $assignments = @(
        Get-Content -Encoding UTF8 -LiteralPath $path |
            Where-Object { $_ -match '^[ \t]*api_key[ \t]*=' }
    )
    if ($assignments.Count -ne 1 -or
        $assignments[0] -cnotmatch '^[ \t]*api_key[ \t]*=[ \t]*"replace-with-api-key"[ \t]*(?:#.*)?$') {
        throw "公开发行配置必须使用占位 API key：$path"
    }
}

function Assert-SystemDependencies {
    param([Parameter(Mandatory)][string]$Executable)

    $tool = Get-Command llvm-objdump -ErrorAction SilentlyContinue
    if ($null -ne $tool) {
        $output = & $tool.Source -p $Executable 2>&1
        $pattern = '^\s*DLL Name:\s*(?<name>\S+)\s*$'
    }
    else {
        $tool = Get-Command dumpbin -ErrorAction SilentlyContinue
        if ($null -eq $tool) {
            throw '运行依赖检查需要 llvm-objdump 或 dumpbin。'
        }
        $output = & $tool.Source /DEPENDENTS $Executable 2>&1
        $pattern = '^\s*(?<name>[A-Za-z0-9._-]+\.dll)\s*$'
    }
    if ($LASTEXITCODE -ne 0) {
        throw "无法读取程序运行依赖：$Executable"
    }
    $dependencies = @(
        $output | Select-String -Pattern $pattern |
            ForEach-Object { $_.Matches[0].Groups['name'].Value }
    )
    $systemDlls = @(
        'advapi32.dll', 'bcrypt.dll', 'bcryptprimitives.dll', 'crypt32.dll',
        'kernel32.dll', 'ntdll.dll', 'oleaut32.dll', 'secur32.dll',
        'userenv.dll', 'ws2_32.dll'
    )
    $unexpected = @(
        $dependencies | Where-Object {
            $_ -notmatch '^(api-ms-win-|ext-ms-win-)' -and $_ -notin $systemDlls
        }
    )
    if ($unexpected.Count -gt 0) {
        throw "程序依赖未随包提供的非系统 DLL：${Executable}；$($unexpected -join ', ')"
    }
}

Assert-SystemDependencies -Executable (Join-Path $distributionRoot 'att.exe')
Assert-SystemDependencies -Executable (Join-Path $distributionRoot 'tools\formic\formic.exe')

Push-Location $distributionRoot
try {
    $actualVersion = (& .\att.exe --version).Trim()
    if ($LASTEXITCODE -ne 0 -or $actualVersion -cne "att $ExpectedVersion") {
        throw "程序版本不符或无法启动：expected=att $ExpectedVersion actual=$actualVersion"
    }
    & .\tools\formic\formic.exe --help > $null
    if ($LASTEXITCODE -ne 0) {
        throw '随包 Formic 无法启动。'
    }
}
finally {
    Pop-Location
}

Write-Output "ATT $ExpectedVersion 发行包的版本、启动、依赖与必需资源检查通过。"
