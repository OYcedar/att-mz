# RPG Maker WriteBack 现行规格

```text
att mv write-back --name NAME
att mz write-back --name NAME
```

WriteBack 不使用 Lua。它只读取冻结来源、当前提取资产和译文，在项目工作区生成：

```text
<att-dir>/projects/<mv|mz>/<name>/write_back/
```

原游戏与冻结来源在整个过程中保持原样。

## 1. 候选构建

ATT 从冻结来源建立完整内容树，并按 recipe 把 Current 译文写回对应 RPG Maker 值：

- 普通字符串替换完整值；
- 固定逐行或逐项内容保持规定的槽数和空槽；
- 自由断行内容按目标数组重新布局；
- 对话、选项和滚动文本按事件命令结构重建；
- Rules 的嵌套 JSON、捕获与 Literal 按原 grammar 重新编码。

未译或非 Current Unit 保留冻结原文。Partial 项目同样可以生成候选，结果会明确报告
保留原文的数量。

布局器无法安全自动断行时，WriteBack 保留该译文的显式硬换行并继续构建候选，不把它
伪报成已经自动布局。成功结果中的 `manual_layout_units` 与结构化人工布局诊断逐项对应。
每项诊断都包含：

- 受影响逻辑单元的精确 `group_location`；
- 每个逻辑单元的 `role`；
- 显示区域 `region`；
- 本次判断采用的 `max_fullwidth_chars`。

发布成功后，这些诊断逐项写入 stderr，并以 Warn 事件
`write_back.manual_layout_required` 写入当前 RunId 的 JSONL。只有人工布局总数不足以让
操作者找到需要处理的位置。人工布局本身不改变成功发布的业务结果；若警告无法呈现，按
独立的进程呈现失败处理。

## 2. 完整验证

发布前重新检查：

- 来源、owner 与 Mutation Claim 指纹；
- 每个 recipe 的原值、目标位置和结构；
- 修改范围没有重复、祖先/后代或跨 owner 冲突；
- 全部 JSON、事件命令、数组形状和控制符有效；
- 未声明位置与冻结来源逐字一致。

候选必须重新解析，并证明改动只落在受管范围内。WriteBack 只搬运文本：不执行脚本，
也没有发布后回调。

## 3. 一次发布

全部候选完成并验证后，ATT 一次替换整个 `write_back/`。任何失败都会让上一次成功
输出原样保留，半成品不会被当作成功结果。发布结果无法确认时按
[目录发布规格](../runtime/directory-publishing.md)保留现场并报告恢复位置。

`write_back/` 是一棵内容树，而不是可以直接运行的完整游戏包。把它按实际部署方式
放进隔离游戏副本、确认游戏真正读取了这些文件，这一步由操作者完成。

目录发布明确要求保留恢复现场时，运行终态是 `recovery_required`，诊断列出恢复路径和
处理办法；只有无法确认目录交换是否生效时才使用 `outcome_unknown`。两者不能互相替代。
