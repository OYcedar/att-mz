# ATT 生产运行时与 CLI 规格

## 1. 进程入口

ATT 只提供一个生产可执行入口：

```text
att [--config FILE] mz <init|extract|translate|write-back>
```

`AttArguments` 和 `MzCommand` 只负责 Clap 解析，不读配置、不构造根、不执行业务。
Help 和 Version 在解析边界直接返回，因此不依赖配置文件或任何生产资源。

当前产物只支持 `x86_64-pc-windows-msvc`。其他目标不是可降级运行环境，
构建时直接失败。

## 2. 启动顺序

生产进程固定按以下边界推进：

```text
Clap 解析
   ↓
定位并严格解析 TOML
   ↓
构造带 time / net / signal driver 的 Tokio Runtime
   ↓
ProductionMzCommandRunner 只构造当前命令所需的纵向切片
   ↓
执行唯一命令
   ↓
按固定顺序 shutdown 所有已构造根
   ↓
销毁 Tokio Runtime
   ↓
呈现业务结果或完整错误
```

配置中的相对路径以配置文件目录为基准。`projects.root` 在每个命令
构造文件根后统一规范化，并校验为本机、非大小写敏感的 NTFS 目录；
路径链中任一 reparse point 都会被拒绝。后续工作区布局只使用这一
规范路径。两条日志流在自身启动边界对 `observability.root` 执行同等的
本机 NTFS 校验。

## 3. 按命令构造

`ProductionMzCommandRunner` 不持有四棵预先构造的用例树。它每次只构造一条切片：

| 命令 | 实际构造的根与业务能力 |
|---|---|
| Init | `SystemFileSystem`、`RusqliteStorage`、工作区创建与 Init 编排 |
| Extract | 文件、SQLite、CPU 以及 Builtin/Rules 提取；只有显式 `--lua` 才构造 Lua Runtime 和 Host |
| Translate | 文件、SQLite、CPU、Delay、LLM、Translation JSONL 以及完整 Standard 翻译；显式 `--lua` 时再构造 Lua |
| WriteBack | 文件、SQLite、CPU、可恢复目录发布、WriteBack JSONL 与完整 Standard 写回；显式 `--lua` 时再构造 Lua |

Translate 先按 CLI 提供的精确 ID 选中一个 MZ Profile，再取得它引用的公共 LLM
Client。只有该 Profile 的系统提示词和全局 LLM TLS PEM 才会在本命令中读取；
系统提示词必须是非空白 UTF-8 Markdown。组合根只构造一个
`OpenAiChatCompletionClient` 和一个 `OpenAiChatCompletionExecutor`，并把同一份
不可变执行 Profile 交给 Standard 与 Translate Lua，因此两者共享 endpoint、直接
Bearer、model、额外 JSON 请求字段、连接池、总准入和客户端限速。

Translation 的 `run_id + project + profile` 在 Profile 选择和项目读取都成功后、
任何翻译副作之前建立。WriteBack 的 `run_id + project` 同样在项目实际打开后
建立；实际三个布局宽度由 Standard 已消费的权威项目事实随日志事件写入，
组合边界不为日志重复读取数据库。

## 4. Ctrl-C 与明确终态

进程每次只准入一个顶层命令。第一次 Ctrl-C 后不再建立新的业务入口，
并把同一个单向合作取消事实交给本次纵向切片。业务编排在每个阶段边界停止
派生后续工作，但不 drop 正持有目录候选、SQLite 操作或 Lua 唯一 finalizer
的高层 Future。

已构造 Lua 时，进程立即停止新 reserve，取消 queued/running job，并在继续
驱动业务 Future 的同时等待所有唯一 finalizer。Translate 还会立即停止
LLM 新准入；已活动 HTTP 与业务 Future 并发驱动到终态，不会因暂停
poll 活动请求而与 LLM shutdown 互等。

Init 在候选准备或建库返回后观察到取消时显式 discard，不再发布；一旦 publish
已被根接管便等待其明确终态。Extract 不再进入下一个 Builtin、Rules 或 Lua 阶段。
Translate 不再补入新的 TaskBlock，已经开始的 TaskBlock 仍按自然顺序完成验收、
提交和任务日志，取消运行不写完成汇总。WriteBack 在读取、布局、改写和发布边界
停止；候选 prepare 后取消会显式 discard，已经发布的输出仍必须写入发布日志。
可信 Lua 进入不交还控制的 native 调用时，进程不伪造超时或成功。

Ctrl-C 后，合作取消或正常完成且完整 shutdown 成功时退出码为 `130`，不呈现
业务完成文案。收尾期间产生的发布结果未知、持久化失败等技术终态必须保留并
呈现；业务技术终态或任一 shutdown 失败时退出码为 `1`。

## 5. shutdown 顺序

命令自己记录已构造的根，并只终结这些实例。完整 Translate 的顺序是：

```text
Lua 停止准入、取消并等待 finalizer
LLM 停止准入并等待活动请求
SQLite 停止 open、终结 session、排空短操作
FileSystem 排空文件、候选和恢复工作
CPU 停止准入、排空并 join
JSONL 关闭准入、排空并确认 sync_data
```

WriteBack 不构造 LLM，Extract 不构造日志，Init 只终结 SQLite 和 FileSystem。
未选择 Lua 时不存在 Lua shutdown 步骤。进程不设置“超时后强拆”；一旦根
已接管副作用，进程必须等待它返回明确终态。

## 6. 呈现与退出码

`CommandResultRenderer` 只在业务命令成功、所有 shutdown 成功且 Tokio Runtime 已销毁
后向 stdout 写入成功结果。Translate 的 `Partial` 和 `Unavailable` 是正常业务结果，
仍使用退出码 `0`。WriteBack 的人工布局诊断也是成功结果的一部分。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version 或命令正常完成 |
| `2` | Clap 参数错误 |
| `1` | 配置、根构造、业务技术失败或 shutdown 失败 |
| `130` | Ctrl-C 后所有可终结资源已受控收尾 |

命令失败和 shutdown 失败同时发生时，stderr 分别呈现两者，后发的清理失败
不覆盖业务首因。
