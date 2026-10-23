# ATT 1.3.10

本次更新精简翻译文档与随包 Skill，并简化调查、译前检查和外部适配器验收的填写要求。

- Survey 的 Generic 决定直接使用 `groups` 指定语义分组，移除嵌套的 `generic_evidence`；排除决定不再填写 `reason`、`evidence`，`members` 可省略。生成与核对流程继续校验分组覆盖、自然顺序、来源文件和 `kind`。
- Placeholder 译前检查决定只填写 `target`、`decision` 和按需指定的 `protection`，不再填写 `reason`、`evidence`。
- 外部适配器检查与布局测量的 `details` 可留空，文件绑定、实际文本观察和尺寸核对继续保留。
- 精简 RPG Maker、Generic、Ren'Py、VX Ace 等翻译指南和引擎参考，集中引用现行规格；同步整理 Formic 任务设计、维护与运行档案说明。

ATT 与随包 Formic 共同使用版本号 `1.3.10`，发行包为 Windows x64 静态 Release。

升级时保留 ATT 根目录的 `config.toml`、`tools/formic/config.toml` 和 `projects/`。
Survey 与译前检查只接受当前决定格式。继续已有任务时，按新格式重新审核决定并运行 `finalize` 或译前检查，重新生成后续计划与 QA 产物；旧格式不自动迁移。
