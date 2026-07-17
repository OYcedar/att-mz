# MZ 项目初始化现行规格

本文记录已经确认并落地的 MZ Init 行为。Init 把一个现存游戏导入为命名工作区，
只有数据库、冻结源目录和固定写回目录作为一个整体发布后，初始化才成功。
文件系统和 SQLite 的生产根适配器不属于当前实现。

## 1. 一次调用与成功结果

一次 Init 提交以下事实：

- 已校验的项目名；
- 原 MZ 游戏根目录；
- 源语言和目标语言；
- 对话正文、滚动文本和帮助说明三个区域的正整数行宽。

`InitService` 先去除两个语言的首尾空白并拒绝空值，再通过
`ExistingDirectoryResolver` 确认游戏根目录。它把解析后的目录和受信项目事实
交给 `ProjectWorkspaceCreationService`；任何一步失败都不返回
`InitOutput`。

成功意味着以下结构已经作为一个可用工作区对外可见：

```text
<projects_root>/<name>/
├─ project.db
├─ source/
│  ├─ data/
│  └─ js/
└─ write_back/
   ├─ data/
   └─ js/
```

`source/data` 和 `source/js` 是原游戏的完整冻结副本；初始化时
`write_back/data` 和 `write_back/js` 是空目录。原游戏路径只服务本次导入，
不进入 metadata，后续 Extract、Translate 和 WriteBack 也不再访问它。

## 2. 唯一工作区布局

`ProjectWorkspaceLayout` 是 crate 内部的受信路径值。它由
`projects_root + ProjectName` 或一个已选定的工作区根建立，并唯一派生：

```text
workspace_root
├─ database_path       = project.db
├─ source_root
│  ├─ source_data   = source/data
│  └─ source_js     = source/js
└─ write_back_root
   ├─ write_back_data = write_back/data
   └─ write_back_js   = write_back/js
```

工作区创建、项目数据库记录读取和既有项目开启复用这一布局。
`StoredProjectRecord` 持有布局；`OpenedProject` 持有整个受信记录并委托路径
getter。消费方不再各自用字符串重新推导数据库、冻结源或写回路径。

## 3. 工作区创建顺序

`ProjectWorkspaceCreationService` 固定执行：

```text
建立工作区布局与完整暂存请求
        ↓
prepare
  <game>/data → source/data
  <game>/js   → source/js
  创建空 write_back/data
  创建空 write_back/js
        ↓
ProjectDatabaseCreationService
  在 <staging>/project.db 建库并写入唯一 metadata
        ↓
publish(CreateNew)
        ↓
<projects_root>/<name>
```

完整暂存请求在任何副作用前构造和校验。服务不预检最终目录是否
存在；`CreateNew` 发布是同名项目并发创建的唯一线性化点，已存在的
目标绝不覆盖。建库位于目录复制成功之后；服务不重读数据库来验证建库结果，
也不在任何阶段自动重试。

## 4. 原子目录根契约

`AtomicDirectoryPublisher` 是 Init 和 Standard WriteBack 共用的环境根。它把
多目录复制、文件覆盖和空目录作为一个候选目录处理：

1. `prepare(request)` 在最终目标同级建立私有暂存根，成功时最终目标未改变；
2. 返回的 `StagedDirectory` 暴露暂存根以便非根服务在其中建立
   `project.db`，但不可复制；
3. `publish(token, CreateNew | Replace)` 或 `discard(token)` 按值消费 token，
   一个候选只能被终结一次。

`CreateNew` 拒绝任何已存在目标并返回 `TargetAlreadyExists`；`Replace` 只替换一个
已存在的目录，目标缺失或不是目录分别返回 `TargetMissing` 或 `TargetNotDirectory`。
根还拥有递归复制的资源限制、符号链接/
reparse point 拒绝策略、同目标发布线性化、交换恢复和暂存清理。

发布终态保留以下业务含义：

- `NotPublished`：候选没有成为最终目标，原目标仍可信；
- `PublishedButCleanupFailed`：新目标已生效，但旧备份残留；
- `OutcomeUnknown`：无法确定对外可见的是原目录、新目录还是不可用状态。

这些结果不得被降级为成功，也不得自动重试。`OutcomeUnknown` 后不再执行
额外清理或探测。显式 `discard` 失败必须保留准确暂存路径。

## 5. 失败与完成边界

工作区创建服务保留以下阶段语义：

- 请求无效或 prepare 返回 `NotPrepared` 时，不建库、不发布；`NotPrepared`
  保留根原因及可选的暂存清理失败；
- 任何建库失败都只调用一次 `discard`，然后返回建库原因；
- 建库与 discard 同时失败时，同时保留首因、清理原因和残留路径；
- publish 已消费 token，无论返回哪种失败，服务都不再调用 discard；
- 只有 `publish(CreateNew)` 返回完全成功时，Init 才报告项目已初始化。

当前 Init 的全部非根业务树已实现，并可通过根测试替身贯通：

```text
InitService
├─ ExistingDirectoryResolver                    根接口
└─ ProjectWorkspaceCreationService
   ├─ ProjectWorkspaceLayout
   ├─ ProjectDatabaseCreationService
   │  └─ SqliteDatabaseCreator              根接口
   └─ AtomicDirectoryPublisher                  根接口
```

`ExistingDirectoryResolver`、`SqliteDatabaseCreator` 和 `AtomicDirectoryPublisher`
只定义了环境契约，没有生产适配器和组合根接线。因此当前可以声明非根
初始化业务成立，不能声明真实磁盘上的生产 Init 已经贯通。
