# ATT 发行物现行规格

`dist/` 是 ATT 面向使用者的完整发行包。它必须在目标 Windows x64 环境中独立工作；
源码仓库、构建目录、调用命令时的当前工作目录和其他 ATT 安装都不是发行资源来源。

## 1. 唯一发行根

实际运行的 `att.exe` 所在目录是唯一发行根。固定布局为：

```text
<att-dir>/att.exe
<att-dir>/config.toml
<att-dir>/LICENSE
<att-dir>/projects/<mv|mz|generic>/<project-name>/
<att-dir>/prompts/
<att-dir>/README.md
<att-dir>/docs/
<att-dir>/skills/
<att-dir>/licenses/
```

程序固定从该目录读取 `config.toml`，并使用同目录下的 `projects/` 和 `prompts/`；
不接受外部配置或资源根，也不把源码目录或调用 cwd 当作备用位置。`projects/` 可以在首次
Init 前不存在，由 ATT 在实际需要时建立。游戏、JSONL、Rules、术语、Placeholder、Lua
等 CLI 显式路径仍按[配置规格](configuration.md#5-路径与敏感信息)从调用 cwd 解析。

发行包中的 Skill 也以同一目录为产品与知识根，只使用包内的程序、配置、项目、Prompt
和文档。缺少本次任务需要的包内资源时，应报告发行缺口，不能从源码仓库、其他安装或
任务目录拼接替代内容。

## 2. 完整发行集合

当前发行包由以下内容组成：

| 内容 | 发行要求 |
|---|---|
| `att.exe` | 目标 Windows x64 平台的 ATT `Release` 构建结果 |
| 非系统运行依赖 | 当前 `Release` 制品实际需要且受支持 Windows 环境不保证提供的运行库，放在程序可直接加载的位置；精确集合以当前制品的依赖检查为准 |
| `config.toml` | 与仓库 `config.example.toml` 内容完全相同，只在发行包中改用程序直接读取的名称 |
| `LICENSE` | 与仓库根 `LICENSE` 完全相同，是 ATT 自有代码和文档的 `AGPL-3.0-only` 许可正文 |
| `README.md` | 与仓库根 `README.md` 完全相同 |
| `docs/` | 与仓库 `docs/` 的完整文件集合和内容完全相同 |
| `prompts/` | 与仓库 `prompts/` 的完整文件集合和内容完全相同 |
| `skills/` | 与仓库 `skills/` 的完整文件集合和内容完全相同 |
| `licenses/` | 与仓库 `licenses/` 的完整文件集合和内容完全相同，包含随包第三方组件所需的许可声明；`THIRD-PARTY-LICENSES.html` 由当前 Cargo.lock 的 Windows x64 Release 依赖生成 |
| `projects/` | 运行期项目工作区；不属于仓库资源同步集合，已有项目内容不能被发行资源同步覆盖或删除 |

`docs/`、`prompts/`、`skills/` 和 `licenses/` 都按整个目录发布。目录内多出源码中已不存在
的旧文件、缺少源码文件或任一文件内容不同，都表示发行资源不一致。

## 3. 包内自足与链接

使用者只靠发行包就能读取现行文档、按照 Skill 组织任务并执行 ATT。所有指向发行文件的
相对链接必须在包内解析到实际文件；文档或 Skill 不能通过本地链接指向源码仓库、构建
目录或维护者机器上的路径。明确指向外部资料的 Web 链接不属于包内文件链接。

发行包只保留当前版本。仓库的 `docs/` 作为完整集合发布，其中包含产品规格和使用者或执行者
完成发行内任务所需的指南。只供源码维护使用的资料放在该同步集合之外。`AGENTS.md`、
`maintenance/`、`config.example.toml`、源码、测试、构建目录与缓存、历史版本文件都不进入
发行包。
当前同步脚本会直接拒绝发行根下的 `AGENTS.md`、`maintenance/`、`config.example.toml`、
`src/`、`tests/` 和 `target/`；其他禁止内容仍须在完整发行检查中确认，不能因为脚本未逐项
列出而保留。

## 4. 同步与验证责任

仓库脚本 `scripts/sync-dist-resources.ps1` 负责以下资源映射：

- `README.md`、`LICENSE` → `dist/` 下的同名文件；
- `config.example.toml` → `dist/config.toml`；
- `licenses/`、`docs/`、`prompts/`、`skills/` → `dist/` 下的同名目录。

普通执行会同步这些资源，`-Check` 只比较映射后的文件集合和逐文件 SHA-256，并检查脚本
明确列出的六项开发材料。该脚本不修改或证明 `att.exe`、运行库和 `projects/` 的状态，
也不证明目标平台、包内链接或独立运行能力。

脚本默认操作仓库 `dist/`；公开发行验证可以用 `-TargetRoot` 指向新建的干净暂存目录。
无论目标在哪里，脚本都只从当前仓库权威资源读取，并拒绝操作目标根之外的路径。

完整发行检查因此同时承担以下责任：

1. 确认 `att.exe` 是当前目标平台的 `Release` 构建结果，并能从发行根启动；
2. 根据实际程序依赖确认所有必需的非系统运行库和许可声明已经随包提供；
3. 执行资源同步检查，确认映射文件没有缺失、陈旧副本或内容差异；
4. 检查包内相对链接全部有效，并确认禁止内容没有进入发行包；
5. 从源码仓库之外调用实际发行程序，确认固定配置、项目和 Prompt 路径只依赖发行根。

只有上述程序、依赖、资源、链接和独立运行检查共同通过，才能把 `dist/` 视为完整发行物。

## 5. 公开 Release

公开 Release 只由 `.github/workflows/release.yml` 从远端 `main` 当前提交上的现有版本标签
构建。标签使用 `vMAJOR.MINOR.PATCH`，版本必须同时等于 `Cargo.toml`、`Cargo.lock`、CLI
`--version` 和 Windows manifest 中的版本；标签提交必须就是触发时的远端 `main`。

构建 job 使用仓库固定的 Rust 工具链，在 Windows x64 MSVC 目标上执行格式、Clippy、测试
和 Release 构建。PCRE2 与 MSVC CRT 必须静态链接；PE 导入表不得包含需要随包提供的非系统
DLL。构建 job 还要用仓库固定的 cargo-about 配置重新生成 Windows x64 Release 第三方许可
清单，并与仓库和发行包中的副本逐字节比较。构建 job 从空的 `dist/` 重新产生发行物、执行
本规格第 4 节全部检查，并把 `dist/` 内容放在压缩包根目录。`projects/` 可以作为空目录进入
压缩包，不得包含维护者或用户状态。

正式附件固定为 `att-vMAJOR.MINOR.PATCH-windows-x64.zip` 和 `SHA256SUMS.txt`。ZIP 使用
兼容 Windows 常用解压工具的标准 Deflate，并采用可用工具的最高压缩级别。只有只读构建
job 产生并上传制品；独立发布 job 下载同次 artifact、复核 SHA-256 后，才以现有标签和
`.github/RELEASE_NOTES.md` 创建 GitHub Release。发布 job 不重新构建，任一复核失败都不得
留下公开 Release。
