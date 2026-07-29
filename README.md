# ATT

ATT 是聚合多种游戏引擎汉化能力的命令行产品。当前已经实现的游戏领域是 RPG Maker，
支持 MV 与 MZ；两种引擎拥有各自的命令域、目录布局和项目身份，并按语义复用共享能力。

ATT 以原游戏之外的项目工作区保存冻结来源、项目事实、提取资产、译文状态和运行诊断，
再从冻结来源生成可检查、可重复建立的候选内容树。原游戏不会成为写回目标，候选内容树
也不等同于完整游戏包。

## 运行环境与路径

ATT 0.1.0 的正式制品是 Windows x64 程序，最低支持 Windows 10 1903。正式 `att.exe`
内嵌 [UTF-8 active code page manifest](https://learn.microsoft.com/windows/apps/design/globalizing/use-utf8-code-page)，
并在启动时确认当前进程 code page 是 65001；
系统版本不支持该 manifest，或者制品没有正确嵌入它时，ATT 会在读取配置和项目之前
明确失败。复制正式 `att.exe` 到其他目录不会丢失内嵌 manifest。

ATT 的文件系统路径支持 Windows 能表示的 Unicode，包括中文、Emoji 和内部空格。
这一保证覆盖 `att.exe` 所在目录、进程 cwd、配置、游戏来源、项目与 Prompt 根、SQLite
数据库、Lua 主程序和纯 Lua 模块。包含未配对 UTF-16 surrogate 的 Windows 名称不是
UTF-8，不能作为 CLI 文本参数或 Lua string 传递；这与普通中文、Emoji 路径不同。

正式制品声明 long-path-aware，Lua 文件边界也强制使用 Windows 长路径组合，因此作者
不需要启用 `LongPathsEnabled`。UNC 路径仍要求运行 ATT 的账户具有共享目录权限，并且
系统策略允许从该位置执行程序；ATT 不建立网络凭据，也不绕过程序执行策略。默认进程测试
直接覆盖本机 Unicode 长路径。维护者可把 `ATT_TEST_UNC_ROOT` 设为一个可写 UNC 目录以
运行 UNC 测试；发行验收设置 `ATT_RELEASE_ACCEPTANCE=1` 后，缺少该 UNC 根会使测试失败，
不能静默跳过。测试专用的 `ATT_TEST_EXECUTABLE` 可以指向已构建 Release 制品的绝对路径，
让同一进程矩阵直接验收该制品；它不是 ATT 产品配置。

## 知识入口

- **查询当前产品事实**：从[ATT 文档总入口](docs/README.md)进入现行规格、调查指南与
  工件验证方法。
