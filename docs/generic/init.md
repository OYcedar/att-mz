# Generic Init 现行规格

首次建立项目：

```text
att --config CONFIG generic init --name NAME --path JSONL_ROOT \
  --source-language LANGUAGE --target-language LANGUAGE
```

首次 Init 必须同时提供输入根、源语言和目标语言。Init 只验证并保存项目身份、路径和
语言对，不扫描、解析或复制 JSONL。

项目工作区固定为：

```text
<projects.root>/generic/<name>/
```

再次 Init 同一项目时可以只提供需要改变的字段；省略值沿用项目当前值。源语言与目标语言
不得相同。改变输入根或语言对会使现有 Extract 快照和相关译文失效，但不会修改外部目录。

Init 取得项目排他租约并原子提交数据库。输入错误或提交失败时，旧项目保持不变。Generic
没有冻结 `source/` 目录。
