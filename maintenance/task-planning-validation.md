# TaskBlock 规划验证指南

本指南规定维护共享 Planner 或新增引擎时需要提供的跨状态证据。产品语义以
[TaskBlock 规划现行规格](../docs/translation/task-planning.md)为准。

使用同一个完整数据源分别覆盖全部待译、部分 Current、跨块去重、非源语、完全保护和全部
Current，并断言每种状态得到的完整 TaskBlock 清单在数量、顺序、边界及 Group、Unit 成员上
完全相同。测试只允许临时 ID、无编号语境的呈现内容和实际执行块集合随状态变化，并断言
过滤无 ID 块后没有重新装箱。

Partial 用例先让同一块中的部分 Unit 通过验收，再从相同完整数据重新规划；已通过的 Unit
留在原块中成为无 ID 的目标译文语境，未通过的模型代表留在原块中获得新的块内连续 ID，
实际发送和记录的 user message 仍包含该块的全部语境。
