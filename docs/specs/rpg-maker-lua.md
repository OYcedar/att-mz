# RPG Maker 可信 Lua 运行现行规格

本文定义 Extract、Translate 与 WriteBack 可信 Lua 的唯一 `ctx` 门面。Lua 可以直接
复用 Rust 的 JSON、RPG Maker 文档、翻译准备、布局和受控文件能力，同时保留完整 SQLite
逃生口；各阶段没有的能力明确为 nil。

## 1. 调用顺序与所有权

一次调用固定执行：

```text
读取完整主程序
      ↓
打开项目 SQLite 交互会话
      ↓
构造 Host calls + 唯一 session finalizer
      ↓
runtime.start(program, bindings) 同步接管
      ↓
等待 VM 终态与清理终态
```

当前一次命令最多选择一个脚本。每次 `start` 创建一个专用 OS 线程；一旦同步接管 bindings，
无论线程创建失败、关闭竞态、VM 失败、取消或 worker panic，都必须产生执行报告并
恰好调用一次 `finalize(self)`。execution handle 被丢弃只发送合作取消；Runtime
仍拥有 finalizer。执行错误和清理错误分别保存，清理错误不覆盖主错。

## 2. VM、租约与可信边界

生产 Runtime 使用进程内 vendored Lua 5.4，每次脚本的 VM 只在其专用 OS 线程上
创建、运行和销毁。SQLite、LLM 和 Host 文件 Future 由主 Tokio Runtime 驱动；Lua
worker 通过同步桥等待结果，不建立私有 Tokio Runtime。

VM 开放完整标准库，包括 `require`、`io`、`os` 和 `debug`。主程序目录加入该 VM 的
`package.path/cpath`，不修改进程 cwd；路径必须无损转换为 UTF-8。Unicode Lua/C
模块搜索和 Lua 5.4 `luaopen_*` 规则由 Runtime 实现，模块句柄持有到 VM 销毁。

每个 VM 使用配置给定的内存、线程栈、Host 值和取消检查预算。完整标准库意味着脚本可调用
`os.execute`、加载 native 模块、替换 debug hook，甚至通过 `os.exit` 或 native crash
终止进程。因此只承诺进程仍存活且 VM/Host 调用交还控制时的合作取消和唯一清理。

Lua 调用始终处于外层命令持有的项目租约内。脚本用 `os.execute` 再启动同项目 ATT
命令不能重入；子进程等待租约并最终返回 `ProjectBusy`，脚本不应同步等待自己持有
的同项目命令。

## 3. 最终 `ctx` 形态

```lua
ctx = {
  phase = "extract" | "translate" | "write_back",
  project = {
    name = string,
    source_root = string,
    database_path = string,
    source_language = string,
    target_language = string,
  },
  json = JsonApi,
  source = SourceApi,
  rpg_maker = RpgMakerApi,
  db = DatabaseApi,

  extract = ExtractApi | nil,
  translation = TranslationApi | nil,
  llm = function(messages) -> response | nil,
  output = OutputApi | nil,
  write_back = WriteBackApi | nil,
}
```

| 阶段 | 始终存在 | 额外存在 | 必须为 nil |
|---|---|---|---|
| Extract | `project/json/source/rpg_maker/db` | `extract` | `translation/llm/output/write_back` |
| Translate | `project/json/source/rpg_maker/db` | `translation/llm` | `extract/output/write_back` |
| WriteBack | `project/json/source/rpg_maker/db` | `output/write_back` | `extract/translation/llm` |

字段存在性是协议的一部分；Host 不注入会失败的占位函数。`project.source_root` 永远是
Init 冻结内容根：MZ 指向 `source/`，MV 指向 `source/www/`。WriteBack 的未发布候选
只通过 `ctx.output` 暴露。

## 4. 公共数据与来源门面

### 4.1 `ctx.json`

```text
NULL
array
object
number
decode
encode
kind
number_text
```

