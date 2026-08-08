<#
.SYNOPSIS
把仓库中的使用者资源同步到 dist，或只检查两边是否一致。

.DESCRIPTION
管理 README.md、LICENSE、config.example.toml -> config.toml、第三方许可证目录、docs、
prompts、skills 和固定的 Formic 工具。
不修改 att.exe。

.PARAMETER Check
只比较文件集合和 SHA-256，不修改 dist。

.PARAMETER TargetRoot
可选的发行根；省略时使用仓库固定的 dist。供公开发行验证在干净暂存目录中复用。

.PARAMETER VCRuntimePath
普通同步 Formic 时可选的 VCRUNTIME140.dll 来源；省略时使用 Windows System32。
#>
[CmdletBinding()]
param(
    [switch]$Check,
    [string]$TargetRoot,
    [string]$VCRuntimePath
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
        Destination = Join-Path $distributionRoot 'config.toml'
    }
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
            'config.example.toml',
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
foreach ($mapping in $fileMappings) {
    Assert-NoReparsePoint -Path $mapping.Source
}
foreach ($mapping in $directoryMappings) {
    Assert-NoReparsePoint -Path $mapping.Source -Recurse
}

if ($Check) {
    Test-SynchronizedResources
    & (Join-Path $PSScriptRoot 'sync-formic-tool.ps1') -Check -TargetRoot $distributionRoot
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
$formicSyncArguments = @{
    TargetRoot = $distributionRoot
}
if (-not [string]::IsNullOrWhiteSpace($VCRuntimePath)) {
    $formicSyncArguments.VCRuntimePath = $VCRuntimePath
}
& (Join-Path $PSScriptRoot 'sync-formic-tool.ps1') @formicSyncArguments
Write-Output '发行资源已同步，并通过逐文件 SHA-256 检查。'
