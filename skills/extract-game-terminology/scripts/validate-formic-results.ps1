<#
.SYNOPSIS
校验 Formic 术语抓取结果完整、可回查且没有越出各自分片。

.DESCRIPTION
先校验原始完整计划，再确认每个计划单元恰好有一个数字编号 JSON。脚本检查结果结构、
候选锚点的分片归属、input 引用的实际文本，以及 include 建议所需的身份、固定写法和独立
支持证据。跨单元重复候选是合法输入，不在此处去重。

.PARAMETER DataRoot
Formic 的 --data 目录，即作业的 input 目录。

.PARAMETER Plan
原始完整 plan.jsonl，不是只含失败单元的续跑计划。

.PARAMETER Out
Formic 的 --out 目录。

.PARAMETER Schema
本次作业使用的结果 schema。省略时使用当前 Skill 的固定资产。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$DataRoot,
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$Plan,
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$Out,
    [string]$Schema = (Join-Path $PSScriptRoot '../assets/formic-result.schema.json')
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$planValidator = Join-Path $PSScriptRoot 'validate-formic-plan.ps1'
& $planValidator -DataRoot $DataRoot -Plan $Plan | Out-Null

$utf8Strict = [System.Text.UTF8Encoding]::new($false, $true)
$dataPath = (Resolve-Path -LiteralPath $DataRoot).Path
$planPath = (Resolve-Path -LiteralPath $Plan).Path
$outPath = if (Test-Path -LiteralPath $Out -PathType Container) {
    (Resolve-Path -LiteralPath $Out).Path
}
else {
    throw "Formic 输出目录不存在：$Out"
}
$schemaPath = if (Test-Path -LiteralPath $Schema -PathType Leaf) {
    (Resolve-Path -LiteralPath $Schema).Path
}
else {
    throw "Formic 结果 schema 不存在：$Schema"
}
$assetSchemaPath = (Resolve-Path -LiteralPath (
        Join-Path $PSScriptRoot '../assets/formic-result.schema.json'
    )).Path
$recordedSchemaPath = Join-Path $outPath 'output-schema.json'
if (-not (Test-Path -LiteralPath $recordedSchemaPath -PathType Leaf)) {
    throw "Formic 输出目录缺少权威 schema 记录：$recordedSchemaPath"
}

try {
    $expectedSchema = [System.Text.Json.Nodes.JsonNode]::Parse(
        [System.IO.File]::ReadAllText($schemaPath, $utf8Strict)
    )
    $assetSchema = [System.Text.Json.Nodes.JsonNode]::Parse(
        [System.IO.File]::ReadAllText($assetSchemaPath, $utf8Strict)
    )
    $recordedSchema = [System.Text.Json.Nodes.JsonNode]::Parse(
        [System.IO.File]::ReadAllText($recordedSchemaPath, $utf8Strict)
    )
}
catch {
    throw "无法读取或解析结果 schema：$($_.Exception.Message)"
}
if (-not [System.Text.Json.Nodes.JsonNode]::DeepEquals($assetSchema, $expectedSchema)) {
    throw "本次作业 schema 与当前发行资产不同：$schemaPath"
}
if (-not [System.Text.Json.Nodes.JsonNode]::DeepEquals($expectedSchema, $recordedSchema)) {
    throw "Formic 输出 schema 与本次作业 schema 不同：$recordedSchemaPath"
}

function Get-StrictObjectProperties {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context,
        [Parameter(Mandatory)]
        [string[]]$Expected
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
    $allowed = [System.Collections.Generic.HashSet[string]]::new(
        $Expected,
        [System.StringComparer]::Ordinal
    )
    foreach ($name in $properties.Keys) {
        if (-not $allowed.Contains($name)) {
            throw "$Context 含未知字段：$name"
        }
    }
    foreach ($name in $Expected) {
        if (-not $properties.ContainsKey($name)) {
            throw "$Context 缺少字段：$name"
        }
    }
    return ,$properties
}

