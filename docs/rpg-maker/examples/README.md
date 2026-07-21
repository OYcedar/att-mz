# RPG Maker 规则与 Lua 可执行示例

本目录同时保存生产解析器直接读取的完整 TOML，以及当前 Lua API 的完整主程序。测试可
把 TOML 原样交给对应生产解析/编译边界，并把 Lua 交给真实 VM、临时 `project.db`、冻结
JSON 夹具和假 LLM。

| TOML | 生产边界 | 覆盖 |
|---|---|---|
| [`mv-dialogue.toml`](mv-dialogue.toml) | MV 姓名定义解析/PCRE2 编译 | marker 与整行姓名 |
| [`extract-rules.toml`](extract-rules.toml) | Extract Rules 解析/编译 | file、plugin、command、路径、逐层/终点解码、pattern |
| [`placeholders.toml`](placeholders.toml) | Placeholder 解析/编译 | 全局/显式 scope、whole/wrapper |
| [`terminology.toml`](terminology.toml) | Terminology 解析 | 默认/显式 triggers、Markdown 字面字符 |

| 文件 | 阶段 | 演示 |
|---|---|---|
| [`lua-standard-data-file.lua`](lua-standard-data-file.lua) | Extract | 自定义 DataFile 标量接入 Standard |
| [`lua-translate-state.lua`](lua-translate-state.lua) | Translate | 首跑请求、二跑 Current、语义变化失效、译文/state 同事务 |
| [`lua-idempotent-write-back.lua`](lua-idempotent-write-back.lua) | WriteBack | 从权威候选重建相同输出，不依赖 post-publish |
| [`lua-complex-protocol.lua`](lua-complex-protocol.lua) | 三阶段 | 跨 Actors/Map/两个私有文档的多目标私有协议 |

示例只创建 `lua_example_*` / `lua_complex_*` 私有表，不修改 ATT 受管表。文件名和夹具
字段是可替换的示范协议；复制到真实游戏时，必须把来源、身份、上下文和物理断言改成
该游戏的已验证事实。

所需最小夹具形状：

<!-- att-example: illustrative -->
```json
// data/QuestEntries.json（用于前三个示例；实际 JSON 不含本注释）
[
  {"id":"arrival","title":"星港へ","description":"港へ向かう。"}
]
```

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
