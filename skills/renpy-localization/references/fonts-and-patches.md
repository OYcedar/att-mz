# Ren'Py 字体与独立补丁

用于处理中文缺字、字体替换及独立补丁打包（仅在交付要求包含独立补丁时执行打包与卸载验证）。

## 字体加载与字符覆盖

排查缺字时，核对目标语言下的样式与字体加载链路：默认字体、`gui.*_font`、直接样式 `font`、文本 `{font=...}` 标签、`config.font_replacement_map` 和 `FontGroup`。机制见 [Ren'Py 样式翻译](https://www.renpy.org/doc/html/translation.html#style-translations)。

- 顺着字体别名与替换映射确认最终加载的字体文件，检查完整译文字符集的 glyph 覆盖，随交付保留字体许可证。
- 主字体缺少特殊符号时，可通过 [FontGroup](https://www.renpy.org/doc/html/text.html#font-groups) 将特殊码点路由到支持它们的字体，其余字符使用中文字体（`FontGroup` 采用第一个命中字符范围的字体）。
- 保留游戏原有的独立 emoji 字体、特殊字符映射及用户的 `persistent` 偏好设置。

## 独立补丁打包与回退

1. **补丁内容**：从已验证的部署清单收集译文文件、消费者适配脚本、语言/字体配置及字体文件。若包含 `.rpyc`，使用匹配引擎从最终 `.rpy` 编译。排除无关本体资源、游戏 EXE、ATT 数据库、配置、凭据、日志、缓存与存档。
2. **目录与说明**：ZIP 根目录结构与安装说明保持一致（例如解压 `game/` 合并到游戏根目录），附上适用版本、文件清单、字体许可与卸载说明。
3. **孤立 `.rpyc` 处理**：Ren'Py 编译 `.rpy` 后会生成同名 `.rpyc`；若卸载时只删除 `.rpy`，残留的孤立 `.rpyc` 仍会被引擎加载（见 [Ren'Py 脚本加载规则](https://www.renpy.org/doc/html/language_basics.html#files)）。因此：
   - 安装或首次编译前，备份会被覆盖的原版同名文件（含原版 `.rpyc`）；
   - 卸载说明与卸载脚本需同时清理本补丁新增的 `.rpy` 及其生成的 `.rpyc`，并恢复备份的原件，不误删游戏原有的其他 `.rpyc` 或用户存档。
4. **交付前核对**：校验最终 ZIP 包内容与结构，在干净副本上验证解压安装与卸载恢复。
