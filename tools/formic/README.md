# Formic

Formic 按计划批量执行需要模型判断和工具查证的独立任务。调用方提供输入目录、JSONL 分片计划和共同任务说明；每个单元由一个 worker 完成多轮 `LLM ↔ 工具` 会话并独立发布结果。

它适合逐文件抽取、逐对象核验、分类审阅和元数据补全。需要全局视图的去重、统一命名、排名与汇总，由调用方在结果齐备后完成。分片和任务说明的写法见[任务设计](docs/task-design.md)。

## 1. 使用随包程序

本目录已包含 Windows x64 的 `formic.exe`。以下 PowerShell 命令均在本目录执行：

```powershell
.\formic.exe --help
```

需要从其他目录调用时，使用 `formic.exe` 的完整路径，并显式传入 `--config`。本包提供程序与用户资料；源码位置、提交和构建说明见[来源记录](FORMIC-SOURCE.md)。

## 2. 配置模型

编辑本目录已有的 `config.toml`，填写模型服务地址、密钥、模型名和上下文大小。若该文件缺失，先复制 `config.example.toml` 创建；已有配置保留原值，按需修改。

Formic 支持 Chat Completions、Responses 和 Anthropic Messages 三种协议。模型支持图片时，可声明 `["text", "image"]`，接收 JPEG、PNG、GIF 和 WebP。协议要求、输入模态、超时、重试和 MCP 配置见[详细使用说明](docs/usage.md#2-最小配置)。

配置完成后执行连接自检。它会发送一次最小 LLM 请求，并依次初始化已启用的 MCP server、发现工具目录，在终端报告结果：

```powershell
.\formic.exe test --config .\config.toml
```

自检只输出终端报告，不创建作业目录和 worker 档案。真实密钥只保存在自己的活动配置中，勿提交或公开分享。

## 3. 准备作业

在自己的任务目录中准备以下文件，运行命令中的 `job` 替换为实际路径：

```text
job/
├─ data/
│  ├─ item-001.txt
│  └─ item-002.txt
├─ plan.jsonl
└─ task.md
```

`plan.jsonl` 每行定义一个单元；`files` 使用相对于 `data/` 的路径：

```jsonl
{"unit":1,"files":["item-001.txt"]}
{"unit":2,"files":["item-002.txt"]}
```

`task.md` 写清当前单元的目标、证据规则、未知情况、输出含义和完成条件，例如：

```markdown
只处理“你的分片”中的对象，抽取名称、日期和直接证据。
证据不足时写 unknown，并说明缺少哪项证据。
当前对象的所有字段都有值或明确未知状态后提交结果。
```

## 4. 运行与续跑

```powershell
.\formic.exe run --data job/data --plan job/plan.jsonl --task job/task.md --out job/out --worker-output-access none --config .\config.toml
```

每个单元独立发布到 `job/out/results/`；运行档案位于 `job/out/runs/run-N/workers/`。并发窗口控制同时活动的工作，计划单元总量、普通工具调用总数和对话回合总数没有额外上限。

首次运行和续跑都必须传 `--worker-output-access`。独立任务通常使用 `none`，worker 可读取冻结的 input；`published` 额外允许辅助查阅已经发布的单元结果。结果出现的顺序取决于完成时间，因此单元正确性必须独立于其他 worker 的结果。

作业中断或部分失败后，保留相同输入、输出目录和权限，在原命令末尾增加 `--resume`：

```powershell
.\formic.exe run --data job/data --plan job/plan.jsonl --task job/task.md --out job/out --worker-output-access none --config .\config.toml --resume
```

Formic 核对作业身份和已发布结果，只继续失败、停止和未开始单元。需要改变数据、计划、任务、schema、权限或模型输入模态时，使用新的输出目录。MCP 初始化失败后也应保留已经建立的作业状态，修复连接后按[启动排错](docs/observability.md#71-作业启动失败)续跑。

## 5. 结构化结果与验收

默认结果为 Markdown；需要 JSON 时，在运行命令中增加 `--output-schema result.schema.json`。只有通过本地 schema 校验的 object 才会发布。无效提交次数达到 `execution.llm_attempts` 时，当前 worker 失败；计数包含首次无效提交。schema 子集和完整示例见[结构化输出](docs/usage.md#9-结构化输出)。

退出码 `0` 表示作业完整或自检全部通过，`1` 表示作业未完成或自检失败，`2` 表示启动失败，`3` 表示收到终止信号。完整状态与恢复方法见[可观测性与排错](docs/observability.md)。

## 文档与许可

- [文档索引](docs/README.md)
- [详细使用说明](docs/usage.md)
- [任务设计](docs/task-design.md)
- [运行档案与排错](docs/observability.md)
- [程序和源码来源](FORMIC-SOURCE.md)

Formic 使用 [GNU Affero General Public License v3.0](LICENSE)。Copyright (C) 2026 yexi。
