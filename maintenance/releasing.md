# ATT 公开发行指南

发行内容和包内检查以[发行物规格](../docs/runtime/distribution.md)为准。

## 1. 准备版本

复用开发阶段已经完成的相关验证。ATT 与 Formic 使用根 `Cargo.toml` 中的
`workspace.package.version`，不单独发布 Formic。同步更新该版本、根 `Cargo.lock`、
`att.exe.manifest` 与 `.github/RELEASE_NOTES.md`；依赖变化时更新对应第三方许可证。
Formic 的源码与测试维护在 `crates/formic/`，随包配置模板和用户资料维护在 `tools/formic/`。
提交并推送本次发行内容。
发包不重复代码审查、普通测试、全量压力测试或本地 Release 构建。

依赖变化后，在 ATT 根目录重新生成 workspace 的统一第三方许可证：

```powershell
cargo about generate --workspace --locked --fail about.hbs -o licenses/THIRD-PARTY-LICENSES.html
```

## 2. 打标签并触发工作流

在已验证的提交上创建版本标签，推送后从 `main` 触发工作流：

```powershell
$metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
$version = ($metadata.packages | Where-Object name -EQ 'att').version
$tag = "v$version"
git tag -a $tag -m "ATT $version"
git push origin $tag
gh workflow run release.yml --ref main -f "tag=$tag"
```

使用触发命令返回的运行链接跟进对应工作流。工作流直接 checkout 指定标签，在 GitHub
托管的 Windows runner 上从同一标签构建 ATT 与 Formic，组装并检查发行包，然后生成 ZIP
和 SHA-256 校验文件。两个程序分别放在发行根和 `tools/formic/`，使用者的调用路径保持不变。
本机无需再构建一份相同程序，也无需预先下载或逐文件比较发行资源。

需要本地调查构建或组包问题时，在 ATT 根目录复用相同入口：

```powershell
$env:PCRE2_SYS_STATIC = '1'
$env:RUSTFLAGS = '-C target-feature=+crt-static'
cargo build --locked --release -p att -p formic
```

两个产物位于 `target/x86_64-pc-windows-msvc/release/`。资源同步脚本只维护资源，不复制或
替换程序；组包时将 `att.exe` 放到发行根、`formic.exe` 放到 `tools/formic/`，再执行包内检查。
更新已有安装时保留两个活动 `config.toml`，具体规则见发行物规格。

## 3. 完成与恢复

工作流成功后确认公开 Release 的版本与两个附件可用。ZIP 的 SHA-256 用于核验下载完整性，
不扩展为源码、文档、许可证或目录树的内容比较。

需要调查包内问题时，可直接检查已下载并解压的包：

```powershell
.\scripts\verify-release-package.ps1 -ExpectedVersion 1.3.2 -TargetRoot D:\att-package
```

构建或组包失败时处理具体错误并重试失败步骤。上传失败保留草稿及已上传的附件；公开失败时，
确认草稿附件齐全后可使用 `gh release edit TAG --draft=false --latest` 继续。
已公开的标签和 Release 保持不变；需要修改已发布内容时发布新版本。
