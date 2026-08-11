# WriteBack 排版规则现行规格

RPG Maker MV/MZ 与 Generic 共用同一种严格 TOML 格式。规则只决定哪些当前 Unit 可以排版，
以及每行最多容纳多少全角字符；它不声明写回方式。ATT 根据当前 Unit 的 kind、role 和 recipe
决定是增加 RPG Maker 事件正文命令，还是在单个字符串内部插入 LF。

## 1. 文件结构与生命周期

根必须恰好包含 `rule` 数组。非空文件使用一个或多个 `[[rule]]`；清空项目规则使用：

```toml
rule = []
```

零字节、只有注释、缺少 `rule`、未知字段、重复字段或错误类型都会使本次 WriteBack 失败。

```text
att mv|mz|generic write-back --name NAME [--layout-rules FILE]
```

- 提供 `--layout-rules FILE`：ATT 先解析整份文件，并针对当前 Extract 的全部 Unit 检查选择器、
  不兼容位置和规则重叠；全部有效后，原子替换项目保存的规则，并用于本次 WriteBack；
- 省略参数：直接复用项目保存的规范规则内容，不重新读取原文件；
- 文件写 `rule = []`：清空项目规则，本次及以后都不执行规则驱动的自动排版；
- 新文件无效：本次 WriteBack 失败，项目原有规则和上一次成功输出都保持不变；
- 项目保存的是校验后的规则内容，不是文件路径；原文件随后移动、修改或删除都不影响复用；
- 新项目尚未保存过非空规则时，省略参数等同于空规则。

RPG Maker 与 Generic 使用完全相同的生命周期。规则只属于 WriteBack，不进入 Translate，
也不改变译文的 Current、Rejected 或 Review 状态。

## 2. 字段

每条规则都必须提供：

| 字段 | 类型 | 含义 |
| --- | --- | --- |
| `max_fullwidth_chars` | 正整数 | 每个显示行允许的最大宽度；一个全角字符计 1，普通 ASCII 字符通常计 0.5 |

每条规则还必须至少提供一个正向选择器：

| 选择器 | 引擎 | 值 |
| --- | --- | --- |
| `scopes` | 两者 | RPG Maker 使用 `database_entry`、`system`、`map`、`event_dialogue`、`event_choices`、`event_scrolling_text`、`event_command`、`plugin_parameter`；Generic 使用 JSONL Unit 的 `kind` 原值 |
| `ids` | 两者 | 当前项目的完整自然 Unit ID |
| `source_files` | 两者 | RPG Maker 使用 `data/Items.json`、`data/Map001.json`、`js/plugins.js` 等自然路径；Generic 使用输入根内以 `/` 分隔的相对 JSONL 路径 |
| `fields` | 仅 RPG Maker | `body`、`scrolling_text` 或标量字段名，如 `description`、`profile` |
| `owners` | 仅 RPG Maker | `builtin` 或 `rules` |
| `rule_numbers` | 仅 RPG Maker | Extract Rules 从 1 开始的自然规则序号 |
| `group_ids` | 仅 Generic | JSONL 的 Group `id` |
| `unit_ids` | 仅 Generic | JSONL 的 Unit `id` |

唯一反向选择器是 `exclude_ids`，值也是完整自然 Unit ID。一个数组中的值取并集；不同字段
之间取交集；最后减去 `exclude_ids`。所有显式数组都必须非空，值不得为空或重复。

每个选择器值都必须在当前项目中存在，整条规则组合后必须至少命中一个 Unit。一个 Unit
不能同时命中两条规则；ATT 不按文件顺序覆盖，也没有优先级。RPG Maker 专用字段出现在
Generic 文件中，或 Generic 专用字段出现在 RPG Maker 文件中，都会明确失败。

```toml
[[rule]]
max_fullwidth_chars = 20
scopes = ['event_dialogue']
fields = ['body']
source_files = ['data/Map023.json']
exclude_ids = ['Map023.json:event17:page1:dialogue42']

[[rule]]
max_fullwidth_chars = 18
ids = ['Map023.json:event17:page1:dialogue42']
```

Generic 可以使用 JSONL 自身的稳定 ID 精确选择：

```toml
[[rule]]
max_fullwidth_chars = 22
source_files = ['story/chapter-01.jsonl']
group_ids = ['opening']
unit_ids = ['narration-1', 'narration-2']
```

## 3. 物化方式

规则没有 `mode`。写回方式由已经验证的项目结构唯一决定：

| 目标 | 结果 |
| --- | --- |
| RPG Maker `event_dialogue` 的 `body` | 每个最终显示行成为一条 `401`；新增行复制其原始母行的 indent 和未知字段 |
| RPG Maker `event_scrolling_text` 的 `scrolling_text` | 每个最终显示行成为一条 `405`；每个原始 `405` 是独立硬段，拆出的续行复制该 `405` |
| RPG Maker 完整单字符串标量字段 | 在字符串值内部插入 U+000A LF；JSON 文件中序列化为 `\n` |
| Generic Unit | 在 `text` 内部插入 U+000A LF；Group 仍占一条物理 JSONL 记录 |

对话 Speaker、Choice 和固定空槽不允许排版。RPG Maker 标量只有在整个物理字符串恰好是
该 TextSlot 时才允许排版；含固定前后缀或多个文本槽的组合字符串不能按一个槽的宽度安全
处理，规则命中时会失败。规则也不能在同一项中同时命中结构增行位置与字符串 LF 位置；
需要相同宽度时仍分别写两条互不重叠的规则。

事件列表由 WriteBack 作为完整结构重建，不在原数组上边遍历边插入；因此增加 `401` 或
`405` 不会使后续命令位置漂移。Choice 数组、`102/402` 对应关系及冻结空槽校验完全不受
排版或补空白开关影响。

## 4. 断行与补空白

排版只处理规则命中的当前译文，人工译文和自动译文规则相同；未命中位置保持排版前文本。
ATT 保留已有 LF 作为硬边界，只在空白或可证明安全的标点边界断行。断点处用于分隔的普通
空白会被 LF 取代；正文字符、Placeholder 和 RPG Maker 控制符不会被删除或改写。找不到一组
安全断点时，整个 Unit 保持排版前文本，不做部分断行。

`complete_continuation_whitespace` 是独立配置，不要求存在排版规则。开启后，已有硬续行和
自动产生的续行如果仍位于未闭合的 `()`、`「」`、`『』`、`“”`、`（）`、`【】`、`《》`、
`〈〉`、`〔〕`、`［］`、`｛｝` 内，会在行首 RPG Maker 控制符之后补一个 U+3000 全角空格；
行首已有半角空格、全角空格或 NBSP 时不重复。关闭后不执行这项补全。

Placeholder 与 RPG Maker 控制符按零显示宽度计算并保持不透明。排版后的候选仍须通过
Placeholder、控制符、recipe、JSON/JSONL 往返和未声明位置逐字一致检查，之后才允许发布。
