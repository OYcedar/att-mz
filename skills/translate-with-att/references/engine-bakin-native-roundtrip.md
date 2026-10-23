# Bakin 原生资源往返与播放器消费者

适用于 RPG Developer Bakin 项目通过外部适配器接入 ATT Generic 的场景。

## 1. 容器解析与无损往返

- **先验证无修改往返**：针对 `data.rbpack`、`BKNPAK` 容器或 `.rbr` 目录记录，先在未修改材料上验证 parser/writer 往返一致，再进入批量翻译。
- **按显示调用筛选字段**：扫描底层 `ReadString` 会同时命中资源路径、内部键和编辑器描述。需按实际显示消费者筛选对白、选项、数据库说明、布局菜单、地点名、系统词汇与标题。
- **关联初始化后再保存**：原生对象的读取与树关联可能是分离的两步。例如 `commonEventListInfo.load` 暂存子文件夹，需调用 `initialize(catalog)` / `initCommonEventListInfo(catalog)` 恢复目录树关联后再 `save`，否则直接 `load → save` 会丢失子文件夹数据（参见 [CommonEventListInfo](https://rpgbakin.com/csreference/doc/en/class_yukar_1_1_common_1_1_rom_1_1_game_settings_1_1_common_event_list_info.html)）。局部适配器无法完整重建关联时，保留对应子块的原始字节。
- **保留未消费字节与资源表结构**：`Map` 等记录若在 reader 返回后仍有尾部字节，或存在非目标子块，写回时原样保留。重建容器时核对资源表的名称、顺序、长度、偏移和压缩标志。

## 2. Generic 映射与命名变量保护

遵循 [Generic JSONL](../../../docs/generic/jsonl.md)、[TaskBlock 规划](../../../docs/translation/task-planning.md) 和 [Placeholder 规格](../../../docs/translation/placeholders.md)：

- **保护命名变量引用**：Bakin HUD、角色详情和事件文字常使用 `\$[key]` 形式的命名变量（由 `GameContentParser.Token.UpdateVariable` 传给 `SystemData.GetStrVariable`，参见 [SystemData](https://rpgbakin.com/csreference/doc/en/class_yukar_1_1_common_1_1_game_data_1_1_system_data.html)）。方括号内即使包含源语言文字也是内部查找键，必须完整保护（不能只匹配反斜杠后字母的控制码而漏掉 `$[...]`）。若旧译文误改了变量键导致画面数值空白，从源映射恢复原始变量引用。
- **字符串变量与存档兼容**：若某个字符串变量既用于显示又用于条件分支比较，保留变量原值与比较常量不变，在显示边界映射译文，避免破坏既有存档或分支判断。

## 3. 窗口标题与播放器原生菜单

- **窗口标题**：游戏内标题（`GameSettings.meta.title`）与桌面窗口标题是不同字段。窗口标题通常走 `Yukar.Player.Program.startApplication → GameSettings.name → SharpKmy.Entry.setWindowTitle`，提取时需将 `GameSettings.name` 一并纳入。
- **原生工具栏菜单**：顶部菜单可能位于 `kmyCore.dll` 或 `kmyWindowsCommon.dll` 的 Windows `RT_MENU` 资源中（而非 .NET 卫星程序集）。按各语言资源的实际菜单层级、命令 ID 修改显示标题，正确处理助记符 `(&X)`、字面量 `&&` 和制表符后的快捷键。
- **启动器覆盖机制**：若启动器从 `bakinplayer.pak`（ZIP 格式）解压并覆盖播放器 EXE/DLL，仅修改外部松散文件会在启动时被还原，需同步更新 `bakinplayer.pak` 内的对应资源。

## 4. 内嵌字体与 UI 布局字段

- **区分布局字体与对白字体**：[SystemGraphics](https://rpgbakin.com/csreference/doc/en/class_yukar_1_1_common_1_1_rom_1_1_game_settings_1_1_system_graphics.html) 分别定义 `defaultLayoutFont` 与 `defaultMessageFont`，具体布局节点还可能显式指定 `font`。替换字体时先确认 Font 记录与底层载荷是否被多处共享，检查完整译文的字符集覆盖，保留原字号与记录参数。
- **布局调整与值类型写回**：若中文字体导致 UI 标签换行、贴边或模糊，核对目标节点及父容器的尺寸、缩放、位置、对齐与内边距。在 C# 反射中修改嵌套值类型（如 boxed `Vector2`）时，必须将修改后的结构体重新赋值给父对象字段后再保存。
