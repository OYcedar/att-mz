# Microsoft Visual C++ Runtime 发行声明

ATT 的 Windows x64 `att.exe` 把自身所需的 Visual C++ Runtime 可再发行代码静态链接进
程序。发行包中的独立工具 Formic v0.1.0 动态依赖 x64 `VCRUNTIME140.dll`，因此发行流程从
当前 Visual Studio 构建环境取得该文件，确认其 PE 架构为 x64 且 Authenticode 签名有效，
再放入 `tools/formic/`。`runtime.json` 记录实际随包文件的版本、SHA-256 和签名身份。

该 DLL 只支持随包 Formic，不改变 `att.exe` 的静态运行库要求。相关代码版权归 Microsoft
Corporation 所有。

相关可再发行代码受构建者接受的 Microsoft Visual Studio 2022 许可条款约束。
Microsoft 发布的现行可再发行代码清单见：
<https://aka.ms/vs/17/redist.txt>。

本声明不授予 Microsoft 许可条款之外的任何权利。
