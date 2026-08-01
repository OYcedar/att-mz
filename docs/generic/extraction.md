# Generic Extract 现行规格

```text
att generic extract --name NAME
```

Extract 读取项目当前绑定的外部 JSONL 根，严格按照
[JSONL 规格](jsonl.md)解析所有输入，并在一个数据库事务中替换项目的当前内容视图。
同一个项目可以重复 Extract 任意次数。ATT 不冻结或复制 Generic 输入；外部操作者可以
持续修改 JSONL，每次 Translate 或 WriteBack 前让 Extract 成功接收当前内容即可。

## 1. 一致读取

ATT 并发扫描、读取和解析文件，建立：

- 原始输入指纹：相对路径与原始文件内容；
- 资产指纹：解析后的 Group、kind、Unit ID、顺序与 text。

提交前 ATT 重新确认输入在读取期间没有改变。任一文件失败、身份重复或输入发生变化时，
整个 Extract 失败，数据库保持原状。

Extract 同时建立供 Translate 使用的稳定文本层次：一个 JSONL 文件是一个 Semantic Scope，
文件中的每行是一个 Group，`units` 数组中的每项是一个 Unit。文件、Group 和 Unit 的自然
顺序来自当前 JSONL 规格；Current、译文和后续模型任务不参与这次整理。

## 2. 修改后的状态

| 外部变化 | 结果 |
|---|---|
| JSON 空白、字段书写顺序或等价转义变化 | 保留译文 |
| Group 在文件内移动到其他行、文件改名或移到其他 JSONL | 保留译文 |
| Group ID 改变 | 删除旧 Group；新 Group 未翻译 |
| 只改变一个 Unit ID，kind、文本和顺序不变 | 该 Unit 作为新 Unit；其余 Unit 保留 |
| kind、Unit 数量、Unit 顺序或任一 text 改变 | 清除该 Group 全部译文 |
| 删除 Group | 删除该 Group 及其译文 |
| 删除 Unit | 删除该 Unit；Unit 数量改变，因此清除该 Group 其余译文 |
| 新增 Group | 新 Group 未翻译 |
| 新增 Unit | Unit 数量改变，因此该 Group 全部未翻译 |

ATT 只清除实际受影响的 Group 状态，其他文件或 Group 的 Current 译文原样保留。

成功提交后，数据库保存的内容是当前文件位置、Group、Unit、译文状态和输入指纹；源文件
副本、去重族、代表项、译文历史和 kind 注册表不在其中。

## 3. 后续命令

Translate 与 WriteBack 启动时重新计算外部输入指纹。指纹与最近成功 Extract 不同时，
命令在修改项目或输出之前停止，并明确要求重新 Extract；同步始终由 Extract 显式完成。
