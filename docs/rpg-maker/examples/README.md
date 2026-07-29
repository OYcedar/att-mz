# RPG Maker 规则与 Lua 可执行示例

本目录同时保存生产解析器直接读取的完整 TOML，以及当前 Lua API 的完整主程序。测试可
把 TOML 原样交给对应生产解析/编译边界，并把 Lua 交给真实 VM、临时 `project.db`、冻结
JSON 夹具，以及需要模型的示例所用假 LLM。

| TOML | 生产边界 | 覆盖 |
|---|---|---|
| [`mv-dialogue.toml`](mv-dialogue.toml) | MV 姓名定义解析/PCRE2 编译 | marker 与整行姓名 |
| [`extract-rules.toml`](extract-rules.toml) | Extract Rules 解析/编译 | file、plugin、command、路径、逐层/终点解码、pattern |
| [`placeholders.toml`](placeholders.toml) | Placeholder 解析/编译 | 全局/显式 scope、whole/wrapper |
| [`terminology.toml`](terminology.toml) | Terminology 解析 | 默认/显式 triggers、Markdown 字面字符 |

| 文件 | 阶段 | 演示 |
|---|---|---|
| [`lua-standard-data-file.lua`](lua-standard-data-file.lua) | Extract | 自定义 DataFile 标量接入 Standard |
| [`lua-managed-translation.lua`](lua-managed-translation.lua) | 三阶段 | Managed 完整快照、托管翻译和按 metadata 安全写回 |
| [`lua-prepare-content.lua`](lua-prepare-content.lua) | Translate | `single/reflow/lines/items` 的低级结构化准备、验收与 Current |
| [`lua-translate-state.lua`](lua-translate-state.lua) | Translate | 首跑请求、二跑 Current、语义变化失效、译文/state 同事务 |
| [`lua-accept-standard.lua`](lua-accept-standard.lua) | 独立 Lua | Standard 人工候选验收、去重传播与原子提交 |
| [`lua-edit-managed.lua`](lua-edit-managed.lua) | 独立 Lua | Managed missing/stale 补译与显式 Current 修订 |
| [`lua-managed-replace-text.lua`](lua-managed-replace-text.lua) | WriteBack | 由 Managed metadata 定位完整 Value 并交给 Host 安全写回 |
| [`lua-idempotent-write-back.lua`](lua-idempotent-write-back.lua) | WriteBack | 从权威候选重建相同输出，不依赖 post-publish |
| [`lua-private-tag.lua`](lua-private-tag.lua) | 三阶段 | Lua 私有解析、验收、持久化并完整重建 `<Help:...>` Value |
| [`lua-complex-protocol.lua`](lua-complex-protocol.lua) | 三阶段 | 跨 Actors/Map/两个私有文档的多目标私有协议 |

Managed 示例不建立私有翻译表：ATT 负责 collection/unit 的普通翻译状态，Lua 只声明
来源关系，并根据 `metadata` 建立冻结来源的完整 text 引用交给 Host 安全写回。三阶段
私有协议示例只创建 `lua_example_*` / `lua_private_tag_*` / `lua_complex_*` 私有表。
人工候选示例不直接修改 ATT 受管表，而是交给 Standard 或 Managed 状态所有者。文件名和
夹具字段是可替换的示范协议；复制到真实游戏时，必须把来源、身份、上下文和物理断言改成
该游戏的已验证事实。

所需最小夹具形状：

<!-- att-example: illustrative -->
```json
// data/QuestEntries.json（用于 Standard、Managed 和私有表示例；实际 JSON 不含本注释）
[
  {"id":"arrival","title":"星港へ","description":"港へ向かう。"}
]
```

`lua-accept-standard.lua` 另假定 Builtin 已从 `data/Items.json[1]` 提取
`description = "药水"`；真实项目必须把完整身份、原文断言和候选一起替换。

`lua-private-tag.lua` 使用同一 `Items.json[1]` 的
`note = "<Help:炎の剣の説明>"`。这个外壳完全由示例 Lua 解释；Host 只读取和写入完整
Value。

<!-- att-example: illustrative -->
```json
// 复杂协议的 data/QuestGraph.json
[
  {"id":"arrival","actorId":1,"mapId":1,"title":"星港へ"}
]
```

<!-- att-example: illustrative -->
```json
// 复杂协议的 data/QuestIndex.json
{"arrival":{"label":"星港へ"}}
```

复杂协议还读取标准 `Actors.json[1].name` 与 `Map001.json.displayName` 组成语义上下文。
