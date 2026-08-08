# ATT 性能测试套件

本目录保存 ATT 的正式性能样本定义、样本专用 Rules、Placeholder 与术语表，以及一个
确定性本地 Chat Completions Provider。实际比较由仓库根目录下的
[`scripts/measure-performance.ps1`](../../scripts/measure-performance.ps1) 执行，判断方法以
[`maintenance/performance-validation.md`](../performance-validation.md) 为准。

这套测试只比较已经构建好的 Windows x64 Release/MSVC 制品。脚本不会替代候选改动的
正确性测试，也不会调用外部模型服务。

## 代表样本

| ID | 类型 | 规模 | 主要用途 |
| --- | --- | --- | --- |
| `boku-to-succubus` | RPG Maker MZ 1.6.0 | 84 张地图、60 个启用插件、2.596 GiB | 高文本量和大型 CommonEvents；默认主样本 |
| `succubus-academia` | RPG Maker MV 1.6.1 | 123 张地图、164 个启用插件、3.609 GiB | 高插件量，以及 Patch、Append、MOD 共存 |
| `princess-honey-trap` | RPG Maker MV 1.6.1 | 72 张地图、38 个启用插件、1.681 GiB | 中等规模 MV、对话姓名规则和嵌套插件参数 |
| `succubus-academia-generic` | Generic JSONL | 基础数据为 5 个 Group、15 个 Unit，可确定性扩展 | 角色资料、行动定义和自定义 Placeholder |

前三个游戏来自仓库本机的 `测试集`，不复制进本目录。样本清单的路径、规模和来源校验记录
由 `测试集/README.md` 与 `测试集/测试集清单.csv` 保存。脚本在预检时要求这些目录真实存在，
并在每次运行中记录其稳定内容身份。

Generic fixture 只借用了
`サキュバスアカデミア-v1_2_0+Append-v1_0_1+MOD/js/plugins/C_AlmaElma.js` 与
`C_AlmaElma_EV2.js` 中“角色资料、行动名称、叙述和台词组成一项记录”的数据形状。fixture
中的叙述和台词是为测试重新编写的中性文本，没有复制游戏剧情；角色名、技能名和自定义
占位符用于保持来源结构可辨认。RPG Maker 的 Rules 不能扫描任意插件源码，因此这部分采用
Generic JSONL，而不是增加无效的 RPG Maker Rules。

## 规则文件和术语表

每个样本目录中的文件都直接供生产命令使用：

- `rules.toml` 只选择实际启用插件中会显示给玩家的参数；嵌套 JSON 参数使用现行 Rules
  路径语法，只有再次编码的 JSON string 才设置 `decode_json = true`；
- `dialogue-rules.toml` 只表达已确认的姓名协议。没有稳定协议的游戏使用显式空规则，不根据
  台词样式猜测说话人；
- `placeholders.toml` 只补充 ATT 内建控制符之外、在对应样本中实际出现的控制符；
- `terms.toml` 收录游戏名、角色名和核心专名。译名用于稳定性能测试输出，不代表作品的官方
  中文译名，通用界面文字不进入术语表。

这些资产已通过当前发行版的 `init`、`extract` 和 `translate` 生产入口验证，四个样本的
Translate 都得到 complete 终态且没有剩余位置；Generic 样本还通过了 `write-back`。以后修改
样本资产时，必须重新做同等范围的验证，不能只检查 TOML 或 JSONL 语法。

## 运行环境

运行前需要：

- Windows x64；
- PowerShell 7.4 或更高版本；
- Node.js，用于运行确定性本地 Provider；
- 当前仓库声明的 Rust 工具链，用于记录 `rustc`、Cargo 和 rustup 身份；
- 基线和候选两个 SHA-256 不同的 Release/MSVC `att.exe`；
- 位于仓库 `测试集` 目录的三个本机游戏样本。

先做预检，不启动性能场景：

```powershell
pwsh -NoProfile -File .\scripts\measure-performance.ps1 `
  -BaselineExe D:\builds\baseline\att.exe `
  -CandidateExe D:\builds\candidate\att.exe `
  -PreflightOnly
```

执行正式比较：

```powershell
pwsh -NoProfile

.\scripts\measure-performance.ps1 `
  -BaselineExe D:\builds\baseline\att.exe `
  -CandidateExe D:\builds\candidate\att.exe `
  -PrimarySample boku-to-succubus `
  -AdditionalSamples @(
    'succubus-academia',
    'princess-honey-trap',
    'succubus-academia-generic'
  ) `
  -FocusStage Translate
```

默认用 AB/BA 顺序运行主样本两轮；只有两轮结果贴线或冲突时才运行第三轮。主样本通过后，
其他样本各运行一对。Generic 默认把基础 fixture 确定性扩展为 2,000 份；可用
`-GenericCopies` 调整，但正式对照必须让基线与候选使用相同值。

脚本默认拒绝哈希相同的两个制品，避免把计时噪声误写成收益。只有修改测试框架后做自检时
才使用 `-AllowIdenticalExecutables`；这种运行的结论固定为 `validation_only`，不能作为性能
证据。

## 证据和清理

每个场景都使用新的发行根、来源副本、项目数据库和输出目录。计时前后会核对 Manual 导出、
最终输出树、模型任务消息、任务终态和项目日志；任一结果不等价时立即停止，已有计时作废。

默认结果写入 `tmp/performance/<时间>/`：

- `raw-results.json` 保存环境、制品、样本身份、各阶段时间、工作集和正确性摘要；
- `report.md` 保存可审查的结论和成对结果；
- `evidence/` 保存命令输出、项目日志和关键导出；
- 成功场景的完整工作区会被删除，使用 `-KeepWorkspaces` 才保留。

`tmp` 中的结果不纳入版本控制。需要长期引用某次结论时，应在变更说明中记录
`raw-results.json` 的路径和相应制品哈希。
