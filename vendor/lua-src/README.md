# ATT vendored Lua

本目录只包含 ATT 当前使用的 Lua 5.4.8，以及供 `mlua-sys` 调用的最小
Windows MSVC 构建接口。

来源：

- `lua-src` 550.1.1：<https://github.com/mlua-rs/lua-src-rs>
- Lua 5.4.8：<https://www.lua.org/ftp/lua-5.4.8.tar.gz>

ATT 的本地修改集中在 `att_win_utf8.c`、`att_win_utf8.h` 及其直接调用处：

- Lua 字符串中的文件路径、环境变量和命令按严格 UTF-8 解释；
- Windows 文件、环境和进程操作使用 UTF-16 API；
- 相对路径通过当前工作目录和
  [`PathAllocCombine`](https://learn.microsoft.com/windows/win32/api/pathcch/nf-pathcch-pathalloccombine)
  解析，并支持普通长路径、UNC 和扩展路径；
- 可执行文件目录通过动态 `GetModuleFileNameW` 缓冲区取得。
- Lua 文件操作的返回数量和 `errno` 不变，错误消息同时保留原始 Windows code；
- 跨越可能 longjmp 的 Lua API 时，外部分配由带 `__gc` 的内部 userdata 持有。

许可证见 [LICENSE](LICENSE)。
