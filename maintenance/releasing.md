# ATT 公开发行指南

公开 Release 只发布远端 `main` 上已经确认的当前版本。发行物内容和验证标准由
[发行物规格](../docs/runtime/distribution.md)负责，本指南只说明维护者如何准备、触发、
核验和恢复 GitHub 发布。

## 1. 发布前准备

1. 确认本次版本的用户结果、失败语义、生产入口和发行资源已经完成相称验证。
2. 同时更新 `Cargo.toml`、`Cargo.lock`、`att.exe.manifest` 与
   `.github/RELEASE_NOTES.md`；四处必须描述同一个当前版本。
3. 确认根 `LICENSE`、Cargo SPDX、README、发行包与 GitHub 仓库描述和 topics 表达同一
   ATT 产品范围与许可。依赖变化时使用当前 `about.toml` 与 `about.hbs` 重新生成
   `licenses/THIRD-PARTY-LICENSES.html`。Formic 固定依赖发生变化时，使用
   `about-formic.toml` 与 `about-formic.hbs` 重新生成
   `licenses/FORMIC-THIRD-PARTY-LICENSES.html`；两份报告都保留在 `licenses/`。
4. 确认 `tools/formic/release.json` 仍固定 Formic v0.1.0、提交
   `8636e4145589c0bfc798c80560917eb26285d228` 和已经审查的 ZIP、EXE 摘要。普通资源同步
   可以从该固定 URL 下载 Formic；`-Check` 必须只检查本地目标，不得联网。确认构建环境能
   提供经过有效签名检查的 x64 `VCRUNTIME140.dll`，并能为实际文件生成 `runtime.json`。
5. 检查 `git status` 和差异，只提交本次发布范围；不得把本机 `dist/`、Formic 作业输入与
   输出、项目状态、密钥、构建目录或临时文件纳入提交。
6. 在本机执行能够运行的格式、Clippy、测试与完整 Release 检查。检查必须覆盖 Formic 固定
   摘要、`formic.exe --help` 启动、PE 依赖、Microsoft Runtime 的架构与签名，以及包内许可
   和相对链接。正式制品仍由 GitHub Actions 从干净 checkout 重新构建，本机制品不得上传。

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

工作流必须确认标签提交等于远端 `main`，构建静态 `Release`，从固定 URL 下载并校验
Formic v0.1.0，从构建环境携带经过签名检查的 x64 `VCRUNTIME140.dll` 与对应
`runtime.json`，同步其余已审查资源，使用最高级别 Deflate 打包并直接创建 Release。格式、
Clippy、测试、第三方许可生成和完整发行检查在标签前完成，不在发包时重复运行。发布完成后
检查：

- Release 名称、标签、正文与当前版本一致；
- `att-v1.0.0-windows-x64.zip` 和 `SHA256SUMS.txt` 都存在；
- GitHub 显示的附件 SHA-256 与校验文件一致；
- 下载并解压后的 `att.exe --version`、根 `LICENSE`、文档、Skill 和第三方许可完整；
- `tools/formic/` 精确包含发行物规格规定的九个文件，`release.json` 中的固定摘要与
  `formic.exe` 一致，`formic.exe --help` 能启动，`VCRUNTIME140.dll` 与 `runtime.json`
  一致；
- 压缩包不含 Formic 游戏语料、计划、任务说明副本、结果、缓存或 worker 档案；
- GitHub 仓库描述、topics 和许可证识别没有残留旧产品或旧许可。

## 4. 分支清理与失败恢复

远端 `main`、版本标签和 Release 全部核验通过后，删除已经合并且不再承担当前工作的远端
分支；保留旧版本标签和 Release。删除前再次确认分支提交已经能够从 `main` 或现有标签
到达。

构建或打包失败时不创建 Release。`gh release create` 失败但已建立 Release 时，workflow 立即删除
该 Release，但保留已有版本标签，供维护者判断是重跑相同提交，还是按第 2 节撤销尚未公开的标签并修复。
固定 Formic 资产无法下载、摘要不符、`formic.exe` 无法启动，或构建环境中的
`VCRUNTIME140.dll` 架构、签名与记录不符时，同样停止本次发布；不得临时改用 `latest`、其他
Formic 安装或未经记录的 Runtime 文件。
