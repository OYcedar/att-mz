# RPG Maker 翻译现行规格

MZ 与 MV 共用唯一的翻译 Profile、Prompt、资源、Planner、LLM 执行和 SQLite 提交能力。
引擎只决定项目位置和审计中的 `engine`，不会改变翻译协议。

## 1. 命令与资源选择

```text
att --config FILE mz translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]

att --config FILE mv translate --name NAME PROFILE_ID \
  [--terms TERMS_TOML] [--placeholders PLACEHOLDERS_TOML] [--lua SCRIPT_LUA]
```

术语或占位符文件被提供时，内容完整替换项目中的对应资源；省略时复用项目数据库的
canonical JSON。文件严格解析和资源解析全部成功后，Standard 才开始规划。Standard
结束后执行显式 Lua；Standard 的 `Complete`、`Partial` 和 `Unavailable` 都是正常业务
结果，技术错误才阻止后续阶段。

项目开启时必须确认冻结来源、每个活动 owner 的来源指纹和资产快照指纹。翻译只读取
`standard_text_group/leaf`；译文按逻辑身份写回 `standard_text_leaf`，不以物理 JSON
地址寻址。

## 2. Terminology TOML

```toml
[[term]]
term = "魔法剣"
translation = "魔法剑"
```

需要完整替换默认 trigger 集合时，写作：

```toml
[[term]]
term = "魔法剣"
translation = "魔法剑"
triggers = ["魔法剣", "魔剣"]
```

权威空术语表使用另一份完整文件：

```toml
term = []
```

根必须显式声明 `term`。每项的 `term` 与 `translation` 必填、非空白且没有首尾空白。
`triggers` 缺席时固定等于 `[term]`；显式提供时必须非空，并完整替换默认集合。术语
原文全局唯一，trigger 也全局唯一；trigger 是区分大小写的字面子串，不是正则。

`term = []` 是权威空术语表。零字节、仅注释、未知字段、重复字段、空值和重复身份均
作为普通无效当前输入拒绝。契约不包含 scope、优先级、多译名或版本。

## 3. Placeholder TOML

```toml
[[rule]]
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ["event_dialogue"]
pattern = '<name>(?<text>.*?)</name>'
```

清除自定义规则使用另一份完整文件：

```toml
rule = []
```

`pattern` 必填并使用本机 PCRE2 UTF/UCP；`scopes` 缺席表示全局。无 `text` 命名捕获时
保护完整匹配；存在时必须只有这一个命名捕获，仅捕获内容进入翻译，匹配外壳受保护。
完整匹配与捕获必须有序、位于原字符串内并对齐 UTF-8 边界，`text` 还必须完整位于
对应匹配内。不接受 `label`、`translate`、`"all"` 或其他命名捕获。

自定义规则零命中合法。Builtin 与 Custom、Custom 与 Custom 的任何跨度重叠都使本叶
规划失败，不按顺序切割，也没有优先级。`rule = []` 只清除自定义规则；固定 RPG Maker
控制符保护继续生效。

三类人写 TOML（Extract Rules、Terminology、Placeholder）都严格拒绝未知与重复字段，
在边界建立受信类型。术语和自定义占位符随后编码为内部 canonical JSON 持久化。

## 4. Profile、Prompt 与共享语言能力

配置从 `[[rpg_maker.translation_profiles]]` 精确选择 `PROFILE_ID`，再取得该 Profile
引用的公共 LLM Client。项目开启后从 metadata 取得规范 `LanguagePair`，精确读取：

```text
<prompts.root>/rpg_maker/<source>--<target>.md
```

Prompt 必须是普通 UTF-8 非空白文件，不做父语言、大小写、默认文件或目录首项回退。
Profile 拥有 planning、request、任务并发及所选 Client；Prompt 与源语言模块属于按项目
语言对解析的资源。Standard 与 Translate Lua 复用同一 Client、Prompt、语言对和语言
语义，Lua 不取得 Standard 的 planning、request 或任务并发策略。

Prompt 内容、语言策略、术语、占位符和相关翻译事实共同进入 translation state；这些
事实未变且叶仍 Current 时不请求模型。

## 5. 自然顺序、上下文与去重

