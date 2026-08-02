# Windows 文件能力与可恢复目录发布

## 1. 安全文件树

为了守住安全边界，ATT 拒绝 reparse point、硬链接、Windows 大小写等价冲突、路径
逃逸和读取期间对象身份变化；除此之外不设人为上限，文件字节、目录项、深度、树
总字节和恢复产物数量都不受限制。

Windows 等价名称按 ordinal ignore-case 的 UTF-16 code unit 判断，Unicode 规范化
和兼容折叠都不参与。每个逻辑路径独占一个物理普通文件：链接数不等于 1 时直接
拒绝，树外别名因此无法绕过指纹，树内也不会有多个路径共享同一份修改。

MV/MZ 来源冻结、Generic 输入一致读取和所有发布候选都建立在这些事实上。ATT 总是
读取完整文件；失败时报告真实的 OS 操作、路径和错误码，而不是看着 metadata 大小
提前拒绝。

独立文件并行处理，完成顺序不影响自然 ordinal 和主错误选择。目录遍历使用堆上
工作栈，再深的合法目录树也不会变成 Rust 栈溢出。

## 2. 租约与发布锁

项目租约位于：

```text
<att-dir>/projects/.att-locks/projects/<engine>/
```

目录发布锁位于：

```text
<att-dir>/projects/.att-locks/directory-publish/<engine>/
```

锁竞争时持续等待并随时响应取消；ATT 不设任意截止时间，也不设本地队列容量。
锁身份采用稳定 Windows 名称摘要。

## 3. 候选

候选请求包含目标、`CreateNew | ReplaceExisting`、来源映射、引擎生成的 overlay
和空目录。发布目标根必须是经过校验的绝对路径；来源映射、overlay 和空目录在候选内的
目标必须是安全相对路径。来源与候选物理隔离。

ATT 先建立带自然 ordinal 的 manifest，再并行复制未修改文件或写入最终字节；单个
文档内部需要保持顺序的结构修改仍串行。候选在发布前不可见，失败或取消时整树丢弃。

MV/MZ 候选完成 recipe 修改后执行 RPG Maker 全量验证。Generic 候选完成 text 替换
后使用生产 JSONL 解析器重新读取，并再次检查外部输入指纹。Lua 走自己的通道，
不参与目录候选。

## 4. 一次发布与恢复

完整候选只验证一次，覆盖普通对象、Windows 等价名称、reparse、硬链接、物理身份
和引擎领域结构。成功后只执行一次 journal 与目录交换。

`CreateNew` 只发布到不存在的目标；`ReplaceExisting` 使用同卷 journal、backup 和
稳定 file ID 完成可恢复交换。提交前失败，目标保持原样；已经生效但收尾失败、
需要恢复或结果为 `outcome_unknown` 时，ATT 分别报告准确影响和恢复路径。

候选一经交付发布器，publish 或 discard 必然运行到明确终态。取消只拦下新工作；
已经进入目录交换的操作会继续完成，不会停在中间状态。

恢复只枚举目标父目录的直接子项名称；仅读取或处理名称匹配当前目标受管前缀的
stage、backup 和 journal，不递归读取或删除无关内容。恢复依据来自 journal 而非项目
日志；主失败和候选清理失败都会保留。

每次 MV/MZ Init 都会在从 `project.db` 复用省略的游戏路径、读取现存工作区、继承设置或
判断 `Unchanged` 之前，对同一目标取得发布锁并恢复；任一 WriteBack 则在建立新候选之前
完成同一动作。ATT 扫描该目标父目录中
属于它的 `.directory-publish-*.(stage|backup|journal)` 受管产物，并按 journal 自动恢复或
清理。`prepare` 在锁内还会再次检查，防止恢复后到候选建立前状态变化：

- 公开记录没有 `PublishedWithResiduals` 字符串。主诊断或相关诊断的 `impact =
  "state_applied_finalization_failed"`，且 `recovery[*].path` 同时列出目标和受管残留时，表示
  新输出已经发布，只有收尾失败。RPG Maker WriteBack 还会写
  `publication.finished.payload.outcome = "recovery_required"`；Generic WriteBack 和 Init 不写
  这条事件，不能因事件缺失否定诊断。保留现场，先消除占用、权限或身份变化等清理失败
  原因，再用同一项目、同一目标和当前预期输入执行一次相应命令；MV/MZ Init 会先清理残留
  再读取现存项目，WriteBack 会先清理残留再建立新候选；
- 若主诊断或相关诊断的 `impact = "recovery_required"`，只有
  `recovery[*].path` 列出与同一操作匹配的 backup/journal 或可清理 stage，且
  `reason.failure` 不是 `journal_corrupt`、`reason.detail` 也不是“目标与已知旧目录均缺失”
  或缺少必要 backup，才先修正诊断中的文件系统原因，再用同一项目、同一目标和相同输入
  执行一次相应 MV/MZ Init 或 WriteBack；MV/MZ Init 会先恢复旧目标，再继承恢复后数据库的
  设置并重新判断 `Unchanged | Updated`，WriteBack 会先恢复旧目标或清理 stage，再处理新
  候选。I/O 原因读取 `reason.kind = "io"` 下的 `operation`、`error_kind` 和
  `raw_os_code`；其他失败读取 `reason.kind`、`reason.failure` 和可用的 `reason.detail`；
- 自动恢复本身报告 `reason.failure = "journal_corrupt"`、目标与已知旧目录均缺失、缺少
  必要 backup，或一次恢复后仍得到同样的 `impact = "recovery_required"` 时，现行接口无法
  修复；保留新诊断和全部 `recovery[*].path` 并报告，不继续重跑，也不手工删除、改名或移动；
- `outcome_unknown` 表示目标内容或身份无法确认，禁止用重跑试探，也不能自行处理目录。

当前 CLI 没有独立的 `recover` 或 `status` 子命令，也没有公开的 journal 解码与人工交换
步骤。满足上述条件时，同目标命令就是现行自动恢复入口；`outcome_unknown` 无法通过它安全确认时，
应报告当前公开能力限制。本节不处理 Generic 项目工作区中的 `.project.db.init-*.tmp`、
它的 `-journal` / `-wal` / `-shm` SQLite sidecar，或 `.generic-write-back-*`；它们的处理
见对应 Generic Init、WriteBack 和诊断指南。

## 5. MV/MZ Init 发布阶段的 OS 5

只在 MV/MZ Init 诊断同时包含以下事实时，把这次失败当作一次可重试的目录发布失败：

- code 为 `filesystem.operation`，阶段为“发布”；
- 结构化 IO 原因的 operation 是“无覆盖重命名”，`error_kind = permission_denied`，
  `raw_os_code = 5`；不解析本地化展示文本；
- 影响为“状态未改变”；
- `<att-dir>/projects/<engine>/<name>` 不存在，且诊断没有 `recovery_required` 或
  `outcome_unknown`。

满足全部条件后，不修改 `projects/` 中的任何文件或目录，直接用原来的 `att.exe`、
`--name`、`--path`、语言和三个宽度参数重跑一次 Init。不要借此更换配置、游戏路径或
项目名。

第二次成功时，以新项目的 `project.db` 和发布终态为准；第二次仍报普通错误或目标已存在
时停止这条 OS 5 重试分支。出现符合第 4 节条件的 `recovery_required` 时使用同目标 MV/MZ Init 的自动恢复
入口；出现 `outcome_unknown` 时停止写入。两者都不能手工删除、移动或编辑项目目录。
