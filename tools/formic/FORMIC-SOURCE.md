# Formic 程序与源码来源

Formic 由 ATT 项目统一维护，源码位于 ATT 仓库的
[`crates/formic/`](https://github.com/yexi-by/att/tree/main/crates/formic)。发行包中的
`formic.exe` 与 `att.exe` 从同一版本标签构建，使用 workspace 的共同版本、锁定工具链和
根 `Cargo.lock`。运行时继续调用本目录的 `formic.exe`。

## 取得对应源码

运行 `formic.exe --version` 查看版本，在 [ATT 仓库](https://github.com/yexi-by/att)
检出对应的 `vMAJOR.MINOR.PATCH` 标签，即可取得当前发行程序的完整源码、测试、配置模板
与维护资料。后续开发和问题反馈均在 ATT 仓库进行。

Formic 迁入时采用原仓库的
[`45de311ab266ffa7cc606dffbb7463552a559ede`](https://github.com/yexi-by/formic/commit/45de311ab266ffa7cc606dffbb7463552a559ede)
作为源码基线；该引用只用于追溯迁入前的历史。

## 从 ATT workspace 构建

在 ATT 仓库根目录运行：

```powershell
$env:PCRE2_SYS_STATIC = '1'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --locked --release -p att -p formic
```

两个程序位于 `target/x86_64-pc-windows-msvc/release/`。发行时将 `att.exe` 放在发行根，
将 `formic.exe` 放在 `tools/formic/`；配置模板和用户文档也由 ATT 仓库中的本目录提供。
开发验证与规模实验见 [Formic 维护指南](https://github.com/yexi-by/att/blob/main/crates/formic/docs/maintenance.md)，
完整组包和发布流程见 [ATT 公开发行指南](https://github.com/yexi-by/att/blob/main/maintenance/releasing.md)。

日常使用方法见[快速开始](README.md)。Formic 使用 GNU AGPL v3，许可正文见 [LICENSE](LICENSE)。
