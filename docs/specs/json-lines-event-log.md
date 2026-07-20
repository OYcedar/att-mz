# 强审计 JSON Lines 账本现行规格

本文定义 RPG Maker MV/MZ 命令共同使用的强审计账本。它不是可丢失的排障日志：业务意图必须先持久化，才允许开始对应副作用；副作用取得终态后必须记录终态，才可报告完整成功。

## 1. 唯一账本

固定路径为：

```text
<observability.root>/
├─ audit.jsonl
├─ .audit.lock
└─ audit.00000000000000000001.jsonl
```

账本拥有一个有界队列、一个专用 worker、一把跨进程文件锁、一份活动文件和一个轮转序列。物理行顺序是所有进程的全局权威顺序。

每条记录是无 BOM 的紧凑 UTF-8 JSON，后接一个 LF。根在实际打开目录、锁、文件和执行 `sync_data` 时验证这些机制可用，不以文件系统品牌名称代替能力检查。

## 2. 稳定 wire

每行顶层字段固定为：

```text
recorded_at_utc
event_id
run_id
engine
project
command
profile | null
event
payload
```

- `recorded_at_utc` 由 worker 在持久化时生成，格式为 UTC 毫秒；
- `event_id` 是每条记录独立的 UUID；
- `run_id` 标识一次命令运行；
- `engine` 是 `mz | mv`，明确标识本次纵向切片；
- `command` 是 `init | extract | translate | write_back`；
- `profile` 仅 Translate 使用，其他命令写 `null`；
- `payload` 是 RPG Maker 可观测性模块拥有的稳定领域 DTO，通用 JSONL Runtime 不认识游戏引擎类型。

允许的事件只有：

```text
run_started
translation_task_started
translation_task_finished
write_back_publish_started
write_back_publish_finished
run_finished
```

翻译任务意图与终态、写回发布意图与终态分别共享一个稳定 `operation_id`。新一次命令总是生成新的 `run_id`，不得把重新运行猜成先前操作的恢复。

`translation_task_finished` 从任务结果枚举派生 `complete | partial | unavailable`、唯一非零尝试次数、可选供应商请求/响应 ID、可选最终 usage、验收决定、未解决结果、协议诊断和已确认数据库写入。`provider_response_id` 缺失时写 `null`；`confirmed_written_units` 明确统计语义文本单元。

审计位置的 `unit_role` 只有 `scalar`、`dialogue_speaker`、`dialogue_body`、`choices`、
`scrolling_text` 五类变体；`scalar` 同时携带字段语义键，任何角色都不携带物理行索引。
逐 ID 拒绝原因包括缺失或重复 ID、形状错误、行数不符、非法行元素、空槽不符以及既有
的空白、语言和 token 验收失败；这些诊断只记录 ID、逻辑位置与原因，不记录原文、译文
或消息正文。

`write_back_publish_finished` 保存目录发布的明确终态、实际布局、输出根、写回摘要、人工布局诊断和 `lua_executed`。摘要中的 `translated_units` 与 `original_units` 只统计语义文本单元。accepted、unresolved 使用 `{group_location, unit_role}` 语义文本位置；每项写回诊断使用非空 `locations` 列表关联一个或多个这样的受影响单元。物理修改目标、模型行数和最终物理命令数只属于内部写回，不进入这些计数。

账本不记录 API key、完整 messages、完整模型响应、完整原文或译文。外部松散字段只在当前事件确实需要无损承载时停留于 wire 边界，不扩散为 Runtime 业务模型。

领域记录使用借用序列化直接写入 worker 的复用缓冲；给定相同 ID 与时间戳时，字段顺序
和输出字节必须与上述稳定 wire 逐字节相同。既有行校验从原始 payload 片段恢复严格领域
类型并直接比较规范字节，不把 payload 转换成通用 JSON 树；这不增加字段或改变损坏判定。

## 3. 业务顺序

一次已经成功解析 CLI 和本次所需配置的命令按以下顺序审计：

1. 构造账本并持久化 `run_started`；
2. 只有该事件确认持久化后，才取得项目租约并执行业务；
3. Translate 在每个 TaskBlock 发起模型请求前持久化 `translation_task_started`；
4. 该任务完成内容验收和数据库提交/拒绝判断后，持久化 `translation_task_finished`；
5. WriteBack 在发布候选前持久化 `write_back_publish_started`；
6. 目录发布取得明确终态后，持久化 `write_back_publish_finished`；
7. 其他根完成 shutdown 后持久化 `run_finished`；
8. 最后关闭并排空审计 writer。

CLI 或配置尚未成功解析时没有业务运行，不建立账本。只有命令、非审计根 shutdown、所需终态事件和账本 shutdown 全部成功，CLI 才输出成功文案。

意图事件只有得到 `Persisted` 才可继续对应副作用。其他终态立即停止；可能已经存在的完整意图行作为未完成操作保留。副作用已经生效但终态记录失败时，结果必须明确表达“状态已生效但审计未确认”，不能误报普通失败或自动重做。

## 4. 通用追加机制

通用 JSONL Runtime 只负责：

```text
事件进入有界队列
  ↓
生成时间并序列化稳定 wire
  ↓
取得跨进程文件锁
  ↓
恢复并校验活动文件尾部
  ↓
按需轮转
  ↓
write_all(JSON + LF)
  ↓
sync_data
  ↓
执行配置授权的 retention
  ↓
返回持久化终态
```

只有 `write_all` 和 `sync_data` 都成功才算持久化。事件进入队列后由 worker 完成，即使调用 Future 被丢弃也不能撤销已经接管的写入。append 不自动重试。

同一次跨进程锁定内，完成尾部恢复与校验的活动文件句柄在未轮转时直接用于追加和
`sync_data`。下一次追加仍重新取得锁并确认当前活动文件，因此该复用不削弱文件替换
检测、完整行校验或逐事件同步。

终态固定为：

- `Persisted`：当前完整记录已经写入并完成 `sync_data`；
- `NotPersisted`：可确认当前记录未进入活动文件；
- `OutcomeUnknown`：写入、刷盘或 worker 交接后无法确认是否完整持久化；
- `PersistedButMaintenanceFailed`：记录已刷盘，但轮转或保留维护失败，并携带残留事实。

## 5. 尾部、轮转与损坏

每次追加都在同一跨进程锁内执行尾部恢复、校验、轮转、写入、刷盘和 retention。

- 最终片段没有 LF：视为崩溃留下的半行，截断到最后一个完整 LF 后 `sync_data`；
- 已带 LF 的完整行必须符合当前唯一 audit wire；
- 完整坏行、未知事件、未知字段、重复字段或类型漂移表示账本损坏，保留现场并拒绝追加；
- 不扫描或改写未知文件。

轮转名称固定为 `audit.` 加 20 位十进制序号和 `.jsonl`。活动文件非空且追加将超过上限时先轮转；新记录刷盘后再删除超出保留数量的最小序号。配置授权的旧轮转文件删除属于账本保留策略，不构成审计缺失。删除前必须复核枚举时与删除时的对象身份一致。

## 6. 完成边界

账本保证正常 Win32 故障和进程崩溃后可依据完整 LF 和严格 wire 恢复活动文件，不承诺任意硬件断电下绝对耐久，也不宣称 JSONL 与业务数据库或目录发布具有分布式原子提交。

进程崩溃可以留下已持久化而没有对应终态的意图；这是可审计事实，不由下一次命令猜测或自动恢复。业务状态仍由四个状态收敛命令及其各自权威存储决定。
