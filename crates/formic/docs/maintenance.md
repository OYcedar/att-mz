# Formic 维护指南

本文说明 ATT workspace 内 Formic 的工程维护方法。共同维护规则见根
[`AGENTS.md`](../../../AGENTS.md)，Formic 外部契约见
[用户文档](../../../tools/formic/docs/README.md)，模块位置见[项目地图](project-map.md)，
发行以 ATT 的[发行物规格](../../../docs/runtime/distribution.md)和
[公开发行指南](../../../maintenance/releasing.md)为准。

本文所有 Cargo 命令均从 ATT 仓库根目录执行，使用根 `rust-toolchain.toml` 和 `Cargo.lock`。
未带目录前缀的 Rust 模块位于 `crates/formic/src/`。

## 1. 工程边界

- **单一当前版本**：Formic 的 CLI、配置、作业清单、结果格式和内部协议只实现当前契约。变更时同步更新生产代码、测试、配置示例和文档，不保留旧字段别名或兼容分支。
- **独立单元**：并发和完成顺序只能改变耗时，不能改变单元结果含义。`published` 是显式辅助读取能力；测试隔离语义时应改变并发完成顺序，确认 `none` 下结果不受其他单元影响。
- **活动窗口**：`max_concurrent_units`、工具并发、请求频率和缓存容量只限制当前活动资源。窗口满时后续工作等待，不把合法单元总量、工具调用总量或对话回合数转成容量错误。
- **直接依赖**：
  - `main.rs` 负责组合和生命周期，不复制配置、计划、工具或输出规则；
  - worker 只消费协议无关 LLM 事件和 Scheduler 响应；
  - 普通工具统一经过 Scheduler，内部提交工具不进入普通工具注册表；
  - 协议模块只转换协议形状，不决定业务完成、重试预算或发布；
  - 输出格式与审计校验由 `output.rs` 拥有；
  - 计划、配置和外部工具参数分别只在各自边界解析一次。

## 2. 配置维护

Formic 的产品配置只来自一份 TOML，不从环境变量或隐式代理读取覆盖。新增配置字段时同步完成：

1. `FileConfig` 定义、边界解析与类型化校验（严格拒绝未知字段）；
2. 生产调用方消费，以及缺失、非法和冲突测试；
3. `tools/formic/config.example.toml` 与 `tools/formic/docs/usage.md` 更新；
4. 若影响续跑语义，加入作业清单身份并验证变化在远端请求前失败；
5. 保留 `tools/formic/config.toml` 中已有的活动配置。

`extra_body_json` 只接受 JSON object，保留值类型，不能覆盖协议管理字段；增加协议管理字段时同步更新冲突集合和三协议测试。

## 3. CLI、计划与 Prompt

- **CLI**（`main.rs`）：新增或修改参数时核对首次运行与 `--resume` 行为、失败阶段、退出码（`0/1/2/3`）、是否属于作业不可变身份，以及 `tools/formic/README.md`、`usage.md` 与 `--help` 的一致性。
- **计划**（`plan.rs`）：负责文件分片和行区间分片两种形状及路径合法性。文件分片可承载文字和支持图片，行区间只承载 UTF-8 文本；始终通过启动时冻结的目录 capability 访问输入。
- **Prompt**（`prompt.rs`）：保持同一作业的 instructions、任务说明和工具目录前缀稳定，当前分片追加在用户消息末尾。

## 4. LLM 与多模态维护

- **协议无关内核**（`llm/mod.rs`）：统一建模共同历史、工具请求、完成类别、usage 和错误分类，各供应商模块（`completions.rs`、`responses.rs`、`anthropic.rs`）只转换协议形状。新增消息内容或工具结果类型时，同步检查三种请求映射、流式终态、历史重放、token 预算、上下文压缩和审计表示。
- **可审计与脱敏边界**：API key、Authorization、秘密 header、实际请求 URL、HTTP 错误正文、SSE envelope/残帧、传输库原始错误文字、图片 data URL/base64 以及 Responses opaque/encrypted replay payload 不进入终端、`stats.jsonl` 或 worker 档案。允许公开的供应商错误事实限于 HTTP 状态、公开服务类别、allowlist code 和已解析 `Retry-After`。
- **请求预算与压缩**：文字估算覆盖当前协议请求结构、工具 schema、扩展 JSON 和重放项（排除图片 base64），视觉 token 按解码尺寸单独加入，使用 checked arithmetic。历史压缩只替换完整且已完成的旧工具往返，初始用户消息不可压缩；超预算回合可分段读取并传递已验收摘要，全部分段成功且候选进入安全预算后才原子替换历史。
- **图片**（`image_input.rs`）：统一处理本地分片、`read_image` 和 MCP 图片的格式、签名、尺寸与原始字节，不自动下载 URL、转码或缩放。

