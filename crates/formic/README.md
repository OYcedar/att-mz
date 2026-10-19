# Formic 开发入口

Formic 是 ATT workspace 中的独立任务执行程序。调用方提供分片计划和任务说明，Formic
负责每个单元的多轮模型与工具执行、取消、恢复、结果发布和运行证据。ATT 当前通过随包
`formic.exe` 支持术语初筛；单元规划与跨单元归并仍由调用方负责。

源码、测试和示例在本目录维护，版本、工具链、依赖锁定、构建和发行由 ATT 统一管理。
用户资料的唯一来源是 [`tools/formic/`](../../tools/formic/README.md)，按现有路径放入发行包。

## 开发与验证

以下命令从 ATT 仓库根目录执行，使用根 `rust-toolchain.toml` 和 `Cargo.lock`：

```powershell
cargo run --locked -p formic -- --help
cargo fmt --all --check
cargo check --locked -p formic --all-targets
cargo clippy --locked -p formic --all-targets -- -D warnings
cargo test --locked -p formic
```

涉及共享依赖、workspace 配置或发行链路时，同时验证全部受影响成员。
Formic 随 ATT 发布，两个程序使用同一版本号；构建和发包步骤见
[ATT 公开发行指南](../../maintenance/releasing.md)。

## 维护导航

- [项目地图](docs/project-map.md)：生产入口、模块职责、数据流与状态所有者。
- [维护指南](docs/maintenance.md)：按行为影响选择验证范围与维护方法。
- [用户文档](../../tools/formic/docs/README.md)：CLI、配置、任务设计、输出和恢复的当前契约。
- [源码来源](../../tools/formic/FORMIC-SOURCE.md)：源码位置、构建入口与迁入基线。

Formic 使用 [AGPL-3.0-only](../../LICENSE)。
