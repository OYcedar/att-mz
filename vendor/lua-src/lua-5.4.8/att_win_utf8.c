#define WIN32_LEAN_AND_MEAN
#define _CRT_SECURE_NO_WARNINGS

#include "att_win_utf8.h"

#include <errno.h>
#include <limits.h>
#include <pathcch.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <wchar.h>
#include <windows.h>

#ifndef EILSEQ
#define EILSEQ EINVAL
#endif

/*
** 宽字符 Win32/CRT 调用失败时，同时保存原始 Windows code。Lua 公开返回值
** 仍使用上游规定的 errno；调用处可以把这里的 code 加入错误消息。
*/
static __declspec(thread) DWORD att_lua_win_error_code;
static __declspec(thread) int att_lua_win_has_error_code;
static char att_lua_win_owner_metatable_key;


static void att_lua_win_clear_error(void) {
  att_lua_win_error_code = ERROR_SUCCESS;
  att_lua_win_has_error_code = 0;
  _set_doserrno(ERROR_SUCCESS);
}


static void att_lua_win_record_error(DWORD error) {
  if (error != ERROR_SUCCESS) {
    att_lua_win_error_code = error;
    att_lua_win_has_error_code = 1;
  }
}


static void att_lua_win_record_doserrno(void) {
  unsigned long error = ERROR_SUCCESS;
  if (_get_doserrno(&error) == 0)
    att_lua_win_record_error((DWORD)error);
}


static int att_lua_win_owner_gc(lua_State *L) {
  void **owner = (void **)lua_touserdata(L, 1);
  if (owner != NULL) {
    free(*owner);
    *owner = NULL;
  }
  return 0;
}


/*
** 只在不能交给宽字符 CRT 直接报告错误的转换和路径组合阶段映射 errno。
** 文件、进程和环境操作本身产生的 errno 原样返回给 Lua。
*/
static void att_lua_win_set_errno(DWORD error) {
  att_lua_win_record_error(error);
  switch (error) {
    case ERROR_FILE_NOT_FOUND:
    case ERROR_PATH_NOT_FOUND:
    case ERROR_INVALID_DRIVE:
    case ERROR_BAD_NETPATH:
    case ERROR_BAD_NET_NAME:
      errno = ENOENT;
      break;
    case ERROR_ACCESS_DENIED:
    case ERROR_NETWORK_ACCESS_DENIED:
    case ERROR_WRITE_PROTECT:
    case ERROR_SHARING_VIOLATION:
    case ERROR_LOCK_VIOLATION:
      errno = EACCES;
      break;
    case ERROR_FILE_EXISTS:
    case ERROR_ALREADY_EXISTS:
      errno = EEXIST;
      break;
    case ERROR_NOT_ENOUGH_MEMORY:
    case ERROR_OUTOFMEMORY:
      errno = ENOMEM;
      break;
    case ERROR_DISK_FULL:
    case ERROR_HANDLE_DISK_FULL:
      errno = ENOSPC;
      break;
    case ERROR_FILENAME_EXCED_RANGE:
    case ERROR_BUFFER_OVERFLOW:
      errno = ENAMETOOLONG;
      break;
    case ERROR_NO_UNICODE_TRANSLATION:
      errno = EILSEQ;
      break;
    case ERROR_INVALID_NAME:
    case ERROR_INVALID_PARAMETER:
      errno = EINVAL;
      break;
    default:
      errno = EIO;
      break;
  }
}


static void att_lua_win_set_hresult_errno(HRESULT result) {
  if (result == E_OUTOFMEMORY)
    errno = ENOMEM;
  else if (HRESULT_FACILITY(result) == FACILITY_WIN32)
    att_lua_win_set_errno(HRESULT_CODE(result));
  else
    errno = EINVAL;
}


