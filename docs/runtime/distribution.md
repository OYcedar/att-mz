# ATT 发行物现行规格

ATT 发布 Windows x64 静态 Release。发行包包含程序、配置模板、Prompt、使用文档、Skill、
许可证和随包 Formic，使用者可以从包内完成安装与运行。

## 1. 发行内容

实际运行的 `att.exe` 所在目录是发行根：

```text
<att-dir>/att.exe
<att-dir>/config.toml
<att-dir>/config.example.toml
<att-dir>/LICENSE
<att-dir>/README.md
<att-dir>/docs/
<att-dir>/prompts/
<att-dir>/skills/
<att-dir>/licenses/
<att-dir>/tools/formic/
```

| 内容 | 用途与来源 |
| --- | --- |
| `att.exe` | 当前版本的 Windows x64 静态 Release 程序 |
| `config.example.toml` | 仓库中的 ATT 配置模板，API key 使用占位值 |
| `config.toml` | 首次从模板创建的活动配置 |
| `LICENSE` | ATT 的 `AGPL-3.0-only` 许可正文 |
| `README.md`、`docs/`、`prompts/`、`skills/` | 当前仓库的使用者资源 |
| `licenses/` | ATT 与 Formic 的第三方许可证；依赖变化时更新对应内容 |
| `tools/formic/` | 静态 `formic.exe`、许可、来源说明、配置模板、README 和用户文档 |

ATT 与 Formic 只依赖目标 Windows 提供的系统 DLL。公开包由干净 checkout 组装，
不携带使用者项目、真实凭据、源码构建目录或开发缓存。`projects/` 由运行过程按需创建；
公开包携带该目录时保持为空。

配置、项目、Prompt 和包内资源的路径规则见[配置规格](configuration.md)。Skill 中的 Python
辅助程序是可选工具，发行包不捆绑 Python。文档与 Skill 使用包内资源和有效的外部资料链接。
项目和写回目标的存储条件见[目录发布规格](directory-publishing.md#3-候选目录)。

## 2. 资源复制与使用者状态

`scripts/sync-dist-resources.ps1` 从仓库复制上述托管资源，默认目标为 `dist/`，也可通过
`-TargetRoot` 指定发行目录。托管目录更新时清理已失效的资源；ATT 与 Formic 的活动
`config.toml` 已存在时保持原样，缺失时才从各自模板创建。脚本不修改 `att.exe` 或 `projects/`。

资源复制以文件操作成功为结果。发行验收不比较源码和包内资源的逐文件摘要、字节、换行格式
或目录全集；许可证和文档的 CRLF/LF 差异不构成发行失败。

## 3. 构建、检查与发布

Release workflow 从明确的版本标签 checkout，使用锁定工具链和依赖构建静态程序，
在空发行目录复制资源并创建占位配置。标签固定发布内容，后续 `main` 的推进不改变该标签。
标签、Cargo 版本、Windows manifest 和程序 `--version` 使用同一版本号。

`scripts/verify-release-package.ps1` 只检查指定发行包自身：

- 程序、配置、Prompt、Skill、文档和许可证等必需资源存在；
- 公开配置使用占位 API key，包中没有使用者项目；
- ATT 与 Formic 只依赖系统 DLL，并能启动；ATT 的版本与标签一致。

这些检查在远端组装发行包后执行一次。开发阶段已经取得的格式、Clippy、行为与性能验证结果
直接复用；发包不另行要求本地构建、业务流程复测、文档链接扫描或全量压力测试。

正式附件为 `att-vMAJOR.MINOR.PATCH-windows-x64.zip` 与 `SHA256SUMS.txt`。ZIP 使用标准
Deflate 压缩；SHA-256 用于下载完整性校验。工作流先上传草稿和附件，再公开 Release。
上传或公开失败时保留已有草稿供恢复，已经公开的标签和 Release 保持不变。
