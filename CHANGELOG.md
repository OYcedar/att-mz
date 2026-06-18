# 更新日志

## v0.1.14 - 2026-06-18

### 更新重点

- 修复 MV 虚拟名字框"首行+后续正文"对话块被误判不可写回，导致约 12905 条对话文本被 `coverage_unwritable` 阻断无法进入翻译流程。名字框首行只更新角色和行路径，纯名字框块的不可写回判定延迟到块结束后统一处理，带后续正文的对话块恢复可写回。
- 修复 MV `actor_name` 虚拟名字框写回时把 `\N[n]` 角色名控制符错误替换成角色名译名，破坏 RPG Maker 运行时动态角色名引用。控制符现在原样保留，角色名译文由 `actor_names` 术语分类写回 `Actors.json` 的 `name` 字段，游戏运行时由引擎替换控制符。
- 收束 MV 虚拟名字框写回为单一事实源：索引阶段唯一计算说话人/正文解析结果，写回阶段只消费当前文本事实，删除写回阶段重复扫描 401 指令解析说话人的三份独立实现（净删约 400 行）；索引阶段 actor_name 控制符解析也收束为单一实现。
- 修复 Windows 普通账户运行 `att-mz.exe` 时因缺少创建符号链接特权报 `WinError 1314` 无法启动。恢复 pex `--venv-copies` 用文件复制代替符号链接安装依赖，保持单体 exe 打包方式不变。

### 升级提醒

- 之前因名字框首行+正文格式被阻断的游戏，重新运行 `rebuild-text-index` 后受影响文本会恢复可写回，可继续翻译。
- 使用 `actor_name` 规则的游戏，写回后名字框行的 `\N[n]` 控制符会保留原样（不再变成角色名译名），这是正确行为；角色名译文仍会正确写回 `Actors.json`。
- Windows 用户若遇到 `att-mz.exe` 启动报 `WinError 1314`，请下载本版本及之后的发行包。

### 验证命令

- `uv run basedpyright`
- 设置 `$env:ATT_MZ_RUST_THREADS = "1"` 后执行 `uv run pytest -q -n 12 --dist=load --durations=30 --durations-min=0.5`
- `cargo fmt --manifest-path rust/Cargo.toml -- --check`
- `cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path rust/Cargo.toml`

### 发行包

- GitHub Release 下载 `att-mz-windows-x86_64.zip`。
- GitHub Release 下载 `att-mz-linux-x86_64.zip`。
- 正式 Windows / Linux ZIP 由 GitHub Actions `release` 工作流生成。

## v0.1.13 - 2026-06-16

### 更新重点

- 修复 MV 虚拟名字框写进游戏文件时的说话人信息串位问题，避免同一位置反复报告“说话人不一致”并阻止写回。
- Release 同时提供 Windows x86_64 和 Linux / WSL x86_64 发行包，支持在 Linux / WSL 中原生运行命令并处理 `/mnt/c/...` 下的游戏目录，响应 Issue #8 的 Linux 可执行文件需求。
- 发行版 Skill、README 和发布文档同步改为平台化命令入口：Windows 包使用 `.\att-mz.exe`，Linux 包使用 `./att-mz`。
- 发布工作流改为先完成完整验证，再分别在 Windows 和 Linux runner 构建发行 ZIP，最后把两个包一起附到 GitHub Release。

### 升级提醒

- Windows 用户继续下载 `att-mz-windows-x86_64.zip`。
- Linux / WSL 用户下载 `att-mz-linux-x86_64.zip`；解压工具丢失执行权限时，执行 `chmod +x att-mz/att-mz`。
- Linux / WSL 包可以运行 CLI 和处理挂载路径下的项目；RPG Maker MV / MZ 的 Windows 游戏本体试玩仍建议在 Windows 环境确认。

### 验证命令

- `uv run basedpyright`
- `uv run pytest -q -n 12 --dist=load --durations=30 --durations-min=0.5`
- `cargo fmt --manifest-path rust/Cargo.toml -- --check`
- `cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path rust/Cargo.toml`

### 发行包

- GitHub Release 下载 `att-mz-windows-x86_64.zip`。
- GitHub Release 下载 `att-mz-linux-x86_64.zip`。
- 正式 Windows / Linux ZIP 由 GitHub Actions `release` 工作流生成。

## v0.1.12 - 2026-06-16

### 更新重点

