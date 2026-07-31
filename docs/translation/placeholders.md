# Placeholder 文件现行规格

Placeholder 保护控制符、模板标记和其他不能由模型改写的片段。文件使用严格 TOML：

```toml
[[rule]]
pattern = '\\SE\[[^]]+\]'

[[rule]]
scopes = ["dialogue", "choice"]
pattern = '<msg>(?<text>.*?)</msg>'
```

仍交给语言判断、术语匹配和模型翻译的文本片段称为 NaturalText；被 Placeholder
替换成 ATT token 的片段称为不透明保护段。

每项只允许：

| 字段 | 要求 |
|---|---|
| `pattern` | 非空、可编译的 PCRE2 表达式 |
| `scopes` | 可省略的非空字符串数组；省略表示适用于全部 kind |

没有 `text` 命名捕获时，完整匹配是不透明保护段。存在 `text` 捕获时，捕获本身仍是可
翻译的 NaturalText，完整匹配中捕获前后的字节分别成为不透明 wrapper。一个规则最多有
一个命名捕获，并且只能命名为 `text`。规则按文件顺序执行；实际保护跨度重叠时会在
Translate 准备阶段明确失败。

## 引擎负责 scope

- MV/MZ 只接受 [RPG Maker 规则规格](../rpg-maker/rules.md#61-rpg-maker-scope)列出的
  kind，并额外保护该规格列出的引擎控制符；
- Generic 把 `scopes` 与 JSONL 的 `kind` 原值精确比较，没有内置控制符。

两者的规则不同：给 MZ 项目使用的 Placeholder 文件不能因为语法相同就直接当作
Generic 项目规则。

ATT 把匹配片段替换成临时 ATT token，再把模型结果中的 token 恢复为原片段。token 的
字符、大小写、编号、数量、顺序和允许位置必须保持；缺失、重复、改写或跨不允许边界移动
都会拒绝该 ID。

`translate --placeholders FILE` 在模型请求之前完整解析并原子替换当前项目规则。省略参数
时复用当前规则；`rule = []` 显式清空。解析失败时项目保持不变，也不发出请求。
