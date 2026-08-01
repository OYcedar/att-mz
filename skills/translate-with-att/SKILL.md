---
name: translate-with-att
description: 使用 ATT 规划、执行、继续、只读诊断、人工审校、修复和验收 MV、MZ、Generic 或组合式游戏翻译。用于 ATT Init、Extract、Translate、WriteBack、Rules、Placeholder、Terminology、Prompt、原子数据库 Lua、动态 JSONL 同步，以及遗漏、拒绝、Partial、Unavailable、Current、写回或发布异常调查。用户明确使用 ATT、提供 ATT 项目或要求继续已有 ATT 翻译任务时使用。不用于开发或评审 ATT 源码与架构、RPG Maker XP/VX/VX Ace、其他翻译工具或脱离 ATT 的普通文本翻译。
---

# 使用 ATT 完成翻译

## 先确定工作模式

先看清用户授权到哪一步，再动手：

- **只读**：解释、审查、诊断或状态核对。只读取产品文档、项目和已有任务材料。
- **执行**：用户要求建立、继续、翻译、修复、写回或交付。先建立或恢复唯一任务
  清单，再执行已授权的操作。
- **协作者**：只完成负责人分配的 TODO 和文件范围；唯一清单和模型请求都交给
  负责人统一打理。

"帮我看看""分析一下"是只读邀请；等用户明确给出修改授权，再进入执行。

## 绑定实际 ATT 和权威文档

1. 确定本次实际使用的 `att.exe` 绝对路径、版本和 SHA-256。
2. 把它的发布目录当作产品与知识根；固定配置、项目和 Prompt 路径分别是同目录下的
   `config.toml`、`projects/` 和 `prompts/`，不搜索或选择其他配置文件。
3. 读发布目录中的 `README.md` 与 `docs/README.md`，再按总入口读取本次引擎、阶段、
   格式和运行能力的现行规格。
4. 记录调用 cwd；命令中显式提供的游戏、JSONL、Rules、术语、Placeholder、Lua 和输出
   路径按文档从 cwd 解析，不用 cwd 改写固定产品路径。

命令、参数、JSONL、配置、状态、错误、数据库、Lua 和恢复行为，都以这个发布目录
里的文档和实际程序为准。其他安装、源码仓库、Git 历史、旧对话和记忆都不是事实
来源。固定 `config.toml` 或本阶段需要的发布文档、Prompt 等资源缺失时，停下来报告
发行内容缺口，不从工作区、任务目录或其他安装中拼接替代资源。首次 Init 前没有
`projects/` 属于正常状态，由 ATT 建立实际项目目录。

## 选择项目

先调查游戏运行时真正读取的内容，再决定：

- RPG Maker MV/MZ 原生数据和 Rules 能完整表达的内容，使用对应 `mv` 或 `mz`
  项目；
- 外部操作者或工具能提供 ATT JSONL 的任意内容，使用 `generic` 项目；
- 一个游戏同时存在两类内容时，建立彼此独立的 MV/MZ 与 Generic 项目。

组合使用时，记录每类内容的唯一项目所有者、外部 JSONL 转换方式和最终消费方式。
每个项目各自保存数据库、术语、Placeholder、状态、日志、模型任务记录和输出，
同一文本只归一个项目所有。公共配置可以定义相同的 Profile，每个项目独立保存
自己的选择。

Generic JSONL 如何产生和如何回写由外部操作者负责。修改 JSONL 后，先按 Generic
文档重新 Extract，再 Translate 或 WriteBack。

## 组织和审查 Generic JSONL

任务需要生成、重做或审查 Generic JSONL 时，先完整读取
`docs/generic/jsonl.md` 的来源分组规则和 `docs/generic/translation.md` 的 TaskBlock 规则。
调查原格式的完整来源路径、运行时关系、自然顺序和写回位置，再检查：

- 必须共同解释的文本是否进入同一 Group；
- 各自完整但属于同一场景、记录序列等较大关联范围的小 Group，是否在同一 JSONL 文件中
  保持自然顺序；
- 是否误把整个源 JSON 或物理文件做成一个巨型 Group，或者把叶子字段逐项拆成 Group、
  逐 Group 建立小文件；
- 稳定 ID、外部转换记录和写回映射能否逐项追溯。

先按关联性确定 Group 和文件边界，再让 ATT 按 Profile 目标字符数组合 TaskBlock，不用
字符数替原格式决定语义。单个关联范围过大时，只沿能够独立理解和写回的真实子结构继续
拆分；没有这种边界时保持完整，并在发出模型请求前确认所选 Client 能够处理实际任务。
用户已经提供 JSONL 时，先报告分组问题及影响；没有修改授权和同步写回过程，不重组输入。

把 Unit、Group、Semantic Scope 和 TaskBlock 分开判断：Unit 是最小验收单位，Group 是
不可拆的语义整体，Semantic Scope 是允许相邻 Group 共同装箱的最大范围，TaskBlock 是
一次模型请求的完整上下文。数据源与 Profile 不变时，完整 TaskBlock 的范围和顺序必须
幂等；Current、复用、非源语、Placeholder 结果、术语命中、临时 ID 和历史任务都不能
改变装箱边界。

