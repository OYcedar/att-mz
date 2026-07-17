# MZ 可信 Lua 运行现行规格

本文记录 MZ 在 Extract、Translate 与 WriteBack 三个阶段运行用户明确指定的
可信 Lua 程序时，Host、Lua 5.4 Runtime 和 SQLite 交互会话共同提供的当前契约。
Lua 程序拥有自己的数据协议、事务划分、模型响应解析、重试与幂等语义；标准
业务流程不解释 Lua 自有数据，也不替 Lua 猜测这些策略。

## 1. 调用顺序与所有权

一次调用固定按以下顺序建立资源：

```text
读取完整主程序
      ↓
runtime.reserve().await
      ↓
打开项目 SQLite 交互会话
      ↓
构造共享 Host calls + 唯一 session finalizer
      ↓
reservation.start(program, bindings)
      ↓
等待 VM 终态与清理终态
```

Runtime 容量必须先于数据库会话取得，排队期间不占用 SQLite 连接。数据库会话
打开后到 `start` 之间没有 `await` 窗口；`start` 同步接管 Host calls 与不可克隆的
唯一 finalizer，而且不返回移交失败。一旦接管，Runtime 的 job supervisor 必须在
VM 完成、失败、取消或 worker panic 后恰好调用一次 `finalize(self)`。

reservation 不可克隆。直接丢弃尚未启动的 reservation 只释放容量，不会打开 VM。
执行 handle 被丢弃只发出合作式取消信号；supervisor 继续拥有 finalizer，并把已
接管资源推进到明确清理终态。执行错误和清理错误分别保存，二者都发生时不得让
清理错误覆盖主错。

## 2. VM 与模块环境

生产 Runtime 使用进程内 vendored Lua 5.4。每个 VM 只在固定数量的专用 OS worker
上创建、运行和销毁，不启用 `mlua` 的 async 或 send 模式。SQLite 与 LLM Future
始终由进程主 Tokio Runtime 驱动；Lua worker 通过同步桥等待 Host 返回，不建立
自己的 Tokio Runtime。

VM 开放完整 Lua 标准库，包括 `require`、`io`、`os` 和 `debug`。主程序所在目录
加入该 VM 的 `package.path` 与 `package.cpath`，进程当前工作目录不改变。主程序
路径和所有项目路径必须能无损转换为 UTF-8；无法转换时在构造 `ctx` 前失败，不用
损失性字符串替代。

`require` 的模块搜索器依次为 preload、Unicode Lua 文件、Unicode C 模块和
Unicode C root 模块。每次搜索都读取该 VM 当时的 `package.path` 或
`package.cpath`，以 Windows Unicode 路径 API 加载文件，不经过窄字符文件搜索器。
C 模块入口严格采用 Lua 5.4 的 `luaopen_*` 点号替换、连字符前后缀回退和 C root
规则；已载入的模块句柄持续持有到 VM 完全销毁后再释放。

每个 VM 使用配置给定的内存上限，并按配置的指令间隔检查合作式取消。worker 数、
队列容量、worker 栈、单 VM 内存上限、取消检查间隔和最大错误字节数都由统一配置
边界建立，Runtime 不根据硬件或脚本内容自行推断。

## 3. `ctx` 精确接口

主程序获得一个全局 `ctx`：

```lua
ctx = {
  phase = "extract" | "translate" | "write_back",

  project = {
    name = string,
    source_root = string,
    database_path = string,
    source_language = string,
    target_language = string,
    output_root = string | nil,
  },

  db = {
    NULL = sentinel,
    blob = function(bytes) -> Blob,
    query = function(sql, parameters?) -> rows,
    execute = function(sql, parameters?) -> integer,
    begin = function(),
    commit = function(),
    rollback = function(),
  },

  llm = function(messages) -> response, -- 仅 Translate；其他阶段为 nil
}
```

`project.source_root` 是 Init 冻结的 `<workspace>/source`。`database_path` 是该工作区
的 `project.db`。只有 WriteBack 在 Standard 输出已经整体发布后获得
`project.output_root`，其值为固定 `<workspace>/write_back`；其他阶段该字段为 nil。

