---
name: renpy-localization
description: 使用 ATT Generic JSONL 完成 Ren'Py 游戏汉化、续跑和漏翻返修，衔接原生翻译模板、外部来源映射、界面显示、字体与独立补丁验证。
---

# Ren'Py 游戏汉化

在隔离的游戏副本中完成 Ren'Py 提取和部署，通过 ATT Generic 管理翻译状态、术语、模型调用与结构验收。先读取 [ATT 翻译主线](../translate-with-att/SKILL.md) 了解通用阶段，按需结合 [JSONL](../../docs/generic/jsonl.md)、[Init](../../docs/generic/init.md)、[Extract](../../docs/generic/extraction.md)、[Translate](../../docs/generic/translation.md) 和 [WriteBack](../../docs/generic/write-back.md) 规格执行。

## 1. 确认引擎与提取范围

确认游戏的 Ren'Py 版本、实际游戏目录及 `.rpy`、`.rpyc`、`.rpa` 分布，使用该游戏自带引擎或兼容 SDK 处理。

覆盖正文、菜单、角色名、自定义界面与启动提示。详细提取与界面接入方法见 [提取与显示消费者](references/extraction-and-consumers.md)。

## 2. 建立 Generic 往返映射

外部转换负责 Ren'Py 语法和定位，ATT 接收符合 JSONL 契约的文本：

- 按 [Generic 分组规则](../../docs/generic/jsonl.md#3-从源格式建立-group-与文件范围)沿实际剧情将连续翻译块、相关旁白和菜单选项归入完整语境 Group；原生翻译块和 label 仅作定位线索，不直接作为拆散语境的边界。
- 在外部映射中保存源文件、原生翻译块/字符串身份、结构位置与原值；ID 稳定且写回位置唯一。
- 先验证小范围未修改文本与替换文本的往返，确保语法、引号转义、多行值与非目标结构保持完好。

## 3. ATT 翻译与续跑

按主技能执行 Init、Extract、完整 Manual 导出、术语、Placeholder、Translate 与 QA。Ren'Py 插值（`[...]`）、文本标签（`{...}`）和转义（`[[`/`{{`）按 [Placeholder 规格](../../docs/translation/placeholders.md) 及实际脚本结构保护。

补充新来源时沿原项目重新 Extract，保持未变来源的稳定 ID，按 [新来源与占位符](references/extraction-and-consumers.md#新来源与占位符) 检查受影响 Group 的译文状态。

## 4. 反向转换与显示接入

消费本轮 WriteBack 的全部 JSONL，按映射生成对话翻译块与字符串表并部署到隔离游戏副本：

- 核对 `show text`、`Text(...)`、screen 组件和 Python 动态生成的界面文本是否已接入翻译查找。
- 使用匹配引擎编译，核对日志及生成的 `.rpyc`。含孤立 `.rpyc`（无对应 `.rpy`）的项目在编译前启用保留机制（如 `--keep-orphan-rpyc`），避免丢失必需脚本。
- 同名 screen 或归档与 loose 脚本并存时，确认实际生效的加载定义。

## 5. 字体、补丁与验收

处理中文缺字、替换字体或打包独立补丁时，读取 [字体与补丁验证](references/fonts-and-patches.md)。完成部署与静态检查后，向任务发起者说明已完成范围及需人工实机检查的场景（包含启动提示与自定义界面）。
