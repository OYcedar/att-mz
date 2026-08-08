# Formic 术语抓取工具

本目录中的 `formic.exe` 是 ATT 随包提供的 Formic v0.1.0。它由
[`extract-game-terminology`](../../skills/extract-game-terminology/SKILL.md) Skill 调用，
负责按计划独立处理术语候选工作单元；计划、全局审查、去重和最终 ATT TOML 仍由外部
Agent 负责。

首次安装会把 ATT 的高吞吐 `config.example.toml` 复制为 `config.toml`。填写 Formic 自己的
模型 URL、模型名、上下文窗口和最大输出；API key 优先通过 `FORMIC_LLM_API_KEY` 环境变量
提供。正常更新只刷新示例，不覆盖已经填写的活动配置。Formic 不读取 ATT 根目录的模型配置，
也不复用当前 Agent 会话。启动时还必须设置 `FORMIC_LLM_PROTOCOL`，可选 `completions`、
`responses` 或 `anthropic`。

ATT 默认以 `--concurrency 64` 运行 Formic，并让内置工具最多同时处理 64 个调用。只有模型
服务公布的配额、明确限速或已经复现的服务拒绝要求更小窗口时才降低并发；确认服务支持更高
并发时可以提高。默认开启只读 `search`、`read` 和 1 GiB 作业内存缓存；单次工具结果最多
1 MiB，搜索最多返回 1000 个匹配及 100 行上下文。

不要把不存在的 MCP server 或 `enabled = false` 的示例写进活动配置。确需外部验证时，只接入
已经确认不会修改外部状态的 server，并以 job 会话复用连接；默认采用 `max_in_flight = 64`、
`max_result_bytes = 1048576` 和 `reconnect = true`，只有 server 的明确限制才降低。

Formic v0.1.0 固定读取进程当前工作目录中的 `config.toml`，所以必须以本目录作为工作目录，
并给 `--data`、`--plan`、`--task`、`--out` 和 `--output-schema` 传入绝对路径。输入、计划、
结果和 worker 运行档案都放在任务目录，不得写进本工具目录。

版本、来源和摘要见 [FORMIC-SOURCE.md](FORMIC-SOURCE.md)。Formic 自身的许可正文位于发行包
内的 `LICENSE`；依赖许可见
[Formic 第三方许可证](../../licenses/FORMIC-THIRD-PARTY-LICENSES.html)，随包 Microsoft
运行库的说明见
[Visual C++ Runtime 发行声明](../../licenses/Microsoft-Visual-Cpp-Runtime-NOTICE.md)。
