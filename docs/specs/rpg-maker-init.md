# RPG Maker 项目初始化现行规格

Init 是 MZ 与 MV 命名工作区的状态收敛命令。两个引擎使用同一套项目、数据库、来源
指纹与发布能力；引擎切片只负责确认游戏目录布局，并把已经验证的内容根交给共享实现。

## 1. 命令与游戏目录

```text
att --config FILE mz init --name NAME --path GAME_ROOT ...
att --config FILE mv init --name NAME --path GAME_ROOT ...
```

- MZ 的 `GAME_ROOT` 必须直接包含普通 `data/`、`js/` 和 `js/rmmz_core.js`；
- MV 的 `GAME_ROOT` 必须直接包含普通 `www/`，且其中包含 `data/`、`js/` 和
  `js/rpg_core.js`；
- 两者都只接受游戏根，不探测另一种引擎，也不把传入的 `www` 自动修正为 MV 游戏根；
- 项目名和游戏根始终必填。源语言、目标语言及三个布局宽度在首次创建时全部必填；
  已有项目省略单项表示复用数据库中的当前事实。

项目按引擎分开定位，因此同名项目可以共存：

```text
<projects.root>/mz/<name>/
<projects.root>/mv/<name>/
```

工作区完整保留对应引擎的相对布局：

```text
MZ                              MV
<project>/source/data           <project>/source/www/data
<project>/source/js             <project>/source/www/js
<project>/write_back/data       <project>/write_back/www/data
<project>/write_back/js         <project>/write_back/www/js
```

原游戏路径只服务本次收敛，不进入 metadata。后续命令只读取工作区内的冻结来源。

## 2. 收敛、租约与来源指纹

`run_started` 持久化后，命令先取得对应引擎与项目名的租约，再观测工作区。目标不存在
时建立完整项目；目标存在时，`project.db` 必须属于同名项目并严格符合当前 schema，
否则作为无效项目数据库失败。

`source_snapshot_fingerprint` 是冻结内容树的 SHA-256。MZ 覆盖 `data/js`，MV 覆盖
`www/data/js`；哈希按稳定 Windows 名称顺序覆盖目录、文件长度与原始字节，并拒绝
reparse point、hardlink、名称碰撞、读取中对象身份变化和资源超限。Init 对传入游戏计算
期望指纹；Extract、Translate 和 WriteBack 每次开启项目时重新确认冻结来源仍与 metadata
一致。只有 Init 可以替换来源和权威指纹。

若现有工作区、来源、语言与布局全部一致，Init 直接返回 `Unchanged`，不建立候选，
也不清空既有输出。任何真实变化或可判定结构修复都在一个未发布候选内完成，并把
`write_back` 重建为空的对应引擎布局；候选只发布一次。

## 3. 当前项目数据库

项目数据库的当前核心结构为：

- `metadata`：项目名、规范源/目标 `LanguageId`、三个布局宽度和来源指纹；
- `standard_asset_owner_state`：`builtin | rules | lua` 的来源指纹与资产快照指纹；
- `standard_text_group`：逻辑组、组角色和完整投影/写回 recipe；
- `standard_text_leaf`：组内 `field_role`、原文、翻译上下文、译文与状态；
- `standard_text_target`：物理修改目标到逻辑组的唯一归属；
- `standard_translation_resource`：术语与自定义占位符的 canonical JSON；
- `standard_project_definition`：活动 MV 对话定义的 canonical JSON。MZ 使用同一结构，
  但不消费 MV 姓名投影。

逻辑译文身份是 `owner + group_location + field_role`，不等于物理 JSON 地址。删除 owner
状态会级联删除该 owner 的组、叶和目标。owner 状态同时绑定来源与完整资产快照，翻译
和写回均据此拒绝提取后发生的项目或资产变化。

来源变化保留既有 owner 快照与译文，直到下一次对应 Extract 用当前来源原子替换；语言
对变化清除所有标准译文与 state，并把术语表重置为空，保留占位符定义。布局宽度变化
只更新 metadata。

## 4. 发布与完成边界

Init 不直接修改可见工作区。创建、更新或修复都在候选中完成来源复制、数据库建立或
对账和空输出布局，然后通过可恢复目录发布器切换一次。数据库准备失败只丢弃未发布
候选；发布已经开始后必须等待明确终态，不猜测回滚。

锁顺序固定为：

```text
项目租约 → 对应引擎的目录发布锁 → SQLite
```

锁分别位于 `<projects.root>/.att-locks/projects/<engine>/` 和
`<projects.root>/.att-locks/directory-publish/<engine>/`。命令、候选终结、审计与全部根
shutdown 均成功后才呈现 `Created`、`Updated` 或 `Unchanged`。
