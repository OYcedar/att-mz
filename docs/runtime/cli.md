# ATT CLI 现行规格

## 1. 统一入口

```text
att --config FILE [--ui-language LANG] [--progress auto|plain|off] ENGINE COMMAND ...
```

`ENGINE` 是 `mv`、`mz` 或 `generic`。除 Help 与 Version 外，配置、引擎、命令和项目名都
必须显式提供，没有默认配置路径。

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

`--progress` 默认为 `auto`。UI 语言可由 `--ui-language` 或 `ATT_UI_LANGUAGE` 指定，选择
优先级为：

```text
--ui-language > ATT_UI_LANGUAGE > Windows 用户首选 UI 语言 > en
```

支持：

```text
ar  zh-Hans  zh-Hant  en  fr  ru  es  ja  ko  vi
```

区域变体按主语言选择，中文按简繁区域选择。显式非法值是输入错误；自动检测不能匹配时
回退英语。Prompt locale 为 `auto` 时按项目目标语言选择可用 Prompt 资源；显式 locale
覆盖自动选择。两者都不改变项目语言。

路径、ID 和动态文本在终端与日志中移除终端转义、换行伪装和双向控制字符。十个 locale
必须分别提供完整界面文本，不以英文 Debug 文本代替。

## 3. 保存状态与省略参数

省略可选参数只在文档明确说明时复用项目状态：

- Generic 首次 Init 必须提供路径和语言；再次 Init 分项复用；
- MV/MZ 首次 Init 还必须提供三个正数全角布局宽度；再次 Init 分项复用；
- MV/MZ 首次 Extract 必须选择 owner；以后省略全部选项时复用完整 owner 集合；
- Translate 省略 Profile 时复用最近成功 Profile；省略术语或 Placeholder 时复用项目资源；
- Generic Extract 没有外部文件参数，始终读取项目绑定的当前 JSONL 根；
- WriteBack 没有运行方案；
- Lua 每次读取本次显式脚本。

项目租约覆盖状态读取、业务执行、必要收尾和最终保存，防止并发命令拼接出不存在的选择。

## 4. 启动和资源

程序按命令需要建立配置、项目、Prompt、Client、CPU、文件系统与 SQLite 能力。没有模型
请求的命令不构造 HTTP Client；没有 Lua 的命令不构造 Lua VM。

文件解析与独立工作默认并行。需要确定顺序的结果按自然顺序合并和提交。窗口已满时上游
等待，不把项目总量当作容量错误。

Ctrl-C 请求合作取消：

- 不再开始新的模型请求或文件任务；
- 等待已经进入提交或发布边界的操作得到明确结果；
- 回滚未提交的 Lua 事务；
- 保存已经确认的 Translate 前序进度；
- 以退出码 `130` 结束。

## 5. 输出与退出码

stdout 只呈现进度和最终业务结果；stderr 呈现警告、降级和错误。项目命令建立 RunId 后，
同一 RunId 用于终端摘要、项目日志和可选模型任务记录。

- `0`：命令得到明确成功结果，包括已明确的 Partial 或 Unavailable；
- `1`：输入、运行、提交、发布或呈现失败；
- `130`：受控取消。

结果不明确时必须明确显示 `outcome_unknown`、影响范围、恢复位置和下一步，不得伪造成功
或回滚。