- 修复写进游戏文件时的长文本布局判断：`\SE[...]`、`\.`, `\|`、`\SM[...]`、`\SA[...]` 等 RPG Maker 控制符不再计入玩家可见行宽，避免普通对话被误判过宽并被错误拆行。
- 修复跨行包裹标点续行缩进：处在 `「」`、`『』`、`（）` 内部的既有分行译文会自动给续行补全角空格；行首有控制符时，空格会插在控制符之后、可见文本之前，不破坏语音、停顿或表情触发顺序。
- 收窄跨行包裹缩进触发条件：只有最终行的第一个可见字符（忽略控制符和占位符）就是受保护开符号时，才把后续行视为包裹标点续行；句中引用不会让下一行错误缩进。
- Rust 写回计划和 Rust 质量检查共用同一套布局辅助，Python 保存前布局入口增加共享契约测试，降低行宽、控制符和续行缩进规则漂移风险。
- 新增 `doctor` 统一流程裁决输出，整合最近翻译运行、质量检查、写回级检查和下一步命令提示，让 Agent 更容易判断继续翻译、手动修复还是准备写回。
- 手动译文导入支持 `--check-only` 保存前校验；导入命令报告会说明规则或译文变化对文本索引、doctor 和写回级检查的连锁影响。
- 支持多模型客户端配置，可在 `setting.toml` 中配置多个 OpenAI 兼容客户端，并通过命令选择本轮使用的客户端。
- 发布与测试门禁调整：发布工作流保留完整类型检查、全量 Python 业务测试、Rust 格式检查、clippy 和 Rust 测试；普通 push / PR 不再重复跑常规 CI。

### 修复细节

- 规则错误会归入规则阻断，避免流程过早进入手动修复。
- 长文本写回先完成最终行布局，再统一归一化跨行包裹状态，避免拆分前后两套缩进判断产生差异。
- 翻译流程会读取最近运行状态，减少同类失败尚未诊断时的误判。
- 可信源快照记录保留文件元数据，降低写回和重建当前运行文件时的状态误判风险。
- 测试体系继续收束到公开 CLI、数据库可观察结果、Rust/native 边界和真实写回副作用，删除不再服务当前契约的大型实现路径测试。

### 升级提醒

- 如果此前为了抵消控制符计宽问题调高过 `long_text_line_width_limit`，升级后建议恢复到适合窗口显示的正常值，再重新运行质量检查。
- 已保存译文不需要手改；重新运行 `quality-report --game <游戏标题> --include-write-probe` 或 `write-back --game <游戏标题>` 时会按新布局规则处理。
- 涉及规则变更、工作区过期或索引过期时，继续按命令提示重新运行 `rebuild-text-index --game <游戏标题>` 或 `prepare-agent-workspace --game <游戏标题> --output-dir <工作区>`。
- 多模型客户端配置仍以 `setting.toml` 为事实源；命令行只选择客户端名称，不直接传模型地址、密钥或模型名。

### 验证命令

- `uv run basedpyright`
- `uv run pytest -q -n 12 --dist=load --durations=30 --durations-min=0.5`
- `cargo fmt --manifest-path rust/Cargo.toml -- --check`
- `cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path rust/Cargo.toml`
- `uv run maturin develop --release`
- `quality-report --game <游戏标题> --include-write-probe`
- `write-back --game <游戏标题>`

### 发行包

- GitHub Release 下载 `att-mz-windows-x86_64.zip`。
- 正式 Windows ZIP 由 GitHub Actions `release` 工作流生成。

## v0.1.11 - 2026-06-12

### 更新重点

- 复杂 MV/MZ 游戏处理能力明显提升：针对插件多、结构复杂、数据不规范的项目，在术语表和规则审查到位时，完整流程更接近一轮跑通。
- 新增当前文本索引主流程：通过 `rebuild-text-index` 统一建立翻译、质量检查、手动补译、覆盖审计和写进游戏文件所需的文本范围，减少大型项目重复扫描。
- 引入 Text Fact Contract v2：`fact_id` 成为翻译、质量检查、手动补译和写进游戏文件的核心身份，减少同路径多文本、旧路径和过期译文造成的误修。
- Rust 原生索引与统一规则运行时升级：文本范围构建、规则命中、候选扫描、索引写入和范围检查等重型路径继续向 Rust 收束。
- 规则系统统一处理插件规则、事件指令规则、Note 标签规则、普通占位符、结构化占位符、MV 虚拟名字框和源文残留检查。
- Agent 编排升级：工作区以 `manifest.json` 为交换边界，Skill 协议由 canonical 模板生成，并强化子代理发现、审查与主代理最终裁决流程。
- 写进游戏文件前置检查更严格：覆盖审计、质量报告、可信源快照、当前规则范围、用户许可和必要 warning 确认会共同决定是否允许继续。
- Debug 能力补强：新增统一 `--debug`、`--debug-timings`、`--debug-llm-messages`，便于查看阶段耗时、Rust 线程数和模型请求消息。

### 实际体验

- 对非常复杂且不规范的 MV/MZ 游戏，当前流程在术语准确、规则充分审查的前提下，已经能显著降低返工概率。
- 复杂项目的处理时间可能增加。原因是能力更依赖子代理隔离上下文、候选交叉审查和主代理最终确认，这是为了正确性、术语一致性和写回安全付出的成本。

