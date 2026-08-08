<#
.SYNOPSIS
建立新的 Formic 术语抓取作业目录。

.DESCRIPTION
创建固定目录结构，并复制当前 Skill 的任务模板、结果 schema、作业上下文模板和术语规则。
目标目录必须尚不存在；脚本不复制配置或凭据，也不会覆盖已有任务材料。

.PARAMETER JobRoot
要创建的作业目录。

.PARAMETER RpgMaker
同时复制 RPG Maker MV/MZ 术语抓取参考。
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory)]
    [ValidateNotNullOrEmpty()]
    [string]$JobRoot,
    [switch]$RpgMaker
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$skillRoot = [System.IO.Path]::GetFullPath((Join-Path $PSScriptRoot '..'))
$releaseRoot = [System.IO.Path]::GetFullPath((Join-Path $skillRoot '../..')).TrimEnd('\', '/')
$jobPath = [System.IO.Path]::GetFullPath($JobRoot)
$releasePrefix = $releaseRoot + [System.IO.Path]::DirectorySeparatorChar

if ($jobPath.Equals($releaseRoot, [System.StringComparison]::OrdinalIgnoreCase) -or
    $jobPath.StartsWith($releasePrefix, [System.StringComparison]::OrdinalIgnoreCase)) {
    throw "作业目录必须位于 ATT 发行目录之外：$jobPath"
}

if (Test-Path -LiteralPath $jobPath) {
    throw "作业目录已经存在，拒绝覆盖已有任务材料：$jobPath"
}

$sources = [ordered]@{
    'task.md' = Join-Path $skillRoot 'assets/formic-unit-task.md'
    'result.schema.json' = Join-Path $skillRoot 'assets/formic-result.schema.json'
    'input/reference/job-context.md' = Join-Path $skillRoot 'assets/formic-job-context.md'
    'input/reference/terminology-rules.md' = Join-Path $skillRoot 'references/terminology-rules.md'
}
if ($RpgMaker) {
    $sources['input/reference/rpg-maker-mv-mz.md'] =
        Join-Path $skillRoot 'references/rpg-maker-mv-mz.md'
}

foreach ($source in $sources.Values) {
    if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
        throw "当前 Skill 缺少建立作业所需的文件：$source"
    }
}

New-Item -ItemType Directory -Path $jobPath | Out-Null
foreach ($relativeDirectory in @(
        'input',
        'input/corpus',
        'input/reference',
        'out',
        'final'
    )) {
    New-Item -ItemType Directory -Path (Join-Path $jobPath $relativeDirectory) | Out-Null
}

foreach ($entry in $sources.GetEnumerator()) {
    $destination = Join-Path $jobPath $entry.Key
    Copy-Item -LiteralPath $entry.Value -Destination $destination
}
New-Item -ItemType File -Path (Join-Path $jobPath 'plan.jsonl') | Out-Null

Write-Output "Formic 术语作业已建立：$jobPath"
Write-Output '下一步：填写 input/reference/job-context.md，把完整语料放入 input/corpus/，再制定并校验 plan.jsonl。'
