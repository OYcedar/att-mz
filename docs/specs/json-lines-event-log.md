# JSON Lines 持久事件日志现行规格

本文定义 MZ Standard Translate 与完整 WriteBack 共用的生产日志根。日志根把已经建立的结构化业务事件转换为稳定 wire，串行追加到两条彼此独立的全局 JSON Lines 流，并在单条记录完成数据刷盘后才向调用方确认。

## 1. 双流与固定路径

日志根由配置中的 `observability.root` 建立。该目录必须位于本机、大小写不敏感的 NTFS 文件系统；从卷根到该目录的任一路径组件都不得是符号链接、junction、mount point 或其他 reparse point。两条流使用固定文件名：

```text
<observability.root>/
├─ translation.jsonl
├─ write_back.jsonl
├─ .translation.lock
└─ .write_back.lock
```

- `translation.jsonl` 只接收 Standard Translate 事件；
- `write_back.jsonl` 只接收完整 WriteBack 事件；
- 两条流分别拥有有界队列、专用 OS worker、跨进程文件锁、活动文件和轮转文件；
- 一个流的排队、锁等待、轮转或失败不会改变另一条流的物理文件；
- 文件物理行顺序是该流的全局权威顺序。

锁文件和活动文件也以不跟随 reparse point 的 Win32 方式打开；已有同名 reparse point 会在读取、写入或加锁前被拒绝。

每条记录都是紧凑 UTF-8 JSON，后接且只接一个 LF。文件没有 BOM。配置分别为两条流建立队列容量、锁等待上限、单条记录字节上限、活动文件字节上限和轮转文件保留数量；单条记录上限必须能够装入活动文件上限。

## 2. 运行身份与运行上下文

`RunIdGenerator` 在一次命令开始业务副作用前生成运行身份。生产实现直接调用 Windows 系统安全随机源，并建立规范小写 UUID v4；随机源失败时显式终止当前命令，不使用时间、进程号或伪随机回退值。

Translate 在项目和 Profile 成功解析后建立一次以下上下文：

```text
run_id + project + profile
```

WriteBack 在项目成功解析后建立一次以下日志上下文：

```text
run_id + project
```

实际布局宽度由已经打开项目的 WriteBackService 作为完成事件事实提交，组合边界不为构造日志根重复读取项目数据库。同一次 WriteBack 运行的事件复用同一个日志上下文。日志根不从事件文本、文件路径或先前记录猜测运行归属。WriteBack 事件携带的项目必须与注入上下文一致，否则该条记录在写文件前拒绝。

`recorded_at_utc` 由日志 worker 在处理该事件时生成，格式固定为 UTC 毫秒：

```text
YYYY-MM-DDTHH:MM:SS.mmmZ
```

## 3. Translation 稳定 wire

Translation 流允许三种顶层事件：

| `event` | 顶层载荷 |
|---|---|
| `task_processed` | `recorded_at_utc`、`run_id`、`project`、`profile`、`task` |
| `task_commit_failed` | 上述字段、`task`、`commit_failure` |
| `run_completed` | `recorded_at_utc`、`run_id`、`project`、`profile`、`summary` |

`task` 固定包含：

```text
task_index
status
attempts
provider_request_id
provider_response_id
finish_reason
final_response_usage
accepted_decisions
confirmed_written_locations
accepted[]
unresolved[]
diagnostics[]
```

`status` 精确表达 `complete`、`partial` 或带结构化原因的 `unavailable`。`final_response_usage` 缺席时为 `null`；存在时只包含最终成功 HTTP 响应的 `prompt_tokens`、`completion_tokens` 和 `total_tokens`。HTTP `x-request-id` 与响应正文 completion ID 分别进入 `provider_request_id` 和 `provider_response_id`，二者不互相代替。

`accepted[]` 记录模型 ID、代表位置与全部传播目标；`unresolved[]` 记录模型 ID、完整位置集合和结构化拒绝原因；`diagnostics[]` 记录响应协议诊断。`task_commit_failed` 保留已经形成的内容验收结果和独立的 Store 失败事实，同时把 `confirmed_written_locations` 记为 `null`，不宣称数据库写入成功。

`summary` 固定包含：

```text
total_tasks
complete_tasks
partial_tasks
unavailable_tasks
accepted_decisions
written_locations
unresolved_decisions
unresolved_locations
protocol_diagnostics
recoverable_request_exhaustions
```

Translation 记录不包含 API 密钥、完整 messages、完整模型响应、完整原文或完整译文。结构化拒绝原因可以保存定位拒绝所必需的 token、原控制片段或源语残留片段，但不把业务正文作为日志载荷复制。

## 4. WriteBack 稳定 wire

WriteBack 流只允许 `run_completed` 事件，顶层字段固定为：

```text
recorded_at_utc
run_id
project
layout_profile
event
output_root
summary
manual_layout_diagnostics[]
lua_executed
```

`layout_profile` 保存本次实际使用的三个正整数宽度：

```text
dialogue_body_max_fullwidth_chars
scrolling_text_max_fullwidth_chars
help_description_max_fullwidth_chars
```

`summary` 固定包含译文位置数、原文位置数、自动换行单元数、插入换行数、插入全角缩进数和人工布局单元数。`manual_layout_diagnostics[]` 逐项保存结构化位置、`dialogue_body | scrolling_text | help_description` 区域和实际宽度；其数量必须与 `summary.manual_layout_units` 相等。`lua_executed` 精确记录本次唯一候选是否经过显式 Lua 阶段。

