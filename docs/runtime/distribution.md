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
<att-dir>/tools/formic/
```

程序固定从该目录读取 `config.toml`，并使用同目录下的 `projects/` 和 `prompts/`；
不接受外部配置或资源根，也不把源码目录或调用 cwd 当作备用位置。`projects/` 可以在首次
Init 前不存在，由 ATT 在实际需要时建立。游戏、JSONL、Rules、术语、Placeholder、Lua
等 CLI 显式路径仍按[配置规格](configuration.md#5-路径与敏感信息)从调用 cwd 解析。

发行包中的 Skill 也以同一目录为产品与知识根，只使用包内的程序、配置、项目、Prompt
和文档。缺少本次任务需要的包内资源时，应报告发行缺口，不能从源码仓库、其他安装或
任务目录拼接替代内容。

`tools/formic/` 是随包术语候选抓取工具的固定目录。Formic 使用该目录自己的
`config.toml`，不读取 ATT 根 `config.toml`；调用它的 Skill 必须按
[Formic 术语表指南](../guides/formic-terminology.md)把作业输入和输出放在发行根之外。

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
| `tools/formic/` | 固定的 Formic v0.1.0 Windows x64 工具；精确集合是 `formic.exe`、`config.toml`、`config.example.toml`、`README.md`、`FORMIC-SOURCE.md`、`release.json`、`LICENSE`、经签名的 x64 `VCRUNTIME140.dll` 和描述该运行库的 `runtime.json` |
| `licenses/` | 与仓库 `licenses/` 的完整文件集合和内容完全相同，包含随包第三方组件所需的许可声明；`THIRD-PARTY-LICENSES.html` 由 ATT 当前 Cargo.lock 的 Windows x64 Release 依赖生成，`FORMIC-THIRD-PARTY-LICENSES.html` 由 Formic v0.1.0 的对应依赖生成 |
| `projects/` | 运行期项目工作区；不属于仓库资源同步集合，已有项目内容不能被发行资源同步覆盖或删除 |

`docs/`、`prompts/`、`skills/` 和 `licenses/` 都按整个目录发布。目录内多出源码中已不存在
的旧文件、缺少源码文件或任一文件内容不同，都表示发行资源不一致。

Formic 固定使用 tag `v0.1.0` 和提交
`8636e4145589c0bfc798c80560917eb26285d228`。官方 Windows ZIP 固定 URL 是
<https://github.com/yexi-by/formic/releases/download/v0.1.0/formic-v0.1.0-windows-x86_64.zip>，
SHA-256 是
`85b37410ea12d3c1aeddfa111c1da52c08155378a89457924da74e2a4f4a0b55`，其中未经修改的
`formic.exe` SHA-256 是
`e29adaa79e200ece77ccaa4797197b8c0caa78c2136854e5dda2713a6f6d47dd`。同步和完整发行检查
必须同时确认固定 URL、版本、完整提交、ZIP 摘要和可执行文件摘要；不得读取 `latest` 或
接受同名但摘要不同的资产。

## 3. 包内自足与链接

使用者只靠发行包就能读取现行文档、按照 Skill 组织任务并执行 ATT。所有指向发行文件的
相对链接必须在包内解析到实际文件；文档或 Skill 不能通过本地链接指向源码仓库、构建
目录或维护者机器上的路径。明确指向外部资料的 Web 链接不属于包内文件链接。

发行包只保留当前版本。仓库的 `docs/` 作为完整集合发布，其中包含产品规格和使用者或执行者
完成发行内任务所需的指南。只供源码维护使用的资料放在该同步集合之外。`AGENTS.md`、
`maintenance/`、根 `config.example.toml`、源码、测试、构建目录与缓存、历史版本文件都不
进入发行包。`tools/formic/config.example.toml` 是固定工具资源，不属于这里禁止的根配置。
当前同步脚本会直接拒绝发行根下的 `AGENTS.md`、`maintenance/`、`config.example.toml`、
`src/`、`tests/` 和 `target/`；其他禁止内容仍须在完整发行检查中确认，不能因为脚本未逐项
列出而保留。

Formic 的游戏语料、作业计划、任务说明副本、单元结果、最终术语工作记录、缓存和 worker
运行档案都是用户任务数据，不属于发行资源。它们不得出现在 `tools/formic/`、`skills/` 或
发行包其他位置；发行同步也不得从维护者的 Formic 作业目录复制这些内容。

## 4. 同步与验证责任

仓库脚本 `scripts/sync-dist-resources.ps1` 负责以下资源映射：

- `README.md`、`LICENSE` → `dist/` 下的同名文件；
- `config.example.toml` → `dist/config.toml`；
- `licenses/`、`docs/`、`prompts/`、`skills/` → `dist/` 下的同名目录。
- 仓库 `tools/formic/` 的包内说明与固定发布记录，以及 Formic v0.1.0 官方 ZIP 中经过摘要
  校验的 `formic.exe`、`config.example.toml` 和 `LICENSE` → `dist/tools/formic/`；其中
  `config.example.toml` 另复制为 Formic 实际读取的 `config.toml`；
- 当前 Visual Studio 构建环境中经过 x64 PE 与 Authenticode 签名检查的
  `VCRUNTIME140.dll` → `dist/tools/formic/`，并生成只描述这个实际文件的 `runtime.json`。

普通执行只从固定 Formic Release URL 联网下载资产，不查询最新版本；下载后先确认
`release.json` 规定的大小、ZIP SHA-256、精确文件集合和文件 SHA-256，再写入目标目录。
`-Check` 不访问网络，只根据仓库中的固定发布记录、目标目录现有文件和 `runtime.json`
检查九个 Formic 文件、其他映射文件的集合与逐文件 SHA-256，并检查脚本明确列出的六项
开发材料。该脚本不修改或证明 `att.exe` 和 `projects/` 的状态，也不代替包内链接与独立
运行检查。

脚本默认操作仓库 `dist/`；公开发行验证可以用 `-TargetRoot` 指向新建的干净暂存目录。
无论目标在哪里，脚本都只从当前仓库权威资源读取，并拒绝操作目标根之外的路径。

完整发行检查因此同时承担以下责任：

1. 确认 `att.exe` 是当前目标平台的 `Release` 构建结果，并能从发行根启动；
2. 在 `tools/formic/` 作为当前工作目录执行 `formic.exe --help`，确认程序能够启动；Formic
   v0.1.0 不提供 `--version`，版本身份只由固定提交与经过校验的 ZIP、EXE 摘要证明；
3. 检查 `att.exe` 与 `formic.exe` 的 PE 架构和实际导入；Formic 唯一非系统动态依赖必须是
   同目录经签名的 x64 `VCRUNTIME140.dll`，且其版本、摘要和签名身份必须与
   `runtime.json` 一致；
4. 确认 ATT、Formic、Formic Rust 依赖和 Microsoft Runtime 的许可正文或声明都已随包
   提供，并与实际制品相符；
5. 执行资源同步检查，确认映射文件没有缺失、陈旧副本或内容差异；
6. 检查包内相对链接全部有效，并确认 Formic 作业数据及其他禁止内容没有进入发行包；
7. 从源码仓库之外调用实际发行程序，确认 ATT 的固定配置、项目和 Prompt 路径只依赖发行
   根，并确认 Formic 只读取 `tools/formic/config.toml`。

只有上述程序、依赖、资源、链接和独立运行检查共同通过，才能把 `dist/` 视为完整发行物。

## 5. 公开 Release

公开 Release 只由 `.github/workflows/release.yml` 从远端 `main` 当前提交上的现有版本标签
构建。标签使用 `vMAJOR.MINOR.PATCH`，版本必须同时等于 `Cargo.toml`、`Cargo.lock`、CLI
`--version` 和 Windows manifest 中的版本；标签提交必须就是触发时的远端 `main`。

标签建立前完成格式、Clippy、测试、ATT 与 Formic 第三方许可重生成、PE 依赖和本规格第 4 节
完整检查。Formic 的版本、完整提交、下载 URL、ZIP 摘要和 EXE 摘要在标签前已经确认，公开
发行期间不得改为其他资产。

Release workflow 不重复这些检查；它只使用锁定工具链和依赖构建静态 `att.exe`，从空的
`dist/` 执行普通资源同步：从固定 URL 下载并校验 Formic，从当前 Visual Studio 构建环境
取得经过签名检查的 x64 `VCRUNTIME140.dll`，生成 `runtime.json`，再校验
`att.exe --version`、打包并发布。`projects/` 作为空目录进入压缩包；Formic 输入、计划、
结果和 worker 档案不得进入压缩包。

正式附件固定为 `att-vMAJOR.MINOR.PATCH-windows-x64.zip` 和 `SHA256SUMS.txt`。ZIP 使用
兼容 Windows 常用解压工具的标准 Deflate，并采用可用工具的最高压缩级别。同一 job 完成构建、
打包和 GitHub Release 创建，不上传或下载中间 artifact。