function Get-StringValue {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context,
        [switch]$AllowEmpty
    )

    if ($Element.ValueKind -ne [System.Text.Json.JsonValueKind]::String) {
        throw "$Context 必须是字符串"
    }
    $value = $Element.GetString()
    if (-not $AllowEmpty -and [string]::IsNullOrWhiteSpace($value)) {
        throw "$Context 不能为空"
    }
    if ($AllowEmpty -and $value.Length -gt 0 -and [string]::IsNullOrWhiteSpace($value)) {
        throw "$Context 为空时必须使用空字符串，不能只包含空白"
    }
    return $value
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

function Assert-Array {
    param(
        [Parameter(Mandatory)]
        [System.Text.Json.JsonElement]$Element,
        [Parameter(Mandatory)]
        [string]$Context
    )

    if ($Element.ValueKind -ne [System.Text.Json.JsonValueKind]::Array) {
        throw "$Context 必须是数组"
    }
}

function Resolve-InputFile {
    param(
        [Parameter(Mandatory)]
        [string]$RelativePath,
        [Parameter(Mandatory)]
        [string]$Context,
        [switch]$RequireCorpus
    )

    if ([string]::IsNullOrWhiteSpace($RelativePath) -or
        [System.IO.Path]::IsPathRooted($RelativePath)) {
        throw "$Context 必须使用 input 根内的非空相对路径：$RelativePath"
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
        throw "$Context 指向的 input 文件不存在：$RelativePath"
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
    if ($RequireCorpus -and -not $normalized.StartsWith(
            'corpus/',
            [System.StringComparison]::OrdinalIgnoreCase
        )) {
        throw "$Context 必须指向 corpus/：$RelativePath"
    }
    [pscustomobject]@{
        FullPath = $resolved
        RelativePath = $normalized
    }
}

function Get-LineCount {
    param(
        [Parameter(Mandatory)]
        [string]$Path
    )

    $reader = [System.IO.StreamReader]::new($Path, $utf8Strict, $true)
    $count = [uint64]0
    try {
        while ($reader.ReadLine() -ne $null) {
            $count++
        }
    }
    finally {
        $reader.Dispose()
    }
    return $count
}

$unitScopes = [System.Collections.Generic.Dictionary[string, object]]::new(
    [System.StringComparer]::Ordinal
)
foreach ($line in [System.IO.File]::ReadAllLines($planPath, $utf8Strict)) {
    if ([string]::IsNullOrWhiteSpace($line)) {
        continue
    }
    $record = $line | ConvertFrom-Json
    $unitKey = ([uint64]$record.unit).ToString(
        [System.Globalization.CultureInfo]::InvariantCulture
    )
    $scopes = [System.Collections.Generic.List[object]]::new()
    $propertyNames = @($record.PSObject.Properties.Name)
    if ($propertyNames -contains 'files') {
        foreach ($rawPath in $record.files) {
            $resolved = Resolve-InputFile -RelativePath ([string]$rawPath) `
                -Context "计划单元 $unitKey" -RequireCorpus
            $scopes.Add([pscustomobject]@{
                    RelativePath = $resolved.RelativePath
                    Start = [uint64]1
                    End = Get-LineCount -Path $resolved.FullPath
                })
        }
    }
    else {
        $resolved = Resolve-InputFile -RelativePath ([string]$record.file) `
            -Context "计划单元 $unitKey" -RequireCorpus
        $scopes.Add([pscustomobject]@{
                RelativePath = $resolved.RelativePath
                Start = [uint64]$record.start
                End = [uint64]$record.end
            })
    }
    $unitScopes.Add($unitKey, $scopes)
}

function Test-UnitContainsLine {
    param(
        [Parameter(Mandatory)]
        [string]$Unit,
        [Parameter(Mandatory)]
        [string]$RelativePath,
        [Parameter(Mandatory)]
        [uint64]$Line
    )

    foreach ($scope in $unitScopes[$Unit]) {
        if ($scope.RelativePath.Equals(
                $RelativePath,
                [System.StringComparison]::OrdinalIgnoreCase
            ) -and $Line -ge $scope.Start -and $Line -le $scope.End) {
            return $true
        }
    }
    return $false
}

$numericFiles = [System.Collections.Generic.Dictionary[string, string]]::new(
    [System.StringComparer]::Ordinal
)
foreach ($file in Get-ChildItem -LiteralPath $outPath -File) {
    if ($file.BaseName -match '^[0-9]+$' -and
        -not $file.Extension.Equals('.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Formic 输出含非 JSON 的数字编号结果：$($file.Name)"
    }
}
foreach ($file in Get-ChildItem -LiteralPath $outPath -File -Filter '*.json') {
    if ($file.Name.Equals('output-schema.json', [System.StringComparison]::OrdinalIgnoreCase)) {
        continue
    }
    if ($file.BaseName -notmatch '^[0-9]+$') {
        throw "Formic 输出目录含未声明的顶层 JSON：$($file.FullName)"
    }
    if ($file.BaseName -notmatch '^[1-9][0-9]*$') {
        throw "Formic 数字结果文件名不是自然编号：$($file.Name)"
    }
    $canonical = ([uint64]::Parse($file.BaseName)).ToString(
        [System.Globalization.CultureInfo]::InvariantCulture
    )
    if (-not $unitScopes.ContainsKey($canonical)) {
        throw "Formic 输出含原始完整计划之外的单元：$($file.Name)"
    }
    if (-not $numericFiles.TryAdd($canonical, $file.FullName)) {
        throw "Formic 输出对同一单元存在多个数字结果：$canonical"
    }
}
foreach ($unit in $unitScopes.Keys) {
    if (-not $numericFiles.ContainsKey($unit)) {
        throw "Formic 输出缺少计划单元结果：$unit.json"
    }
}

$lineChecks = [System.Collections.Generic.List[object]]::new()
function Add-LineCheck {
    param(
        [Parameter(Mandatory)]
        [string]$FullPath,
        [Parameter(Mandatory)]
        [uint64]$Line,
        [Parameter(Mandatory)]
        [string[]]$Needles,
        [Parameter(Mandatory)]
        [string]$Context
    )

    [void]$lineChecks.Add([pscustomobject]@{
            FullPath = $FullPath
            Line = $Line
            Needles = $Needles
            Context = $Context
        })
}

$candidateCount = 0
foreach ($unit in @($unitScopes.Keys | Sort-Object { [uint64]$_ })) {
    $resultPath = $numericFiles[$unit]
    try {
        $document = [System.Text.Json.JsonDocument]::Parse(
            [System.IO.File]::ReadAllText($resultPath, $utf8Strict)
        )
    }
    catch {
        throw "Formic 单元 $unit 的结果不是合法 JSON：$($_.Exception.Message)"
    }
    try {
        $root = Get-StrictObjectProperties -Element $document.RootElement `
            -Context "Formic 单元 $unit 的结果" -Expected @('candidates')
        Assert-Array -Element $root['candidates'] -Context "Formic 单元 $unit 的 candidates"

        $index = 0
        foreach ($candidateElement in $root['candidates'].EnumerateArray()) {
            $index++
            $candidateCount++
            $context = "Formic 单元 $unit 的候选 $index"
            $candidate = Get-StrictObjectProperties -Element $candidateElement -Context $context `
                -Expected @(
                    'term',
                    'category',
                    'identity_or_definition',
                    'recommendation',
                    'fixed_form',
                    'reason',
                    'anchors',
                    'variants',
                    'input_evidence',
                    'external_evidence'
                )
            $term = Get-StringValue -Element $candidate['term'] -Context "$context 的 term"
            $category = Get-StringValue -Element $candidate['category'] `
                -Context "$context 的 category" -AllowEmpty
            $identity = Get-StringValue -Element $candidate['identity_or_definition'] `
                -Context "$context 的 identity_or_definition" -AllowEmpty
            $recommendation = Get-StringValue -Element $candidate['recommendation'] `
                -Context "$context 的 recommendation"
            if ($recommendation -notin @('include', 'exclude', 'needs_review')) {
                throw "$context 的 recommendation 无效：$recommendation"
            }
            $fixedForm = Get-StringValue -Element $candidate['fixed_form'] `
                -Context "$context 的 fixed_form" -AllowEmpty
            [void](Get-StringValue -Element $candidate['reason'] -Context "$context 的 reason")

            foreach ($arrayName in @(
                    'anchors',
                    'variants',
                    'input_evidence',
                    'external_evidence'
                )) {
                Assert-Array -Element $candidate[$arrayName] -Context "$context 的 $arrayName"
            }

            $anchorSignatures = [System.Collections.Generic.HashSet[string]]::new(
                [System.StringComparer]::OrdinalIgnoreCase
            )
            $anchorCount = 0
            foreach ($anchorElement in $candidate['anchors'].EnumerateArray()) {
                $anchorCount++
                $anchorContext = "$context 的 anchor $anchorCount"
                $anchor = Get-StrictObjectProperties -Element $anchorElement -Context $anchorContext `
                    -Expected @('path', 'line', 'quote')
                $rawPath = Get-StringValue -Element $anchor['path'] -Context "$anchorContext 的 path"
                $lineNumber = Get-NaturalNumber -Element $anchor['line'] `
                    -Context "$anchorContext 的 line"
                $quote = Get-StringValue -Element $anchor['quote'] -Context "$anchorContext 的 quote"
                if (-not $quote.Contains($term, [System.StringComparison]::Ordinal)) {
                    throw "$anchorContext 的 quote 不含候选原文：$term"
                }
                $resolved = Resolve-InputFile -RelativePath $rawPath -Context $anchorContext `
                    -RequireCorpus
                if (-not (Test-UnitContainsLine -Unit $unit -RelativePath $resolved.RelativePath `
                        -Line $lineNumber)) {
                    throw "$anchorContext 越出单元主动分片：$($resolved.RelativePath) 第 $lineNumber 行"
                }
                $signature = "$($resolved.RelativePath)`n$lineNumber"
                [void]$anchorSignatures.Add($signature)
                Add-LineCheck -FullPath $resolved.FullPath -Line $lineNumber -Needles @($quote) `
                    -Context $anchorContext
            }
            if ($anchorCount -eq 0) {
                throw "$context 没有分片内 anchor"
            }

            $variantIndex = 0
            foreach ($variantElement in $candidate['variants'].EnumerateArray()) {
                $variantIndex++
                $variantContext = "$context 的 variant $variantIndex"
                $variant = Get-StrictObjectProperties -Element $variantElement `
                    -Context $variantContext -Expected @('form', 'path', 'line', 'relation')
                $form = Get-StringValue -Element $variant['form'] -Context "$variantContext 的 form"
                $rawPath = Get-StringValue -Element $variant['path'] -Context "$variantContext 的 path"
                $lineNumber = Get-NaturalNumber -Element $variant['line'] `
                    -Context "$variantContext 的 line"
                $relation = Get-StringValue -Element $variant['relation'] `
                    -Context "$variantContext 的 relation"
                if ($relation -notin @('same_identity', 'possible_same_identity')) {
                    throw "$variantContext 的 relation 无效：$relation"
                }
                $resolved = Resolve-InputFile -RelativePath $rawPath -Context $variantContext `
                    -RequireCorpus
                Add-LineCheck -FullPath $resolved.FullPath -Line $lineNumber -Needles @($form) `
                    -Context $variantContext
            }

            $hasIndependentSupport = $false
            $evidenceIndex = 0
            foreach ($evidenceElement in $candidate['input_evidence'].EnumerateArray()) {
                $evidenceIndex++
                $evidenceContext = "$context 的 input evidence $evidenceIndex"
                $evidence = Get-StrictObjectProperties -Element $evidenceElement `
                    -Context $evidenceContext `
                    -Expected @('path', 'line', 'quote', 'relation', 'summary')
                $rawPath = Get-StringValue -Element $evidence['path'] `
                    -Context "$evidenceContext 的 path"
                $lineNumber = Get-NaturalNumber -Element $evidence['line'] `
                    -Context "$evidenceContext 的 line"
                $quote = Get-StringValue -Element $evidence['quote'] `
                    -Context "$evidenceContext 的 quote"
                $relation = Get-StringValue -Element $evidence['relation'] `
                    -Context "$evidenceContext 的 relation"
                if ($relation -notin @('support', 'conflict')) {
                    throw "$evidenceContext 的 relation 无效：$relation"
                }
                [void](Get-StringValue -Element $evidence['summary'] `
                        -Context "$evidenceContext 的 summary")
                $resolved = Resolve-InputFile -RelativePath $rawPath -Context $evidenceContext
                Add-LineCheck -FullPath $resolved.FullPath -Line $lineNumber -Needles @($quote) `
                    -Context $evidenceContext
                $signature = "$($resolved.RelativePath)`n$lineNumber"
                if ($relation -eq 'support' -and -not $anchorSignatures.Contains($signature)) {
                    $hasIndependentSupport = $true
                }
            }

            $externalIndex = 0
            foreach ($externalElement in $candidate['external_evidence'].EnumerateArray()) {
                $externalIndex++
                $externalContext = "$context 的 external evidence $externalIndex"
                $external = Get-StrictObjectProperties -Element $externalElement `
                    -Context $externalContext `
                    -Expected @('locator', 'source_kind', 'relation', 'summary')
                [void](Get-StringValue -Element $external['locator'] `
                        -Context "$externalContext 的 locator")
                [void](Get-StringValue -Element $external['source_kind'] `
                        -Context "$externalContext 的 source_kind")
                $relation = Get-StringValue -Element $external['relation'] `
                    -Context "$externalContext 的 relation"
                if ($relation -notin @('support', 'conflict')) {
                    throw "$externalContext 的 relation 无效：$relation"
                }
                [void](Get-StringValue -Element $external['summary'] `
                        -Context "$externalContext 的 summary")
                if ($relation -eq 'support') {
                    $hasIndependentSupport = $true
                }
            }

            if ($recommendation -eq 'include') {
                if ([string]::IsNullOrWhiteSpace($category)) {
                    throw "$context 建议 include，但 category 为空"
                }
                if ([string]::IsNullOrWhiteSpace($identity)) {
                    throw "$context 建议 include，但 identity_or_definition 为空"
                }
                if ([string]::IsNullOrWhiteSpace($fixedForm)) {
                    throw "$context 建议 include，但 fixed_form 为空"
                }
                if (-not $hasIndependentSupport) {
                    throw "$context 建议 include，但没有 anchor 之外的支持证据"
                }
            }
        }
    }
    finally {
        $document.Dispose()
    }
}

