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

## 2. 验证与发布

WriteBack 启动时确认外部输入与最近 Extract 一致，先在候选目录生成全部文件，再使用生产
JSONL 解析器重新读取，并确认除 `text` 外的全部事实与当前输入一致。

候选完成后再次确认外部输入指纹。输入在生成期间改变时，候选不发布，并明确要求重新
Extract。

所有验证成功后，ATT 一次替换整个 `write_back/`。进入目录交换前失败或取消时，上一次
成功输出保持。发布结果无法确认时，ATT 保留恢复现场并如实报告实际影响，不宣称成功也
不擅自回滚；恢复位置按[目录发布规格](../runtime/directory-publishing.md)说明。