Planner 按稳定 RPG Maker 来源顺序读取活动 owner，再按组与角色组织逻辑叶：

```text
DialogueSpeaker → DialogueBody(0) → DialogueBody(1) → ...
```

一个 Dialogue Group 永不跨 TaskBlock 拆分。正文按原始 `body[n]` 硬边界保留；选择项、
滚动文本和标量使用各自稳定角色顺序。并发准备只改变完成时间，不改变顺序、代表叶、
任务 ID 或提交顺序。

Speaker 与 Body 不互相去重。相同 Speaker 可以跨对话组复用；Body 的去重与 translation
state 包含该组源 Speaker 原文上下文，因此同一句正文在不同说话人下不是同一翻译事实。
recipe 中的 Literal/SpeakerSlot 外壳不属于翻译语义：仅外壳变化且逻辑原文与上下文
不变时可以继承译文。

去重只接受当前项目、当前语言对中完全相同的受信翻译输入；不 trim、不折叠大小写、
不做 Unicode 模糊匹配。哈希只作索引，最终仍以类型相等确认。重复叶使用自然顺序中
最早的代表，验收后把同一译文传播到完整等价集合。

## 6. 任务规划与模型协议

Planner 先保持最大相关组，再按 Profile 的完整 messages 字符预算切分 TaskBlock；单个
语义组超过预算时明确失败，不切碎逻辑原子。每个可翻译叶获得稳定任务内 ID，System
Prompt、上下文、术语和受保护文本共同形成 user message。

公共 LLM 根固定提交 `model`、`messages` 和 `stream=false`，并透传 Client 的其他受信
JSON parameters。网络重试只按 Profile 的明确延迟和 `Retry-After` 上限执行；协议失败
不伪装成网络故障。

响应只接受一个可完整消费的 JSON 数组信封，可有首尾空白、至多一个开头 BOM，以及
恰好包裹全部 JSON 的单层无标记或 `json` 围栏。不搜索说明文字中的数组，不修复尾逗号、
注释、引号、ID 或译文。结构失败使整批 `Unavailable(ModelResponseUnusable)`；结构
成立后，单个 ID 的缺失、重复、未知、空白、无自然语言、源文残留或占位符错误只拒绝
该 ID，其他结果仍可形成 `Partial`。

译后处理固定执行：结构与空白检查、已知控制符归一为 token、严格校验 token 多重集、
投影目标语言文本、源语残留检查、可选安全语言修复、恢复保护片段，并确认没有 ATT
保留前缀残留。DialogueSpeaker 还拒绝包含 CR、LF 或 NUL 的译文，避免把多行或非法
字符串写入姓名框。模型输出不会修改原文、物理目标或 recipe。

## 7. 原子验收与持久化

每个 TaskBlock 在发送第一次请求前写 `translation_task_started`。验收后，Result Store
在一个事务中再次确认：

- metadata 来源指纹；
- 所有 owner 的来源与资产快照指纹；
- 本次术语和占位符 canonical JSON；
- 每个目标逻辑叶的 group、role、原文、上下文和旧 state。

只有全部事实仍与规划时一致，才按逻辑身份更新译文与 state。单个批次不会留下半提交；
已经确认的前序批次不因后续 `Partial`、`Unavailable` 或取消而回滚。任务终态审计记录
accepted、unresolved、协议诊断及 `confirmed_written_leaves`；位置均为逻辑组与字段角色，
计数明确表示逻辑叶。

## 8. Translate Lua 与完成结果

Translate Lua 获得公共 `ctx.project/json/source/rpg_maker/db`、阶段专属
`ctx.translation` 与 `ctx.llm`；`extract`、`output`、`write_back` 为 nil。
`ctx.translation.prepare/accept` 复用 Standard 的术语、占位符、语言分析与验收语义，
`ctx.llm` 复用本次 Client。可信脚本仍可通过 `ctx.db` 实现游戏专用协议；RPG Maker
文档能力的唯一门面是 `ctx.rpg_maker`。

成功输出报告任务、写入和剩余逻辑叶摘要；`Partial` 或 `Unavailable` 不冒充全部翻译
完成，也不升级为退出码 1。合作取消完成受控收尾后返回 130；配置、输入、技术、审计
或 shutdown 失败返回 1。
