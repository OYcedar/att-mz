# Generic WriteBack 现行规格

```text
att generic write-back --name NAME
```

输出固定为：

```text
<att-dir>/projects/generic/<name>/write_back/
```

命令永远不修改外部输入目录。

## 1. 输出内容

- 保留输入 `.jsonl` 的相对路径；
- 保留 Group 顺序、Unit 顺序、ID 和 kind；
- Current Unit 用译文替换 `text`；
- 其他 Unit 保留当前原文；
- 每条 Group 使用紧凑 JSON 占一行；
- 文件使用 LF，非空文件末尾有 LF；
- 输出只包含输入根内的 JSONL 文件，其他文件不复制。

Partial 项目允许写回。结果明确报告使用译文的 Unit 数与保留原文的 Unit 数。

候选写入前执行全局译文符号修复。修复器使用原文符号作为模板，只替换译文中能够唯一
对应的现有符号；不插入、删除或移动字符，译文空白和 Placeholder 逐字保留。局部多解不
妨碍其他确定位置继续修复；修复器内部无法安全完成时保留该 Unit 原译文并继续构建候选。
用户取消不计为内部跳过，仍按 WriteBack 的现有取消终态结束。发布汇总同时报告尝试、
实际修复、内部跳过的 Unit 数和替换符号数。

符号修复前，WriteBack 使用项目快照中的当前 Placeholder 规则重新保护原文和当前译文。
保护、绑定或语言投影失败时，命令以 `relative_path`、`group_id`、`unit_id`、`kind` 和
`side` 组成的结构化 Unit 诊断失败，不发布候选，也不把真实 Placeholder 错误计为内部跳过。

## 2. 验证与发布

WriteBack 启动时确认外部输入与最近 Extract 一致，先在候选目录生成全部文件，再使用生产
JSONL 解析器重新读取，并确认除 `text` 外的全部事实与当前输入一致。

候选完成后再次确认外部输入指纹。输入在生成期间改变时，候选不发布，并明确要求重新
Extract。

所有验证成功后，ATT 一次替换整个 `write_back/`。进入目录交换前失败或取消时，上一次
成功输出保持。发布结果无法确认时，ATT 保留恢复现场并如实报告实际影响，不宣称成功也
不擅自回滚；恢复位置按[目录发布规格](../runtime/directory-publishing.md)说明。

诊断属于目录发布器且恢复路径是 `.directory-publish-*.(stage|backup|journal)` 时，保持
项目、输入、目标和恢复产物不变，按[目录发布规格第 4 节](../runtime/directory-publishing.md#4-一次发布与恢复)
读取 `diagnostic.publication` occurrence 的 `report.effect`、`primary` 和递归 `related`。
目录发布 issue 直接保存 `output_root`、`candidate_root`、`residual_path` 或
`recovery_artifacts`，嵌套 backend diagnostic 保存具体文件系统 code、operation、I/O kind
和 OS code；不从本地化 message 或通用 detail 猜测。先排除 `filesystem.journal_corrupt`、
目标与已知旧目录均缺失、缺少必要 backup 等不能自动修复的情况，并修正实际文件系统
原因；只有符合自动恢复条件时，才执行一次同一项目、同一目标的 WriteBack。

若同一 occurrence 的 `related` 中存在 `relation = "cleanup"`，逐项读取其 FileSystem issue
中的精确 path：`.directory-publish-*` 仍按上一段条件判断，不能因为路径名匹配就直接重跑；
项目工作区中的 `.generic-write-back-*` 没有公开清理入口。两类残留可能同时存在，处理前者
不表示后者已经解决。对 scratch 残留同时读取主 report 和
`publication.finished.payload.result`：结果明确 `not_published` 时，修正原失败后可以重新
WriteBack，但新运行不会清除旧路径；旧残留必须报告，只有操作者核实精确路径并明确授权
外部删除后才能处理。result 为 `outcome_unknown` 或 report effect 为 `outcome_unknown` 时
禁止重跑试探，保留现场并报告。

Generic WriteBack 必须写 `publication.started` 和唯一 `publication.finished`。成功结果为
`published`，汇总 `files`、`translated_units`、`retained_source_units`、
`symbol_repair_attempted_units`、`symbol_repair_repaired_units`、
`symbol_repair_skipped_units` 和 `symbol_repair_replacements`；失败结果为 `not_published`、
`recovery_required` 或 `outcome_unknown`，并引用同一 `diagnostic.publication`
occurrence，不复制诊断。

发布完成后，外部操作者仍需消费全部译后 JSONL，并按
[全量验收指南](../guides/acceptance.md)核对完整写回、源语残留、组合项目和实际消费者。
Generic WriteBack 成功只证明 ATT 输出明确，不证明最终游戏已经采用译文。