/* 返回由 malloc 分配并以 NUL 结尾的 UTF-16 字符串。 */
static wchar_t *att_lua_win_utf8_to_wide(const char *value) {
  int count;
  wchar_t *wide;

  if (value == NULL) {
    errno = EINVAL;
    return NULL;
  }

  count = MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS,
                              value, -1, NULL, 0);
  if (count == 0) {
    att_lua_win_set_errno(GetLastError());
    return NULL;
  }
  if ((size_t)count > SIZE_MAX / sizeof(wchar_t)) {
    errno = ENOMEM;
    return NULL;
  }

  wide = (wchar_t *)malloc((size_t)count * sizeof(wchar_t));
  if (wide == NULL) {
    errno = ENOMEM;
    return NULL;
  }
  if (MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS,
                          value, -1, wide, count) == 0) {
    DWORD error = GetLastError();
    free(wide);
    att_lua_win_set_errno(error);
    return NULL;
  }
  return wide;
}


/* 返回由 malloc 分配并以 NUL 结尾的严格 UTF-8 字符串。 */
static char *att_lua_win_wide_to_utf8(const wchar_t *value) {
  int count;
  char *utf8;

  if (value == NULL) {
    errno = EINVAL;
    return NULL;
  }

  count = WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS,
                              value, -1, NULL, 0, NULL, NULL);
  if (count == 0) {
    att_lua_win_set_errno(GetLastError());
    return NULL;
  }

  utf8 = (char *)malloc((size_t)count);
  if (utf8 == NULL) {
    errno = ENOMEM;
    return NULL;
  }
  if (WideCharToMultiByte(CP_UTF8, WC_ERR_INVALID_CHARS,
                          value, -1, utf8, count, NULL, NULL) == 0) {
    DWORD error = GetLastError();
    free(utf8);
    att_lua_win_set_errno(error);
    return NULL;
  }
  return utf8;
}


static wchar_t *att_lua_win_current_directory(void) {
  DWORD capacity = GetCurrentDirectoryW(0, NULL);
  wchar_t *directory;

  if (capacity == 0) {
    att_lua_win_set_errno(GetLastError());
    return NULL;
  }

  for (;;) {
    DWORD length;
    if ((size_t)capacity > SIZE_MAX / sizeof(wchar_t)) {
      errno = ENOMEM;
      return NULL;
    }
    directory = (wchar_t *)malloc((size_t)capacity * sizeof(wchar_t));
    if (directory == NULL) {
      errno = ENOMEM;
      return NULL;
    }

    length = GetCurrentDirectoryW(capacity, directory);
    if (length == 0) {
      DWORD error = GetLastError();
      free(directory);
      att_lua_win_set_errno(error);
      return NULL;
    }
    if (length < capacity)
      return directory;

    free(directory);
    if (length == MAXDWORD) {
      errno = ENAMETOOLONG;
      return NULL;
    }
    capacity = length + 1;
  }
}


/*
** 把 Lua 的 UTF-8 路径变成宽字符绝对路径。PathAllocCombine 负责解析
** 相对路径和 "."、".."，并在需要时生成长路径形式；返回值由 LocalFree
** 释放。完全限定的盘符路径、UNC 和显式扩展路径由该 API 原样作为根处理。
*/
static wchar_t *att_lua_win_path(const char *filename) {
  const ULONG flags =
      PATHCCH_ALLOW_LONG_PATHS | PATHCCH_FORCE_ENABLE_LONG_NAME_PROCESS;
  wchar_t *input = att_lua_win_utf8_to_wide(filename);
  wchar_t *directory;
  wchar_t *combined = NULL;
  wchar_t *cursor;
  HRESULT result;

  if (input == NULL)
    return NULL;
  for (cursor = input; *cursor != L'\0'; ++cursor) {
    if (*cursor == L'/')
      *cursor = L'\\';
  }

  if (*input == L'\0') {
    combined = (wchar_t *)LocalAlloc(LMEM_FIXED, sizeof(wchar_t));
    if (combined == NULL) {
      free(input);
      errno = ENOMEM;
      return NULL;
    }
    *combined = L'\0';
    free(input);
    return combined;
  }

  directory = att_lua_win_current_directory();
  if (directory == NULL) {
    int saved_errno = errno;
    free(input);
    errno = saved_errno;
    return NULL;
  }

  result = PathAllocCombine(directory, input, flags, &combined);
  free(directory);
  free(input);
  if (FAILED(result)) {
    att_lua_win_set_hresult_errno(result);
    return NULL;
  }
  return combined;
}


