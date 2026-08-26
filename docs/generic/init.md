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

已有工作区的数据库必须符合当前代码声明的精确 schema。ATT 不识别旧格式，不迁移，不
自动修复，也不从其他项目复制译文；不符合时按当前项目数据库损坏报错。

首次 Init 使用固定候选：

```text
<project>/.project.db.init.tmp
```

候选可能有同名 `-journal`、`-wal` 或 `-shm` SQLite sidecar。失败时 ATT 尝试清理；清理失败
则诊断显示准确自然路径。后续 Init 不自动删除无法确认的残留，使用者应先解除占用并确认
候选不是需要保留的数据库，再按诊断处理。

再次 Init 同一项目时，只提供需要改变的字段，省略值沿用项目当前值。源语言与目标语言
必须不同。

改变输入根只替换项目绑定路径，不先删除最近成功 Extract 快照或译文。新根的完整原始
输入与已存快照一致时，Translate 和 WriteBack 继续使用该快照；不一致时必须先成功
Extract，在此之前不修改项目正文或输出。

改变语言对保留 Extract 快照和已有译文正文，清空旧目标语术语。旧语言对的人工与自动
正文都不再是 Current，不参与模型语境或 WriteBack，但在新结果通过验收并原子覆盖前
不得删除。外部目录始终保持原样。

Init 取得项目排他租约并原子提交数据库。输入错误或提交失败时，旧项目保持不变。Generic
项目直接引用外部 JSONL 根，没有冻结的 `source/` 目录。
