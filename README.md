# ATT

**ATT 是一个面向 Agent 的游戏翻译命令行工具。**

它把游戏翻译中复杂、繁重的自动化工作——文本提取、状态管理、批量模型请求、失败恢复、
译文写回——全部交给程序完成；把调查、判断、审校等高价值智能工作留给 Agent。两者配合，
即可实现对游戏的全自动翻译。

## 分工：ATT 做什么，Agent 做什么

| ATT 自动完成（繁重的确定性工作） | Agent 智能完成（高价值的判断工作） |
| --- | --- |
| 从游戏数据中精确提取全部可翻译文本 | 调查游戏结构，圈定翻译范围 |
| 上下文分组、全局去重、任务装箱 | 为每类文本选择正确的提取能力 |
| 模型请求、并发限速、超时重试 | 制作术语表、提取规则与 Placeholder |
| 译文状态持久化，中断后断点续译 | 审校译文质量，用 Lua 精确修订 |
| 原子写回、失败恢复、结构化诊断 | 对照游戏实际运行效果完成最终验收 |

## 支持范围

- **RPG Maker MV / MZ**：原生支持。标准数据库、事件、系统文本走 Builtin；插件参数、
  `note` 标签、指定事件参数等走 Extract Rules；两代引擎共享同一套能力（XP、VX、
  VX Ace 不在支持范围内）。
- **Generic**：面向任意游戏或文本的通用域。ATT 理解约定的 JSONL 契约；把游戏格式转成
  JSONL、以及把译后 JSONL 放回游戏，由了解目标格式的操作者或外部工具完成。
- **语言**：源语言当前内置日语与英语模块（判断哪些文本需要翻译、验收译文无源语残留），
  目标语言可以是任意语言。
- **模型服务**：任何兼容 Chat Completions 的接口，在 `config.toml` 中配置 URL、
  API Key 和模型 ID 即可使用。

## 运行环境

- Windows 10 1903 或更高版本，x64；
- 完整支持中文、Emoji、空格、长路径等 Unicode 路径；
- 界面支持十种语言，按系统语言自动选择，也可用 `--ui-language` 指定。

## 快速开始

### 1. 配置模型

发行包是一个独立目录，包含 `att.exe`、`config.toml`、`docs/`、`skills/` 等固定内容。
打开 `att.exe` 旁边的 `config.toml`，填好三项即可使用：

```toml
[llm.clients.primary]
url = "https://api.example.com/v1/chat/completions"   # 你的模型接口地址
api_key = "replace-with-api-key"                       # 你的 API Key
model = "replace-with-model-id"                        # 你的模型 ID
```

ATT 只读取程序同目录下的这一份配置。字段、语言模块、翻译 Profile 等完整说明见
[配置规格](docs/runtime/configuration.md)。

### 2. 搭配 Agent 使用（推荐）

ATT 发行包自带 Agent 工作流（`skills/`）和完整产品文档（`docs/`）。把下面这段提示词
交给你的 Agent，填入实际路径和语言，剩下的交给它：

```text
使用 <ATT目录>/att.exe 对 <游戏目录> 进行翻译，严格遵循 <ATT目录>/skills 下的工作流。

最终目标：将目标游戏中非图片性质、正常游玩下可见的 <源语言> 文本全部翻译为 <目标语言>。

要求：
- 克制使用 generic 功能；只有确认 <游戏引擎> 的原生功能确实无法完整覆盖某部分内容时，
  才允许对该部分使用 generic；
- 遇到问题时多查阅 <ATT目录>/docs 和 <ATT目录>/skills；
- 如果 docs 和 skills 确实无法解决问题，立即停止操作并解释原因。
```

Agent 会按工作流调查游戏、建立项目、执行翻译、审校修订并生成输出；你只需在关键节点
确认结果。

### 3. 手动使用

每个翻译项目按四个命令推进（以 MV 为例，MZ 与 Generic 同理）：

```text
att mv init --name mygame --path "D:\Games\MyGame" --source-language ja --target-language zh-Hans --dialogue-max-fullwidth-chars 40 --scrolling-text-max-fullwidth-chars 40 --help-description-max-fullwidth-chars 34
att mv extract --name mygame --builtin
att mv translate --name mygame
att mv write-back --name mygame
```

1. `init` 建立项目并保存游戏来源副本（原游戏始终保持原样）；
2. `extract` 读取当前输入，建立可翻译内容；
3. `translate` 调用模型生成并保存译文，中断后重跑即可续译；
4. `write-back` 在项目工作区生成可检查的译后输出。

需要人工或 Agent 精确修订译文时，使用独立的 `lua` 命令操作项目数据库。完整命令语法、
参数与退出码见 [CLI 规格](docs/runtime/cli.md)。

## 文档导航

ATT 把权威知识全部放在发行包内，Agent 和用户读的是同一份：

- [文档总入口](docs/README.md)：按当前任务、阶段或观察到的结果路由到对应文档；
- [翻译项目指南](docs/guides/translation-project.md)：从调查游戏到建立完整任务的完整流程；
- [诊断与恢复指南](docs/guides/diagnosis-and-recovery.md)：失败、Partial、取消、状态不明时怎么办；
- [全量验收指南](docs/guides/acceptance.md)：确认整个游戏翻译真正完成；
- `skills/`：Agent 执行工作流（`translate-with-att`）与术语提取工作流（`extract-game-terminology`）。

## 许可

ATT 随包提供 Lua 与 PCRE2 等第三方组件，许可声明见 [licenses/](licenses/) 目录。
