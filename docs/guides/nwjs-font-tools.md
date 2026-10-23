# NW.js 运行观察与 RPG Maker 字体工具

这两项随包工具用于 WriteBack 后的界面观察和字体替换，操作对象是独立游戏副本与工具报告，不修改 ATT 项目数据库。

运行观察和字体工具按“游戏根 → 内容根”的顺序解析 `package.json` 中的本地 HTML 入口（相对路径以对应配置文件所在目录为基准）；两处配置共存时优先采用游戏根入口，与启动 `Game.exe` 的行为一致。

## 1. NW.js 实际界面观察

在隔离游戏副本上运行。工具启动并持有该副本的 `Game.exe`，使用独立 NW.js 用户数据目录，通过仅监听本机回环地址的 Chrome DevTools Protocol 读取实际绘制，结束时关闭自有进程。同一输出目标通过旁边的 `.<output-name>.runtime.lock` 串行建立运行现场。

自动检查已知场景：

```powershell
python skills/translate-with-att/scripts/inspect_nwjs_runtime.py smoke `
  --game D:\games\translated-copy `
  --output D:\review\nwjs-smoke `
  --confirm-isolated-copy
```

`smoke` 和 `observe` 先等待调试连接就绪且游戏离开 `Scene_Boot`（最长等待 `--startup-timeout` 秒，默认 75 秒；正常启动立即继续）。`smoke` 随后依次尝试标题、新游戏、对话、菜单、任务日志、选项和存档，每次切换后等待 `--settle-ms` 毫秒（默认 900）再记录。任务日志仅在当前菜单存在唯一 quest/journal/mission/task/log 命令且具备实际 handler 时进入；对话需观察到 `Window_Message` 绘制文本才算已验证。

记录正常游玩：

```powershell
python skills/translate-with-att/scripts/inspect_nwjs_runtime.py observe `
  --game D:\games\translated-copy `
  --output D:\review\nwjs-observe `
  --confirm-isolated-copy
```

`observe` 不自动切换场景。省略 `--duration` 时持续到窗口关闭，传入秒数时按时结束并发布报告；按 Ctrl+C 取消并返回退出码 `130`（若报告目录已完成交换则保留已发布报告）。

输出目录包含 `report.json`、`events.jsonl`、实际绘制 `draws.jsonl`、英文候选、像素越界、字体加载检查、`runtime-errors.jsonl` 和截图。`qa_status` 含义：

- `needs_review`：启动未完成，或观察到运行时错误、英文残留候选、像素越界、请求的 font family 未加载；
- `unverified`：无上述发现，但仍有未访问场景，或无法确认逐字符是否发生字体回退；
- `clean`：`smoke` 必需 hooks 已安装，全部必验场景均有语义匹配的非空文本绘制与有效 PNG 截图（至少 64×64），且无发现或未验证项。

像素越界检查 `Bitmap.drawText` 位图边界与纯文本 `Window_Base.drawTextEx`；含控制符或换行的 `drawTextEx` 记录在 `layout-measurement-unverified.jsonl`。

若自定义 NW.js 包装器不支持命令行调试端口，工具会明确失败；此时直接按玩家方式启动隔离副本，确认进入标题与首段交互界面无报错并留存截图。若译本启动报错而未修改的原版副本正常，重点排查 WriteBack、字体、资源与配置差异。

## 2. 递归字体调查、替换与恢复

工具支持传入自定义 OTF/TTF 文件，或直接选用三款随包字体：

- `noto-sans-sc`：Noto Sans CJK SC Regular 2.004；
- `noto-serif-sc`：Noto Serif CJK SC Regular 2.003；
- `lxgw-wenkai`：LXGW WenKai GB Regular 1.522。

只读调查（`inspect`）：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py inspect `
  --game D:\games\translated `
  --font noto-sans-sc `
  --translations D:\review\translations-after-manual.jsonl `
  --coverage-text D:\review\additional-player-text.txt `
  --output D:\review\font-inspect.json
```

执行可逆替换（`apply`）：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py apply `
  --game D:\games\translated `
  --font lxgw-wenkai `
  --translations D:\review\translations-after-manual.jsonl `
  --coverage-text D:\review\additional-player-text.txt `
  --state D:\review\font-state `
  --output D:\review\font-apply.json
```

恢复原状（`restore`）：

```powershell
python skills/translate-with-att/scripts/manage_rpg_maker_fonts.py restore `
  --game D:\games\translated `
  --state D:\review\font-state `
  --output D:\review\font-restore.json
```

### 引用识别与替换规则

- **已证明引用**：带字体后缀的资源路径、CSS 字体声明、已知加载 API、MV/MZ 标准字体字段、JSON/XML 中键名明确表示字体的完整值，以及 TOML 裸键单行字符串赋值。`apply` 自动替换这些引用指向的字体资源。
- **保留运行时别名**：CSS `@font-face` 和字体加载 API 已注册的 family 名称（如 `GameFont`）原样保留，仅替换其指向的字体文件；未注册、仅由旧文件名 stem 推导的名称随文件名更新。
- **`YEP_LoadCustomFonts`**：活动插件的 `Font Filenames` 与 `Font Families` 按逗号分隔逐项配对，保留 family 名称与顺序，仅更新加载文件名（以实际 HTML 入口所在目录的 `fonts/` 为基准）。列表缺失、项数不一致或资源缺失时保留原值并记入 Review。
- **进入 Review 的情况**：INI 候选、CSS `tech()` 或无法解析的动态来源、停用插件列表及无法证明消费者的字体资产保持原值并进入 Review。

### 字符覆盖与状态恢复

- `--translations` 传入当前 `translation export`（按 WriteBack 行为投影字符：current 取译文，pending/rejected 取原文）；`--coverage-text` 可重复传入额外玩家可见文本。
- 工具使用 Unicode cmap 4/12/13 检查基础字符与普通结合字符；孤立或未附着在基础字符后的变体选择符记入 `coverage.unattached_variation_selectors`，缺字记入 `missing_characters`（使 `qa_status` 为 `needs_review`）。
- `apply` 在写入前建立包含替换前后快照的 `state` 目录。`state/status.json` 记录 `prepared`、`applied`、`rolled_back`、`restored` 或 `recovery_required`；出现 `recovery_required` 时执行 `restore` 恢复，不直接重试 `apply`。

## 3. 字体来源与许可

三款随包字体均为上游官方 Release 的未修改文件，使用 SIL Open Font License 1.1。版本、下载地址、文件大小和 SHA-256 记录在 `licenses/fonts/SOURCES.json`，许可原文保存在同目录对应的 OFL 文件中。