保留全局去重。少量相同原文需要不同译文时，先完成自动翻译，再按
Lua 文档精确修订目标 Unit，不为少量例外重写提取或去重规则。

## 分开术语制作与 ATT 接入

ATT 发布目录中的术语规格只定义 ATT 接受的文件、匹配方式和项目生命周期。任务还没有
经过确认的术语内容，并且用户授权制作或重做术语表时，先使用独立的
[通用游戏术语表制作 Skill](../extract-game-terminology/SKILL.md)从实际语料发现、筛选、
定译和验收，再按当前 ATT 文档转换成输入文件。

通用 Skill 不复制 ATT 字段，ATT 文档也不靠名称字段、词频或词性判断什么应当成为术语。
用户已经提供术语表时，不擅自重新制作；替换项目前仍按实际使用位置确认影响。

## 管理执行任务

执行型任务完整读取发布目录中的：

- `docs/guides/translation-project.md`
- `docs/guides/task-artifacts.md`
- `docs/guides/task-list-template.md`

用户给出清单时先完整读取并核对身份。没有清单时，只在安全任务根查找当前目标的
清单；确实没有匹配项才复制模板建立一份。多份同时匹配时停下来请用户指明，不按
日期猜测。

清单只保留提取、翻译、写回三个阶段，Init 是提取阶段的前置工作。一个游戏使用
多个 ATT 项目时，在同一清单中分别记录各项目。

每个 TODO 在执行前写明可观察的完成标准，完成后立即补充结果和证据。只展开当前
阶段；新事实使旧结论失效时追加复核 TODO，历史完成记录保持原样。

## 执行与判断结果

- 修改项目、发出模型请求或发布输出前，先确认相应授权。
- 模型、凭据、费用、语言、范围、外部服务和唯一原件，都按用户指定的来。
- 按项目文档执行 Init、Extract、Translate 和 WriteBack，参数以文档为准。
- Init 诊断为 `filesystem.operation`、发布、无覆盖重命名、`OS 5` 和“状态未改变”时，按
  `docs/runtime/directory-publishing.md#5-init-发布阶段的-os-5` 检查项目目录不存在且没有
  恢复终态；满足后只用原命令重试一次。否则不重试；不得手工删除、移动或编辑 ATT 项目。
- Extract 成功但报告 Rules 非字符串跳过警告时，按 Rules 文档检查规则号、来源文件、
  command code、parameter、实际类型和数量。已确认的混合类型直接参数可以继续；类型
  出乎规则设计时先修正规则或来源范围。警告既不等于 Extract 失败，也不能在覆盖审查中
  直接忽略。
- 修改 Rules、术语、Placeholder、Prompt 或 Lua 前，先确认全部实际使用位置和
  影响。
- Lua 是项目数据库原子事务；记录脚本、参数和 SHA-256，把它当数据库工具来用。
- 用数据库和发布终态判断结果；日志和任务记录只用于诊断。
- 分别解释 Complete、Partial、Unavailable、取消与 outcome unknown；退出码零
  只代表进程正常结束，翻译是否完成要看权威状态。
- 检查 Partial 或重试时，不要只数带 ID 的原文。任务记录中的 JSON user message 必须
  包含原 TaskBlock 的全部 Group 和 Unit；语境 Unit 省略 ID，需要模型输出的 Unit 才在
  每个块内从 `"0"` 连续编号。数据源与 Profile 未变时，前后两次记录中的块边界和自然
  顺序应相同；只允许 ID、受 Placeholder 保护的显示文本和实际发送块集合变化。
- 思考输出开启时，把任务记录中的 System、User、Thinking、Raw Assistant、逐 ID 诊断和
  最终结果放在一起核对。Raw Assistant 是经过现行敏感信息替换的模型 `message.content`，
  可用于人工或 agent 排查 JSON 结构、ID、原文回显、截断、转义和源语残留，但不是权威
  状态；它缺失时只报告证据不足，不因此重发请求、重放或提交。
- 当前发行版的 MV/MZ 项目数据库只接受当前 schema。旧项目不能由程序识别或迁移；确认
  数据库不符合当前发行规格时，在新的项目工作区重新 `Init + Extract`，不要修改旧库或
  从旧库自动带入译文。Generic JSONL 和 Generic 项目按各自现行规格处理。
- Translate 出现 HTTP 失败或 Unavailable 时，读取对应任务记录和项目日志中的结构化
  状态、供应商 code/type/message。修改 Endpoint、Model 或 parameters 前，把用户要求
  写成精确目标值并逐字段核对；一次请求成功不能代替语义符合要求，也不另发没有授权的
  Profile 探针。
- 状态不明时停下重复请求和写入，保留现场，按对应文档重新观察权威状态。

## 验收与交付

以下事实都成立，才宣布完成：

- 范围内文本的来源、项目所有者和写回关系有当前证据；
- 遗漏、未译内容、拒绝和质量问题都有解释；
- 术语、Placeholder、控制符、人称、语气和实际运行效果已经按范围审查；
- 每个项目的数据库和 WriteBack 都对应当前输入；
- Generic 输出已经由负责的外部过程消费并验证；
- 交付记录写明输出、验证、未验证内容和剩余风险。

只读任务保持清单原样，交付结论、证据、未确认事实，以及执行修复所需的授权。
