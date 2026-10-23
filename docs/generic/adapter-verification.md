# Generic 外部适配器验收

Generic WriteBack 生成译后 JSONL，外部适配器负责把这些文字写回游戏原格式。本页定义随包 `AdapterVerification` 工具接受的结构化证据契约。

## 1. 验证对象与方法

先用少量真实替换文本验证提取、写回和游戏消费者读取（包含姓名控制、引号、注音、选项、原生窗口和固定字节缓冲区等不同路径）。

完整封包后重新读取交付目录中的实际资源：

- 逐 Unit 对比原格式读出的源文与当前 Extract 原文；
- 逐 Unit 对比最终原格式读出的正文与实际 WriteBack（含标点与换行处理）；
- 在原生语义边界核对控制段、内部键、显示映射、跳转和资源查找；
- 若能通过实际绘字算法、字体和可用区域计算尺寸，可记录完整文字宽高。

读回验证在文本归一化之前执行（例如确认姓名是否处于引号外、配置行是否满足原生加载格式、封包索引是否符合二分查找排序规则）。

## 2. 生成外部报告

随包 `skills/translate-with-att/scripts/att_toolbox/adapter_verification.py` 提供 `AdapterVerification`。导入前将随包 `scripts` 与 `skills/_shared` 加入 Python 模块路径。

构造参数：

| 参数 | 内容 |
| --- | --- |
| `translations` | 当前完整 `translation export` |
| `generic_input`、`write_back` | 当前输入与 ATT 实际写回目录 |
| `source_root`、`output_root` | 互不重叠的原版根和最终游戏根 |
| `source_files`、`output_files` | 实际读取文件的相对路径（含封包、EXE、DLL、配置与字体） |
| `verifier_files` | 独立读取器、适配器和检查程序的文件路径 |

构造时固定文件摘要基线，执行读取与检查后调用 `write(report_path)` 输出报告：

- `observe_unit(manual_id, *, source_file, source_location, source_text, output_file, output_location, output_text, display_text=None)`：记录从实际资源读出的源文、输出正文及可选的最终显示文字。每个 Unit 对应一条观察。
- `check(name, *, kind, method, manual_ids, status, details="", source_files=(), output_files=())`：记录适配器检查（`kind` 为 `consumer` 或 `changed_text_roundtrip`；`method` 为 `native_static` 或 `runtime`；`status` 为 `passed`、`failed` 或 `unverified`）。`changed_text_roundtrip` 通过时，其覆盖范围内至少有一条正文实际发生改变。
- `measure(manual_id, *, text, width, height, available_width, available_height, method, details="")`：记录像素级排版尺寸（`text` 与 `display_text` 或 `output_text` 一致，可用尺寸大于零），由 QA 判断是否溢出。

## 3. 报告格式与失效

生成的 JSON 对象包含：

```text
translations, generic_input, write_back,
source_root, output_root, source_files, output_files, verifier_files,
units, checks, measurements
```

任何已绑定的输入、输出、字体、配置或验证程序发生变化，报告即失效，需重新生成。

## 4. 汇入 QA 与返修

```powershell
python skills/translate-with-att/scripts/translation_qa.py scan `
  --translations task/translations.jsonl --generic-input task/input `
  --write-back projects/generic/项目名/write_back `
  --adapter-report task/adapter-verification.json --output task/qa
```

`--adapter-report` 用于独立 Generic 项目（需同时提供 `--write-back`）。QA 核对文件绑定并汇总源文映射、最终正文、消费者及布局的覆盖情况：`external_source_mismatch`、`external_output_mismatch` 和 `external_consumer_failure` 指向适配器返修；`layout_overflow` 进入 Review。
