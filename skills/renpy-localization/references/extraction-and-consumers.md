# Ren'Py 提取与显示消费者

在初次转换、补提取或定位漏翻时使用。ATT 的输入、状态和输出契约以包内 Generic 文档为准；本页说明 Ren'Py 外部适配要点。

## 原生模板与结构定位

Ren'Py 原生对话翻译块和菜单/字符串表是两类主要来源。模板生成方式见 [Ren'Py 翻译文档](https://www.renpy.org/doc/html/translation.html#generating-translation-files)，具体命令选项以游戏自带版本为准。

- **对话翻译块**：保留翻译块身份、说话者、属性、`voice` 等非正文结构。空目标模板从对应源节点或源注释恢复原文。
- **字符串表**：保留 `old` 的准确值和上下文标签（如 `{#...}`），将 ATT 结果写入 `new`，不修改作为查找键的 `old`。
- **语法解析**：使用词法或语法解析处理转义、引号、相邻字符串、多行值和注释，不对 `.rpy` 做无边界全文替换。
- **共享字符串**：相同源字符串在原生字符串表中共享查找结果。若不同场景需要不同译法，先确认显示位置是否支持 [Ren'Py 上下文标签](https://www.renpy.org/doc/html/translation.html#menu-and-string-translations)；确实共用同一查找键时统一译法，避免反向转换按遍历顺序互相覆盖。

## 模板之外的界面

原生生成器默认识别菜单及 `_()` 标记的字符串（见 [Ren'Py 字符串翻译函数](https://www.renpy.org/doc/html/translation.html)）。以下位置可能不在原生模板中，需从显示调用反查：

- screen 中的 `text` / `textbutton`；
- 脚本中的 `show text`、`Text(...)`；
- 角色名定义、Python 动态拼接字符串及自定义显示组件。

对缺少翻译查找接入的硬编码显示文本，可在保持游戏逻辑（变量、资源路径、按钮 `action`、比较键不变）的前提下补充 `_()` 标记或接入字符串表。

## 新来源与占位符

- **补提取归组**：按 [Generic JSONL 的语义边界](../../../docs/generic/jsonl.md#3-从源格式建立-group-与文件范围)归组，保留未变来源的稳定 ID。属于已有对话语境的漏提问句、回答或选项补入原 Group 并恢复自然顺序。
- **状态更新**：重新 Extract 后，按 [Generic Extract](../../../docs/generic/extraction.md) 检查受影响 Group（Group 成员变化会使原有自动译文失去 Current），补译或通过 Manual 审校后再执行 WriteBack。
- **占位符与标签**：按 [Placeholder 规格](../../../docs/translation/placeholders.md) 保护 `[...]` 插值、`{...}` 文本标签及 `[[` / `{{` 转义，核对恢复后的标签配对与顺序。

## 编译与加载顺序

- 使用匹配引擎执行 [Ren'Py CLI](https://www.renpy.org/doc/html/cli.html#translate) 编译。对包含无源码 `.rpyc` 的游戏，编译前确认保留孤立 `.rpyc`（如 `--keep-orphan-rpyc`），避免丢失必需脚本。
- 同名 screen 或 `.rpa` 归档与松散脚本并存时，核对引擎实际加载优先级，移除测试钩子后再交付。