foreach ($group in $lineChecks | Group-Object FullPath) {
    $checksByLine = [System.Collections.Generic.Dictionary[uint64, object]]::new()
    foreach ($check in $group.Group) {
        if (-not $checksByLine.ContainsKey($check.Line)) {
            $checksByLine[$check.Line] = [System.Collections.Generic.List[object]]::new()
        }
        [void]$checksByLine[$check.Line].Add($check)
    }

    $reader = [System.IO.StreamReader]::new($group.Name, $utf8Strict, $true)
    $currentLine = [uint64]0
    try {
        while (($text = $reader.ReadLine()) -ne $null) {
            $currentLine++
            if (-not $checksByLine.ContainsKey($currentLine)) {
                continue
            }
            foreach ($check in $checksByLine[$currentLine]) {
                foreach ($needle in $check.Needles) {
                    if (-not $text.Contains($needle, [System.StringComparison]::Ordinal)) {
                        throw ('{0} 无法在原始 input 中回查：{1} 第 {2} 行不含 [{3}]' -f
                            $check.Context, $group.Name, $currentLine, $needle)
                    }
                }
            }
            [void]$checksByLine.Remove($currentLine)
        }
    }
    finally {
        $reader.Dispose()
    }
    if ($checksByLine.Count -gt 0) {
        $missing = @($checksByLine.Keys | Sort-Object | ForEach-Object { $_ }) -join ', '
        throw "结果引用超出 input 文件实际行数：$($group.Name)，缺少第 $missing 行"
    }
}

Write-Output "Formic 结果校验通过：$($unitScopes.Count) 个单元、$candidateCount 个候选均可回查。"
