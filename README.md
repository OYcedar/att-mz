<div align="center">

# 🎮 ATT

**面向 Agent 的游戏翻译命令行工具**

繁重的工作交给 ATT，重要的判断交给 Agent。

![平台](https://img.shields.io/badge/平台-Windows%20x64-0078D6)
![支持游戏](https://img.shields.io/badge/支持-RPG%20Maker%20MV%20%2F%20MZ-green)
![界面语言](https://img.shields.io/badge/界面语言-10%20种-orange)
![许可](https://img.shields.io/badge/许可-AGPL--3.0--only-blue)

[快速开始](#-快速开始) ·
[支持什么](#-支持什么) ·
[遇到问题](#-遇到问题) ·
[完整文档](docs/README.md)

</div>

---

翻译一个游戏，既有大量重复、繁重的工作，也有需要理解和判断的工作。
ATT 负责前者，Agent 负责后者——你把任务交给 Agent，它驱动 ATT 完成整个翻译。

## 🤝 各自负责什么

| ⚙️ ATT 自动完成 | 🧠 Agent 完成 |
| --- | --- |
| 按已选择的 Builtin、Rules 或 JSONL 输入提取文本 | 调查游戏结构，确定完整翻译范围 |
| 把文本按场景分组、去掉重复、打包成翻译任务 | 制作术语表，统一角色名、地名等写法 |
| 批量调用翻译模型，自动处理超时和重试 | 检查译文质量，逐条修正不满意的地方 |
| 记住每一段的翻译进度，中断后可以继续 | 遇到异常时判断原因，决定下一步 |
| 把译文安全地写回游戏文件 | 对照游戏实际运行效果做最终验收 |

## ✨ 支持什么

- **🕹️ RPG Maker MV / MZ**：直接支持标准数据库、地图事件对话和系统文本；插件参数、
  `note` 等非标准位置需要明确的 Extract Rules（更早的 XP、VX、VX Ace 不支持）。
- **🧩 其他游戏**：把游戏文本整理成 ATT 规定的文本清单格式（JSONL）即可翻译；
  清单与原游戏格式之间的提取和写回由你或外部工具负责。
- **🌐 语言**：自动判断哪些日语、英语文本需要翻译，并按配置检查不允许的源语残留；
  目标语言不限。
- **🔌 翻译模型**：任何兼容 OpenAI Chat Completions 接口的服务都可以，
  填上接口地址、API Key 和模型名即可使用。

## 📦 使用前准备

- 一台 Windows x64 电脑；
- 一个可用的翻译模型服务（见上文）；
- 推荐使用一个能执行命令、读写文件的 Agent；也可以手动运行 ATT。

## 🚀 快速开始

### 第 1 步：下载并解压

从 [GitHub Releases](https://github.com/yexi-by/att/releases/latest) 下载 Windows x64
压缩包和 `SHA256SUMS.txt`，核对 SHA-256 后完整解压到一个可写目录。不要只单独复制
`att.exe`。

### 第 2 步：填写模型配置

ATT 是一个独立目录，里面有 `att.exe`、`config.toml`、`docs/`、`skills/` 等内容。
打开 `att.exe` 旁边的 `config.toml`，找到 `[llm.clients.primary]`，填好三行：

```toml
[llm.clients.primary]
url = "https://api.example.com/v1/chat/completions"   # 模型接口地址
api_key = "replace-with-api-key"                       # API Key
model = "replace-with-model-id"                        # 模型名
```

### 第 3 步：把任务交给 Agent（推荐）

把下面这段话发给你的 Agent，替换掉尖括号里的内容：

```text
使用 <ATT目录>/att.exe 对 <游戏目录> 进行翻译，严格遵循 <ATT目录>/skills 下的工作流。

最终目标：将目标游戏中非图片性质、正常游玩下可见的 <源语言> 文本全部翻译为 <目标语言>。

要求：
- 克制使用 generic 功能；只有确认 <游戏引擎> 的原生功能确实无法完整覆盖某部分内容时，
  才允许对该部分使用 generic；
- 遇到问题时多查阅 <ATT目录>/docs 和 <ATT目录>/skills；
- 如果 docs 和 skills 确实无法解决问题，立即停止操作并解释原因。
```

Agent 会自己读 `skills/` 里的工作流和 `docs/` 里的说明，完成调查、翻译、检查和输出，
你只需要在关键节点确认结果。

### 第 4 步：也可以手动使用

在 PowerShell 中进入 ATT 解压目录。每个游戏翻译项目分四步（以 RPG Maker MV 为例）：

```text
init ──▶ extract ──▶ translate ──▶ write-back
建立项目    提取文本     调用模型翻译     生成译后文件
```

```text
.\att.exe mv init --name mygame --path "D:\Games\MyGame" --source-language ja --target-language zh-Hans --dialogue-max-fullwidth-chars 40 --scrolling-text-max-fullwidth-chars 40 --help-description-max-fullwidth-chars 34
.\att.exe mv extract --name mygame --builtin
.\att.exe mv translate --name mygame primary
.\att.exe mv write-back --name mygame
```

1. `init`：建立项目，并保存一份游戏副本，之后的操作都不改动原游戏；
2. `extract`：从游戏里提取需要翻译的文本；
3. `translate`：调用模型翻译，按 Ctrl-C 受控取消后可重新执行同一条命令继续；
4. `write-back`：在项目文件夹里生成翻译后的游戏文件，供你检查。

少量未完成译文优先使用 Manual TOML：

```text
.\att.exe mv manual export --name mygame manual.toml
.\att.exe mv manual check --name mygame manual.toml
.\att.exe mv manual apply --name mygame manual.toml
```

需要批量读取上下文、复杂筛选或程序化修改时，再使用 Lua。Lua 同时提供便利的翻译 API 和
可以直接执行任意数据库修改的原始 SQLite API。

## ❓ 遇到问题

`docs/` 目录里有完整说明，从[文档总入口](docs/README.md)开始，按你当前的情况选择：

| 你的情况 | 看什么 |
| --- | --- |
| 第一次翻译游戏 | [翻译项目指南](docs/guides/translation-project.md) |
| 命令失败、结果不完整或状态不明 | [诊断与恢复指南](docs/guides/diagnosis-and-recovery.md) |
| 检查整个游戏是否翻译完整 | [全量验收指南](docs/guides/acceptance.md) |
| 查命令参数和配置写法 | [CLI 规格](docs/runtime/cli.md) · [配置规格](docs/runtime/configuration.md) |

## 📄 许可

Copyright (C) 2026 yexi-by。

ATT 1.0 及后续当前版本以 [GNU Affero General Public License v3.0 only](LICENSE) 发布，
SPDX 标识为 `AGPL-3.0-only`。Lua、PCRE2、Rust crates 与构建运行库等第三方组件保留
各自许可，声明见 [licenses/](licenses/) 目录。
