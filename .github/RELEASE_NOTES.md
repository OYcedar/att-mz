# ATT 1.2.0

ATT 1.2 扩展 OpenAI-compatible 模型接入与流式执行，统一翻译状态、候选验收和译后 QA，
并更新随包 Formic 与游戏汉化辅助能力。

## 模型服务与翻译执行

- 模型 Client 支持 `chat_completions` 与 `responses` 两种协议，`url` 可以填写基础地址或完整
  端点；`stream` 可以选择完整 JSON 或 SSE 流式响应，两种方式都会在响应完整结束后统一验收
  和保存译文。
- HTTP 诊断会区分 DNS、TCP、TLS、发送、读取与各类超时，并按当前配置处理重试、
  `Retry-After`、限速和取消，让失败位置与可继续范围更明确。
- 自动译文、人工译文与 Rejected 候选采用统一状态规则。替代候选通过逐项验收后再提交，
  Placeholder 按源文实际绑定校验，未完整与取消任务会准确汇总已保存结果和剩余工作。

## QA 与随包工具

- Generic 项目可以使用
  `python skills/translate-with-att/scripts/translation_qa.py scan --translations <译文 JSONL> --generic-input <当前 JSONL 根> --output <QA 目录>`
  独立执行全量静态 QA；游戏调查、术语制作、译前检查和译后验收流程已同步精简。
- 随包 Formic 更新到当前静态 Windows x64 构建，并采用当前 TOML 配置、文档和第三方许可材料。
- 新增 MV 玻璃姓名牌汉化 Skill 与 `ATT_NamePlate.js`，可以按游戏实际界面选择接入方式并配置
  姓名牌颜色。

## 从 v1.1.0 升级

1. 升级前用 v1.1.0 为每个项目执行
   `att <mv|mz|generic> manual export --name <项目> --selection all <备份>.toml`。
2. 把 v1.2.0 解压到新目录，带入原 `projects/`，再以新的 `config.example.toml` 为字段集合
   重建 `config.toml`；填写服务值并选择 `protocol` 与必填的 `stream`。
3. 保持项目数据库与最近一次 Extract 快照不变，依次执行 `manual check` 和 `manual apply`，
   将有效条目写为当前人工译文；按提示修正未通过项，未填写项随后用 Manual 或 Translate 完成。
4. 使用 Formic 时同样按新模板重建其 `config.toml`，并为每次 `formic run` 指定必填的
   `--worker-output-access none|published`。

v1.1.0 的项目正文会保留在数据库中，但旧译文适用性与 v1.2.0 不同，未按上述步骤重新应用的
正文不会作为 Current 参与 WriteBack。
