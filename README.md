# ATT

ATT 是面向 Agent 的游戏翻译命令行工具，运行于 Windows x64。它原生支持 RPG Maker MV/MZ，
也可通过 Generic JSONL 接入其他游戏和自定义文本来源，串联提取、翻译、审校与写回。

Agent 负责理解游戏、制定规则和审校文字，ATT 负责执行与状态管理，人工通过实际游玩确认最终效果。

[下载发行版](https://github.com/yexi-by/att/releases/latest) · [使用文档](docs/README.md) · [反馈问题](https://github.com/yexi-by/att/issues)

## 功能

- **多种文本来源**：提取 MV/MZ 数据库、事件和指定插件参数；通过 JSONL 接入其他来源。
- **保留翻译语境**：按语义组织完整文本组，结合术语表与全局去重，减少重复请求。
- **灵活调用模型**：支持兼容 OpenAI Chat Completions 或 Responses 的服务，以及流式响应、并发和重试。
- **保护结构与进度**：验收控制符、Placeholder 和正文结构，保存已确认的结果，支持中断后继续处理。
- **审校与修订**：提供源语残留提示、译文导出、Manual 补译和 Lua 数据库接口。
- **独立生成译本**：保留原版，在独立目录写回；随包 Skill 和工具协助调查、术语、静态 QA、字体与封包。

## 快速开始（以 Codex 为例）

### 1. 准备 ATT

从 [Releases](https://github.com/yexi-by/att/releases/latest) 下载 Windows x64 发行包和
`SHA256SUMS.txt`，核对 SHA-256 后完整解压，例如放在 `D:\Tools\ATT`。
选择本地固定磁盘上可写、大小写不敏感的 NTFS 目录；具体要求见[目录发布规格](docs/runtime/directory-publishing.md#3-候选目录)。
保持程序、配置、文档、Prompt、Skill 和工具的目录结构完整。

更新已有安装时，保留 ATT 的 `config.toml`、`tools\formic\config.toml` 和 `projects/`，
参照新版本的配置模板核对字段。

### 2. 配置翻译服务

编辑 `att.exe` 同目录的 `config.toml`，在现有配置中填写模型服务：

```toml
[llm.clients.primary]
protocol = "chat_completions"
url = "https://api.example.com/v1"
api_key = "replace-with-api-key"
model = "replace-with-model-id"
stream = false
```

需要 Responses 时将 `protocol` 改为 `responses`；`stream` 必填。保留模板中的其他配置，
按服务限制调整并发、超时和重试，详见[配置说明](docs/runtime/configuration.md)。
ATT 使用这里的凭据调用翻译模型，Codex 的登录单独完成。

在 PowerShell 中检查配置和模型连接：

```powershell
Set-Location 'D:\Tools\ATT'
.\att.exe test
```

### 3. 交给 Codex

按[官方文档](https://learn.chatgpt.com/docs/codex/cli)安装 Codex CLI，在上述目录运行 `codex`，
首次启动时完成登录。将下面的任务发给 Codex，替换游戏路径和语言：

```text
使用当前目录的 att.exe，将 D:\Games\Example 中正常游玩可见的非图片日语文本翻译为简体中文。

先完整读取 skills/translate-with-att/SKILL.md，并按其中引用读取当前发行版文档。
以原版游戏为只读基线，在独立目录生成译本。根据真实文本来源选择提取方式，
制作规则、术语和 Placeholder，完成翻译、静态 QA、写回及所需的字体与封包处理。

最后给出译本路径、静态 QA 结果，以及需要我实机检查的场景和待确认项。
```

Codex 需要读取游戏目录、写入 ATT 项目与译本目录，并访问你配置的模型服务。
按任务实际需要授予这些权限即可。

### 4. 实机验收

按 Codex 给出的路径启动译本，检查标题、菜单、对话、换行和插件界面。
发现问题时，把场景、截图和触发步骤发回同一任务，继续修订。
翻译流程完成后，仍需通过[人工实机验收](docs/guides/acceptance.md)确认显示和玩法正常。

## 工作原理

ATT 将游戏格式、翻译状态和模型请求分开处理：

1. **提取**：Agent 调查文本来源并确定规则，ATT 将原文、语境和写回关系整理进项目数据库。
2. **规划**：先按完整原文建立稳定任务块，再判断已有译文与重复文本；需要请求时保留完整语境。
3. **翻译**：模型返回候选，ATT 逐项验收结构和 Placeholder，并按自然顺序提交结果。
   源语残留等质量提示交由 Agent 审校。
4. **写回**：从当前有效译文构建独立候选，验证后发布。MV/MZ 输出译后游戏；Generic 输出译后 JSONL，
   由外部转换流程放回原格式。

数据库保存当前项目事实和译文，日志与任务记录用于复查。中断后依据已经确认的状态继续处理，
具体失败与恢复方式见[诊断与恢复指南](docs/guides/diagnosis-and-recovery.md)。
图片文字、特殊脚本和非标准插件内容由 Agent 根据实际结构选择相应工具处理。

完整流程从[翻译 Skill](skills/translate-with-att/SKILL.md)进入；大规模术语候选可使用随包
[Formic](tools/formic/README.md)，启用前单独配置其模型服务。

## 反馈与联系

欢迎通过 [GitHub Issues](https://github.com/yexi-by/att/issues)反馈问题或建议。
我可能无法及时查看和回复，但仍建议优先提交 Issue，方便保留上下文、跟踪处理，也帮助遇到同类问题的人。
反馈时请附上 ATT 版本、复现步骤和相关日志，并移除密钥等敏感信息。

也可以添加我的个人联系方式，注明 ATT；已有 Issue 时请一并附上链接：

- Telegram：[@yexi666](https://t.me/yexi666)
- QQ：21729598122

## 许可

ATT 以 [AGPL-3.0-only](LICENSE) 发布。随包第三方工具与依赖的许可见 `licenses/` 和各工具目录。
