# Formic 术语抓取工具

本目录中的 `formic.exe` 是 ATT 随包提供的 Formic v0.1.0。它由
[`extract-game-terminology`](../../skills/extract-game-terminology/SKILL.md) Skill 调用，
负责按计划独立处理术语候选工作单元；计划、全局审查、去重和最终 ATT TOML 仍由外部
Agent 负责。

使用前编辑本目录的 `config.toml`，填写 Formic 自己的模型 URL、API key、模型名、上下文
窗口和最大输出。Formic 不读取 ATT 根目录的模型配置，也不复用当前 Agent 会话。启动时还
必须设置 `FORMIC_LLM_PROTOCOL`，可选 `completions`、`responses` 或 `anthropic`。

Formic v0.1.0 固定读取进程当前工作目录中的 `config.toml`，所以必须以本目录作为工作目录，
并给 `--data`、`--plan`、`--task`、`--out` 和 `--output-schema` 传入绝对路径。输入、计划、
结果和 worker 运行档案都放在任务目录，不得写进本工具目录。

版本、来源和摘要见 [FORMIC-SOURCE.md](FORMIC-SOURCE.md)。Formic 自身的许可正文位于发行包
内的 `LICENSE`；依赖许可见
[Formic 第三方许可证](../../licenses/FORMIC-THIRD-PARTY-LICENSES.html)，随包 Microsoft
运行库的说明见
[Visual C++ Runtime 发行声明](../../licenses/Microsoft-Visual-Cpp-Runtime-NOTICE.md)。
