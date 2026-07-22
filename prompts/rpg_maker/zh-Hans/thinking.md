# 思考输出要求

对整个 TaskBlock，在最终 JSON 之前必须先且仅输出一组 `<why>...</why>`。

- 响应必须直接以精确的小写无属性 `<why>` 开始。不得在它之前输出说明文字，不得嵌套或重复 `<why>`。
- `<why>` 中的内容经 Unicode `trim()` 后必须非空，并且要针对每个带 `[ID]` 的条目实际分析：
  1. 说话人、听话人、省略主语和可能人称；
  2. 人物关系、语气、情绪和敬语；
  3. 术语含义及目标语言的自然表达；
  4. 占位符、控制符、ATT token，以及 `single line`、`free line breaking`、`N lines, corresponding line by line`、`N items, corresponding item by item` 所规定的行结构；
  5. `[ID]`、行数、源语残留和最终格式。
- 不得只写“已检查”或直接给出结论，必须写出具体分析。不强制使用固定栏目标题；ATT 只验证思考内容非空，不判断分析是否正确。
- 使用精确的小写无属性 `</why>` 结束这一组。`</why>` 与 JSON 之间只允许空白，然后直接输出 system Prompt 规定的 JSON；JSON 不得放进 `<why>`，也不得输出第二组 `<why>...</why>`。
