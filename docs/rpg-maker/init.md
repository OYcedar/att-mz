# RPG Maker Init 现行规格

Init 绑定游戏根和语言对，并把本次来源冻结到项目工作区。后续 Extract 与 WriteBack 使用这份
来源副本，原游戏保持原样。

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

## 来源检查与冻结

`GAME_ROOT` 是游戏根目录。MV 在根下使用 `www/data/` 与 `www/js/`，MZ 使用 `data/` 与
`js/`；传入路径应包含这些相对目录。ATT 核对必要布局、文件安全和来源快照一致性。错误引擎、
硬链接歧义或不安全的目录关系会使 Init 失败。项目及发布目标的存储条件见
[目录发布规格](../runtime/directory-publishing.md)。

检查通过后，ATT 逐字冻结引擎内容目录中的 `data/` 与 `js/`。Init 不全面解析其中的 JSON 或
验证其 UTF-8 内容；Extract 在读取所选来源时检查它需要的 JSON 语法与结构。因此，Init 成功
表示来源快照已经建立，Extract 成功才表示所选内容可以提取。

游戏根存在标准 NW.js `package.json`，且 `main` 指向根内安全的相对 `.html` 普通文件时，
ATT 还逐字冻结该 `package.json` 与活动 HTML。二者共同参与来源快照身份；修改任一文件后，
再次 Init 会建立新快照。

## 再次 Init 与失败处理

再次 Init 时：

- 规范内容没有变化：保留提取资产和译文；
- 来源或语言变化：替换来源或项目语言事实，保留已有正文和适用性状态；状态只在与当前事实
  精确匹配时消费，语言事实恢复且其他绑定事实未变时可以重新成为 Current，来源影响由后续
  Extract 根据新快照重判；
- 输入检查或发布失败：保留旧来源与项目状态。

Init 取得项目排他租约，先建立和验证候选，再一次发布来源与数据库结果。目录发布的
恢复规则见[目录发布规格](../runtime/directory-publishing.md)。

项目数据库必须符合当前代码声明的精确 schema；不符合时按当前项目数据库损坏报错。Init
只处理当前项目格式，不执行格式迁移、自动修复或跨项目复制译文。
