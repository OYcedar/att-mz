<#
.SYNOPSIS
把仓库中的使用者资源同步到 dist，或只检查两边是否一致。

.DESCRIPTION
管理 README.md、LICENSE、config.example.toml、第三方许可证目录、docs、prompts、skills
和随包 Formic。ATT 与 Formic 的 config.toml 都是使用者的活动配置：已有文件保持原字节
不变，缺失时才从各自的 config.example.toml 初始化。
不修改 att.exe。

.PARAMETER Check
只比较文件集合和 SHA-256，不修改 dist。

.PARAMETER TargetRoot
可选的发行根；省略时使用仓库固定的 dist。供公开发行验证在干净暂存目录中复用。

.PARAMETER RequireDefaultConfig
要求 ATT 与 Formic 的活动配置和无密钥发行模板完全一致。公开发行检查使用此开关；普通
本地同步和检查不使用。
#>
[CmdletBinding()]
param(
    [switch]$Check,
    [switch]$RequireDefaultConfig,
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

$defaultConfigSource = Join-Path $repositoryRoot 'config.example.toml'
$activeConfigDestination = Join-Path $distributionRoot 'config.toml'
$formicSource = Join-Path $repositoryRoot 'tools\formic'
$formicDestination = Join-Path $distributionRoot 'tools\formic'
$formicDefaultConfigSource = Join-Path $formicSource 'config.example.toml'
$formicActiveConfigDestination = Join-Path $formicDestination 'config.toml'
$formicFileMappings = @(
    'config.example.toml',
    'FORMIC-SOURCE.md',
    'formic.exe',
    'LICENSE',
    'VCRUNTIME140.dll'
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

function Get-FileDigest {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash
}

function Get-DirectoryDigestMap {
    param(
        [Parameter(Mandatory)]
        [string]$Root
    )

    Assert-NoReparsePoint -Path $Root -Recurse
    $resolvedRoot = (Resolve-Path -LiteralPath $Root).Path
    $relativePrefixLength = $resolvedRoot.TrimEnd('\', '/').Length
    $result = @{}
    foreach ($file in Get-ChildItem -LiteralPath $resolvedRoot -Recurse -File -Force) {
        $relative = $file.FullName.Substring($relativePrefixLength).TrimStart('\', '/')
        $relative = $relative.Replace('\', '/')
        $result[$relative] = Get-FileDigest -Path $file.FullName
    }
    return $result
}

function Assert-PlaceholderApiKey {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [Parameter(Mandatory)]
        [string]$Description
    )

    $content = Get-Content -Raw -LiteralPath $Path
    $assignments = @(
        $content -split '\r?\n' |
            Where-Object { $_ -match '^[ \t]*api_key[ \t]*=' }
    )
    if (
        $assignments.Count -ne 1 -or
        $assignments[0] -cnotmatch '^[ \t]*api_key[ \t]*=[ \t]*"replace-with-api-key"[ \t]*(?:#.*)?$'
    ) {
        throw "$Description 必须恰好包含一个固定占位 api_key：$Path"
    }
}

function Test-SynchronizedResources {
    $failures = [System.Collections.Generic.List[string]]::new()

    foreach ($mapping in $fileMappings) {
        if (-not (Test-Path -LiteralPath $mapping.Destination -PathType Leaf)) {
            $failures.Add("发行文件缺失：$($mapping.Destination)")
            continue
        }
        if ((Get-FileDigest -Path $mapping.Source) -ne
            (Get-FileDigest -Path $mapping.Destination)) {
            $failures.Add(
                "发行文件与源码不同：$($mapping.Source) -> $($mapping.Destination)"
            )
        }
    }

    if (-not (Test-Path -LiteralPath $activeConfigDestination -PathType Leaf)) {
        $failures.Add("ATT 活动配置缺失或不是普通文件：$activeConfigDestination")
    }
    elseif ($RequireDefaultConfig -and
        (Get-FileDigest -Path $defaultConfigSource) -ne
        (Get-FileDigest -Path $activeConfigDestination)) {
        $failures.Add("公开发行的 ATT 活动配置必须与发行模板完全一致：$activeConfigDestination")
    }

    if (-not (Test-Path -LiteralPath $formicDestination -PathType Container)) {
        $failures.Add("Formic 发行目录缺失：$formicDestination")
    }
    else {
        $expectedFormicFiles = @($formicFileMappings + 'config.toml') | Sort-Object
        $actualFormicItems = @(Get-ChildItem -LiteralPath $formicDestination -Force)
        $actualFormicFiles = @($actualFormicItems | Where-Object { -not $_.PSIsContainer } |
                ForEach-Object Name | Sort-Object)
        if ($actualFormicItems.Count -ne $actualFormicFiles.Count -or
            ($expectedFormicFiles -join "`n") -cne ($actualFormicFiles -join "`n")) {
            $failures.Add("Formic 发行目录文件集合不正确：$formicDestination")
        }
        foreach ($name in $formicFileMappings) {
            $source = Join-Path $formicSource $name
            $destination = Join-Path $formicDestination $name
            if (-not (Test-Path -LiteralPath $destination -PathType Leaf)) {
                $failures.Add("Formic 发行文件缺失：$destination")
            }
            elseif ((Get-FileDigest -Path $source) -ne (Get-FileDigest -Path $destination)) {
                $failures.Add("Formic 发行文件与源码不同：$destination")
            }
        }
    }

    if (-not (Test-Path -LiteralPath $formicActiveConfigDestination -PathType Leaf)) {
        $failures.Add("Formic 活动配置缺失或不是普通文件：$formicActiveConfigDestination")
    }
    elseif ($RequireDefaultConfig -and
        (Get-FileDigest -Path $formicDefaultConfigSource) -ne
        (Get-FileDigest -Path $formicActiveConfigDestination)) {
        $failures.Add("公开发行的 Formic 活动配置必须与发行模板完全一致：$formicActiveConfigDestination")
    }

    foreach ($mapping in $directoryMappings) {
        if (-not (Test-Path -LiteralPath $mapping.Destination -PathType Container)) {
            $failures.Add("发行目录缺失：$($mapping.Destination)")
            continue
        }

        $sourceFiles = Get-DirectoryDigestMap -Root $mapping.Source
        $distributionFiles = Get-DirectoryDigestMap -Root $mapping.Destination
        $allRelativePaths = @($sourceFiles.Keys + $distributionFiles.Keys) |
            Sort-Object -Unique

        foreach ($relativePath in $allRelativePaths) {
            if (-not $sourceFiles.ContainsKey($relativePath)) {
                $failures.Add("发行目录包含源码已不存在的文件：$($mapping.Destination)\$relativePath")
            }
            elseif (-not $distributionFiles.ContainsKey($relativePath)) {
                $failures.Add("发行目录缺少文件：$($mapping.Destination)\$relativePath")
            }
            elseif ($sourceFiles[$relativePath] -ne $distributionFiles[$relativePath]) {
                $failures.Add("发行文件与源码不同：$($mapping.Destination)\$relativePath")
            }
        }
    }

    foreach ($forbidden in @(
            'AGENTS.md',
            'maintenance',
            'src',
            'tests',
            'target'
        )) {
        $path = Join-Path $distributionRoot $forbidden
        if (Test-Path -LiteralPath $path) {
            $failures.Add("发行目录包含开发材料：$path")
        }
    }

    if ($failures.Count -gt 0) {
        throw "发行资源检查失败：`n$($failures -join "`n")"
    }
}

Assert-NoReparsePoint -Path $repositoryRoot
Assert-NoReparsePoint -Path $distributionRoot -Recurse
Assert-PlaceholderApiKey -Path $defaultConfigSource -Description 'ATT 发行配置模板'
Assert-NoReparsePoint -Path $formicSource -Recurse
Assert-PlaceholderApiKey -Path $formicDefaultConfigSource -Description 'Formic 发行配置模板'
foreach ($mapping in $fileMappings) {
    Assert-NoReparsePoint -Path $mapping.Source
}
foreach ($mapping in $directoryMappings) {
    Assert-NoReparsePoint -Path $mapping.Source -Recurse
}

if ($Check) {
    Test-SynchronizedResources
    Write-Output '发行资源与源码一致。'
    return
}

$stagingRoot = Join-Path $distributionRoot '.resource-sync'
Assert-DistributionChild -Path $stagingRoot

if (Test-Path -LiteralPath $stagingRoot) {
    throw "发行资源同步无法开始：临时目录已存在：$stagingRoot。确认没有同步正在运行后删除该目录。"
}

try {
    New-Item -ItemType Directory -Path $stagingRoot | Out-Null

    $stagedFormic = Join-Path $stagingRoot 'formic'
    New-Item -ItemType Directory -Path $stagedFormic | Out-Null
    foreach ($name in $formicFileMappings) {
        Copy-Item -LiteralPath (Join-Path $formicSource $name) `
            -Destination (Join-Path $stagedFormic $name)
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
        $destinationDirectory = [System.IO.Path]::GetDirectoryName($mapping.Destination)
        if (-not $destinationDirectory.Equals(
                $distributionRoot,
                [System.StringComparison]::OrdinalIgnoreCase
            )) {
            Assert-DistributionChild -Path $destinationDirectory
        }
        if (-not (Test-Path -LiteralPath $destinationDirectory -PathType Container)) {
            New-Item -ItemType Directory -Path $destinationDirectory | Out-Null
        }
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
    foreach ($name in $formicFileMappings) {
        Copy-Item -LiteralPath (Join-Path $stagedFormic $name) `
            -Destination (Join-Path $formicDestination $name) -Force
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

Test-SynchronizedResources
Write-Output '发行资源已同步，并通过逐文件 SHA-256 检查。'
