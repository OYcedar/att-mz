# Formic 程序与源码来源

本目录提供 Formic 0.3.0 的 Windows x64 本地修订制品，运行时直接调用同目录的 `formic.exe`。

| 项目 | 当前制品 |
| --- | --- |
| 源码仓库 | [yexi-by/formic](https://github.com/yexi-by/formic) |
| 源码提交 | `f54d0fb308dbda5611b7780d6e4474bfba67eb56` |
| 来源状态 | 已在维护机的独立源码工程 `D:\Formic` 提交，尚未推送远端 |
| 编译器 | Rust 1.97.1 |
| 构建目标 | `x86_64-pc-windows-msvc`，Release，静态 C Runtime |
| `formic.exe` SHA-256 | `b25bd10097b404cb03bbce30d74d094cf65b5bd30782da2b1c882fd64dcc4825` |

当前提交可在上述本地源码工程核验，远端仓库尚不能取得这次修订。公开分发本制品前，应先推送该提交并确认对应源码可获取。

## 从对应源码构建

源码构建和规模实验在独立的 Formic 源码工程中执行。取得上述提交后，在其根目录运行：

```powershell
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo +1.97.1 build --locked --release --target x86_64-pc-windows-msvc
```

产物位于该工程的 `target/x86_64-pc-windows-msvc/release/formic.exe`。ATT 随包目录提供预构建程序与用户文档，日常使用方法见[快速开始](README.md)。

Formic 使用 GNU AGPL v3，许可正文见 [LICENSE](LICENSE)。