char *att_lua_win_module_dir_utf8(void) {
  DWORD capacity = 256;
  wchar_t *module = NULL;
  wchar_t *separator;
  char *utf8;

  att_lua_win_clear_error();
  for (;;) {
    DWORD length;
    wchar_t *grown;

    if ((size_t)capacity > SIZE_MAX / sizeof(wchar_t)) {
      free(module);
      errno = ENOMEM;
      return NULL;
    }
    grown = (wchar_t *)realloc(module,
                               (size_t)capacity * sizeof(wchar_t));
    if (grown == NULL) {
      free(module);
      errno = ENOMEM;
      return NULL;
    }
    module = grown;

    SetLastError(ERROR_SUCCESS);
    length = GetModuleFileNameW(NULL, module, capacity);
    if (length == 0) {
      DWORD error = GetLastError();
      free(module);
      att_lua_win_set_errno(error);
      return NULL;
    }
    if (length < capacity)
      break;
    if (capacity > MAXDWORD / 2) {
      free(module);
      errno = ENAMETOOLONG;
      return NULL;
    }
    capacity *= 2;
  }

  separator = wcsrchr(module, L'\\');
  if (separator == NULL)
    separator = wcsrchr(module, L'/');
  if (separator == NULL) {
    free(module);
    errno = EINVAL;
    return NULL;
  }
  *separator = L'\0';
  utf8 = att_lua_win_wide_to_utf8(module);
  free(module);
  return utf8;
}


FILE *att_lua_win_fopen(const char *filename, const char *mode) {
  wchar_t *path;
  wchar_t *wide_mode;
  FILE *file;
  int saved_errno;

  att_lua_win_clear_error();
  path = att_lua_win_path(filename);
  if (path == NULL)
    return NULL;
  wide_mode = att_lua_win_utf8_to_wide(mode);
  if (wide_mode == NULL) {
    saved_errno = errno;
    LocalFree(path);
    errno = saved_errno;
    return NULL;
  }

  file = _wfopen(path, wide_mode);
  saved_errno = errno;
  if (file == NULL)
    att_lua_win_record_doserrno();
  free(wide_mode);
  LocalFree(path);
  errno = saved_errno;
  return file;
}


FILE *att_lua_win_freopen(const char *filename,
                          const char *mode,
                          FILE *stream) {
  wchar_t *path;
  wchar_t *wide_mode;
  FILE *file;
  int saved_errno;

  att_lua_win_clear_error();
  path = att_lua_win_path(filename);
  if (path == NULL)
    return NULL;
  wide_mode = att_lua_win_utf8_to_wide(mode);
  if (wide_mode == NULL) {
    saved_errno = errno;
    LocalFree(path);
    errno = saved_errno;
    return NULL;
  }

  file = _wfreopen(path, wide_mode, stream);
  saved_errno = errno;
  if (file == NULL)
    att_lua_win_record_doserrno();
  free(wide_mode);
  LocalFree(path);
  errno = saved_errno;
  return file;
}


int att_lua_win_remove(const char *filename) {
  wchar_t *path;
  int result;
  int saved_errno;

  att_lua_win_clear_error();
  path = att_lua_win_path(filename);
  if (path == NULL)
    return -1;
  result = _wremove(path);
  saved_errno = errno;
  if (result != 0)
    att_lua_win_record_doserrno();
  LocalFree(path);
  errno = saved_errno;
  return result;
}


int att_lua_win_rename(const char *fromname, const char *toname) {
  wchar_t *from;
  wchar_t *to;
  int result;
  int saved_errno;

  att_lua_win_clear_error();
  from = att_lua_win_path(fromname);
  if (from == NULL)
    return -1;
  to = att_lua_win_path(toname);
  if (to == NULL) {
    saved_errno = errno;
    LocalFree(from);
    errno = saved_errno;
    return -1;
  }

  result = _wrename(from, to);
  saved_errno = errno;
  if (result != 0)
    att_lua_win_record_doserrno();
  LocalFree(to);
  LocalFree(from);
  errno = saved_errno;
  return result;
}


