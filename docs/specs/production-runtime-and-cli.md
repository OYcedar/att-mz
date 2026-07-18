# ATT 生产运行时与 CLI 规格

## 1. 唯一入口

```text
att [--config FILE] mz <init|extract|translate|write-back>
```

四个命令是唯一用户入口，也是状态收敛入口；不增加维护、重试或恢复命令。Clap 的 Help/Version 不读取配置。当前生产目标只支持 `x86_64-pc-windows-msvc`。

## 2. 启动与配置选择

```text
解析 CLI
  ↓
读取一次受限 TOML，检查完整语法和未知顶层分区
  ↓
仅解析当前命令实际选择的分区
  ↓
构造 ConfiguredMzCommand 互斥变体
  ↓
构造 Tokio Runtime 和当前纵向切片
  ↓
构造 audit.jsonl，持久化 run_started
  ↓
取得项目租约并执行命令
  ↓
终结非审计根，持久化 run_finished，终结账本
  ↓
呈现结果
```

已知但未选的配置分区不解析、不验证；未选 Client 的密钥不物化。配置边界选择一次
Profile，并把同一个 `Arc<TranslationExecutionProfile>` 与 Client 注入 Standard 和
可选 Lua。业务输入只携带已经建立的受信执行事实。

## 3. 按命令构造

| 命令 | 当前纵向切片 |
|---|---|
| Init | 文件、SQLite 建库/快照、目录发布、审计 |
| Extract | 文件、SQLite、CPU、所选提取能力、审计；显式 `--lua` 才构造 Lua |
| Translate | 文件、SQLite、CPU、Delay、LLM、Standard、审计；显式 `--lua` 才构造 Lua |
| WriteBack | 文件、SQLite、CPU、目录发布/候选编辑、Standard、审计；显式 `--lua` 才构造 Lua |

三个可选 Lua 命令把脚本路径和执行能力组合成一个 `SelectedLua`；不存在“路径已选但依赖缺失”的内部状态。

## 4. 项目租约

四个顶层 MZ 服务在读取项目、打开 SQLite、发送网络请求或建立候选前取得项目租约，并持有到业务终态及相关操作审计确认结束。MZ 的 `ProjectCommandLeaseService` 只选择固定锁目录并提交受信项目 identity，通用文件根负责按 Windows 非大小写敏感语义生成稳定锁文件名：

```text
ProjectName + <projects.root>/.att-locks/projects
  ↓ 通用文件根规范化不透明 identity 并计算 SHA-256
<projects.root>/.att-locks/projects/<digest>.lock
```

文件系统根只提供通用独占文件租约，不理解 identity 是项目名，也不理解 MZ。目录发布锁位于 `<projects.root>/.att-locks/directory-publish/`；stage、backup 和 journal 仍在目标同父目录。

锁序固定为：

```text
项目租约 → 目录发布锁 → SQLite/session
```

同项目四命令互斥，不同项目并行。超时是稳定的“项目正忙”用户结果，不继续副作用。可信 Lua 通过子进程启动同项目命令也不能重入。

不对整个 `projects.root` 做 NTFS 品牌预检。普通读取、Extract 和 Translate 不承担目录发布才需要的限制；各根在实际操作时验证自己需要的独占锁、稳定身份、同卷 rename、追加和刷盘能力。

## 5. 四命令收敛

- Init 首次创建项目；已有项目逐项继承未显式给出的语言/布局，并在来源与全部事实相同时快速返回 `Unchanged`，不建立候选；
- Extract 只替换当前选择的 owner，同快照返回 `Unchanged`；
- Translate 复用持久资源和逐叶 state，全部 Current 时不请求模型、不写译文；
- WriteBack 每次从冻结来源重建一个候选，Standard 与可选 Lua 修改同一候选，只发布一次。

## 6. 强审计顺序

所有命令在取得项目租约前确认 `run_started` 已持久化。Translate 在每个 TaskBlock 请求模型前写任务意图，任务验收和提交确定后写任务终态；WriteBack 在发布前写发布意图，发布取得终态后写发布终态。

意图审计失败意味着对应网络/发布副作用没有开始。副作用已生效而终态审计失败时，命令报告“状态已生效但审计未确认”。其他根 shutdown 完成后才写 `run_finished`，账本最后关闭。只有上述事实全部确认后才输出成功文案。

## 7. 取消与进程结果

合作取消是正常完成分支：

```text
OperationCompletion<T>
├─ Completed(T)
└─ Cancelled

ProductionCommandRunReport
├─ Succeeded(output)
├─ Interrupted
└─ Failed(failure)
```

进程不通过错误文本、`source.to_string()` 或 downcast 判断 Ctrl-C。第一次 Ctrl-C 后停止派生新阶段；根已经接管的 SQLite、目录发布、审计、CPU 和 HTTP 工作继续到明确终态。候选尚未发布时显式 discard；publish 已开始时等待终态。Lua handle 的取消是合作式的，脚本不交还控制时不伪造成功或超时。

## 8. shutdown 与退出码

只终结当前命令实际构造的实例。完整 Translate 的顺序是：Lua 等待唯一 finalizer、LLM 停止准入并等待活动请求、SQLite 终结唯一会话并排空短操作、FileSystem/CPU 排空、记录 `run_finished`、最后排空并关闭 audit writer。

| 退出码 | 含义 |
|---|---|
| `0` | Help、Version、命令成功，包括正常 Partial/Unavailable |
| `2` | Clap 参数错误 |
| `1` | 配置、技术错误、任一 shutdown/审计失败，或状态已生效但收尾未确认 |
| `130` | Ctrl-C 后完成受控收尾 |

用户错误只呈现稳定中文类别，不暴露根类型名、内部状态枚举、检查 ID 或底层错误文本；技术原因链保留给内部诊断。