该门面无损区分 JSON null、array、object、string、boolean 与任意精度 number。
`number(text)` 从严格 JSON 数字文本建立值，`number_text` 取回规范数字文本；普通 Lua
number 不能替代不可精确表示的大整数。规范十进制文本能精确表示为 i64 时，`number`
和 `decode` 都建立 Lua integer；其他 JSON number 才建立精确 number userdata，二者不
产生两套数值类型规则。`decode` 完整消费一份 UTF-8 JSON，`encode` 产生规范 JSON；
object 键、数组连续性、循环引用和非法值都在边界明确拒绝。

### 4.2 统一 Host 值预算

`runtime.lua.host_values` 约束每次 Lua→Host 或 Host→Lua 值转换。根值深度为 1，容器
和标量各计一个节点，动态 UTF-8 字符串和二进制叶子的原始字节共同计入字节预算；
协议固定字段名不重复计入调用值。JSON 文本解码以完整输入字节为边界，编码以完整
输出字节为边界。

同一计数能力覆盖 JSON、来源和候选路径/内容、RPG Maker 路径、Extract groups、Translate
prepare/accept、LLM messages/response、SQLite 参数/结果及 WriteBack layout 参数/
结果。各门面不得另建不一致的节点、深度或字节上限。超过预算统一抛出
`domain="binding"`、`kind="host_value_budget_exceeded"` 的 Host error。

`json.array/object` 只给现有 Lua table 安装私有类型标记，`json.kind` 只查询该标记或
标量类型；二者不把 table 内容转换到 Host，因此不为标记或查询额外遍历整棵 table。
该 table 在 `json.encode`、`output.write_json` 或其他真正消费其内容的 Host 门面处，
才按完整值执行节点、深度和字节预算。这样私有标记是常数操作，同时任何实际跨边界
的数据都不能绕过预算。

### 4.3 `ctx.source`

```text
read
read_text
read_json
list
```

所有路径相对冻结 `source/`，拒绝绝对路径、父级逃逸、reparse point 和资源超限。
`read` 返回原始字节，`read_text` 要求 UTF-8，`read_json` 使用 `ctx.json` 的无损模型，
`list` 返回稳定直接子项。该门面只读，不访问 Init 时的外部游戏目录。

### 4.4 `ctx.rpg_maker`

```text
DECODE_JSON
data
map
plugin_parameter
open

document.value
document.location
document.text
document.note_tag
document.comment_tag
```

`data`、`map` 和 `plugin_parameter` 精确建立 RPG Maker 来源，`open` 从 `ctx.source` 打开并
解析受限文档。Document 的 value/location/text API 使用与 Rust Builtin、Rules、
Translate 和 WriteBack 相同的 `RpgMakerLocation`、嵌套 JSON 解码和字符串叶语义；
`DECODE_JSON` 明确表示穿过一层 JSON 字符串。Note/Comment Tag 使用共享标签解析器和
occurrence 定位，不按键名递归猜结构。

## 5. 数据库门面

`ctx.db` 在三个阶段都保留：

```text
NULL / blob
query / execute
begin / commit / rollback
```

SQLite NULL 映射 sentinel；INTEGER、有限 REAL、TEXT 和 opaque Blob 保持类型。普通
Lua string 永远是 TEXT。参数必须是无洞数组；query 返回按列顺序排列的二维数组。
`begin()` 固定执行 `BEGIN DEFERRED`。正常返回仍有活动事务时，finalizer 回滚并返回
`UnclosedTransaction`；先前显式提交保持。

完整 SQL 是可信脚本的逃生口。Lua 自建事务与 WriteBack 候选目录发布不是同一个
原子单元；脚本必须自行承担已提交数据库副作用的幂等和恢复语义。

## 6. 阶段专属核心能力

### 6.1 Extract：`ctx.extract`

```text
replace_standard
clear_standard
```

`replace_standard(snapshot)` 使用 `ctx.rpg_maker` 建立的逻辑组、叶、recipe 和物理目标，
按 Lua owner 原子替换 `standard_text_group/leaf/target` 并刷新 owner 的来源与资产快照
指纹。空 snapshot 是 active 空快照。`clear_standard()` 停用 Lua owner 并级联删除其
标准资产。Rust 按 `owner + group_location + field_role` 继承 translation/state；脚本
不手写标准表 SQL。

