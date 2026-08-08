<#
.SYNOPSIS
校验 Formic 术语抓取计划完整且没有重叠。

.DESCRIPTION
按照 Formic 的两种计划形状严格解析 JSONL，并确认计划只主动处理 input/corpus/、恰好覆盖
其中每个非空 UTF-8 文件一次。行区间只允许用于每个物理行都是合法 JSON 的 .jsonl 文件。

.PARAMETER DataRoot
Formic 的 --data 目录，即作业的 input 目录。

.PARAMETER Plan
要校验的 plan.jsonl。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$DataRoot,
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$Plan
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$utf8Strict = [System.Text.UTF8Encoding]::new($false, $true)
$dataPath = if (Test-Path -LiteralPath $DataRoot -PathType Container) {
    (Resolve-Path -LiteralPath $DataRoot).Path
}
else {
    throw "Formic 数据目录不存在：$DataRoot"
}
$planPath = if (Test-Path -LiteralPath $Plan -PathType Leaf) {
    (Resolve-Path -LiteralPath $Plan).Path
}
else {
    throw "Formic 计划文件不存在：$Plan"
}
$corpusPath = Join-Path $dataPath 'corpus'
$referencePath = Join-Path $dataPath 'reference'

foreach ($requiredDirectory in @($corpusPath, $referencePath)) {
    if (-not (Test-Path -LiteralPath $requiredDirectory -PathType Container)) {
        throw "Formic 作业缺少目录：$requiredDirectory"
    }
}
foreach ($requiredReference in @('terminology-rules.md', 'job-context.md')) {
    $path = Join-Path $referencePath $requiredReference
    if (-not (Test-Path -LiteralPath $path -PathType Leaf)) {
        throw "Formic 作业缺少只读参考：$path"
    }
}
$jobContext = [System.IO.File]::ReadAllText(
    (Join-Path $referencePath 'job-context.md'),
    $utf8Strict
)
if ($jobContext.Contains('【待填写', [System.StringComparison]::Ordinal)) {
    throw "作业上下文仍有待填写项目：$(Join-Path $referencePath 'job-context.md')"
}

function Assert-NoReparseTree {
    param(
        [Parameter(Mandatory)]
        [string]$Root
    )

    $pending = [System.Collections.Generic.Stack[string]]::new()
    $pending.Push((Get-Item -LiteralPath $Root -Force).FullName)
    while ($pending.Count -gt 0) {
        $current = Get-Item -LiteralPath $pending.Pop() -Force
        if (($current.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "语料目录不能包含 reparse point：$($current.FullName)"
        }
        if ($current.PSIsContainer) {
            foreach ($child in Get-ChildItem -LiteralPath $current.FullName -Force) {
                $pending.Push($child.FullName)
            }
        }
    }
}

function Get-StrictObjectProperties {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context
    )

    if ($Element.ValueKind -ne [System.Text.Json.JsonValueKind]::Object) {
        throw "$Context 必须是 JSON object"
    }
    $properties = [System.Collections.Generic.Dictionary[
        string, System.Text.Json.JsonElement
    ]]::new([System.StringComparer]::Ordinal)
    foreach ($property in $Element.EnumerateObject()) {
        if (-not $properties.TryAdd($property.Name, $property.Value.Clone())) {
            throw "$Context 含重复字段：$($property.Name)"
        }
    }
    return $properties
}

function Get-NaturalNumber {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context
    )

    if ($Element.ValueKind -ne [System.Text.Json.JsonValueKind]::Number) {
        throw "$Context 必须是不小于 1 的自然数"
    }
    $raw = $Element.GetRawText()
    if ($raw -notmatch '^[1-9][0-9]*$') {
        throw "$Context 必须是不小于 1 的自然数"
    }
    try {
        return [uint64]::Parse(
            $raw,
            [System.Globalization.NumberStyles]::None,
            [System.Globalization.CultureInfo]::InvariantCulture
        )
    }
    catch {
        throw "$Context 超出自然编号可表示范围：$raw"
    }
}

function Get-RequiredString {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context
    )

    if ($Element.ValueKind -ne [System.Text.Json.JsonValueKind]::String) {
        throw "$Context 必须是非空字符串"
    }
    $value = $Element.GetString()
    if ([string]::IsNullOrWhiteSpace($value)) {
        throw "$Context 必须是非空字符串"
    }
    return $value
}

