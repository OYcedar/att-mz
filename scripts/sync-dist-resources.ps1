<#
.SYNOPSIS
把仓库中的使用者资源复制到发行目录。

.DESCRIPTION
管理 README.md、LICENSE、config.example.toml、第三方许可证目录、docs、prompts、skills
和随包 Formic。ATT 与 Formic 的 config.toml 都是使用者的活动配置：已有文件保持原字节
不变，缺失时才从各自的 config.example.toml 初始化。Formic 目录收敛到当前托管集合。
不修改 att.exe。

.PARAMETER TargetRoot
可选的发行根；省略时使用仓库固定的 dist。
#>
[CmdletBinding()]
param(
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

if (-not (Test-Path -LiteralPath $distributionRoot -PathType Container)) {
    throw "发行目录不存在：$distributionRoot"
}

$fileMappings = @(
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'README.md'
        Destination = Join-Path $distributionRoot 'README.md'
    },
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'LICENSE'
        Destination = Join-Path $distributionRoot 'LICENSE'
    },
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'config.example.toml'
        Destination = Join-Path $distributionRoot 'config.example.toml'
    }
)

$activeConfigDestination = Join-Path $distributionRoot 'config.toml'
$formicSource = Join-Path $repositoryRoot 'tools\formic'
$formicDestination = Join-Path $distributionRoot 'tools\formic'
$formicActiveConfigDestination = Join-Path $formicDestination 'config.toml'
$formicFileMappings = @(
    'config.example.toml',
    'FORMIC-SOURCE.md',
    'formic.exe',
    'LICENSE',
    'README.md'
)
$formicDirectoryMappings = @(
    'docs'
)

$directoryMappings = @(
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'licenses'
        Destination = Join-Path $distributionRoot 'licenses'
    },
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'docs'
        Destination = Join-Path $distributionRoot 'docs'
    },
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'prompts'
        Destination = Join-Path $distributionRoot 'prompts'
    },
    [pscustomobject]@{
        Source = Join-Path $repositoryRoot 'skills'
        Destination = Join-Path $distributionRoot 'skills'
    }
)

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
            throw "拒绝操作 reparse point：$($current.FullName)"
        }
        if ($Recurse -and $current.PSIsContainer) {
            foreach ($child in Get-ChildItem -LiteralPath $current.FullName -Force) {
                $pending.Push($child.FullName)
            }
        }
    }
}