int att_lua_win_getenv_utf8(const char *name, char **value) {
  wchar_t *wide_name;
  const wchar_t *wide_value;
  char *utf8_value = NULL;
  int saved_errno;

  att_lua_win_clear_error();
  if (value == NULL) {
    errno = EINVAL;
    return -1;
  }
  *value = NULL;
  wide_name = att_lua_win_utf8_to_wide(name);
  if (wide_name == NULL)
    return -1;

  wide_value = _wgetenv(wide_name);
  if (wide_value != NULL) {
    utf8_value = att_lua_win_wide_to_utf8(wide_value);
    if (utf8_value == NULL) {
      saved_errno = errno;
      free(wide_name);
      errno = saved_errno;
      return -1;
    }
  }
  free(wide_name);
  *value = utf8_value;
  return 0;
}


FILE *att_lua_win_popen(const char *command, const char *mode) {
  wchar_t *wide_command;
  wchar_t *wide_mode;
  FILE *file;
  int saved_errno;

  att_lua_win_clear_error();
  wide_command = att_lua_win_utf8_to_wide(command);
  if (wide_command == NULL)
    return NULL;
  wide_mode = att_lua_win_utf8_to_wide(mode);
  if (wide_mode == NULL) {
    saved_errno = errno;
    free(wide_command);
    errno = saved_errno;
    return NULL;
  }

  file = _wpopen(wide_command, wide_mode);
  saved_errno = errno;
  if (file == NULL)
    att_lua_win_record_doserrno();
  free(wide_mode);
  free(wide_command);
  errno = saved_errno;
  return file;
}


int att_lua_win_system(const char *command) {
  wchar_t *wide_command;
  int result;
  int saved_errno;

  att_lua_win_clear_error();
  if (command == NULL)
    return _wsystem(NULL);
  wide_command = att_lua_win_utf8_to_wide(command);
  if (wide_command == NULL)
    return -1;

  result = _wsystem(wide_command);
  saved_errno = errno;
  if (result == -1)
    att_lua_win_record_doserrno();
  free(wide_command);
  errno = saved_errno;
  return result;
}


int att_lua_win_take_error(unsigned long *code) {
  if (code == NULL) {
    errno = EINVAL;
    return 0;
  }
  if (!att_lua_win_has_error_code)
    return 0;
  *code = (unsigned long)att_lua_win_error_code;
  att_lua_win_error_code = ERROR_SUCCESS;
  att_lua_win_has_error_code = 0;
  return 1;
}


/*
** Lua API 发生内存错误时会 longjmp。外部分配在传给 lua_pushstring 或
** luaL_gsub 前先挂到该 userdata；成功后显式释放，异常时由 __gc 释放。
*/
void **att_lua_win_push_allocation_owner(lua_State *L) {
  void **owner = (void **)lua_newuserdatauv(L, sizeof(void *), 0);
  *owner = NULL;
  lua_rawgetp(L, LUA_REGISTRYINDEX, &att_lua_win_owner_metatable_key);
  if (lua_isnil(L, -1)) {
    lua_pop(L, 1);
    lua_createtable(L, 0, 1);
    lua_pushcfunction(L, att_lua_win_owner_gc);
    lua_setfield(L, -2, "__gc");
    lua_pushvalue(L, -1);
    lua_rawsetp(L, LUA_REGISTRYINDEX, &att_lua_win_owner_metatable_key);
  }
  lua_setmetatable(L, -2);
  return owner;
}


void att_lua_win_release_allocation_owner(lua_State *L, int index) {
  int saved_errno = errno;
  void **owner = (void **)lua_touserdata(L, index);
  if (owner != NULL) {
    free(*owner);
    *owner = NULL;
  }
  lua_remove(L, index);
  errno = saved_errno;
}