function Resolve-InputFile {
    param(
        [Parameter(Mandatory)]
        [string]$RelativePath,
        [Parameter(Mandatory)]
        [string]$Context
    )

    if ([System.IO.Path]::IsPathRooted($RelativePath)) {
        throw "$Context 只能使用 input 根内的相对路径：$RelativePath"
    }
    if ($RelativePath.Contains('\', [System.StringComparison]::Ordinal)) {
        throw "$Context 的相对路径必须统一使用 `/`：$RelativePath"
    }
    $segments = @($RelativePath -split '[\\/]')
    if ($segments.Count -eq 0 -or
        @($segments | Where-Object { $_ -eq '' -or $_ -eq '.' -or $_ -eq '..' }).Count -gt 0) {
        throw "$Context 含空路径段、`.` 或 `..`：$RelativePath"
    }
    $candidate = Join-Path $dataPath $RelativePath
    if (-not (Test-Path -LiteralPath $candidate -PathType Leaf)) {
        throw "$Context 指向的文件不存在：$RelativePath"
    }

    $current = $dataPath
    foreach ($segment in $segments) {
        $current = Join-Path $current $segment
        $item = Get-Item -LiteralPath $current -Force
        if (($item.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "$Context 不能经过 reparse point：$RelativePath"
        }
    }

    $resolved = (Resolve-Path -LiteralPath $candidate).Path
    $root = $dataPath.TrimEnd('\', '/')
    $prefix = $root + [System.IO.Path]::DirectorySeparatorChar
    if (-not $resolved.StartsWith($prefix, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "$Context 解析后越出 input 根：$RelativePath"
    }
    $normalized = [System.IO.Path]::GetRelativePath($dataPath, $resolved).Replace('\', '/')
    [pscustomobject]@{
        FullPath = $resolved
        RelativePath = $normalized
    }
}

function Get-CorpusFileInfo {
    param(
        [Parameter(Mandatory)]
        [string]$FullPath,
        [Parameter(Mandatory)]
        [string]$RelativePath
    )

    $isJsonLines = [System.IO.Path]::GetExtension($FullPath).Equals(
        '.jsonl',
        [System.StringComparison]::OrdinalIgnoreCase
    )
    $reader = [System.IO.StreamReader]::new($FullPath, $utf8Strict, $true)
    $lineCount = [uint64]0
    try {
        while (($line = $reader.ReadLine()) -ne $null) {
            $lineCount++
            if ($isJsonLines) {
                if ([string]::IsNullOrWhiteSpace($line)) {
                    throw "JSONL 语料含空记录：$RelativePath 第 $lineCount 行"
                }
                try {
                    $document = [System.Text.Json.JsonDocument]::Parse($line)
                    $document.Dispose()
                }
                catch {
                    throw "JSONL 语料不是逐行合法 JSON：$RelativePath 第 $lineCount 行：$($_.Exception.Message)"
                }
            }
        }
    }
    finally {
        $reader.Dispose()
    }
    if ($lineCount -eq 0) {
        throw "主动发现语料不能为空：$RelativePath"
    }
    [pscustomobject]@{
        FullPath = $FullPath
        RelativePath = $RelativePath
        LineCount = $lineCount
        IsJsonLines = $isJsonLines
    }
}

Assert-NoReparseTree -Root $corpusPath

$fileInfoByRelative = [System.Collections.Generic.Dictionary[string, object]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($file in Get-ChildItem -LiteralPath $corpusPath -Recurse -File -Force) {
    $relative = [System.IO.Path]::GetRelativePath($dataPath, $file.FullName).Replace('\', '/')
    $info = Get-CorpusFileInfo -FullPath $file.FullName -RelativePath $relative
    if (-not $fileInfoByRelative.TryAdd($relative, $info)) {
        throw "主动发现语料路径重复：$relative"
    }
}
if ($fileInfoByRelative.Count -eq 0) {
    throw "主动发现语料目录为空：$corpusPath"
}

$coverage = [System.Collections.Generic.Dictionary[string, object]]::new(
    [System.StringComparer]::OrdinalIgnoreCase
)
foreach ($relative in $fileInfoByRelative.Keys) {
    $coverage[$relative] = [System.Collections.Generic.List[object]]::new()
}
$seenUnits = [System.Collections.Generic.HashSet[uint64]]::new()
$unitCount = 0
$lineNumber = 0
$planReader = [System.IO.StreamReader]::new($planPath, $utf8Strict, $true)
try {
    while (($rawLine = $planReader.ReadLine()) -ne $null) {
        $lineNumber++
        if ([string]::IsNullOrWhiteSpace($rawLine)) {
            continue
        }
        try {
            $document = [System.Text.Json.JsonDocument]::Parse($rawLine)
        }
        catch {
            throw "计划文件第 $lineNumber 行不是合法 JSON：$($_.Exception.Message)"
        }
        try {
            $properties = Get-StrictObjectProperties -Element $document.RootElement `
                -Context "计划文件第 $lineNumber 行"
            $allowed = [System.Collections.Generic.HashSet[string]]::new(
                [System.StringComparer]::Ordinal
            )
            foreach ($name in @('unit', 'files', 'file', 'start', 'end')) {
                [void]$allowed.Add($name)
            }
            foreach ($name in $properties.Keys) {
                if (-not $allowed.Contains($name)) {
                    throw "计划文件第 $lineNumber 行含未知字段：$name"
                }
            }
            if (-not $properties.ContainsKey('unit')) {
                throw "计划文件第 $lineNumber 行缺少 unit"
            }
            $unit = Get-NaturalNumber -Element $properties['unit'] `
                -Context "计划文件第 $lineNumber 行的 unit"
            if (-not $seenUnits.Add($unit)) {
                throw "计划文件第 $lineNumber 行的单元号重复：$unit"
            }

            $hasFiles = $properties.ContainsKey('files')
            $hasFile = $properties.ContainsKey('file')
            $hasStart = $properties.ContainsKey('start')
            $hasEnd = $properties.ContainsKey('end')
            if ($hasFiles -and -not ($hasFile -or $hasStart -or $hasEnd)) {
                if ($properties['files'].ValueKind -ne [System.Text.Json.JsonValueKind]::Array) {
                    throw "计划单元 $unit 的 files 必须是非空字符串数组"
                }
                $unitPaths = [System.Collections.Generic.HashSet[string]]::new(
                    [System.StringComparer]::OrdinalIgnoreCase
                )
                $fileCount = 0
                foreach ($element in $properties['files'].EnumerateArray()) {
                    $fileCount++
                    $rawPath = Get-RequiredString -Element $element -Context "计划单元 $unit 的 files"
                    $resolved = Resolve-InputFile -RelativePath $rawPath -Context "计划单元 $unit"
                    if (-not $resolved.RelativePath.StartsWith(
                            'corpus/',
                            [System.StringComparison]::OrdinalIgnoreCase
                        )) {
                        throw "计划单元 $unit 只能主动处理 corpus/：$rawPath"
                    }
                    if (-not $fileInfoByRelative.ContainsKey($resolved.RelativePath)) {
                        throw "计划单元 $unit 指向未登记的 corpus 文件：$rawPath"
                    }
                    if (-not $unitPaths.Add($resolved.RelativePath)) {
                        throw "计划单元 $unit 重复列出文件：$rawPath"
                    }
                    $info = $fileInfoByRelative[$resolved.RelativePath]
                    $coverage[$resolved.RelativePath].Add([pscustomobject]@{
                            Unit = $unit
                            Start = [uint64]1
                            End = [uint64]$info.LineCount
                        })
                }
                if ($fileCount -eq 0) {
                    throw "计划单元 $unit 的 files 不能为空"
                }
            }
            elseif (-not $hasFiles -and $hasFile -and $hasStart -and $hasEnd) {
                $rawPath = Get-RequiredString -Element $properties['file'] `
                    -Context "计划单元 $unit 的 file"
                $resolved = Resolve-InputFile -RelativePath $rawPath -Context "计划单元 $unit"
                if (-not $resolved.RelativePath.StartsWith(
                        'corpus/',
                        [System.StringComparison]::OrdinalIgnoreCase
                    )) {
                    throw "计划单元 $unit 只能主动处理 corpus/：$rawPath"
                }
                if (-not $fileInfoByRelative.ContainsKey($resolved.RelativePath)) {
                    throw "计划单元 $unit 指向未登记的 corpus 文件：$rawPath"
                }
                $info = $fileInfoByRelative[$resolved.RelativePath]
                if (-not $info.IsJsonLines) {
                    throw "计划单元 $unit 使用行区间，但文件不是逐行 JSONL：$rawPath"
                }
                $start = Get-NaturalNumber -Element $properties['start'] `
                    -Context "计划单元 $unit 的 start"
                $end = Get-NaturalNumber -Element $properties['end'] `
                    -Context "计划单元 $unit 的 end"
                if ($end -lt $start) {
                    throw "计划单元 $unit 的 end 不能小于 start"
                }
                if ($start -gt $info.LineCount) {
                    throw "计划单元 $unit 的 start 超出文件行数：$rawPath 共 $($info.LineCount) 行"
                }
                if ($end -gt $info.LineCount) {
                    throw "计划单元 $unit 的 end 超出文件行数：$rawPath 共 $($info.LineCount) 行"
                }
                $coverage[$resolved.RelativePath].Add([pscustomobject]@{
                        Unit = $unit
                        Start = $start
                        End = $end
                    })
            }
            else {
                throw "计划单元 $unit 必须使用 files，或 file + start + end，两种形状只能选一种"
            }
            $unitCount++
        }
        finally {
            $document.Dispose()
        }
    }
}
finally {
    $planReader.Dispose()
}

if ($unitCount -eq 0) {
    throw "计划文件不含任何单元：$planPath"
}

foreach ($relative in $fileInfoByRelative.Keys) {
    $info = $fileInfoByRelative[$relative]
    $intervals = @($coverage[$relative] | Sort-Object Start, End)
    if ($intervals.Count -eq 0) {
        throw "计划遗漏主动发现语料：$relative"
    }
    $coveredEnd = [uint64]0
    foreach ($interval in $intervals) {
        if ($interval.Start -le $coveredEnd) {
            throw "计划在 $relative 第 $($interval.Start)-$($interval.End) 行发生重叠，涉及单元 $($interval.Unit)"
        }
        $expectedStart = $coveredEnd + 1
        if ($interval.Start -ne $expectedStart) {
            throw "计划遗漏 $relative 第 $expectedStart-$($interval.Start - 1) 行"
        }
        $coveredEnd = [uint64]$interval.End
    }
    if ($coveredEnd -ne $info.LineCount) {
        throw "计划遗漏 $relative 第 $($coveredEnd + 1)-$($info.LineCount) 行"
    }
}

Write-Output "Formic 计划校验通过：$unitCount 个单元恰好覆盖 $($fileInfoByRelative.Count) 个 corpus 文件。"
