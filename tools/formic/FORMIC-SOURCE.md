# Formic 来源

本目录的 `formic.exe` 是从当前 Formic 源码静态构建的 Windows x64 `Release`，不是
上游 v0.2.0 的未修改公开发行文件。

- 上游基线：[Formic v0.2.0](https://github.com/yexi-by/formic/releases/tag/v0.2.0)，提交
  `fc8056a30776ebf05fc69276934cbce0e196d6cd`；
- 当前源码提交：`f4340137ac0a1a12e4e4d9b91c56d18a62f3373d`；
- 构建方式：Rust MSVC 目标，`Release`，静态 C Runtime；
- `formic.exe` SHA-256：`2d3f6094e7752772b9d101e465f3307db2be8ce78a2e50048cbf1cd5382d6945`。

本轮没有创建公开 tag 或 Release。公开分发此修改版前，必须同时向接收者提供上述当前提交
对应的完整源码；不能把上游 v0.2.0 源码误称为本二进制的对应源码。

Formic 使用 GNU AGPL v3，许可正文见同目录 `LICENSE`。