### 6.2 Translate：`ctx.translation` 与 `ctx.llm`

```text
translation.system_prompt
translation.language_pair
translation.prepare

PreparedText.status
PreparedText.model_text
PreparedText.terms
PreparedText.accept
```

`prepare` 复用当前持久术语/占位符快照、语言模块和逐叶 state 规则，返回 Current、
NotApplicable 或 Pending。脚本只为 Pending 组织自己的批次和 messages；
`model_text/terms` 提供受保护文本和本叶术语。`accept` 复用 Rust 的空白、ATT token、
源语残留、可选修复、控制符恢复及最终 state 计算，不能绕过验收伪造 Current。

`ctx.llm(messages)` 只接受无洞 `{ role, content }` 数组。它与 Standard 使用同一个
公共 Client 和 Executor；Lua 不能覆盖 URL、API key、model、stream 或 parameters。
成功返回 content、finish_reason、可选 HTTP request ID、可选正文 response ID 和可选
usage；缺失元数据在 Lua 中为 `nil`。
Lua 决定分组、调用次数、Retryable 重试和事务提交。

### 6.3 WriteBack：`ctx.output` 与 `ctx.write_back`

`ctx.output` 绑定本次尚未发布的唯一候选：

```text
read / read_text / read_json / list
create_directory
write / write_text / write_json
remove
```

路径使用逻辑 `data/...` 或 `js/...`，拒绝逃逸和 reparse point；MZ 直接映射到候选根，
MV 在 Host 边界映射到候选 `www/`。写入和删除只作用于候选。`write_json` 使用
`ctx.json` 规范编码。所有读写共同计入目录候选条目、深度、单文件和总字节预算。

`ctx.write_back.layout(region, segments)` 复用 Rust 保守布局器。region 只接受
`dialogue_body`、`scrolling_text`、`help_description`；segments 保留显式文本/控制
边界，返回自动布局结果或结构化人工诊断，不让 Lua 重写宽度算法。

Lua Host 只负责在调用方声明的逻辑 `{data, js}` 范围内修改候选，不拥有完整候选的
最终校验。RPG Maker WriteBack 验证 MZ 顶层恰好为普通 `data/js`，或 MV 顶层恰好为
普通 `www` 且其中恰好为普通 `data/js`。随后 Publisher 才复核完整候选并按值消费
token 发布。校验失败时只丢弃一次，并
同时保留校验首因与清理次错。Lua 看不到最终输出路径的可写句柄，也不能自行
validate、discard 或 publish。

## 7. Host 错误、取消与关闭

JSON、来源、RPG Maker、SQLite、LLM、候选文件和布局错误都以类型化 userdata 抛给 Lua：

```lua
{
  domain = string,
  kind = string,
  message = string,
  retry_after_ms = integer | nil,
}
```

`domain/kind` 可供 `pcall` 判断，message 只用于诊断且不含密钥或完整敏感载荷。未捕获
Host 错误作为 Binding 失败返回；Lua 语法、普通运行错误、上下文错误、取消和 worker
panic 保持独立终态。

RPG Maker 文档门面的错误固定使用 `domain="rpg_maker"`；不按 MV/MZ 重复建立错误域。

`ctx.json` 的结构、语法和参数错误固定使用 `domain="json"`、
`kind="invalid_value"`；包括 `array/object/number/decode/encode/kind/number_text` 在内
的所有 JSON 调用都经过同一类型化错误信封。错误被 `pcall` 捕获时四个公开字段可读，
未捕获时仍按其 JSON Host error 身份进入 Binding 终态，不降格为普通 Lua Execute。

Runtime shutdown 停止新的 `start`，对当前唯一脚本发出合作取消，等待唯一 finalizer
并 join 专用线程。取消是正常 `OperationCompletion::Cancelled`，不通过底层错误文本
识别。进程不设置超时后强拆，也不伪造清理成功。
