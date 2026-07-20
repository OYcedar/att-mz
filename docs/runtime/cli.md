# ATT 生产运行时与 CLI 现行规格

## 1. 统一入口与独立命令域

RPG Maker 是 ATT 当前唯一已实现的游戏领域，该领域目前只支持 MV 与 MZ。CLI 使用
`mv | mz` 选择域内版本、目录布局和工作区身份；这两个值不与 RPG Maker 并列，也不
表示其他 RPG Maker 版本已经实现。

```text
att --config FILE mz init|extract|translate|write-back ...
att --config FILE mv init|extract|translate|write-back ...
```

MZ 与 MV 是 RPG Maker 域内的独立命令域，但都路由到同一纵向实现。版本切片只建立
持久身份、项目/游戏布局和 MV 姓名投影输入；共享业务代码不按版本复制两套流程。

普通命令缺少 `--config FILE` 是 CLI 解析错误；Help/Version 不需要配置。不存在默认配置
路径。当前生产目标是 `x86_64-pc-windows-msvc`。

命令参数如下：

```text
att mz|mv init --name NAME --path GAME_ROOT
  [--source-language LANG]
  [--target-language LANG]
  [--dialogue-max-fullwidth-chars COUNT]
  [--scrolling-text-max-fullwidth-chars COUNT]
  [--help-description-max-fullwidth-chars COUNT]

att mz extract --name NAME
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+

att mv extract --name NAME
  (--builtin | --rules RULES_TOML | --lua SCRIPT_LUA)+
  [--dialogue-rules DIALOGUE_TOML]

att mz|mv translate --name NAME PROFILE_ID
  [--terms TERMS_TOML]
  [--placeholders PLACEHOLDERS_TOML]
  [--lua SCRIPT_LUA]

att mz|mv write-back --name NAME [--lua SCRIPT_LUA]
```

`--dialogue-rules` 只属于 MV 且要求同时选择 `--builtin`。成功文案不额外添加版本前缀。

## 2. 启动与配置选择

```text
解析 CLI
  ↓
读取一次受限 TOML，检查完整语法和未知顶层分区
  ↓
只解析当前命令需要的配置并建立受信类型
  ↓
构造 ConfiguredRpgMakerCommand(layout, command)
  ↓
构造 Tokio Runtime 与当前纵向切片
  ↓
构造 audit.jsonl，持久化带 engine 的 run_started
  ↓
取得对应版本的项目租约并执行命令
  ↓
终结非审计根，持久化 run_finished，终结账本
  ↓
呈现结果
```

已知但未选的配置分区不解析、不验证；未选 Profile 除选择所需 ID 外的内容与未选 Client
密钥不物化。Translate 第一阶段建立全局语言目录、Prompt 根、所选 RPG Maker Profile
与 Client；项目开启后才按 metadata 的规范 `LanguagePair` 读取
`<prompts.root>/rpg_maker/<source>--<target>.md`。

## 3. 按命令构造

| 命令 | 当前纵向切片 |
|---|---|
| Init | 文件、SQLite 建库/快照、目录发布、审计 |
| Extract | 文件、SQLite、CPU、所选 Builtin/Rules、审计；显式 `--lua` 才构造 Lua |
| Translate | 文件、SQLite、CPU、Delay、LLM、Standard、审计；显式 `--lua` 才构造 Lua |
| WriteBack | 文件、SQLite、CPU、目录发布/候选编辑、Standard、审计；显式 `--lua` 才构造 Lua |

可选 Lua 把脚本和执行能力组合成一个受信选择，不允许“有路径但缺依赖”的内部状态。

Extract、Translate 与 WriteBack 各自在命令生命周期内构造一个私有 Rayon CPU 池；文档
解析、规则扫描、资产编解码、规划准备与写回计算都通过同一根能力共享线程和准入预算，
业务模块不使用全局 Rayon 池。等待 CPU 准入时取消则任务不执行；已经准入的任务即使
调用 Future 被丢弃也会完成，shutdown 停止新准入并排空全部已接管作业。

## 4. MV/MZ 布局与项目租约

MZ 只接受顶层同时包含 `data/`、`js/` 和 `js/rmmz_core.js` 的游戏根；MV 只接受包含
`www/data`、`www/js` 和 `www/js/rpg_core.js` 的游戏根。不探测另一种布局，不自动修正
传入的 MV `www`。

项目、项目锁与发布锁分别位于：

```text
<projects.root>/<engine>/<project-name>
<projects.root>/.att-locks/projects/<engine>/<digest>.lock
<projects.root>/.att-locks/directory-publish/<engine>/
```

当前外部契约仍以 `engine` 命名该身份字段，其值只能是 `mz | mv`，二者都属于 RPG Maker。
同一版本同一项目的四个命令互斥；不同版本的同名项目独立。通用文件根只规范化不透明
identity 并管理锁，不解释 RPG Maker 版本业务。

锁顺序固定为：

```text
项目租约 → 目录发布锁 → SQLite/session
```

超时返回稳定的“项目不存在或正忙”结果，不继续副作用。可信 Lua VM 运行在专用 OS
线程中，同样不能重入自己所在命令已经持有的项目租约。

## 5. 四命令收敛

- Init 建立或收敛一个项目，来源与全部事实相同时快速返回 `Unchanged`；
- Extract 只替换选中的 owner；MV Builtin 的对话定义与资产快照原子提交；
- Translate 复用持久 TOML 资源的 canonical 形式和逐语义单元 state，全部 Current 时不请求
  模型；
- WriteBack 每次从冻结来源重建候选，Standard 与可选 Lua 修改同一候选，只发布一次。

## 6. 强审计、取消与退出码

所有业务事件都带 `engine`。所有命令在取得租约前确认 `run_started`；Translate 在请求
前写任务意图，WriteBack 在发布前写发布意图。副作用已经生效而终态审计失败时报告
“状态已生效但收尾失败”，不伪装普通失败或自动重做。

第一次 Ctrl-C 后停止派生新阶段；SQLite、发布、审计、CPU 和 HTTP 已接管的工作继续到
明确终态。候选尚未发布时 discard；publish 已开始时等待终态。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version、命令成功，包括正常 Partial/Unavailable |
| `2` | CLI 解析错误 |
| `1` | 配置、输入或技术错误，shutdown/审计失败，或状态生效但收尾未确认 |
| `130` | Ctrl-C 后完成受控收尾 |

用户可修复错误只呈现安全的路径、行列、字段和原因，不包含配置原文、API key、Client
parameters、Prompt 内容、完整模型消息或内部错误链。
