# ATT

ATT 是一个 Windows x64 命令行翻译工具，提供三个各自独立的命令域：

- `mv`：RPG Maker MV；
- `mz`：RPG Maker MZ；
- `generic`：只处理 ATT 约定的 JSONL，不关心具体游戏格式。

同一个游戏可以同时建立 MV/MZ 项目和 Generic 项目，两者分工明确：RPG Maker 原生数据
交给 MZ 项目；插件脚本或其他原生提取没有覆盖的文本，由熟悉格式的操作者整理成
JSONL，交给 Generic 项目。两个项目各自保存自己的数据库、译文、术语、Placeholder、
日志和输出，互不共享。公共配置中的 Profile 定义可以复用，但每个项目分别记录自己的
选择，模型任务记录也分别留在各自项目中。

把任意游戏格式整理成 Generic JSONL，以及把 Generic 输出放回游戏，由了解目标格式的
操作者或外部工具完成；ATT 专注于 JSONL 之上的翻译流程。

## 基本流程

每种项目都走同样清晰的四步：

1. `init` 建立或更新项目；
2. `extract` 读取当前输入，建立可翻译内容；
3. `translate` 生成并保存译文；
4. `write-back` 在项目工作区生成可检查的输出。

MV/MZ 的 Init 会保存游戏来源副本。Generic 始终绑定外部 JSONL 目录：修改 JSONL 后
重新执行 Extract，ATT 只清除受影响内容的旧状态。

Lua 是独立的数据库操作命令，不参与 Extract、Translate 或 WriteBack。翻译完成后，
可以用它精确修订译文，或按项目需要修改数据库。

## 运行环境

正式制品是 Windows x64 程序，最低支持 Windows 10 1903。`att.exe` 内嵌 UTF-8 active
code page 与 long-path-aware manifest。文件系统路径支持 Windows 能表示的 Unicode，
包括中文、Emoji、内部空格、长路径和 UNC 路径；访问 UNC 取决于运行账户的权限。

## 文档入口

想了解怎样选择引擎、各命令和文件格式遵守什么约定，从
[ATT 文档总入口](docs/README.md)开始。
