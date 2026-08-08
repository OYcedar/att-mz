# ATT 发行物现行规格

`dist/` 是 ATT 面向使用者的完整发行包。它必须在目标 Windows x64 环境中独立工作；
源码仓库、构建目录、调用命令时的当前工作目录和其他 ATT 安装都不是发行资源来源。

## 1. 唯一发行根

实际运行的 `att.exe` 所在目录是唯一发行根。固定布局为：

```text
<att-dir>/att.exe
<att-dir>/config.toml
<att-dir>/config.example.toml
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

根 `config.example.toml` 和 `tools/formic/config.example.toml` 是托管模板；各自的
`config.toml` 是实际读取的活动配置。干净发行包首次从模板创建活动配置，使用者填写后
属于本地状态，普通资源更新不能覆盖。

## 2. 完整发行集合

当前发行包由以下内容组成：

| 内容 | 发行要求 |
|---|---|
| `att.exe` | 目标 Windows x64 平台的 ATT `Release` 构建结果 |
| 非系统运行依赖 | 当前 `Release` 制品实际需要且受支持 Windows 环境不保证提供的运行库，放在程序可直接加载的位置；精确集合以当前制品的依赖检查为准 |
| `config.example.toml` | 与仓库根同名文件内容完全相同，是 ATT 当前不含真实凭据、高吞吐发行默认的托管模板 |
| `config.toml` | 干净公开发行中与根 `config.example.toml` 内容完全相同；首次配置后属于本地使用者状态，普通资源更新不得覆盖 |
| `LICENSE` | 与仓库根 `LICENSE` 完全相同，是 ATT 自有代码和文档的 `AGPL-3.0-only` 许可正文 |
| `README.md` | 与仓库根 `README.md` 完全相同 |
| `docs/` | 与仓库 `docs/` 的完整文件集合和内容完全相同 |
| `prompts/` | 与仓库 `prompts/` 的完整文件集合和内容完全相同 |
| `skills/` | 与仓库 `skills/` 的完整文件集合和内容完全相同 |
| `licenses/` | 与仓库 `licenses/` 的完整文件集合和内容完全相同；`THIRD-PARTY-LICENSES.html` 对应 ATT 依赖，`FORMIC-THIRD-PARTY-LICENSES.html` 对应随包 Formic v0.1.0 依赖 |
| `tools/formic/` | Formic v0.1.0 Windows x64：`formic.exe`、同目录运行库、许可与来源说明、配置模板和首次创建后保留的活动配置 |
| `projects/` | 运行期项目工作区；不属于仓库资源同步集合，已有项目内容不能被发行资源同步覆盖或删除 |

`docs/`、`prompts/`、`skills/`、`licenses/`、两个配置模板和 Formic 的静态文件都是发行
托管资源，必须与当前仓库权威来源一致。两份活动 `config.toml` 只在干净发行根中要求与
各自模板完全相同；普通更新遇到已有活动配置时必须逐字节保留，不能把它的内容差异当作资源
不同。目录内多出源码中已不存在的托管文件、缺少托管文件或任一托管文件内容不同，才表示
发行资源不一致。

## 3. 包内自足与链接

使用者只靠发行包就能读取现行文档、按照 Skill 组织任务并执行 ATT。所有指向发行文件的
相对链接必须在包内解析到实际文件；文档或 Skill 不能通过本地链接指向源码仓库、构建
目录或维护者机器上的路径。明确指向外部资料的 Web 链接不属于包内文件链接。

发行包只保留当前版本。仓库的 `docs/` 作为完整集合发布，其中包含产品规格和使用者或执行者
完成发行内任务所需的指南。只供源码维护使用的资料放在该同步集合之外。`AGENTS.md`、
`maintenance/`、源码、测试、构建目录与缓存、历史版本文件都不进入发行包。根
两个 `config.example.toml` 是允许且必须存在的托管模板。
当前同步脚本会直接拒绝发行根下的 `AGENTS.md`、`maintenance/`、`src/`、`tests/` 和
`target/`；其他禁止内容仍须在完整发行检查中确认，不能因为脚本未逐项列出而保留。

## 4. 同步与验证责任

仓库脚本 `scripts/sync-dist-resources.ps1` 负责以下资源映射：

- `README.md`、`LICENSE`、`config.example.toml` → `dist/` 下的同名文件；
- `dist/config.toml` 不存在时，从根 `config.example.toml` 首次创建；已经存在时逐字节保留；
- `licenses/`、`docs/`、`prompts/`、`skills/` → `dist/` 下的同名目录。
- `tools/formic/` 中的程序、运行库、许可、来源说明和配置模板 → 发行包同名目录；活动
  `config.toml` 缺失时从模板创建，已经存在时逐字节保留。

普通执行同步这些资源；`-Check` 只比较映射后的文件集合和逐文件 SHA-256，并检查脚本明确
列出的开发材料。普通检查只要求活动配置存在，不把它与模板作摘要比较；公开发行检查才要求
二者完全相同。该脚本不修改或证明 `att.exe` 和 `projects/` 的状态，也不代替包内链接与
独立运行检查。

脚本默认操作仓库 `dist/`；公开发行验证可以用 `-TargetRoot` 指向新建的干净暂存目录。
无论目标在哪里，脚本都只从当前仓库权威资源读取，并拒绝操作目标根之外的路径。

完整发行检查因此同时承担以下责任：

1. 确认 `att.exe` 是当前目标平台的 `Release` 构建结果，并能从发行根启动；
2. 确认 `formic.exe --help` 能从随包目录启动，并且需要的运行库和许可已经随包提供；
3. 执行资源同步检查，确认映射文件没有缺失、陈旧副本或内容差异；
4. 确认两个托管配置模板采用已经验证的高吞吐默认且不含秘密；检查干净公开发行时，还必须
   确认两份活动配置逐字节等于各自模板，并且不含真实 API key、token 等凭据；
5. 检查包内相对链接全部有效，并确认禁止内容没有进入发行包；
6. 从源码仓库之外调用实际发行程序，确认 ATT 的固定配置、项目和 Prompt 路径只依赖发行根。

只有上述程序、依赖、资源、链接和独立运行检查共同通过，才能把 `dist/` 视为完整发行物。

## 5. 公开 Release

公开 Release 只由 `.github/workflows/release.yml` 从远端 `main` 当前提交上的现有版本标签
构建。标签使用 `vMAJOR.MINOR.PATCH`，版本必须同时等于 `Cargo.toml`、`Cargo.lock`、CLI
`--version` 和 Windows manifest 中的版本；标签提交必须就是触发时的远端 `main`。

标签建立前完成格式、Clippy、测试、第三方许可重生成、PE 依赖和本规格第 4 节完整检查。

Release workflow 不重复这些检查；它只使用锁定工具链和依赖构建静态 `att.exe`，从空的
`dist/` 同步托管资源并首次创建两份活动配置，再确认活动配置与各自模板完全相同且不含真实凭据，
校验 `att.exe --version`、打包并发布。`projects/` 作为空目录进入压缩包。

正式附件固定为 `att-vMAJOR.MINOR.PATCH-windows-x64.zip` 和 `SHA256SUMS.txt`。ZIP 使用
兼容 Windows 常用解压工具的标准 Deflate，并采用可用工具的最高压缩级别。同一 job 完成构建、
打包和 GitHub Release 创建，不上传或下载中间 artifact。
