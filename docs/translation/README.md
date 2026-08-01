# 公共翻译能力

MV、MZ 和 Generic 各自拥有自己的项目状态与业务流程，同时复用语义相同的翻译能力：

- [语言](language.md)：语言 ID、源语判断、源语残留检查和安全修复；
- [术语](terminology.md)：ATT 接受的术语文件、命中和模型上下文；术语内容的发现、筛选
  与定译由[通用游戏术语表制作 Skill](../../skills/extract-game-terminology/SKILL.md)
  负责；
- [Placeholder](placeholders.md)：不可改写片段的保护与恢复；
- [TaskBlock 规划](task-planning.md)：Unit、Group、Semantic Scope、稳定装箱与临时 ID；
- [Prompt 与模型协议](prompts.md)：locale、消息、响应信封和数字 ID；
- [模型任务记录](task-records.md)：可读请求与结果记录。

复用的是能力，不是数据：资源在每个项目内分别保存，修改任一项目都不会影响另一个
项目。翻译 Profile 由公共配置统一定义；每个项目分别保存自己最近成功使用的
Profile ID。
