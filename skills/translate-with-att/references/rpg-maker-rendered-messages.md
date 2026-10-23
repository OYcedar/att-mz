# RPG Maker 动态消息的最终显示消费者

源字段已有当前译文，但战斗结果、人物姓名或其他动态组合消息在画面中仍显示原文时，按本页排查。优先从数据来源修复；仅当加载链路无法由静态提取规则覆盖时，才在游戏侧做最小显示适配。

## 1. 追踪消息生成与转义链路

原生 MV/MZ 的 `Game_Message.add` 将文本追加到 `_texts`，`allText` 用 LF 拼接后交给 `Window_Message.startMessage` → `createTextState` → `convertEscapeCharacters` → `Bitmap.drawText`（参考 [MV Game_Message](https://github.com/rpgtkoolmv/corescript/blob/master/js/rpg_objects/Game_Message.js)、[MV Window_Base](https://github.com/rpgtkoolmv/corescript/blob/master/js/rpg_windows/Window_Base.js)、[MZ Window](https://github.com/stak/rmmz-corescript/blob/master/rmmz_windows.js)）。

排查“源数据已译而画面仍是原文”时，沿活动插件的覆写顺序定位分叉点：

- 检查消息入队前是否由插件动态拼接了未提取的字面量或缓存了旧模板；
- 检查 `\N[n]`、`\P[n]` 或自定义姓名控制码在 `convertEscapeCharacters` 阶段读取的是哪个角色字段或插件私有表；
- 检查插件是否使用自有窗口或直接调用 `Bitmap.drawText` 绕过了标准消息队列。

## 2. 混合字段、标点与显示适配

- **模板与片段提取**：将未覆盖的动态模板或插件私有字段纳入 Rules 或 Generic 提取，保持颜色、字号、等待、图标等控制码符合 [Placeholder 规格](../../../docs/translation/placeholders.md)。
- **子类覆写与方法捕获**：插件子类可能重写了 `createTextState`，或在加载时捕获了旧基类方法引用。需要挂载游戏侧显示适配时，确认实际生效的类与加载顺序，在完整消息进入排版与宽度计算之前完成文本映射（不要在底层逐字调用的 `Bitmap.drawText` 才替换字符串，以免破坏游标推进与自动换行宽度计算）。
- **等价标点查找**：若因全角/半角标点差异导致运行时查找失配，仅在游戏侧显示映射的查找键上做规范化，不修改 ATT 原文、自然 ID 或已入库译文。
