# ATT

ATT 是一个 Windows x64 命令行翻译工具。目前提供三个独立的命令域：

- `mv`：RPG Maker MV；
- `mz`：RPG Maker MZ；
- `generic`：只处理 ATT 约定的 JSONL，不理解具体游戏格式。

同一个游戏可以同时建立 MV/MZ 项目和 Generic 项目。例如，RPG Maker 原生数据由 MZ
项目处理，插件脚本或其他未被原生提取覆盖的文本由外部操作者整理成 JSONL，再交给
Generic 项目。两个项目的数据库、译文、术语、Placeholder、日志和输出互不共享。
公共配置中的 Profile 定义可以复用，但项目分别保存自己的选择；模型任务记录也分别位于
各自项目中。

ATT 不负责把任意游戏格式转换成 Generic JSONL，也不负责把 Generic 输出重新放回游戏。
这些工作由了解目标格式的操作者或外部工具完成。

## 基本流程

每种项目都使用同样清楚的四步：

1. `init` 建立或更新项目；
2. `extract` 读取当前输入并建立可翻译内容；
3. `translate` 生成并保存译文；
4. `write-back` 在项目工作区生成可检查的输出。

MV/MZ 的 Init 保存游戏来源副本。Generic 始终绑定外部 JSONL 目录；修改 JSONL 后，必须
重新执行 Extract，ATT 只清除受影响内容的旧状态。

Lua 是独立的数据库操作命令，不参与 Extract、Translate 或 WriteBack。它适合在翻译完成
后精确修订译文或按项目需要修改数据库。

## 运行环境

正式制品是 Windows x64 程序，最低支持 Windows 10 1903。`att.exe` 内嵌 UTF-8 active
code page 与 long-path-aware manifest。文件系统路径支持 Windows 能表示的 Unicode，
包括中文、Emoji、内部空格、长路径和 UNC 路径；访问 UNC 仍取决于运行账户的权限。

## 文档入口

[ATT 文档总入口](docs/README.md)说明怎样选择引擎，并指向各命令和文件格式的现行规格。
