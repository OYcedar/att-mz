# Formic 维护边界

本目录遵守 ATT 根目录的维护规则。Formic 负责独立单元的多轮模型与工具执行、资源调度、
取消、恢复、原子发布和可审计证据；调用方负责分片规划与跨单元归并。

- 单元正确性不依赖其他 worker 的完成顺序或共享可变业务状态。
- worker 只消费协议无关模型事件和 Scheduler 响应，普通工具统一经过冻结的工具目录与
  Scheduler；内部结果提交和上下文压缩提交属于 worker 控制通道。
- 已发布结果不可覆盖，单元失败不回滚其他已确认进展；作业身份、结果与恢复状态保持一致。
- worker 审计完成是结果发布条件；派生统计仍只作为证据。秘密、传输 envelope、无效
  payload 和图片编码不得进入公开档案。

开始维护时读取[项目地图](docs/project-map.md)和[维护指南](docs/maintenance.md)。
Formic 的用户契约由 [`tools/formic/docs/`](../../tools/formic/docs/README.md) 定义；按当前
改动读取其中的使用、任务设计或可观测性文档。ATT 的翻译协议与项目状态规格不替代 Formic
自己的契约。跨程序调用和共享能力变化时，再读取相应 ATT 规格并检查全部调用方。

配置模板、README 和用户文档只维护 `tools/formic/` 中的一份；本目录仅保存开发资料。
构建、版本和发行遵循 ATT workspace 与根[发行指南](../../maintenance/releasing.md)。