## 5. 结构化 MZ 位置

持久 wire 不使用 `MzLocation` 的展示文本。每个位置完整保存来源、路径步骤和 Tag 语义：

```text
location.kind = value
  source
  steps[]

location.kind = note_tag
  source
  container_steps[]
  tag_name
  occurrence

location.kind = comment_tag
  source
  command_steps[]
  tag_name
  occurrence
```

来源精确区分：

- `data`：标准数据文件名；
- `map`：`map_id`；
- `plugin_parameter`：插件索引、插件名和参数名。

路径步骤精确区分 `object_key`、`array_index` 和 `decode_json_string`。这些稳定 DTO 独立于领域类型定义 Serde；领域类型本身不直接派生持久格式。所有 wire 对象拒绝未知字段、重复字段、缺失字段和字段类型漂移。

## 6. 追加、确认与取消

一次 `append` 按以下顺序执行：

```text
结构化事件进入有界队列
        ↓
专用 worker 生成 recorded_at_utc 并序列化
        ↓
取得当前流的跨进程 Windows 文件锁
        ↓
恢复并严格校验活动文件尾部
        ↓
按需轮转
        ↓
write_all(JSON + LF)
        ↓
sync_data
        ↓
清理超出保留数量的已知轮转文件
        ↓
向调用方返回终态
```

只有 `write_all` 和 `sync_data` 都成功，当前记录才算持久化。仅进入队列、仅完成序列化或仅写入操作系统缓存都不能返回成功。

事件一旦成功进入队列，worker 就拥有其完成责任。调用方随后丢弃 `append` Future 只会丢失该调用方对终态的等待，不会撤销已经接管的写入。显式 finalizer 关闭新准入，排空已经接管的事件，等待 worker 退出并 join；进程组合边界必须持有并调用该唯一终结令牌。

## 7. 跨进程锁、尾部恢复与损坏判定

同一流的恢复、校验、轮转、追加、刷盘和保留清理都在对应 Windows 跨进程文件锁内完成，因此多个 `att.exe` 进程不会把两条记录交叉写入同一物理行。worker 从启动到明确终结始终持有日志根整条路径链的无删除共享句柄，日志根不能在取锁后被重命名、替换或换成 reparse point。

每次追加前，worker 从已经确认的同一文件身份与长度继续校验；文件身份改变或长度回退时，从头重新校验。校验规则为：

- 最终记录缺少 LF：把它视为进程中断留下的未完成尾部，截断到最后一个完整 LF 之后并执行 `sync_data`；即使该未完成尾部已经超过单条上限，也只按 LF 边界整体截断；
- 已有完整行：必须满足当前流的严格 wire，且总长度不得超过单条记录上限；
- 已带 LF 的完整坏行、未知事件、未知字段、重复字段或类型错误：拒绝后续追加，保留原文件供诊断，不猜测修复。

该恢复只处理活动文件尾部，不扫描或改写未知文件。

## 8. 轮转与保留

当活动文件非空，并且追加当前记录将超过活动文件字节上限时，先轮转活动文件：

```text
translation.00000000000000000001.jsonl
write_back.00000000000000000001.jsonl
```

序号固定为 20 位十进制。轮转只识别对应流名、精确 20 位序号和 `.jsonl` 后缀；其他文件不属于日志根，不删除也不改名。已识别名称的目录项必须能够以不跟随 reparse 的方式打开为普通文件，并在枚举时记录其卷与 file ID。新序号取当前已知轮转文件中的最大序号加一，rename 禁止覆盖现有目标。

当前记录通过 `sync_data` 后，日志根按序号从小到大删除超出配置保留数量的已知轮转文件。删除前以 DELETE 句柄重新打开对象，复核它仍为普通文件且 file ID 与枚举时相同，再通过同一句柄设置删除 disposition；枚举后换入的外来文件不得被删除。保留清理失败不能撤销已经持久化的当前记录，必须返回“已持久化但维护失败”终态并报告残留路径。

轮转开始前失败，或者原活动文件已移入轮转位置但建立新活动文件失败后能够完整恢复原文件时，当前记录确定未持久化。新活动文件建立和原文件恢复同时失败时，日志根不猜测物理文件归属，返回结果未知。

## 9. 持久化终态与完成边界

`append` 的失败终态固定为：

- `NotPersisted`：在当前记录开始写入前失败，当前记录确定没有进入活动文件，既有完整记录仍可信；
- `OutcomeUnknown`：已经开始写入或刷盘，或者 worker 接管后没有交还终态，无法确认当前记录是否完整持久化；
- `PersistedButMaintenanceFailed`：当前记录已经通过 `sync_data`，但轮转保留清理没有完全结束，并返回准确残留路径。

启动失败发生在接纳事件之前，精确区分日志根创建、NTFS 条件校验和 worker 创建失败。shutdown 精确区分 worker 完成报告丢失与 worker panic。

本契约保证正常 Win32 故障和进程崩溃后，可以按完整 LF 与严格 wire 恢复活动文件，并且不会把未刷盘的队列接纳误报为成功。它不宣称任意硬件断电下的绝对耐久；调用方必须根据上述终态决定是否可以继续业务流程，不得把结果未知或维护残留降级为完全成功。
