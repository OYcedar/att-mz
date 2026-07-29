#ifndef att_win_utf8_h
#define att_win_utf8_h

#include <stdio.h>

#include "lua.h"

/*
** ATT 的 Lua 字符串路径、环境变量和命令都是 NUL 结尾的严格 UTF-8；
** 以下 C ABI 只用于同一静态库内的 Windows x64 MSVC 构建。
**
** module_dir 和 getenv 的非 NULL 输出由 malloc 分配。调用方必须先用
** push_allocation_owner 建立 Lua userdata，再把指针放进 owner；这样 Lua API
** longjmp 时由 __gc 释放。正常返回前用 release_allocation_owner 显式释放。
** FILE* 使用对应 CRT close；其他包装函数沿用 CRT 返回值并设置 errno。
**
** 转换、PathCch 或宽字符 CRT 失败时，take_error 单次取走线程本地的原始
** Windows code；Lua 的第三返回值仍保持上游规定的 errno。
*/
char *att_lua_win_module_dir_utf8(void);
FILE *att_lua_win_fopen(const char *filename, const char *mode);
FILE *att_lua_win_freopen(const char *filename, const char *mode, FILE *stream);
int att_lua_win_remove(const char *filename);
int att_lua_win_rename(const char *fromname, const char *toname);
int att_lua_win_getenv_utf8(const char *name, char **value);
FILE *att_lua_win_popen(const char *command, const char *mode);
int att_lua_win_system(const char *command);
int att_lua_win_take_error(unsigned long *code);
void **att_lua_win_push_allocation_owner(lua_State *L);
void att_lua_win_release_allocation_owner(lua_State *L, int index);

#endif
