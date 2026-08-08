<#
.SYNOPSIS
把固定的 Formic Release 安装到发行目录，或只检查已有安装。

.DESCRIPTION
普通同步读取仓库 tools/formic/release.json，下载并验证固定 ZIP，只安装清单明确允许的
上游文件，再加入 ATT 的包内说明、无密钥配置和 Microsoft Visual C++ Runtime。
-Check 不访问网络，只检查发行目录中的精确文件集合、摘要、配置、运行库元数据和签名。

.PARAMETER Check
只检查已有安装，不下载或修改文件。

.PARAMETER TargetRoot
可选的发行根；省略时使用仓库固定的 dist。

.PARAMETER VCRuntimePath
普通同步时可选的 VCRUNTIME140.dll 来源；省略时使用 Windows System32 中的同名文件。
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
$distributionRoot = (Resolve-Path -LiteralPath $distributionRoot).Path

$sourceRoot = Join-Path $repositoryRoot 'tools\formic'
$releaseManifestPath = Join-Path $sourceRoot 'release.json'
$formicRoot = Join-Path $distributionRoot 'tools\formic'
$supportFileNames = @('README.md', 'FORMIC-SOURCE.md', 'release.json')
$includedUpstreamNames = @('formic.exe', 'config.example.toml', 'LICENSE')
$expectedInstalledNames = @(
    'formic.exe',
    'config.example.toml',
    'config.toml',
    'LICENSE',
    'README.md',
    'FORMIC-SOURCE.md',
    'release.json',
    'VCRUNTIME140.dll',
    'runtime.json'
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

function Assert-TargetChild {
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

function Remove-TargetTree {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    Assert-TargetChild -Path $Path
    if (-not (Test-Path -LiteralPath $Path)) {
        return
    }
    Assert-NoReparsePoint -Path $Path -Recurse
    Remove-Item -LiteralPath $Path -Recurse -Force
}

function Get-FileSha256 {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    (Get-FileHash -Algorithm SHA256 -LiteralPath $Path).Hash.ToLowerInvariant()
}

function Get-StreamSha256 {
    param(
        [Parameter(Mandatory)]
        [System.IO.Stream]$Stream
    )

    $algorithm = [System.Security.Cryptography.SHA256]::Create()
    try {
        $digest = $algorithm.ComputeHash($Stream)
        ([System.BitConverter]::ToString($digest)).Replace('-', '').ToLowerInvariant()
    }
    finally {
        $algorithm.Dispose()
    }
}

function Assert-SafeLeafName {
    param(
        [Parameter(Mandatory)]
        [string]$Name,
        [Parameter(Mandatory)]
        [string]$Description
    )

    if (
        [string]::IsNullOrWhiteSpace($Name) -or
        [System.IO.Path]::IsPathRooted($Name) -or
        $Name.Contains('/') -or
        $Name.Contains('\') -or
        $Name.Contains(':') -or
        [System.IO.Path]::GetFileName($Name) -cne $Name -or
        $Name -ceq '.' -or
        $Name -ceq '..'
    ) {
        throw "$Description 不是安全的根目录文件名：$Name"
    }
}

function Assert-ExactNameSet {
    param(
        [Parameter(Mandatory)]
        [object[]]$ActualNames,
        [Parameter(Mandatory)]
        [object[]]$ExpectedNames,
        [Parameter(Mandatory)]
        [string]$Description
    )

    $actual = @($ActualNames | ForEach-Object { [string]$_ })
    $expected = @($ExpectedNames | ForEach-Object { [string]$_ })
    $missing = @($expected | Where-Object { $actual -cnotcontains $_ })
    $extra = @($actual | Where-Object { $expected -cnotcontains $_ })
    if ($actual.Count -ne $expected.Count -or $missing.Count -gt 0 -or $extra.Count -gt 0) {
        throw (
            "$Description 不符合固定集合：" +
            "missing=[$($missing -join ', ')] extra=[$($extra -join ', ')]"
        )
    }
}

function Assert-FileMatches {
    param(
        [Parameter(Mandatory)]
        [string]$Path,
        [Parameter(Mandatory)]
        [long]$ExpectedSize,
        [Parameter(Mandatory)]
        [string]$ExpectedSha256,
        [Parameter(Mandatory)]
        [string]$Description
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "$Description 缺失：$Path"
    }
    Assert-NoReparsePoint -Path $Path
    $item = Get-Item -LiteralPath $Path -Force
    if ($item.Length -ne $ExpectedSize) {
        throw "$Description 大小不符：expected=$ExpectedSize actual=$($item.Length) path=$Path"
    }
    $actualSha256 = Get-FileSha256 -Path $Path
    if ($actualSha256 -cne $ExpectedSha256.ToLowerInvariant()) {
        throw "$Description SHA-256 不符：expected=$ExpectedSha256 actual=$actualSha256 path=$Path"
    }
}

function Assert-SameFile {
    param(
        [Parameter(Mandatory)]
        [string]$Source,
        [Parameter(Mandatory)]
        [string]$Destination,
        [Parameter(Mandatory)]
        [string]$Description
    )

    if (-not (Test-Path -LiteralPath $Source -PathType Leaf)) {
        throw "$Description 的仓库文件缺失：$Source"
    }
    Assert-NoReparsePoint -Path $Source
    $sourceItem = Get-Item -LiteralPath $Source -Force
    Assert-FileMatches -Path $Destination -ExpectedSize $sourceItem.Length `
        -ExpectedSha256 (Get-FileSha256 -Path $Source) -Description $Description
}

function Assert-EmptyApiKey {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $content = Get-Content -Raw -LiteralPath $Path
    $assignments = @(
        $content -split '\r?\n' |
            Where-Object { $_ -match '^[ \t]*api_key[ \t]*=' }
    )
    if (
        $assignments.Count -ne 1 -or
        $assignments[0] -cnotmatch '^[ \t]*api_key[ \t]*=[ \t]*""[ \t]*(?:#.*)?$'
    ) {
        throw "Formic 配置必须恰好包含一个空 api_key：$Path"
    }
}

function Get-ReleaseManifest {
    if (-not (Test-Path -LiteralPath $releaseManifestPath -PathType Leaf)) {
        throw "Formic Release 清单缺失：$releaseManifestPath"
    }
    Assert-NoReparsePoint -Path $releaseManifestPath
    $manifest = Get-Content -Raw -LiteralPath $releaseManifestPath | ConvertFrom-Json

    $releaseName = [string]$manifest.name
    $releaseVersion = [string]$manifest.version
    $releaseTag = [string]$manifest.tag
    $releaseCommit = [string]$manifest.commit
    if (
        $releaseName -cne 'Formic' -or
        $releaseVersion -cnotmatch '^\d+\.\d+\.\d+$' -or
        $releaseTag -cne "v$releaseVersion" -or
        $releaseCommit -cnotmatch '^[0-9a-f]{40}$'
    ) {
        throw (
            'Formic Release 的 name、version、tag 或完整提交不一致：' +
            "name=$releaseName version=$releaseVersion tag=$releaseTag commit=$releaseCommit"
        )
    }

    $assetFileName = [string]$manifest.asset.fileName
    Assert-SafeLeafName -Name $assetFileName -Description 'Formic Release 附件名'
    $expectedAssetFileName = "formic-v$releaseVersion-windows-x86_64.zip"
    $expectedAssetPath = "/yexi-by/formic/releases/download/$releaseTag/$expectedAssetFileName"
    $assetUri = $null
    if (
        $assetFileName -cne $expectedAssetFileName -or
        -not [System.Uri]::TryCreate(
            [string]$manifest.asset.url,
            [System.UriKind]::Absolute,
            [ref]$assetUri
        ) -or
        $assetUri.Scheme -cne 'https' -or
        $assetUri.Host -cne 'github.com' -or
        [System.Uri]::UnescapeDataString($assetUri.AbsolutePath) -cne $expectedAssetPath
    ) {
        throw "Formic Release URL 或附件名与固定版本不一致：$($manifest.asset.url)"
    }

    $assetSize = [long]$manifest.asset.size
    $assetSha256 = [string]$manifest.asset.sha256
    if ($assetSize -le 0 -or $assetSha256 -cnotmatch '^[0-9a-fA-F]{64}$') {
        throw 'Formic Release 附件大小或 SHA-256 无效。'
    }

    $entries = [System.Collections.Generic.Dictionary[string, object]]::new(
        [System.StringComparer]::Ordinal
    )
    foreach ($property in $manifest.archiveEntries.PSObject.Properties) {
        $name = [string]$property.Name
        Assert-SafeLeafName -Name $name -Description 'Formic ZIP entry'
        $entry = $property.Value
        $entrySize = [long]$entry.size
        $entrySha256 = [string]$entry.sha256
        if (
            $entrySize -lt 0 -or
            $entrySha256 -cnotmatch '^[0-9a-fA-F]{64}$' -or
            $entry.include -isnot [bool]
        ) {
            throw "Formic ZIP entry 清单无效：$name"
        }
        if (-not $entries.TryAdd($name, [pscustomobject]@{
                    Size = $entrySize
                    Sha256 = $entrySha256.ToLowerInvariant()
                    Include = [bool]$entry.include
                })) {
            throw "Formic ZIP entry 清单包含重复名称：$name"
        }
    }

    $actualIncludedNames = @(
        foreach ($entryName in $entries.Keys) {
            if ($entries[$entryName].Include) {
                $entryName
            }
        }
    )
    Assert-ExactNameSet -ActualNames $actualIncludedNames `
        -ExpectedNames $includedUpstreamNames -Description 'Formic 安装用上游文件'

    [pscustomobject]@{
        AssetFileName = $assetFileName
        AssetUri = $assetUri.AbsoluteUri
        AssetSize = $assetSize
        AssetSha256 = $assetSha256.ToLowerInvariant()
        Entries = $entries
    }
}

function Install-VerifiedArchiveEntries {
    param(
        [Parameter(Mandatory)]
        [string]$ArchivePath,
        [Parameter(Mandatory)]
        [pscustomobject]$Manifest,
        [Parameter(Mandatory)]
        [string]$DestinationRoot
    )

    $archive = [System.IO.Compression.ZipFile]::OpenRead($ArchivePath)
    try {
        $actualEntries = [System.Collections.Generic.Dictionary[string, object]]::new(
            [System.StringComparer]::Ordinal
        )
        foreach ($archiveEntry in $archive.Entries) {
            $entryName = [string]$archiveEntry.FullName
            Assert-SafeLeafName -Name $entryName -Description '下载的 Formic ZIP entry'
            if (-not $actualEntries.TryAdd($entryName, $archiveEntry)) {
                throw "下载的 Formic ZIP 包含重复 entry：$entryName"
            }
        }

        Assert-ExactNameSet -ActualNames @($actualEntries.Keys) `
            -ExpectedNames @($Manifest.Entries.Keys) -Description '下载的 Formic ZIP entry'

        foreach ($entryName in $Manifest.Entries.Keys) {
            $expected = $Manifest.Entries[$entryName]
            $archiveEntry = $actualEntries[$entryName]
            if ($archiveEntry.Length -ne $expected.Size) {
                throw (
                    "Formic ZIP entry 大小不符：entry=$entryName " +
                    "expected=$($expected.Size) actual=$($archiveEntry.Length)"
                )
            }

            $entryStream = $archiveEntry.Open()
            try {
                $actualSha256 = Get-StreamSha256 -Stream $entryStream
            }
            finally {
                $entryStream.Dispose()
            }
            if ($actualSha256 -cne $expected.Sha256) {
                throw (
                    "Formic ZIP entry SHA-256 不符：entry=$entryName " +
                    "expected=$($expected.Sha256) actual=$actualSha256"
                )
            }

            if (-not $expected.Include) {
                continue
            }
            $destination = Join-Path $DestinationRoot $entryName
            Assert-TargetChild -Path $destination
            $input = $archiveEntry.Open()
            $output = $null
            try {
                $output = [System.IO.File]::Open(
                    $destination,
                    [System.IO.FileMode]::CreateNew,
                    [System.IO.FileAccess]::Write,
                    [System.IO.FileShare]::None
                )
                $input.CopyTo($output)
            }
            finally {
                if ($null -ne $output) {
                    $output.Dispose()
                }
                $input.Dispose()
            }
            Assert-FileMatches -Path $destination -ExpectedSize $expected.Size `
                -ExpectedSha256 $expected.Sha256 -Description "Formic 上游文件 $entryName"
        }
    }
    finally {
        $archive.Dispose()
    }
}

function Assert-Amd64PeFile {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $bytes = [System.IO.File]::ReadAllBytes($Path)
    if ($bytes.Length -lt 64 -or $bytes[0] -ne 0x4d -or $bytes[1] -ne 0x5a) {
        throw "文件不是有效的 PE：$Path"
    }
    $peOffset = [System.BitConverter]::ToInt32($bytes, 0x3c)
    if (
        $peOffset -lt 0 -or
        $peOffset -gt ($bytes.Length - 6) -or
        $bytes[$peOffset] -ne 0x50 -or
        $bytes[$peOffset + 1] -ne 0x45 -or
        $bytes[$peOffset + 2] -ne 0 -or
        $bytes[$peOffset + 3] -ne 0
    ) {
        throw "文件没有有效的 PE 标头：$Path"
    }
    $machine = [System.BitConverter]::ToUInt16($bytes, $peOffset + 4)
    if ($machine -ne 0x8664) {
        throw ('文件不是 x64 PE：machine=0x{0:x4} path={1}' -f $machine, $Path)
    }
}

function Get-ValidatedRuntimeMetadata {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    if (-not (Test-Path -LiteralPath $Path -PathType Leaf)) {
        throw "VCRUNTIME140.dll 不存在：$Path"
    }
    Assert-NoReparsePoint -Path $Path
    Assert-Amd64PeFile -Path $Path

    $signature = Get-AuthenticodeSignature -LiteralPath $Path
    if ($signature.Status -ne [System.Management.Automation.SignatureStatus]::Valid) {
        throw "VCRUNTIME140.dll Authenticode 签名无效：status=$($signature.Status) path=$Path"
    }
    if ($null -eq $signature.SignerCertificate) {
        throw "VCRUNTIME140.dll 缺少签名证书：$Path"
    }
    $signer = [string]$signature.SignerCertificate.Subject
    if ($signer -cnotmatch '(?i)(?:^|,\s*)O\s*=\s*Microsoft Corporation(?:\s*,|$)') {
        throw "VCRUNTIME140.dll 签名者不属于 Microsoft Corporation：$signer"
    }

    $item = Get-Item -LiteralPath $Path -Force
    $versionInfo = [System.Diagnostics.FileVersionInfo]::GetVersionInfo($item.FullName)
    if (
        $versionInfo.OriginalFilename -ine 'VCRUNTIME140.dll' -and
        $versionInfo.InternalName -ine 'VCRUNTIME140.dll'
    ) {
        throw (
            'Microsoft 签名的 x64 文件不是 VCRUNTIME140.dll：' +
            "original=$($versionInfo.OriginalFilename) internal=$($versionInfo.InternalName) path=$Path"
        )
    }
    $fileVersion = $versionInfo.FileVersion
    if ([string]::IsNullOrWhiteSpace($fileVersion)) {
        throw "VCRUNTIME140.dll 缺少文件版本：$Path"
    }

    [pscustomobject][ordered]@{
        size = [long]$item.Length
        sha256 = Get-FileSha256 -Path $item.FullName
        fileVersion = $fileVersion.Trim()
        signer = $signer
    }
}

function Assert-RuntimeRecord {
    param(
        [Parameter(Mandatory)]
        [string]$RuntimePath,
        [Parameter(Mandatory)]
        [string]$RecordPath
    )

    if (-not (Test-Path -LiteralPath $RecordPath -PathType Leaf)) {
        throw "Formic 运行库记录缺失：$RecordPath"
    }
    Assert-NoReparsePoint -Path $RecordPath
    $record = Get-Content -Raw -LiteralPath $RecordPath | ConvertFrom-Json
    Assert-ExactNameSet -ActualNames @($record.PSObject.Properties.Name) `
        -ExpectedNames @('size', 'sha256', 'fileVersion', 'signer') `
        -Description 'Formic runtime.json 字段'
    $actual = Get-ValidatedRuntimeMetadata -Path $RuntimePath
    if (
        [long]$record.size -ne $actual.size -or
        [string]$record.sha256 -cne $actual.sha256 -or
        [string]$record.fileVersion -cne $actual.fileVersion -or
        [string]$record.signer -cne $actual.signer
    ) {
        throw "Formic runtime.json 与 VCRUNTIME140.dll 不一致：$RecordPath"
    }
}

function Assert-InstalledFormic {
    param(
        [Parameter(Mandatory)]
        [string]$Root,
        [Parameter(Mandatory)]
        [pscustomobject]$Manifest
    )

    if (-not (Test-Path -LiteralPath $Root -PathType Container)) {
        throw "Formic 工具目录缺失：$Root"
    }
    Assert-NoReparsePoint -Path $Root -Recurse
    $items = @(Get-ChildItem -LiteralPath $Root -Force)
    $directories = @($items | Where-Object { $_.PSIsContainer })
    if ($directories.Count -gt 0) {
        throw "Formic 工具目录包含子目录：$($directories.FullName -join ', ')"
    }
    Assert-ExactNameSet -ActualNames @($items.Name) -ExpectedNames $expectedInstalledNames `
        -Description 'Formic 工具目录'

    foreach ($entryName in $includedUpstreamNames) {
        $expected = $Manifest.Entries[$entryName]
        Assert-FileMatches -Path (Join-Path $Root $entryName) -ExpectedSize $expected.Size `
            -ExpectedSha256 $expected.Sha256 -Description "Formic 上游文件 $entryName"
    }
    foreach ($fileName in $supportFileNames) {
        Assert-SameFile -Source (Join-Path $sourceRoot $fileName) `
            -Destination (Join-Path $Root $fileName) -Description "Formic 支持文件 $fileName"
    }

    $exampleConfig = Join-Path $Root 'config.example.toml'
    $activeConfig = Join-Path $Root 'config.toml'
    Assert-SameFile -Source $exampleConfig -Destination $activeConfig `
        -Description 'Formic 无密钥 config.toml'
    Assert-EmptyApiKey -Path $activeConfig

    Assert-RuntimeRecord -RuntimePath (Join-Path $Root 'VCRUNTIME140.dll') `
        -RecordPath (Join-Path $Root 'runtime.json')
}

Assert-NoReparsePoint -Path $repositoryRoot
Assert-NoReparsePoint -Path $distributionRoot -Recurse
Assert-NoReparsePoint -Path $sourceRoot -Recurse
foreach ($supportFileName in $supportFileNames) {
    $supportPath = Join-Path $sourceRoot $supportFileName
    if (-not (Test-Path -LiteralPath $supportPath -PathType Leaf)) {
        throw "Formic 支持文件缺失：$supportPath"
    }
}

$releaseManifest = Get-ReleaseManifest
if ($Check) {
    Assert-InstalledFormic -Root $formicRoot -Manifest $releaseManifest
    Write-Output 'Formic 工具与固定 Release、包内支持文件和运行库记录一致。'
    return
}

$runtimeSource = if ([string]::IsNullOrWhiteSpace($VCRuntimePath)) {
    $systemRoot = [System.Environment]::GetEnvironmentVariable('SystemRoot')
    if ([string]::IsNullOrWhiteSpace($systemRoot)) {
        throw '找不到 Windows SystemRoot；请使用 -VCRuntimePath 指定 x64 VCRUNTIME140.dll。'
    }
    Join-Path $systemRoot 'System32\VCRUNTIME140.dll'
}
else {
    [System.IO.Path]::GetFullPath($VCRuntimePath)
}
if (-not (Test-Path -LiteralPath $runtimeSource -PathType Leaf)) {
    throw "VCRUNTIME140.dll 来源不存在：$runtimeSource"
}
$runtimeSource = (Resolve-Path -LiteralPath $runtimeSource).Path
$null = Get-ValidatedRuntimeMetadata -Path $runtimeSource

$stagingRoot = Join-Path $distributionRoot '.formic-tool-sync'
Assert-TargetChild -Path $stagingRoot
if (Test-Path -LiteralPath $stagingRoot) {
    throw "Formic 工具同步无法开始：临时目录已存在：$stagingRoot"
}

try {
    New-Item -ItemType Directory -Path $stagingRoot | Out-Null
    $stagedFormicRoot = Join-Path $stagingRoot 'formic'
    New-Item -ItemType Directory -Path $stagedFormicRoot | Out-Null
    $archivePath = Join-Path $stagingRoot $releaseManifest.AssetFileName
    Assert-TargetChild -Path $archivePath

    $previousProgressPreference = $ProgressPreference
    try {
        $ProgressPreference = 'SilentlyContinue'
        Invoke-WebRequest -Uri $releaseManifest.AssetUri -OutFile $archivePath
    }
    finally {
        $ProgressPreference = $previousProgressPreference
    }
    Assert-FileMatches -Path $archivePath -ExpectedSize $releaseManifest.AssetSize `
        -ExpectedSha256 $releaseManifest.AssetSha256 -Description 'Formic Release ZIP'
    Install-VerifiedArchiveEntries -ArchivePath $archivePath -Manifest $releaseManifest `
        -DestinationRoot $stagedFormicRoot

    foreach ($supportFileName in $supportFileNames) {
        Copy-Item -LiteralPath (Join-Path $sourceRoot $supportFileName) `
            -Destination (Join-Path $stagedFormicRoot $supportFileName)
    }
    Copy-Item -LiteralPath (Join-Path $stagedFormicRoot 'config.example.toml') `
        -Destination (Join-Path $stagedFormicRoot 'config.toml')
    Assert-EmptyApiKey -Path (Join-Path $stagedFormicRoot 'config.toml')

    $stagedRuntime = Join-Path $stagedFormicRoot 'VCRUNTIME140.dll'
    Copy-Item -LiteralPath $runtimeSource -Destination $stagedRuntime
    $runtimeMetadata = Get-ValidatedRuntimeMetadata -Path $stagedRuntime
    $runtimeMetadata | ConvertTo-Json | Set-Content `
        -LiteralPath (Join-Path $stagedFormicRoot 'runtime.json') -Encoding utf8NoBOM

    Assert-InstalledFormic -Root $stagedFormicRoot -Manifest $releaseManifest

    $toolsRoot = Join-Path $distributionRoot 'tools'
    Assert-TargetChild -Path $toolsRoot
    if (-not (Test-Path -LiteralPath $toolsRoot)) {
        New-Item -ItemType Directory -Path $toolsRoot | Out-Null
    }
    elseif (-not (Test-Path -LiteralPath $toolsRoot -PathType Container)) {
        throw "发行 tools 路径不是目录：$toolsRoot"
    }
    Assert-NoReparsePoint -Path $toolsRoot

    Remove-TargetTree -Path $formicRoot
    Move-Item -LiteralPath $stagedFormicRoot -Destination $formicRoot
    Assert-InstalledFormic -Root $formicRoot -Manifest $releaseManifest
}
finally {
    if (Test-Path -LiteralPath $stagingRoot) {
        Remove-TargetTree -Path $stagingRoot
    }
}

Write-Output 'Formic 工具已从固定 Release 同步，并通过文件、配置和运行库检查。'
