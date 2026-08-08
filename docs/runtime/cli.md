# ATT CLI 现行规格

## 1. 统一入口

```text
att [--ui-language LANG] ENGINE COMMAND ...
```

`ENGINE` 取 `mv`、`mz` 或 `generic`。Help 与 Version 开箱即用；其余命令读取实际运行的
`att.exe` 同目录下固定 `config.toml`，并要求显式给出引擎、命令和项目名。CLI 不提供配置
路径参数。

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

att mv|mz|generic manual export --name NAME FILE.toml
att mv|mz|generic manual check  --name NAME FILE.toml
att mv|mz|generic manual apply  --name NAME FILE.toml

att mv|mz|generic write-back --name NAME

att mv|mz|generic lua --name NAME SCRIPT.lua [-- ARG...]
```

Extract、Translate 和 WriteBack 不接受 `--lua`。Manual 不接受筛选、兼容格式或模型参数。
独立 Lua 不接受 `--profile`。

## 2. UI 与进度

实时进度没有 CLI 或配置选项。stderr 连接交互终端时，ATT 固定使用 20 格单行 ASCII
进度条并显示当前阶段和 `已完成/总量`；尚无真实总量时显示旋转符号和当前阶段。进度条在
同一行刷新，完成后清除，再输出最终业务结果。stderr 被重定向或运行于非交互环境时不输出
实时进度。

UI 语言可由 `--ui-language` 或 `ATT_UI_LANGUAGE` 指定，选择优先级为：

```text
--ui-language > ATT_UI_LANGUAGE > Windows 用户首选 UI 语言 > en
```

支持：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

区域变体按主语言归并，中文按简繁区域区分。显式非法值按输入错误处理；自动检测匹配不上
时回退英语。Prompt 固定使用中文指令并根据项目语言对渲染源语言和目标语言，不跟随 UI
语言变化。

路径、可读 ID 和动态文本进入终端与日志前，会先移除终端转义、换行伪装和双向控制字符。
普通 CLI 不显示 hash、UUID、数据库随机键、编码位置或供应商请求 ID。

## 3. 保存状态与省略参数

省略可选参数时，ATT 只在以下明确说明的情形复用项目状态：

- Generic 首次 Init 必须提供路径和语言；再次 Init 分项复用；
- MV/MZ 首次 Init 还必须提供三个正数全角布局宽度；再次 Init 分项复用；
- MV/MZ 首次 Extract 必须选择 owner；以后省略全部选项时复用完整 owner 集合；
- Translate 省略 Profile 时复用最近成功 Profile；省略术语或 Placeholder 时复用项目资源；
- Generic Extract 始终读取项目绑定的当前 JSONL 根；
- Manual 每次读取显式 TOML；
- WriteBack 没有运行方案；
- Lua 每次读取显式脚本。

从状态读取、业务执行、必要收尾到最终保存，项目租约全程在场，两条并发命令不能拼出一份
不存在的选择。

## 4. 启动、取消与资源

程序只按命令的实际需要建立配置、项目、Prompt、Client、文件系统、SQLite 和 Lua 能力。
Manual、Lua、Init、Extract 与 WriteBack 不构造模型 Client；Manual check/apply 和 Lua 高级
译文 API 也不会请求模型。

文件解析与相互独立的工作默认并行；要求确定顺序的结果仍按自然顺序合并和提交。处理窗口
装满时上游等待，不把合法项目总量变成容量错误。

Ctrl-C 请求合作取消：

- 不再开始新的模型请求或文件任务；
- 等待已经进入提交或目录交换边界的操作得到明确结果；
- 保存已经确认的 Translate 前序进度；
- 完成已经形成请求、响应或明确终态的模型任务记录；
- Lua 只回滚取消时仍打开的事务，之前的 autocommit 或显式 COMMIT 保留；
- 以退出码 `130` 结束。

## 5. Manual 输出

Manual 的完整格式和检查规则见 [Manual TOML 规格](../manual/README.md)。CLI 摘要只报告：

- export：导出条目数和目标文件；
- check：有效、未填写和错误数量；
- apply：已应用、未填写和错误数量。

检查错误逐项显示可读 `id`、原因和修改方法。未填写项不是错误，因此 check 在只有未填写项
时仍退出 `0`。apply 发现任一错误时不修改数据库并退出 `1`。

## 6. 运行文件

Init、Extract、Translate、WriteBack 和 Lua 建立自然序号 RunId；Manual 只输出自己的检查摘要，
不建立项目日志。RunId 形式为：

```text
run-000001
run-000002
```

同次运行的文件使用相同 RunId：

```text
<project>/logs/run-000001.jsonl
<project>/task-records/run-000001/task-000001.md
```

ATT 在项目租约内扫描既有日志和任务记录，并用原子创建保留下一个编号。冲突时递增，不用
UUID、hash 或额外计数数据库。

## 7. 输出与退出码

stdout 呈现最终业务结果，实时进度、警告、降级和错误走 stderr。错误只说明对象、直接原因
和修改方法，不倾倒内部阶段、状态字段袋、数据库行、SQLite code、查询文本、供应商请求 ID
或指纹差异。

项目日志仍可写时，警告和错误以同样的可读三字段保存；日志无法建立或继续写入时，stderr
直接显示这三项。Partial、Unavailable、人工布局、取消后已经生效的状态和任务记录故障都
必须可见，但不重复输出无实际作用的确认或内部诊断。

项目日志或任务记录故障本身不改变已经确定的业务结果和项目状态。用于呈现警告、错误、
成功结果或取消终态的 stdout/stderr 写入、flush、后台线程或 channel 失败时，进程不能
假装已经告知使用者，必须返回 `1`。

- `0`：命令得到明确成功结果，包括已明确的 Partial、Unavailable，以及只有未填写项的
  `manual check`；
- `1`：输入、检查、运行、提交、发布或呈现失败；
- `130`：受控取消。

状态已经明确但必须保留或处理恢复现场时，ATT 显示实际对象、原因、修改方法和自然恢复
路径。只有提交、发布或进程异常使最终状态确实无法确认时，才报告结果未知；两者都不能
伪造成成功或已经回滚。
