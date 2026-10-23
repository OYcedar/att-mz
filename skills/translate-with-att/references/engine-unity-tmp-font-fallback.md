# Unity TMP 字体修复与 MOD 交付经验

适用于 Unity 游戏已写入译文，但 TextMeshPro（TMP）文本显示中文方框（缺字），需要通过静态资产修补或运行时插件挂载中文字体的场景。

## 1. 从日志与组件定位缺字字体

- 查看游戏运行日志（`Player.log`）：TMP 缺字警告会直接输出缺失字符的 Unicode 码点、目标 `TMP_FontAsset` 名称和所在文本对象名称。
- 区分“字体缺少中文 glyph”与“文本本身仍是乱码/替换符 `U+FFFD`”。菜单、标题和特效字可能各自使用不同的 `TMP_FontAsset` 与材质预设。

## 2. 检查与配置 TMP 后备字体链

参照 Unity [TMP_FontAsset](https://docs.unity3d.com/Packages/com.unity.textmeshpro@3.0/api/TMPro.TMP_FontAsset.html) 与 [TMP_Settings](https://docs.unity3d.com/Packages/com.unity.textmeshpro@3.0/api/TMPro.TMP_Settings.html)：

- 优先检查目标字体的 `fallbackFontAssetTable`、全局 `TMP_Settings` 以及包内是否已内置中文字体资产；若已有内置中文字体，直接将其补充到目标字体的后备链中。
- **静态字体 vs 动态字体**：静态 TMP 字体需检查预烘焙的 `characterTable`、`glyphTable` 和图集纹理（atlas）；动态 TMP 字体则检查 `atlasPopulationMode`、引用的源字体文件（`sourceFontFile`）字符覆盖及动态图集尺寸配置。
- 检查 `matchMaterialPreset` 设置，关注后备字体在描边、阴影和行高上与主字体的视觉一致性。

## 3. 静态资产修补（UnityPy）

- **恢复类型树（TypeTree）**：当资源文件不含完整 MonoBehaviour 类型树时，结合当前构建提取类型信息：Mono 构建读取 `Managed` 目录程序集，Windows IL2CPP 构建使用 `GameAssembly.dll` 与 `global-metadata.dat`（参见 [UnityPy TypeTreeGenerator](https://github.com/K0lb3/UnityPy/blob/master/UnityPy/helpers/TypeTreeGenerator.py)）。
- **按头部筛选减少解析开销**：扫描大型 bundle 时，先用 `parse_monobehaviour_head()` 筛选目标组件，再做完整反序列化。
- **跨文件引用（`m_FileID`）**：`m_FileID` 是当前资产文件 `externals` 表的局部索引，跨文件引用字体资产时必须核对当前文件的外部引用表，不能直接照搬其他文件中的 `m_FileID` 数值。
- **回读校验**：保存后重新打开资产，验证修改后的后备链引用有效且未改动对象保持原样。

## 4. 运行时字体插件（BepInEx）

当静态资产不便直接替换或用户要求以插件形式交付时：

- 按游戏的 Unity 版本、Mono / IL2CPP 运行时和加载器版本编写插件，在 TMP 字体初始化或字符查找前将中文字体资产加入目标字体的 `fallbackFontAssetTable`（注意防重与防循环引用）。
- 若中文字体所在的 bundle 晚于首屏 UI 加载，采用按需挂载并在挂载成功后触发已显示文本的 `SetAllDirty()` / 重新生成网格，避免每帧全量扫描资源。
