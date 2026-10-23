# TyranoScript／Electron ASAR 汉化与补丁经验

适用于 Electron 应用从 `resources/app.asar` 加载 TyranoScript 场景（`data/scenario/*.ks`）、配置（`data/system/Config.tjs`）和网页界面的汉化任务。

## 1. 显示消费者与提取范围

- 追踪 `package.json` 入口、主进程页面、场景加载器及活动插件。除场景正文外，检查选择项 `glink.text`、`ptext.text`、`tb_ptext_show.text`、按钮提示、系统弹窗和默认姓名。
- **语境分组**：Builder 的 `tb_start_text`／`_tb_end_text` 等标签块仅用于定位文本范围；按 [Generic 分组规则](../../../docs/generic/jsonl.md#3-从源格式建立-group-与文件范围)沿场景将连续对白、旁白和选项归入同一 Group，不把每个标签块切散为独立 Group。
- **JS 字面量与角色姓名**：JavaScript 脚本使用 AST 定位显示字符串，注意 UTF-16 代码单元偏移与 UTF-8 字节偏移的转换。`#name`、角色注册和语音配置常共用角色标识，需区分内部查找键与显示姓名，在显示处映射译名以保留语音和立绘关联。

## 2. 控制保护与文本槽拓扑

- 按 [Placeholder 规格](../../../docs/translation/placeholders.md) 保护 Tyrano 内联标签、宏参数、插值和 HTML 包装标签。
- 属性替换时排除闭合 `]`、引号与分隔符。回填后用 Tyrano 场景解析器核对命令顺序、`label`/`jump` 跳转关系以及正文所处的标签槽位（避免正文越过 `[p]`、移出字体 wrapper 或跑到角色宏之前）。

## 3. 字体与 ASAR 封包发布

- **CSS 与字体加载**：`Config.tjs` 的默认字体不一定覆盖 `ptext`、直接 CSS 样式或特效文字。顺着 `font-family`、`@font-face` 和样式表确认所有文本消费者的字体覆盖，随包附上字体许可证。
- **ASAR 读写与完整性**：Electron 将 `app.asar` 作为虚拟文件系统读取（参见 [Electron ASAR 文档](https://www.electronjs.org/docs/latest/tutorial/asar-archives)）。
  - 大型 ASAR 归档采用按索引 seek 和分块流式读写，正确计算 Pickle 头部、JSON UTF-8 字节长度、对齐及条目 `offset`/`size`，保留配套的 `.asar.unpacked` 外置关系。
  - 若目标构建启用了 [ASAR Integrity](https://www.electronjs.org/docs/latest/tutorial/asar-integrity) 校验，需同步更新匹配的完整性哈希。
  - 封包完成后独立重读 ASAR 索引与修改条目，校验 JSON/JS 语法。
- **小型差分补丁**：若需交付轻量补丁，可打包修改/新增文件与流式重建脚本，在安装前校验原版归档指纹、备份原件并支持一键回滚恢复。
