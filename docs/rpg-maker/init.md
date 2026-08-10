# RPG Maker Init 现行规格

首次建立项目：

```text
att mv init --name NAME --path GAME_ROOT \
  --source-language LANGUAGE --target-language LANGUAGE

att mz init --name NAME --path GAME_ROOT \
  --source-language LANGUAGE --target-language LANGUAGE
```

项目工作区固定为：

```text
<att-dir>/projects/<mv|mz>/<name>/
```

首次 Init 需要一次给齐游戏根和语言对。再次 Init 可以分项省略未改变的值，沿用项目当前
设置。MV 和 MZ 项目即使同名，也各自拥有独立工作区。

## 来源

ATT 按引擎核对游戏根的必要目录和文件；遇到错误引擎、无效 JSON、无效 UTF-8、硬链接
歧义或不安全的目录关系时明确拒绝。检查通过后，在项目工作区建立只读来源副本，之后
Extract 与 WriteBack 只与这份副本打交道，原游戏始终保持原样。

再次 Init 时：

- 规范内容没有变化：保留提取资产和译文；
- 来源、引擎或语言语义变化：替换来源，并清除不能再证明有效的状态；
- 输入检查或发布失败：保留旧来源与项目状态。

Init 取得项目排他租约，先建立和验证候选，再一次发布来源与数据库结果。目录发布的
恢复规则见[目录发布规格](../runtime/directory-publishing.md)。

项目数据库必须符合当前代码声明的精确 schema。ATT 不识别旧格式，不迁移，不自动修复，
也不从其他项目复制译文；不符合时按当前项目数据库损坏报错。
