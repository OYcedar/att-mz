# ATT 1.3.0

ATT 1.3 提供模型连接自检和 RPG Maker 启动标题同步。本次维护修复控制符验收、已有译文复验、
任务记录和输入诊断，并统一文档中的操作与恢复说明。

## 验收、状态与记录修复

- Rules 提取标准数据库字段或事件参数时，继承该物理位置的内建控制契约；Manual、Translate
  和 WriteBack 使用同一判断。
- Generic 用源文已经建立的 Placeholder 绑定复验当前译文，保留标签翻译后仍合法的人工或自动正文。
- 能唯一定位的空数组模型候选保存为 Rejected；任务记录明确区分拒绝候选已保存与项目未改变。
- RPG Maker 任务记录对 User JSON 使用统一脱敏；Lua print 保留参数分隔，前序失败后的任务说明
  明确指出未提交原因。
- 配置与 JSONL 错误指出字段、位置和期望类型；CLI 参数、日志阶段和任务终态补齐十种语言说明。

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

- 随包 Formic 的精确源码、构建与公开状态见
  [来源记录](https://github.com/yexi-by/att/blob/main/tools/formic/FORMIC-SOURCE.md)。
  `formic test --config config.toml` 可以依次检查真实 LLM 请求、MCP 初始化和完整工具目录；
  自检失败后会继续检查后续服务并输出汇总，不会建立或修改作业档案。
- Formic 结构化输出支持 nullable、`const`、字符串与数组长度和数值范围约束；一次反馈全部格式
  问题，并在提交结果与普通工具混用时于工具执行前拒绝整个回合。
- Windows 发布检出统一使用 LF，发行包中的托管文档与配置模板会和仓库权威内容逐字节一致。
- Formic worker 档案明确显示已停止状态，工具名、模型名和图片路径在 Markdown 中保留正确字符。
  随包指南直接使用预构建程序，并说明结构化结果与上下文压缩各自的无效提交上限。

## 使用与验收

- 使用与程序同一发行根中的配置、Prompt 和文档；普通资源同步保留 ATT 与 Formic 的活动配置。
- 项目使用当前数据格式。按引擎规格执行 Init、Extract，再进行翻译；MV/MZ 的 WriteBack 包含
  符合条件的启动标题消费者。
- 翻译未完整时按 pending 与 Rejected 选择继续方式；Rejected 使用 `--retry-rejected`，或导出到
  Manual 集中修订。脚本静态检查与 Agent 语义审校分别完成。
- 替换为新版 `ATT_NamePlate.js` 后，姓名牌使用当前固定视觉样式；原有颜色、偏移和间距参数
  不再生效，请按实际游戏界面完成普通人物对话、旁白、长姓名、上下位置消息和对话回看的实机检查。
