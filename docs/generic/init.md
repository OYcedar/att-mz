# Generic Init 现行规格

首次建立项目：

```text
att generic init --name NAME --path JSONL_ROOT \
  --source-language LANGUAGE --target-language LANGUAGE
```

首次 Init 同时提供输入根、源语言和目标语言。Init 验证并保存项目身份、路径和语言对；
JSONL 的扫描、解析和复制由 Extract 负责。

项目工作区固定为：

```text
<att-dir>/projects/generic/<name>/
```

已有工作区的数据库不符合当前 schema 时，ATT 不迁移、覆盖或自动复制旧项目译文。在同一
发行下使用新的项目名重新 Init，再从当前外部 JSONL 根执行 Extract；需要保留的旧译文
必须在项目外审查后，按当前项目能力重新翻译或精确修订。

首次 Init 若诊断 operation 为 `cleanup_generic_initial_database_candidate`，恢复路径是当前
工作区中的 `.project.db.init-*.tmp` 或它的 `-journal` / `-wal` / `-shm` SQLite sidecar。
ATT 没有公开命令清理它，后续 Init 也不会自动清理。保留该路径并使用新项目名重新 Init
可以继续任务；旧残留必须报告，只有操作者核实诊断给出的精确路径并明确授权外部删除后
才能处理。

再次 Init 同一项目时，只提供需要改变的字段，省略值沿用项目当前值。源语言与目标语言
必须不同。改变输入根或语言对会使现有 Extract 快照和相关译文失效，外部目录保持原样。

Init 取得项目排他租约并原子提交数据库。输入错误或提交失败时，旧项目保持不变。Generic
项目直接引用外部 JSONL 根，没有冻结的 `source/` 目录。
