# ATT 运行时导航

## 命令需要哪些发行资源和配置

| 命令 | 固定资源与读取的配置 |
|---|---|
| Help、Version | 无 |
| Init、Extract、MV/MZ WriteBack、Lua | `<att-dir>/config.toml` 与固定的 `<att-dir>/projects/`；没有额外配置字段 |
| Translate | 固定的 `projects/`、`prompts/`，以及 `[prompts]`、`[translation]`、全部 `[[languages]]`、所选 Profile 和 Client |
| Generic WriteBack | 固定的 `projects/`；存在自动译文时还读取固定 `prompts/` 和上述翻译配置 |

所有业务命令都读取 `att.exe` 同目录下唯一的 `config.toml`。ATT 只解析当前命令真正用到
的配置值；没被选中的 Client，其凭据始终留在配置文件里。

## 项目保存什么选择

- Init 保存项目来源和语言；
- MV/MZ Extract 保存 Builtin/Rules 选择；
- Translate 保存最近成功使用的 Profile、当前术语和 Placeholder；
- Generic 保存外部 JSONL 根与最近成功 Extract 的输入指纹；
- Lua 每次读取本次显式脚本，不保存脚本或运行方案；
- WriteBack 没有可保存的运行选项。

## 哪些值不能配置

线程、worker、内部窗口、批次、SQLite 策略、日志缓冲、锁路径、发布目录和项目规模都由
负责执行的代码决定，配置因此保持精简。文件、目录、Group、Unit、Task、Lua 和 SQL
结果的总量都不设上限。

## 继续阅读

- [CLI](cli.md)
- [配置](configuration.md)
- [发行物](distribution.md)
- [Chat Completions](chat-completions.md)
- [SQLite](sqlite.md)
- [项目日志](project-log.md)
- [目录发布](directory-publishing.md)