## 5. 工具、缓存与 MCP

- **内置工具**（`tools.rs`）：拥有 schema、参数默认、路径权限、结果截断和缓存键规范化。阻塞文件系统与图片读取在 blocking task 中执行，并在队列、文件块、行和匹配边界响应取消。错误结果不缓存。
- **Scheduler**（`scheduler.rs`）：收件箱满时等待；不同 server 许可互不占用；同一模型回合的多个普通工具调用并发进入调度器，完成后按模型原始调用顺序回注结果。
- **MCP**（`mcp.rs`）：工具目录在 worker 启动前发现并冻结。原始传输消息上限（`max_message_bytes`）与最终模型结果上限（`max_result_bytes`）分别控制。已发送但超时或传输关闭的远端调用不自动重放，作为“结局未知且未重放”的不可缓存工具错误回注模型；远端明确返回的 JSON-RPC 工具错误同样作为工具结果回注。本地会话建立失败或收到结果后的媒体留档失败则终止当前 worker。

## 6. 状态、输出与续跑

- **作业身份**（`job.rs`）：会改变结果解释的不可变事实写入作业清单；续跑校验在 MCP 初始化和 LLM 请求之前执行，不一致时保持现场并失败。
- **原子发布**（`output.rs`）：结果、schema、状态和档案始终相对启动时绑定的输出目录 capability 操作。结果先写同目录临时文件再 rename 公开，已发布结果不可覆盖；失败 worker 不留完成记录，已发布的其他单元不回滚。
- **审计与统计**：worker 审计完整是结果发布的必要条件；`stats.jsonl` 和进程 metrics 是派生证据，写入失败不改写业务结局。

## 7. 验证与文档同步

常用验证命令：

```powershell
cargo fmt --all --check
cargo check --locked -p formic --all-targets
cargo clippy --locked -p formic --all-targets -- -D warnings
cargo test --locked -p formic
```

按改动范围补充验证：

| 改动范围 | 验证重点 |
| --- | --- |
| CLI、配置、作业身份 | 真实二进制参数、退出码、`run` 首次/续跑、`test` 逐项报告与零作业产物 |
| LLM 协议与压缩 | 三协议请求体、SSE 事件、工具回合、大工具回合分段压缩与取消不提交 |
| 图片与工具权限 | 四种图片格式、混合分片、MCP 图片留档、伪造参数拒绝与并发完成顺序无关性 |
| MCP 与输出续跑 | fake stdio/HTTP、分页发现、超时不重放、原子发布、损坏现场拒绝与取消竞态 |
| 并发、内存与规模 | `cargo run --locked --release -p formic --example scale_run -- 5000 1000 8 20`（遵守[性能工程要求](../../../maintenance/performance.md)与[性能验证指南](../../../maintenance/performance-validation.md)） |

文档按职责同步（用户文档只维护 `tools/formic/` 中的一份）：

| 变化内容 | 更新位置 |
| --- | --- |
| 项目定位、最短用法 | `tools/formic/README.md` |
| CLI、配置、计划、工具、输出、续跑 | `tools/formic/docs/usage.md` |
| 单元边界与 `task.md` 写法 | `tools/formic/docs/task-design.md` |
| 档案、统计、状态与排错 | `tools/formic/docs/observability.md` |
| 模块、依赖、状态或数据流 | `crates/formic/docs/project-map.md` |
| 共同发行与包内资源 | `docs/runtime/distribution.md`、`maintenance/releasing.md` |
