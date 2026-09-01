# ATT 1.3.0

ATT 1.3 新增模型连接自检，完整同步 RPG Maker 启动标题，并改进翻译诊断、随包 Formic
和 MV 姓名牌模板。

## 模型连接自检

- 新增 `att test`。命令会完整校验发行配置，并按 Client ID 顺序逐项使用当前协议、流式设置、
  Endpoint、认证、代理、PEM、超时、限速和请求参数检查全部 LLM Client。
- 单个 Client 失败后会继续检查后续 Client，并统一输出结果与汇总；命令不会建立翻译项目、
  数据库、项目日志或任务记录。
- 每个 Client 会发送一次真实模型请求，执行时可能产生少量模型用量。

## 翻译执行与诊断

- 重试汇总中的 attempted、recovered 与 exhausted 现在都准确表示首次请求后的额外 HTTP
  attempt。
- 未完整翻译会按实际 started/total 完成任务确认阶段，并保留未开始任务和剩余工作。
- 模型响应诊断会区分 JSON 语法错误与响应结构不符合契约，并使用自然任务编号、可读 Unit ID
  和临时 output ID 定位问题。
- 需要复核的译文会根据实际提交结果说明已写入或进度已保留，并直接给出 Manual 复核路径。

## RPG Maker MV/MZ

- Init 会把标准根 `package.json` 及其 `main` 指向的安全 HTML 纳入只读来源快照。
- `System.json.gameTitle` 继续只建立一个 Builtin Unit。WriteBack 会把译题同步到仍与原题一致的
  `package.json.window.title` 和唯一标准 `<title>`，同时逐字保留独立标题及其他内容。
- 游戏调查工具使用同一规则识别启动标题消费者，避免建立重复翻译 Unit。
- MV 姓名牌模板更新为透明背景、白色细边和贴合对话框的布局。新模板采用固定视觉样式，
  不再读取原有颜色、偏移和间距参数。

## 随包 Formic 与发行资源

- 随包 Formic 同步到公开源码提交
  [`f1bce99`](https://github.com/yexi-by/formic/commit/f1bce99f0732a4d5a40e50d42c97232122734eed)。
  新增 `formic test --config config.toml`，可以依次检查真实 LLM 请求、MCP 初始化和完整工具目录；
  自检失败后会继续检查后续服务并输出汇总，不会建立或修改作业档案。
- Formic 结构化输出支持 nullable、`const`、字符串与数组长度和数值范围约束；一次反馈全部格式
  问题，并在提交结果与普通工具混用时于工具执行前拒绝整个回合。
- Windows 发布检出统一使用 LF，发行包中的托管文档与配置模板会和仓库权威内容逐字节一致。

## 从 ATT 1.2 升级

- 项目数据库和活动配置格式保持不变，可以继续使用现有 `projects/`、ATT `config.toml` 和
  Formic `config.toml`。
- 现有 MV/MZ 项目如需启用启动标题同步，先重新执行 `att <mv|mz> init --name <项目>`，再执行
  `att <mv|mz> extract --name <项目>`；随后生成的 WriteBack 会包含符合条件的启动标题消费者。
- 替换为新版 `ATT_NamePlate.js` 后，姓名牌使用当前固定视觉样式；原有颜色、偏移和间距参数
  不再生效，请按实际游戏界面完成普通人物对话、旁白、长姓名、上下位置消息和对话回看的实机检查。
