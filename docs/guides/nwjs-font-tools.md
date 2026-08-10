# NW.js 运行观察与 RPG Maker 字体工具

这两项随包工具用于 WriteBack 后的实际界面检查和字体替换。它们不会修改 ATT 项目数据库，
也不能代替来源所有权、Placeholder、语言残留和 WriteBack 验收。

## 1. NW.js 实际界面观察

只在可丢弃的完整游戏副本上运行。工具会为这个副本启动自己的 `Game.exe`，给它使用独立的
NW.js 用户数据目录，通过仅监听本机回环地址的 Chrome DevTools Protocol 读取实际绘制，
结束时只关闭自己启动并持有的进程。工具不发送键盘事件，也不查找或终止其他 NW.js 进程。

自动检查已知场景：

```powershell
python skills/translate-with-att/scripts/inspect_nwjs_runtime.py smoke `
  --game D:\games\translated-copy `
  --output D:\review\nwjs-smoke `
  --confirm-isolated-copy
```

`smoke` 依次尝试标题、新游戏、对话、菜单、任务日志、选项和存档。任务日志只有在当前菜单
存在唯一的 quest、journal、mission、task 或 log 命令，并且该命令有实际 handler 时才会进入；
它不会从全局类名猜测插件场景。对话只有实际观察到 `Window_Message` 绘制文本才算已验证。

记录正常游玩：

```powershell
python skills/translate-with-att/scripts/inspect_nwjs_runtime.py observe `
  --game D:\games\translated-copy `
  --output D:\review\nwjs-observe `
  --confirm-isolated-copy
```

`observe` 不自动切换场景。省略 `--duration` 时持续到游戏窗口关闭或按 Ctrl+C；传入秒数时才
定时结束。应使用鼠标或游戏本身支持的正常控制方式游玩。

输出目录包含 `report.json`、实际绘制 `draws.jsonl`、英文候选、像素越界、字体加载检查和
截图。`qa_status` 解释如下：

- `needs_review`：实际观察到英文、可测的像素越界，或请求的 font family 未加载；
- `unverified`：没有上述发现，但仍有未访问场景，或无法证明每个 glyph 没有回退字体；
- `clean`：`smoke` 的必验场景都有实际绘制证据，且没有已知发现或未验证项。

英文候选仍需区分专名、资源事实和漏译。像素越界会同时检查 `Bitmap.drawText` 的实际位图
边界与可安全测量的纯文本 `Window_Base.drawTextEx`；含控制符或换行的 `drawTextEx` 不猜测
宽度，而是在 `layout-measurement-unverified.jsonl` 中记录为未验证。`document.fonts.check` 能
确认请求的 family 是否加载，不能证明单个字符最终来自哪个字体，因此工具不会把未知的逐字
回退伪装成通过。

## 2. 递归字体调查、替换与恢复

工具接受一个 OTF/TTF 文件，也可以直接选择三款随包字体：

- `noto-sans-sc`：Noto Sans CJK SC Regular 2.004；
- `noto-serif-sc`：Noto Serif CJK SC Regular 2.003；
- `lxgw-wenkai`：LXGW WenKai GB Regular 1.522。

先只读调查：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py inspect `
  --game D:\games\translated `
  --font noto-sans-sc `
  --coverage-text D:\review\all-visible-chinese.txt `
  --output D:\review\font-inspect.json
```

工具递归建立“字体资产 → 已声明 family/stem → 静态消费者”关系。带字体后缀的完整资源路径、
CSS 字体声明、已识别的加载 API、MV/MZ 标准字体字段，以及键名明确表示字体的完整配置值，
都属于已证明引用。`apply` 会一次处理这些引用，包括数字、图标和展示字体，不要求逐项确认。
普通正文或普通字符串即使整值恰好等于字体 family/stem 也不会自动修改；动态值、部分表达式和
无法证明消费者的字体资产进入 Review。

执行可逆替换：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py apply `
  --game D:\games\translated `
  --font lxgw-wenkai `
  --coverage-text D:\review\all-visible-chinese.txt `
  --state D:\review\font-state `
  --output D:\review\font-apply.json
```

`apply` 先建立包含每项替换前后完整字节和摘要的 `state`，再原子写入。选中字体缺少
`--coverage-text` 中的字符只会进入 Review，不会阻止安全的已证明引用替换。没有已证明引用、
或所有引用已经指向选中字体时命令仍成功退出；报告用 `applied`、`no_op` 和 `qa_status`
分别说明是否写入、是否无需写入和质量结论。

恢复：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py restore `
  --game D:\games\translated `
  --state D:\review\font-state `
  --output D:\review\font-restore.json
```

`restore` 逐项接受 state 中记录的 apply 前或 apply 后字节，因此进程在部分写入后中断也能
恢复；遇到第三种字节时拒绝覆盖。写入中途失败只回滚已经尝试的文件，不会删除或覆盖尚未
尝试位置上由其他进程建立的内容。`state/status.json` 记录 `prepared`、`applied`、
`rolled_back`、`restored` 或 `recovery_required`；出现 `recovery_required` 时必须停止使用该
游戏目录，并根据 `manifest.json` 和前后快照恢复，不得重试 apply。

## 3. 字体来源与许可

三款字体均为上游官方 Release 的未修改文件，使用 SIL Open Font License 1.1。精确版本、
下载地址、文件大小和 SHA-256 记录在 `licenses/fonts/SOURCES.json`；Noto CJK 与
LXGW WenKai GB 的许可原文分别保存在同目录对应的 OFL 文件中。