### 升级提醒

- 升级后建议重新运行 `rebuild-text-index --game <游戏标题>`；必要时重新运行 `prepare-agent-workspace --game <游戏标题> --output-dir <工作区>`。
- 旧规则表不自动迁移；遇到规则校验失败时，请按当前命令重新导出并导入规则。
- 旧工作区、旧索引、旧 schema、旧规则哈希不再作为成功入口；当前命令会明确提示需要重建、重新准备或重新导入。
- 自定义正则需要检查 PCRE2 语法，命名分组请使用 `(?<name>...)`。

### 已知风险与反馈

- 项目近期迭代速度较快，复杂能力已经提前落地，但仍可能存在隐藏 bug、边界样本遗漏或个别游戏适配不足。遇到问题欢迎提交 Issue，并尽量附上命令、日志、游戏结构特征和可复现步骤。

### 后续计划

- 性能极致优化：继续减少重复扫描，优化 Rust 热路径、索引复用、并发调度和大型项目耗时。
- 流程编排能力优化：继续收束 Agent 工作区、规则审查、补译、质量修复和写进游戏文件之间的协作边界。
- MV/MZ 主流程稳定后，转向更久远的 RPG Maker 引擎支持。

### 验证命令

- `uv run basedpyright`
- `uv run pytest`
- `cargo fmt --manifest-path rust/Cargo.toml -- --check`
- `cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings`
- `cargo test --manifest-path rust/Cargo.toml`

### 发行包

- GitHub Release 下载 `att-mz-windows-x86_64.zip`。
- 正式 Windows ZIP 由 GitHub Actions `release` 工作流生成。

## v0.1.10 - 2026-06-01

### 功能变化

- 正文翻译提示词不再向模型暴露游戏内部定位路径，改用批次内临时短 ID 绑定模型输出和本地文本。
- 翻译返回解析改为严格 JSON；模型返回可修复但不合法的 JSON 时记录为“模型返回不可解析”，避免保存非协议输出。
- 插件规则、插件源码规则、事件指令规则和 Note 标签规则增加当前游戏漂移检查；规则不再命中当前结构时显式提示重新导出并导入。
- 规则导入时会清理不再属于当前规则范围的已保存译文，并先写入可恢复备份。
- 当前运行插件源码审计恢复路径更精确，只对写回映射覆盖的运行时 selector 做译文质量反查。

### 协议与文档

- 项目级 `AGENTS.md` 同步当前 CLI 入口：开发版使用 `uv run python main.py <命令> ...`，发行版使用 `.\att-mz.exe <命令> ...`，不再要求 `--agent-mode` 或 `--json`。
- 自定义正文提示词允许不写输出协议模板占位符：缺少时自动追加本轮输出协议；只写了部分模板占位符时显式报错。
- `--system-prompt` 帮助文案和配置模板补充了输出协议模板说明。
- README 和开发文档补充 RGSS 系列引擎后续适配范围说明。
- 发布工作流改为从 `CHANGELOG.md` 提取当前 tag 的版本段落作为 GitHub Release 正文，避免空泛自动发布说明。

### 依赖变化

- 移除 `json-repair` 运行依赖。

### 验证

- `uv run basedpyright`
- `uv run pytest`
- Rust 原生扩展和发行包冒烟测试由 GitHub Actions `release` 工作流执行。

### 发行包

- 正式发行包由 GitHub Actions `release` 工作流生成 `att-mz-windows-x86_64.zip`，并附在 GitHub Release 下载区。

## v0.1.9 - 2026-05-31

### CLI 协议

- CLI 统一为 Agent JSON 协议：命令 stdout 固定输出最终 JSON 报告，stderr 承载日志和已有长任务的简单文本进度。
- 删除 `--json` 和 `--agent-mode` 参数；传入这些参数会返回 `argument_error` JSON。
- `list`、`add-game`、`translate`、`write-back`、`run-all`、规则导入导出和术语相关命令默认输出 AgentReport 风格 JSON。
- `export-plugins-json`、`export-event-commands-json` 和 `export-terminology` 增加最小 JSON 摘要，包含输出路径和关键计数。

### 日志与进度

- 保留简单文本进度行，删除 Rich 动态进度条和 Rich 表格报告。
- stderr 日志固定为无 ANSI 单行文本，启动日志、结束日志、错误摘要和长任务进度不会污染 stdout。
- `--debug` 保留为排障日志级别开关，不影响 stdout JSON 协议。

### Agent 契约与文档

- 开发版 Skill、发行版 Skill、CLI 契约文档、README、进阶文档和性能脚本命令示例同步移除 `--json` / `--agent-mode`。
- 运行依赖移除 `rich`，发行包命令示例统一使用固定 Agent 协议。

### 验证

- `uv run basedpyright`
- `uv run pytest`
