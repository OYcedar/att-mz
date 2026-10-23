# Unity / Naninovel 的显示消费者与漏翻返修

适用于 Unity 游戏（含使用 Naninovel 框架的项目）通过外部适配器接入 ATT Generic 的场景。

## 1. 定位多源显示消费者

同一 Unity 游戏常混合使用 Naninovel 脚本（`.nani`）、`TextAsset`、`MonoBehaviour` 序列化字段、UI Toolkit `VisualTreeAsset`、TextMeshPro（TMP）、图片纹理和托管程序集（`Assembly-CSharp.dll`）硬编码字符串：

- 遇到漏翻时，按界面对象反查数据来源、注册赋值路径和最终显示调用。
- UI Toolkit 界面需同时检查序列化属性与运行时代码中的 `Label.text` / 按钮赋值覆盖。
- 核对语言管理器与设置界面的语言标识，确认引擎语言与自定义 UI 使用相同的语言切换恢复逻辑。

## 2. 资源已译但运行时仍显示原文：排查托管缓存

- **ManagedText 绑定**：Naninovel 的 [Managed Text](https://naninovel.com/guide/managed-text#managed-text-variables) 通过带 `[ManagedText]` 属性的静态字符串字段或 `ManagedTextProvider` 组件更新文本。若资源已译而界面未变，检查目标类型是否已注册、资源路径是否被加载，以及语言切换后 UI 是否重新读取了该字段。
- **静态构造函数（`.cctor`）早期缓存**：在 Mono / IL2CPP 程序集中，若某个类的 `.cctor` 在启动时已将静态字段的原文复制到了静态数组或字典中，后续再由语言管理器更新静态字段将不会刷新已缓存的集合。需修改读取方法直接访问最新字段，或在字段更新后同步刷新缓存。

## 3. Naninovel 脚本、姓名与 Generic 回填

- **保留脚本与表达式结构**：保护命令名、参数名、内部标识、插值及富文本外壳，只提取可见参数文本。例如官方 [Name Labels](https://naninovel.com/guide/characters#name-labels) 中 `as` 参数的显示姓名可本地化，而角色 ID、玩家自定义姓名变量和存档记录键必须保持原值。
- **补提取与状态更新**：新增来源时，更新完整 JSONL 输入后沿现有 ATT 项目重新 Extract（Extract 会替换当前内容视图，输入必须包含全部既有来源与新增来源）。向已有 Group 插入新 Unit 会使该 Group 原有自动译文失去 Current，需补译或经 Manual 审校确认后再执行 WriteBack。

## 4. 有效资源实例与容器校验

- 同名资源可能同时存在于松散文件、`sharedassets` 和 Addressables bundle 中，需沿实际加载链路定位生效的资产实例。
- 修改 bundle 后，若目标构建的 Addressables catalog 校验文件大小或 CRC，需同步更新 catalog 元数据。TMP 字体缺字处理见 [Unity TMP 字体修复经验](engine-unity-tmp-font-fallback.md)。
