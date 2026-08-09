# Formic 来源

本目录的 `formic.exe` 是从当前 Formic 源码静态构建的 Windows x64 `Release`，不是
上游 v0.2.0 的未修改公开发行文件。

- 上游基线：[Formic v0.2.0](https://github.com/yexi-by/formic/releases/tag/v0.2.0)，提交
  `fc8056a30776ebf05fc69276934cbce0e196d6cd`；
- 当前源码提交：`0c09a4e5848678f0b924928f794b578629800703`；
- 构建方式：Rust MSVC 目标，`Release`，静态 C Runtime；
- `formic.exe` SHA-256：`5e8cbd437b5dd9d98c9f3c3550e2e8352bb1ea3ead79184e38e3adbe90138b6f`。

本轮没有创建公开 tag 或 Release。公开分发此修改版前，必须同时向接收者提供上述当前提交
对应的完整源码；不能把上游 v0.2.0 源码误称为本二进制的对应源码。

Formic 使用 GNU AGPL v3，许可正文见同目录 `LICENSE`。