function Assert-DistributionChild {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    Assert-NoReparsePoint -Path $distributionRoot
    $root = [System.IO.Path]::GetFullPath($distributionRoot).TrimEnd('\', '/')
    $candidate = [System.IO.Path]::GetFullPath($Path).TrimEnd('\', '/')
    $prefix = $root + [System.IO.Path]::DirectorySeparatorChar
    if (-not $candidate.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "拒绝操作发行目录之外的路径：$candidate"
    }
}

Assert-NoReparsePoint -Path $repositoryRoot
Assert-NoReparsePoint -Path $distributionRoot
Assert-NoReparsePoint -Path $activeConfigDestination
Assert-NoReparsePoint -Path (Join-Path $distributionRoot 'tools')
Assert-NoReparsePoint -Path $formicDestination -Recurse
Assert-NoReparsePoint -Path $formicSource -Recurse
foreach ($mapping in $fileMappings) {
    Assert-NoReparsePoint -Path $mapping.Source
}
foreach ($mapping in $directoryMappings) {
    Assert-NoReparsePoint -Path $mapping.Source -Recurse
}

$stagingRoot = Join-Path $distributionRoot '.resource-sync'
Assert-DistributionChild -Path $stagingRoot

if (Test-Path -LiteralPath $stagingRoot) {
    throw "发行资源同步无法开始：临时目录已存在：$stagingRoot。确认没有同步正在运行后删除该目录。"
}

New-Item -ItemType Directory -Path $stagingRoot | Out-Null
try {
    $stagedFormic = Join-Path $stagingRoot 'formic'
    New-Item -ItemType Directory -Path $stagedFormic | Out-Null
    foreach ($name in $formicFileMappings) {
        Copy-Item -LiteralPath (Join-Path $formicSource $name) `
            -Destination (Join-Path $stagedFormic $name)
    }
    foreach ($name in $formicDirectoryMappings) {
        Copy-Item -LiteralPath (Join-Path $formicSource $name) `
            -Destination $stagedFormic -Recurse -Force
    }

    foreach ($mapping in $fileMappings) {
        $staged = Join-Path $stagingRoot ([System.IO.Path]::GetFileName($mapping.Destination))
        Copy-Item -LiteralPath $mapping.Source -Destination $staged -Force
    }
    foreach ($mapping in $directoryMappings) {
        $staged = Join-Path $stagingRoot ([System.IO.Path]::GetFileName($mapping.Destination))
        Copy-Item -LiteralPath $mapping.Source -Destination $staged -Recurse -Force
    }

    foreach ($mapping in $fileMappings) {
        Assert-DistributionChild -Path $mapping.Destination
        Assert-NoReparsePoint -Path $mapping.Destination
        $staged = Join-Path $stagingRoot ([System.IO.Path]::GetFileName($mapping.Destination))
        Copy-Item -LiteralPath $staged -Destination $mapping.Destination -Force
    }
    if (-not (Test-Path -LiteralPath $activeConfigDestination)) {
        Copy-Item -LiteralPath (Join-Path $stagingRoot 'config.example.toml') `
            -Destination $activeConfigDestination
    }
    elseif (-not (Test-Path -LiteralPath $activeConfigDestination -PathType Leaf)) {
        throw "ATT 活动配置不是普通文件：$activeConfigDestination"
    }

    Assert-DistributionChild -Path $formicDestination
    if (-not (Test-Path -LiteralPath $formicDestination -PathType Container)) {
        New-Item -ItemType Directory -Path $formicDestination | Out-Null
    }
    $currentFormicItems = @(
        $formicFileMappings + $formicDirectoryMappings + 'config.toml'
    )
    foreach ($item in Get-ChildItem -LiteralPath $formicDestination -Force) {
        if ($item.Name -notin $currentFormicItems) {
            Assert-DistributionChild -Path $item.FullName
            if ($item.PSIsContainer) {
                Assert-NoReparsePoint -Path $item.FullName -Recurse
                Remove-Item -LiteralPath $item.FullName -Recurse -Force
            }
            else {
                Remove-Item -LiteralPath $item.FullName -Force
            }
        }
    }
    foreach ($name in $formicFileMappings) {
        Copy-Item -LiteralPath (Join-Path $stagedFormic $name) `
            -Destination (Join-Path $formicDestination $name) -Force
    }
    foreach ($name in $formicDirectoryMappings) {
        $destination = Join-Path $formicDestination $name
        Assert-DistributionChild -Path $destination
        if (Test-Path -LiteralPath $destination) {
            Assert-NoReparsePoint -Path $destination -Recurse
            Remove-Item -LiteralPath $destination -Recurse -Force
        }
        Move-Item -LiteralPath (Join-Path $stagedFormic $name) -Destination $destination
    }
    if (-not (Test-Path -LiteralPath $formicActiveConfigDestination)) {
        Copy-Item -LiteralPath (Join-Path $stagedFormic 'config.example.toml') `
            -Destination $formicActiveConfigDestination
    }
    elseif (-not (Test-Path -LiteralPath $formicActiveConfigDestination -PathType Leaf)) {
        throw "Formic 活动配置不是普通文件：$formicActiveConfigDestination"
    }
    foreach ($mapping in $directoryMappings) {
        Assert-DistributionChild -Path $mapping.Destination
        if (Test-Path -LiteralPath $mapping.Destination) {
            Assert-NoReparsePoint -Path $mapping.Destination -Recurse
            Remove-Item -LiteralPath $mapping.Destination -Recurse -Force
        }
        $staged = Join-Path $stagingRoot ([System.IO.Path]::GetFileName($mapping.Destination))
        Move-Item -LiteralPath $staged -Destination $mapping.Destination
    }
}
finally {
    if (Test-Path -LiteralPath $stagingRoot) {
        Assert-DistributionChild -Path $stagingRoot
        Assert-NoReparsePoint -Path $stagingRoot -Recurse
        Remove-Item -LiteralPath $stagingRoot -Recurse -Force
    }
}

Write-Output '发行资源已复制，已有活动配置保持原样。'
