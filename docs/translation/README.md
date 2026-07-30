# 公共翻译能力

MV、MZ 和 Generic 分别拥有自己的项目状态与业务流程，但会复用语义相同的翻译能力：

- [语言](language.md)：语言 ID、源语判断、源语残留检查和安全修复；
- [术语](terminology.md)：术语文件、命中和模型上下文；
- [Placeholder](placeholders.md)：不可改写片段的保护与恢复；
- [Prompt 与模型协议](prompts.md)：locale、消息、响应信封和数字 ID；
- [模型任务记录](task-records.md)：可读请求与结果记录。

“复用能力”不表示项目共享数据。资源在每个项目内分别保存，任一项目的修改都不会改变
另一个项目。翻译 Profile 由公共配置统一定义；每个项目仍分别保存自己最近成功使用的
Profile ID。
