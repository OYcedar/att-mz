# ATT

ATT 是供 Agent 调用的游戏翻译命令行工具，运行于 Windows x64。它直接处理 RPG Maker
MV/MZ 游戏，也可以通过 Generic JSONL 接入其他游戏或自定义脚本中的文本；项目以
[AGPL-3.0-only](LICENSE) 发布。

## 为什么做 ATT

完整翻译一个游戏，不只是把句子送进模型。开始翻译前需要查清文本藏在哪里，翻译过程中
需要维持角色名和专名一致、保护控制符、处理中断与重试，结束后还要把译文安全写回并检查
是否遗漏。单靠聊天窗口很难稳定处理这些工作，单靠固定脚本又无法判断每个游戏的特殊情况。

ATT 因此把两类职责分开：程序负责可重复执行的数据处理和状态管理，Agent 负责调查游戏、
制作规则与术语表、检查译文并处理例外。这样既保留 Agent 的判断能力，也让长时间翻译可以
继续、复查和重新执行。

## ATT 解决什么问题

- 从 RPG Maker 数据库、地图事件和明确指定的插件参数中提取可翻译文本；
- 通过 Generic JSONL 接收其他引擎或自定义工具整理出的文本；
- 由 Agent 从完整游戏原文制作术语表；大量独立语料分片可以使用随包 Formic 生成候选；
- 按语境组织任务、消除重复，并调用兼容 OpenAI Chat Completions 或 Responses 的模型服务；
- 保护控制符和 Placeholder，统一应用术语表，并检查源语言残留；
- 持久保存翻译进度，中断后从已确认的结果继续；
- 以原游戏为只读基线，在独立目录生成译本，供人工运行和验收。

Agent 负责游戏调查、静态 QA、写回和封包，并根据非标准插件数据、图片文字和游戏特有脚本的
真实结构选择 Rules、Generic、图像翻译或外部工具；任务发起者指定的人工负责实机验收。

## 快速开始：搭配 Agent

### 1. 下载并配置 ATT

从 [GitHub Releases](https://github.com/yexi-by/att/releases/latest) 下载 Windows x64
发行包和 `SHA256SUMS.txt`，核对 SHA-256 后完整解压到新的可写目录。运行时让 `att.exe` 与同版
配置、文档、Prompt、Skill 和工具保持在同一发行根。更新已有安装时，以当前
`config.example.toml` 为字段权威，把旧配置中的实际服务取值逐项填入新活动配置；项目数据需要
转换时使用该版本明确提供的一次性转换步骤。

打开 `config.toml`，填写模型服务：

```toml
[llm.clients.primary]
protocol = "chat_completions"
url = "https://api.example.com/v1"
api_key = "replace-with-api-key"
model = "replace-with-model-id"
stream = false
```

`stream` 是必填项。`protocol` 省略时使用 `chat_completions`；需要 Responses 时改为
`responses`。`url` 可以
填写基础地址或完整端点，ATT 会按协议补全路径。`stream = true` 时以流式 HTTP 响应接收
模型结果，`false` 时等待完整 JSON；两种方式都会在响应完整结束后再验收和保存译文。

`config.example.toml` 保存当前不含真实凭据的完整字段和发行默认。旧活动配置提供实际服务取值
参考，当前模板定义字段集合。服务明确限制上下文、输出、并发或请求频率时，再按实际限制降低
相应配置。
制作术语表时，Agent 可以直接处理全部语料；选择 Formic 批量生成候选时填写
`tools\formic\config.toml`。普通更新保留这份活动配置。

### 2. 把翻译目标交给 Agent

将下面的内容发给能够执行命令和读写文件的 Agent，并替换尖括号中的内容：

```text
使用 <ATT目录>\att.exe 翻译 <游戏目录>。

完整读取 <ATT目录>\skills\translate-with-att\SKILL.md，并按其中引用读取当前发行版文档。
目标是把正常游玩时可见的非图片 <源语言> 文本翻译为 <目标语言>。

按“调查 → Extract → 术语 → Translate → QA → WriteBack → 字体/封包”推进。以原游戏为只读
基线，在独立目录生成译本；根据游戏实际数据制作 Rules、对话姓名规则和 Placeholder，并使用
<ATT目录>\skills\extract-game-terminology\SKILL.md 制作术语表。证据暂时不足的位置记录为待确认
项并交给我实机检查。最后报告译本目录、静态 QA 结果、人工检查项和待确认的翻译位置。
```

### 3. 检查结果

Agent 会在 ATT 项目目录中生成译后游戏，不会覆盖原目录。先阅读 Agent 报告的验收结果，
再运行译后游戏检查标题界面、菜单、主要对话、换行和插件界面；发现问题时，把具体场景交给
同一个 Agent 继续修正规则、术语或译文。