阶段能力固定如下：

| 阶段 | `ctx.phase` | `ctx.db` | `ctx.llm` | `project.output_root` |
|---|---|---|---|---|
| Extract | `extract` | 有 | nil | nil |
| Translate | `translate` | 有 | 有 | nil |
| WriteBack | `write_back` | 有 | nil | 已发布输出路径 |

Translate 的 `ctx.llm` 使用本次选中的同一翻译 Profile、共享 HTTP 连接池和速率额度。
其他阶段不构造 LLM 能力，也不注入占位函数。

## 4. SQLite 值与事务

SQLite 与 Lua 的值映射是确定的：

| SQLite | Lua |
|---|---|
| NULL | `ctx.db.NULL` sentinel |
| INTEGER | integer |
| REAL | number，NaN 与 Inf 拒绝 |
| TEXT | string |
| BLOB | opaque Blob userdata；`:bytes()` 返回原始字节 |

普通 Lua string 始终是 TEXT，不根据内容猜测 BLOB。只有 `ctx.db.blob(bytes)` 显式
建立 BLOB。参数数组必须从 1 开始、连续且没有其他键；nil 表示无参数。支持的参数
只有 NULL sentinel、integer、有限 number、string 和 Blob，boolean、table 及其他
userdata 均拒绝。

`query` 返回 `rows[row][column]` 的两层无洞数组，严格保留数据库列顺序，不根据列名
建立对象。`begin()` 固定委托 SQLite 会话执行 `BEGIN DEFERRED`；`commit()` 与
`rollback()` 使用同一连接。主程序正常返回时若 finalizer 观察到活动事务，会先
回滚并把本次调用报告为 `UnclosedTransaction`，不能伪装成成功。

## 5. LLM 接口

Translate 的 `ctx.llm` 只接受一个无洞 messages 数组。每一项必须且只能包含
`role` 与 `content` 两个字符串字段；role 只接受 `system`、`user`、`assistant`。
Lua 完整拥有响应 content 的解释、验收和是否再次调用模型的决定，Host 不修复或
解析其业务内容。

成功返回：

```lua
{
  content = string,
  finish_reason = string,
  request_id = string | nil,
  response_id = string,
  usage = {
    prompt_tokens = integer,
    completion_tokens = integer,
    total_tokens = integer,
  } | nil,
}
```

`request_id` 是 HTTP `x-request-id`，`response_id` 是响应正文 completion ID；两者
不得混用。usage 只描述本次成功 HTTP 响应。

## 6. Host 错误

SQLite、LLM、参数绑定和 Host 结果映射错误以 userdata 抛给 Lua，可由 `pcall` 检查：

```lua
{
  domain = string,
  kind = string,
  message = string,
  retry_after_ms = integer | nil,
}
```

`domain` 与 `kind` 是机器可判断字段，`message` 只用于诊断。Lua 捕获错误后可以按
自身策略继续；未捕获的 Host userdata 作为 Binding 失败返回。Lua 语法错误、普通
Lua 运行错误、上下文错误、取消和 worker panic 保持各自独立终态。

## 7. 取消、关闭与可信边界

Runtime shutdown 停止新 reservation，取消已预留、排队和运行中的脚本，等待所有
supervisor 完成唯一 finalizer，再 join 全部 Lua worker。没有超时后强拆或伪造成功。
正常进程边界必须显式等待 shutdown。最后一个 Runtime 句柄被直接释放时，实现仍会
停止准入、请求取消并关闭队列作为安全兜底，但调用方无法取得 worker join 结果，
不能把该路径视为已经确认的成功关闭。

Lua 脚本属于完全可信的本机程序。完整标准库意味着脚本可以调用 `os.execute`、加载
本地 C 模块、替换 debug hook，甚至通过 `os.exit` 或 native crash 直接终止进程。
因此系统不承诺任意可信脚本下都有界取消或必然 finalization。系统只承诺：进程仍
存活，并且 VM 或 Host 调用能够交还控制时，取消会被观察，Runtime 会执行唯一
finalizer，并准确报告 VM 与清理的两个终态。
