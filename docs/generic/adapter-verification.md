# Generic 外部适配器验收

Generic WriteBack 生成译后 JSONL。外部适配器负责把这些文字放回原格式，并证明最终游戏
仍按预期读取和显示。本页定义随包验收工具接受的证据；Generic JSONL 本身不增加引擎字段。

## 1. 验证对象与方法

先确认原格式的真实消费者，再用少量真实替换文本验证提取、写回和消费者读取。样本覆盖
本次涉及的不同路径，例如姓名控制、引号、注音、选项、原生窗口和固定字节缓冲区。
只做未修改数据的往返，会漏掉仅在替换分支出现的错误。

完整封包后重新读取交付目录中的实际资源：

- 逐 Unit 对比原格式读出的源文与当前 Extract 原文；
- 逐 Unit 对比最终原格式读出的正文与实际 WriteBack，包含 WriteBack 的标点、换行处理；
- 在原生语义边界核对控制段、内部键、显示映射、跳转和资源查找；
- 对需要排版的文字，使用实际绘字算法、字体和可用区域测量完整文字宽高。

读回不能先删除需要验证的语法差异。姓名处在引号内外、配置行是否满足原生加载条件、
文件是否能按原生比较规则被索引命中，都必须在文本归一化之前验证。优先使用引擎原生
读取路径、独立的已确认实现，或依据真实原生调用编写的检查；提取器和回填器共享的假设
不能作为独立通过证据。

启动兼容性在大批量翻译前调查：区域与代码页检查、资源排序、必要运行库、字体解码及
动态原生控件。基本启动观察只覆盖实际观察到的阶段。新游戏入口、对白、设置、选项和
存读档逐场景验收；游戏启动和界面操作遵循本任务已有授权，未观察部分交给人工。

## 2. 生成外部报告

随包 `skills/translate-with-att/scripts/att_toolbox/adapter_verification.py` 提供
`AdapterVerification`。已有适配器在导入该模块前，把随包 `scripts` 与 `skills/_shared`
加入 Python 模块路径。该工具不实现特定引擎解析器，不启动游戏或请求模型。

构造时传入：

| 参数 | 内容 |
| --- | --- |
| `translations` | 当前完整 `translation export` |
| `generic_input`、`write_back` | 当前输入与 ATT 实际写回目录 |
| `source_root`、`output_root` | 互不重叠的原版根和最终游戏根 |
| `source_files`、`output_files` | 实际读取文件的相对路径，包含影响结果的封包、EXE、DLL、配置与字体 |
| `verifier_files` | 独立读取器、适配器和检查程序的文件路径 |

构造会固定文件内容。随后执行读取与检查，调用以下方法，最后 `write(report_path)`。
写报告时再次核对基线；检查期间文件变动会使报告生成失败。报告应放在任务资料目录。

`observe_unit(manual_id, ...)` 保存从实际资源读出的：

- `source_file`、`source_location`、`source_text`；
- `output_file`、`output_location`、`output_text`；
- 可选 `display_text`：姓名映射等解析后的实际显示文字，用于布局测量绑定。

位置使用文件名、记录号、字段名等可读描述。正文来自读取器，不能直接复制预期译文充当
观察。一个 Unit 对应一条观察；未知或重复自然 ID 无效。缺少观察的位置保留为未验证。

`check(name, ...)` 保存适配器检查：

| 字段 | 含义 |
| --- | --- |
| `kind` | `consumer` 或 `changed_text_roundtrip` |
| `method` | `native_static`（原生语义静态检查）或 `runtime`（实际运行观察） |
| `manual_ids` | 本项确实覆盖的已观察 Unit |
| `status` | `passed`、`failed` 或 `unverified` |
| `details` | 检查的条件、取得的事实及仍存在的边界 |
| `source_files`、`output_files` | 已进入基线的实际证据文件 |

检查名称在报告内唯一。`changed_text_roundtrip` 通过时，其范围内至少有一条正文实际发生
改变。一个 Unit 的任一消费者检查失败或未验证，其他通过项不能将它覆盖为消费者已验证。
检查的原生正确性由适配器负责，公共工具只校验报告契约、文件绑定和覆盖关系。

`measure(manual_id, ...)` 接受 `text`、`width`、`height`、`available_width`、
`available_height`、`method` 和 `details`。尺寸以像素计，来自完整排版结果；`text` 必须与
该 Unit 观察的 `display_text` 一致，未提供显示投影时与 `output_text` 一致。可用尺寸必须
大于零。QA 根据尺寸计算溢出，不接受预先填写的“布局通过”布尔值。

## 3. 报告格式与失效

工具生成一个 JSON object，固定字段为：

```text
translations, generic_input, write_back,
source_root, output_root, source_files, output_files, verifier_files,
units, checks, measurements
```

`translations` 绑定导出文件；`generic_input` 和 `write_back` 绑定目录内全部 JSONL。
资源与验证程序保存路径及内容摘要。摘要只用于核对证据对应的字节，不作为用户操作凭据。
`units`、`checks`、`measurements` 对应上述观察、检查与测量；未知字段和无效字段被拒绝。
正文不做 Unicode、控制符或换行归一化后再比较。

任何已绑定输入、输出、字体、配置或验证程序发生变化，报告都会失效。增删 JSONL 同样
使绑定失效。适配器必须把可能影响检查结果的实际文件全部列入，未列入的资源不在证明范围。

## 4. 汇入 QA 与返修

```powershell
python skills/translate-with-att/scripts/translation_qa.py scan `
  --translations task/translations.jsonl --generic-input task/input `
  --write-back projects/generic/项目名/write_back `
  --adapter-report task/adapter-verification.json --output task/qa
```

`--adapter-report` 只用于独立 Generic，并要求同时提供当前 WriteBack。QA 重新核对全部
绑定，汇总源文映射、最终正文、消费者及布局的实际覆盖数量。只有覆盖完整且相应检查通过，
才移除对应未验证项；静态通过仍保留没有实际运行观察的范围。报告未覆盖的布局不采用固定
字符数阈值代替测量。

`external_source_mismatch`、`external_output_mismatch` 和 `external_consumer_failure`
指向适配器返修；不会自动加入 Manual 改译清单。布局溢出属于 Review，结合原生布局和译文
判断修复位置。选择 QA Review 导出 Manual ID 前会再次核对外部证据，拒绝使用已经失效的报告。

报告始终是旁路证据，不改变 ATT Current、Rejected、Translate、WriteBack 或发布恢复状态，
不自动重新请求模型，也不授权启动游戏。交付同时说明静态结果、实际观察场景与未验证项。
