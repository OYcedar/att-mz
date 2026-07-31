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

再次 Init 同一项目时，只提供需要改变的字段，省略值沿用项目当前值。源语言与目标语言
必须不同。改变输入根或语言对会使现有 Extract 快照和相关译文失效，外部目录保持原样。

Init 取得项目排他租约并原子提交数据库。输入错误或提交失败时，旧项目保持不变。Generic
项目直接引用外部 JSONL 根，没有冻结的 `source/` 目录。
