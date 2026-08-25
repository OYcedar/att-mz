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
- 使用随包 Formic 从完整游戏原文并发抓取术语候选，再由 Agent 统一筛成术语表；
- 按语境组织任务、消除重复，并调用兼容 OpenAI Chat Completions 或 Responses 的模型服务；
- 保护控制符和 Placeholder，统一应用术语表，并检查不允许的源语言残留；
- 持久保存翻译进度，中断后从已确认的结果继续；
- 在不修改原游戏的前提下生成译后目录，供运行和验收。

ATT 不替代游戏调查和最终验收。非标准插件数据、图片文字和游戏特有脚本仍需要 Agent 先确认
真实结构，再选择 Rules、Generic 或外部工具。

## 快速开始：搭配 Agent

### 1. 下载并配置 ATT

从 [GitHub Releases](https://github.com/yexi-by/att/releases/latest) 下载 Windows x64
发行包和 `SHA256SUMS.txt`，核对 SHA-256 后完整解压到可写目录。不要只复制 `att.exe`，
运行时还需要同目录的配置、文档、Prompt、Skill 和工具。更新已有安装时，不要把新 ZIP 直接
解压覆盖旧目录；先解压到新目录，再把旧目录根部和 `tools\formic` 中已经填写的两份
`config.toml` 复制到新目录。仓库资源同步脚本会自动执行同样的保留规则。

打开 `config.toml`，填写模型服务：

```toml
[llm.clients.primary]
protocol = "chat_completions"
url = "https://api.example.com/v1"
api_key = "replace-with-api-key"
model = "replace-with-model-id"
```

`protocol` 省略时同样使用 `chat_completions`；需要 Responses 时改为 `responses`。`url` 可以
填写基础地址或完整端点，ATT 会按协议补全路径。

`config.example.toml` 保存当前不含真实凭据的发行默认，供查看新字段和新默认；它不是已有活动配置
的替代品。服务明确限制上下文、输出、并发或请求频率时，再按实际限制降低相应配置。
制作术语表时还要填写 `tools\formic\config.toml`；普通更新同样不会覆盖这份活动配置。

### 2. 把翻译目标交给 Agent

将下面的内容发给能够执行命令和读写文件的 Agent，并替换尖括号中的内容：

```text
使用 <ATT目录>\att.exe 翻译 <游戏目录>。

开始前完整读取 <ATT目录>\skills 中适用于该游戏引擎和当前阶段的 Skill，并按其中引用顺序
查阅 <ATT目录>\docs。目标是把正常游玩时可见、且不是图片组成的 <源语言> 文本翻译为
<目标语言>。

先调查真实文本范围，再建立项目；根据游戏实际数据制作 Rules、对话姓名规则和 Placeholder。
制作术语表时读取 <ATT目录>\skills\extract-game-terminology\SKILL.md。不要猜测未确认的字段
或协议。使用 ATT 的正式流程完成提取、翻译、写回和验收，
不要修改原游戏目录。遇到无法由现行文档和生产命令确认的情况时停止，并说明具体对象、原因
和需要我决定的事项。最后报告译后目录、验收结果和仍未覆盖的内容。
```

### 3. 检查结果

Agent 会在 ATT 项目目录中生成译后游戏，不会覆盖原目录。先阅读 Agent 报告的验收结果，
再运行译后游戏检查标题界面、菜单、主要对话、换行和插件界面；发现问题时，把具体场景交给
同一个 Agent 继续修正规则、术语或译文。
