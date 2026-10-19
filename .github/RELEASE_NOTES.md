# ATT 1.3.7

Formic 源码已并入 ATT workspace，由 ATT 统一维护、构建和发行。两个程序从同一版本标签
构建，源码、测试和维护资料见 [Formic 开发入口](https://github.com/yexi-by/att/blob/v1.3.7/crates/formic/README.md)。

- 随包 `formic.exe` 使用共同版本号 `1.3.7`，新增 `--version`，便于确认程序版本和反馈问题。
- Formic 仍位于 `tools/formic/`，既有调用路径与活动配置保留；使用方法见
  [Formic 用户文档](https://github.com/yexi-by/att/blob/v1.3.7/tools/formic/README.md)。
- 原 Formic 仓库已归档，后续开发和问题反馈统一在 [ATT 仓库](https://github.com/yexi-by/att)进行。

升级时请保留 ATT 根目录的 `config.toml`、`tools/formic/config.toml` 和 `projects/`，
参照新版本配置模板核对字段。
