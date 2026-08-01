# ATT CLI 现行规格

## 1. 统一入口

```text
att [--ui-language LANG] [--progress auto|plain|off] ENGINE COMMAND ...
```

`ENGINE` 取 `mv`、`mz` 或 `generic`。Help 与 Version 开箱即用；其余命令读取实际运行的
`att.exe` 同目录下固定的 `config.toml`，并要求显式给出引擎、命令和项目名。CLI 不提供
配置路径参数。

当前命令：

```text
att mv|mz init --name NAME [--path GAME_ROOT]
  [--source-language LANG] [--target-language LANG]
  [--dialogue-max-fullwidth-chars COUNT]
  [--scrolling-text-max-fullwidth-chars COUNT]
  [--help-description-max-fullwidth-chars COUNT]

att generic init --name NAME [--path JSONL_ROOT]
  [--source-language LANG] [--target-language LANG]

att mz extract --name NAME [--builtin] [--rules RULES_TOML]
att mv extract --name NAME [--builtin] [--rules RULES_TOML]
  [--dialogue-rules DIALOGUE_TOML]
att generic extract --name NAME

att mv|mz|generic translate --name NAME [PROFILE_ID]
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML]

att mv|mz|generic write-back --name NAME

att mv|mz|generic lua --name NAME SCRIPT_LUA [-- ARG...]
```

Extract、Translate 和 WriteBack 不接受 `--lua`。独立 Lua 不接受 `--profile`。

## 2. UI 与进度

`--progress` 默认为 `auto`。UI 语言可由 `--ui-language` 或 `ATT_UI_LANGUAGE` 指定，
选择优先级为：

```text
--ui-language > ATT_UI_LANGUAGE > Windows 用户首选 UI 语言 > en
```

支持：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

区域变体按主语言归并，中文按简繁区域区分。显式非法值按输入错误处理；自动检测
匹配不上时回退英语。Prompt 固定使用中文指令并根据项目语言对渲染源语言和目标语言，
不跟随 UI 语言变化。

路径、ID 和动态文本进入终端与日志前，会先移除终端转义、换行伪装和双向控制字符。
十个 locale 各自提供完整界面文本，英文 Debug 文本不会拿来凑数。

## 3. 保存状态与省略参数

省略可选参数时，ATT 只在以下明确说明的情形复用项目状态：

- Generic 首次 Init 必须提供路径和语言；再次 Init 分项复用；
- MV/MZ 首次 Init 还必须提供三个正数全角布局宽度；再次 Init 分项复用；
- MV/MZ 首次 Extract 必须选择 owner；以后省略全部选项时复用完整 owner 集合；
- Translate 省略 Profile 时复用最近成功 Profile；省略术语或 Placeholder 时复用项目资源；
- Generic Extract 没有外部文件参数，始终读取项目绑定的当前 JSONL 根；
- WriteBack 没有运行方案；
- Lua 每次读取本次显式脚本。

从状态读取、业务执行、必要收尾到最终保存，项目租约全程在场，两条并发命令拼不出
一份不存在的选择。

## 4. 启动和资源

程序按命令的实际需要建立配置、项目、Prompt、Client、CPU、文件系统与 SQLite 能力：
不发模型请求的命令不构造 HTTP Client，不运行 Lua 的命令不构造 Lua VM。

文件解析与相互独立的工作默认并行；要求确定顺序的结果仍按自然顺序合并和提交。
处理窗口装满时上游原地等待——项目再大也只是多等一会儿，不会变成容量错误。

Ctrl-C 请求合作取消：

- 不再开始新的模型请求或文件任务；
- 等待已经进入提交或发布边界的操作得到明确结果；
- 回滚未提交的 Lua 事务；
- 保存已经确认的 Translate 前序进度；
- 完成已经形成请求、响应或明确终态的模型任务记录；
- 以退出码 `130` 结束。

## 5. 输出与退出码

stdout 呈现进度和最终业务结果，警告、降级和错误走 stderr。项目命令建立 RunId 后，
终端摘要、项目日志和可选模型任务记录共用同一个 RunId。

Partial、Unavailable、跳过项、人工布局、取消后已经生效的状态和任务记录旁路故障都
必须有结构化诊断。当前项目 JSONL 仍可写时，这些事实进入同一 RunId；项目日志无法建立
或继续写入时，stderr 显示具体阶段、对象、稳定 code、原因和处理办法。日志队列丢弃普通
事件时，警告同时报告实际丢失数量和日志路径，不能只显示笼统的降级横幅。

项目日志或任务记录故障本身不改变已经确定的业务结果和项目状态。用于呈现警告、错误、
成功结果或取消终态的 stdout/stderr 写入、flush、后台线程或 channel 失败时，进程不能
假装已经告知使用者，必须返回 `1`。

- `0`：命令得到明确成功结果，包括已明确的 Partial 或 Unavailable；
- `1`：输入、运行、提交、发布或呈现失败；
- `130`：受控取消。

状态已经明确但必须保留或处理恢复现场时，ATT 显示 `recovery_required`、影响范围、恢复
位置和下一步。只有提交、发布或进程异常使最终状态确实无法确认时才显示
`outcome_unknown`；两种终态都不能伪造成成功或回滚。
