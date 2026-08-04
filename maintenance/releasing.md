# ATT 公开发行指南

公开 Release 只发布远端 `main` 上已经确认的当前版本。发行物内容和验证标准由
[发行物规格](../docs/runtime/distribution.md)负责，本指南只说明维护者如何准备、触发、
核验和恢复 GitHub 发布。

## 1. 发布前准备

1. 确认本次版本的用户结果、失败语义、生产入口和发行资源已经完成相称验证。
2. 同时更新 `Cargo.toml`、`Cargo.lock`、`att.exe.manifest` 与
   `.github/RELEASE_NOTES.md`；四处必须描述同一个当前版本。
3. 确认根 `LICENSE`、Cargo SPDX、README、发行包与 GitHub 仓库描述和 topics 表达同一
   产品范围与许可。依赖变化时使用当前 `about.toml` 与 `about.hbs` 重新生成
   `licenses/THIRD-PARTY-LICENSES.html`；第三方许可继续保留在 `licenses/`。
4. 检查 `git status` 和差异，只提交本次发布范围；不得把本机 `dist/`、项目状态、密钥、
   构建目录或临时文件纳入提交。
5. 在本机执行能够运行的格式、Clippy、测试与静态 Release 检查。正式制品仍由 GitHub
   Actions 从干净 checkout 重新构建，本机制品不得上传。

## 2. 主分支与标签

完成提交和验证后，把当前版本合并到远端 `main`。若重写分支与旧主分支没有共同祖先，
应建立一个明确的双父合并提交：第一个父提交是当前版本，第二个父提交是合并前远端
`main`，工作树保持当前版本。这样能够保留旧历史，并让远端 `main` 通过快进接收新提交，
不需要强推。

确认远端 `main` 精确指向待发布提交后，在该提交创建并推送带说明的三段版本标签：

```powershell
git tag -a v1.0.0 -m "ATT 1.0"
git push origin v1.0.0
```

标签一旦用于公开 Release 就不可移动。工作流失败且必须修改源码、文档或 workflow 时，
先删除尚未公开的失败标签，完成新提交与验证后再重新创建；不得让同一公开标签指向不同
提交。

## 3. 触发与核验

从 `main` 手动触发 Release workflow，并传入已经存在的标签：

```powershell
$headSha = (git rev-parse origin/main).Trim()
$knownRunIds = @(
    gh run list --workflow release.yml --event workflow_dispatch --limit 100 `
        --json databaseId |
        ConvertFrom-Json |
        ForEach-Object { [string]$_.databaseId }
)
if ($LASTEXITCODE -ne 0) {
    throw '读取现有 Release workflow run 失败。'
}
gh workflow run release.yml --ref main -f tag=v1.0.0
if ($LASTEXITCODE -ne 0) {
    throw '触发 Release workflow 失败。'
}
$deadline = [DateTimeOffset]::UtcNow.AddMinutes(2)
do {
    Start-Sleep -Seconds 2
    $run = gh run list --workflow release.yml --event workflow_dispatch --branch main `
        --limit 20 --json databaseId,headSha,createdAt |
        ConvertFrom-Json |
        Where-Object {
            $_.headSha -ceq $headSha -and
            [string]$_.databaseId -notin $knownRunIds
        } |
        Sort-Object { [DateTimeOffset]$_.createdAt } -Descending |
        Select-Object -First 1
    if ($LASTEXITCODE -ne 0) {
        throw '读取新 Release workflow run 失败。'
    }
    if ($null -eq $run -and [DateTimeOffset]::UtcNow -ge $deadline) {
        throw '两分钟内没有找到刚触发的 Release workflow run。'
    }
} while ($null -eq $run)
gh run watch $run.databaseId --exit-status
```

工作流必须确认标签提交等于远端 `main`，构建静态 `Release`，同步已审查的发行资源，
使用最高级别 Deflate 打包并直接创建 Release。格式、Clippy、测试、第三方许可生成和完整
发行检查在标签前完成，不在发包时重复运行。发布完成后检查：

- Release 名称、标签、正文与当前版本一致；
- `att-v1.0.0-windows-x64.zip` 和 `SHA256SUMS.txt` 都存在；
- GitHub 显示的附件 SHA-256 与校验文件一致；
- 下载并解压后的 `att.exe --version`、根 `LICENSE`、文档、Skill 和第三方许可完整；
- GitHub 仓库描述、topics 和许可证识别没有残留旧产品或旧许可。

## 4. 分支清理与失败恢复

远端 `main`、版本标签和 Release 全部核验通过后，删除已经合并且不再承担当前工作的远端
分支；保留旧版本标签和 Release。删除前再次确认分支提交已经能够从 `main` 或现有标签
到达。

构建或打包失败时不创建 Release。`gh release create` 失败但已建立 Release 时，workflow 立即删除
该 Release，但保留已有版本标签，供维护者判断是重跑相同提交，还是按第 2 节撤销尚未公开的标签并修复。
