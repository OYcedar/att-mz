//! SQLite 数据库创建、查询与事务执行的根能力契约。

use std::error::Error;
use std::fmt;
use std::future::Future;
use std::path::PathBuf;

/// SQLite 拥有型值。
///
/// 根接口返回完整存储类型，使上层持久化边界能够识别外部修改造成的字段类型错误，
/// 而不是把错误类型强制转换为期望值。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SqliteValue {
    Null,
    Integer(i64),
    Real(f64),
    Text(String),
    Blob(Vec<u8>),
}

impl SqliteValue {
    /// 返回适合诊断的 SQLite 存储类型名称。
    pub(crate) fn kind_name(&self) -> &'static str {
        match self {
            Self::Null => "NULL",
            Self::Integer(_) => "INTEGER",
            Self::Real(_) => "REAL",
            Self::Text(_) => "TEXT",
            Self::Blob(_) => "BLOB",
        }
    }
}

/// 一条参数化 SQLite 命令。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteCommand {
    statement: String,
    parameters: Vec<SqliteValue>,
}

impl SqliteCommand {
    pub(crate) fn new(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> Self {
        Self {
            statement: statement.into(),
            parameters,
        }
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn parameters(&self) -> &[SqliteValue] {
        &self.parameters
    }
}

/// 一条参数化 SQLite 查询。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteQuery {
    id: String,
    statement: String,
    parameters: Vec<SqliteValue>,
}

impl SqliteQuery {
    pub(crate) fn new(statement: impl Into<String>, parameters: Vec<SqliteValue>) -> Self {
        Self {
            id: "query".to_owned(),
            statement: statement.into(),
            parameters,
        }
    }

    /// 为领域快照中的查询指定可公开的稳定身份。
    ///
    /// 身份只用于安全诊断；SQL 与参数始终留在私有错误源中。
    pub(crate) fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    pub(crate) fn id(&self) -> &str {
        &self.id
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn parameters(&self) -> &[SqliteValue] {
        &self.parameters
    }
}

/// 查询返回的一行拥有型数据，列顺序与查询投影一致。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteRow {
    values: Vec<SqliteValue>,
}

impl SqliteRow {
    pub(crate) fn new(values: Vec<SqliteValue>) -> Self {
        Self { values }
    }

    pub(crate) fn values(&self) -> &[SqliteValue] {
        &self.values
    }

    pub(crate) fn into_values(self) -> Vec<SqliteValue> {
        self.values
    }
}

/// 一条不需要解析 SQL 即可扩展为多行 `VALUES` 的显式 INSERT 描述。
#[derive(Clone, Debug, PartialEq)]
enum SqliteBatchExecution {
    /// 按顺序逐组执行同一条语句。
    Repeated {
        parameter_sets: Vec<Vec<SqliteValue>>,
    },
    /// 按 SQLite 当前连接的真实变量上限扩展为多行 `VALUES`。
    BulkInsert {
        /// `INSERT INTO ... (columns)`，不包含 `VALUES`。
        statement_prefix: String,
        /// 每行除公共参数外拥有的参数数量。
        row_parameter_count: usize,
        /// 所有行的参数按自然行序连续存放。
        parameter_values: Vec<SqliteValue>,
    },
}

pub(crate) enum SqliteBatchRows<'a> {
    Repeated(std::slice::Iter<'a, Vec<SqliteValue>>),
    Flat(std::slice::ChunksExact<'a, SqliteValue>),
}

impl<'a> Iterator for SqliteBatchRows<'a> {
    type Item = &'a [SqliteValue];

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            Self::Repeated(rows) => rows.next().map(Vec::as_slice),
            Self::Flat(rows) => rows.next(),
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match self {
            Self::Repeated(rows) => rows.size_hint(),
            Self::Flat(rows) => rows.size_hint(),
        }
    }
}

impl ExactSizeIterator for SqliteBatchRows<'_> {
    fn len(&self) -> usize {
        match self {
            Self::Repeated(rows) => rows.len(),
            Self::Flat(rows) => rows.len(),
        }
    }
}

/// 使用同一单行语句批量执行的参数组。
///
/// `statement` 始终保留可逐组执行的单行语义，测试替身和不理解 bulk 模式的纯语义
/// 消费者可以继续按公共参数加当前行参数执行它。生产 SQLite 根仅在 `bulk_insert`
/// 存在时把相同 INSERT 扩展为多行 `VALUES`。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteBatch {
    statement: String,
    /// 在整批执行期间只拥有并绑定一次、占据最前面编号槽位的公共参数。
    shared_parameters: Vec<SqliteValue>,
    execution: SqliteBatchExecution,
}

impl SqliteBatch {
    pub(crate) fn new(statement: impl Into<String>, parameter_sets: Vec<Vec<SqliteValue>>) -> Self {
        Self {
            statement: statement.into(),
            shared_parameters: Vec::new(),
            execution: SqliteBatchExecution::Repeated { parameter_sets },
        }
    }

    /// 建立带公共编号参数的批次。
    ///
    /// `shared_parameters` 依次绑定到 `?1..?N` 且整批只绑定一次；每个参数组依次绑定到
    /// `?N+1..`。这允许同一大值服务全部参数组，而不为每组复制一份拥有型值。
    pub(crate) fn with_shared_parameters(
        statement: impl Into<String>,
        shared_parameters: Vec<SqliteValue>,
        parameter_sets: Vec<Vec<SqliteValue>>,
    ) -> Self {
        Self {
            statement: statement.into(),
            shared_parameters,
            execution: SqliteBatchExecution::Repeated { parameter_sets },
        }
    }

    /// 建立可由生产根按 SQLite 真实变量上限扩展的显式多行 INSERT。
    ///
    /// `statement_prefix` 必须是省略 `VALUES` 的 `INSERT INTO ... (columns)`。每个 VALUES
    /// 元组先引用全部公共参数 `?1..?S`，再引用该行的 `row_parameter_count` 个参数。
    /// 公共参数因此只绑定一次，却能在每行重复使用。该类型不解析或改写任意调用方 SQL。
    #[cfg(test)]
    pub(crate) fn bulk_insert(
        statement_prefix: impl Into<String>,
        row_parameter_count: usize,
        shared_parameters: Vec<SqliteValue>,
        parameter_sets: Vec<Vec<SqliteValue>>,
    ) -> Self {
        Self::bulk_insert_flat(
            statement_prefix,
            row_parameter_count,
            shared_parameters,
            parameter_sets.into_iter().flatten().collect(),
        )
    }

    /// 建立参数连续存放的多行 INSERT，避免为每行另行分配 `Vec`。
    pub(crate) fn bulk_insert_flat(
        statement_prefix: impl Into<String>,
        row_parameter_count: usize,
        shared_parameters: Vec<SqliteValue>,
        parameter_values: Vec<SqliteValue>,
    ) -> Self {
        let statement_prefix = statement_prefix.into();
        let statement = single_row_bulk_insert_statement(
            &statement_prefix,
            shared_parameters.len(),
            row_parameter_count,
        );
        Self {
            statement,
            shared_parameters,
            execution: SqliteBatchExecution::BulkInsert {
                statement_prefix,
                row_parameter_count,
                parameter_values,
            },
        }
    }

    pub(crate) fn statement(&self) -> &str {
        &self.statement
    }

    pub(crate) fn shared_parameters(&self) -> &[SqliteValue] {
        &self.shared_parameters
    }

    pub(crate) fn parameter_rows(&self) -> SqliteBatchRows<'_> {
        match &self.execution {
            SqliteBatchExecution::Repeated { parameter_sets } => {
                SqliteBatchRows::Repeated(parameter_sets.iter())
            }
            SqliteBatchExecution::BulkInsert {
                row_parameter_count,
                parameter_values,
                ..
            } => {
                assert!(
                    *row_parameter_count > 0 && parameter_values.len() % row_parameter_count == 0,
                    "bulk INSERT 参数必须在迭代前通过行宽校验"
                );
                SqliteBatchRows::Flat(parameter_values.chunks_exact(*row_parameter_count))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn parameter_set_count(&self) -> usize {
        match &self.execution {
            SqliteBatchExecution::Repeated { parameter_sets } => parameter_sets.len(),
            SqliteBatchExecution::BulkInsert {
                row_parameter_count,
                parameter_values,
                ..
            } if *row_parameter_count > 0 => parameter_values.len() / row_parameter_count,
            SqliteBatchExecution::BulkInsert { .. } => 0,
        }
    }

    pub(crate) fn bulk_insert_spec(&self) -> Option<(&str, usize, &[SqliteValue])> {
        match &self.execution {
            SqliteBatchExecution::Repeated { .. } => None,
            SqliteBatchExecution::BulkInsert {
                statement_prefix,
                row_parameter_count,
                parameter_values,
            } => Some((
                statement_prefix.as_str(),
                *row_parameter_count,
                parameter_values,
            )),
        }
    }
}

fn single_row_bulk_insert_statement(
    statement_prefix: &str,
    shared_parameter_count: usize,
    row_parameter_count: usize,
) -> String {
    let parameter_count = shared_parameter_count
        .checked_add(row_parameter_count)
        .expect("SQLite 单行 bulk INSERT 参数数量必须可表示为 usize");
    let mut statement = String::with_capacity(
        statement_prefix
            .len()
            .saturating_add(parameter_count.saturating_mul(5))
            .saturating_add(11),
    );
    statement.push_str(statement_prefix);
    statement.push_str(" VALUES (");
    for index in 1..=parameter_count {
        if index > 1 {
            statement.push_str(", ");
        }
        statement.push('?');
        statement.push_str(&index.to_string());
    }
    statement.push(')');
    statement
}

/// 一个 SQLite 事务中的拥有型执行步骤。
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SqliteTransactionStep {
    /// 执行一次参数化命令。
    Execute(SqliteCommand),
    /// 普通模式只准备一次单行语句；显式 bulk INSERT 按真实变量上限生成多行语句。
    ExecuteMany(SqliteBatch),
    /// 只准备一次语句，按顺序执行全部参数组，每组必须恰好修改一行。
    ExecuteManyExactlyOne(SqliteBatch),
    /// 查询必须不返回任何行，否则事务在后续步骤之前失败并回滚。
    RequireNoRows(SqliteQuery),
    /// 查询必须不返回任何行；命中时在确认回滚后把第一行作为领域诊断事实返回。
    ///
    /// SQL 与参数仍由存储实现私有持有，调用方只能消费自己声明的固定列投影。
    RequireNoRowsReturningFirstRow(SqliteQuery),
    /// 只准备一次查询，按顺序校验全部参数组均不返回任何行。
    RequireNoRowsMany(SqliteBatch),
}

/// 必须在同一个连接和写事务中顺序执行的完整计划。
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct SqliteTransactionPlan {
    steps: Vec<SqliteTransactionStep>,
}

impl SqliteTransactionPlan {
    pub(crate) fn new(steps: Vec<SqliteTransactionStep>) -> Self {
        Self { steps }
    }

    pub(crate) fn steps(&self) -> &[SqliteTransactionStep] {
        &self.steps
    }
}

/// 创建新数据库的明确终态。
#[derive(Debug)]
pub(crate) enum CreateDatabaseError<E> {
    AlreadyExists,
    NotCreated(E),
    OutcomeUnknown(E),
    ResidualArtifact(E),
}

/// 把一个现存 SQLite 数据库在线复制到 create-only 目标的明确终态。
#[derive(Debug)]
pub(crate) enum SnapshotDatabaseError<E> {
    SourceNotFound,
    DestinationAlreadyExists,
    NotCreated(E),
    ResidualArtifact(E),
    OutcomeUnknown(E),
}

impl<E: fmt::Display> fmt::Display for SnapshotDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SourceNotFound => formatter.write_str("源数据库不存在"),
            Self::DestinationAlreadyExists => formatter.write_str("快照目标数据库已经存在"),
            Self::NotCreated(source) => write!(formatter, "数据库快照未创建：{source}"),
            Self::ResidualArtifact(source) => {
                write!(formatter, "数据库快照创建失败且存在残留文件：{source}")
            }
            Self::OutcomeUnknown(source) => write!(formatter, "数据库快照结果未知：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for SnapshotDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::SourceNotFound | Self::DestinationAlreadyExists => None,
            Self::NotCreated(source)
            | Self::ResidualArtifact(source)
            | Self::OutcomeUnknown(source) => Some(source),
        }
    }
}

impl<E: fmt::Display> fmt::Display for CreateDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AlreadyExists => formatter.write_str("目标数据库已经存在"),
            Self::NotCreated(source) => write!(formatter, "数据库未创建：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库创建结果未知：{source}"),
            Self::ResidualArtifact(source) => {
                write!(formatter, "数据库创建失败且存在残留文件：{source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for CreateDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::AlreadyExists => None,
            Self::NotCreated(source)
            | Self::OutcomeUnknown(source)
            | Self::ResidualArtifact(source) => Some(source),
        }
    }
}

/// 查询现存数据库的失败语义。
#[derive(Debug)]
pub(crate) enum QueryExistingDatabaseError<E> {
    /// 目标主数据库文件不存在；实现没有创建文件。
    NotFound,
    /// 打开、准备、绑定、读取或关闭查询资源失败。
    QueryFailed(E),
}

impl<E: fmt::Display> fmt::Display for QueryExistingDatabaseError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::QueryFailed(source) => write!(formatter, "数据库查询失败：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for QueryExistingDatabaseError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound => None,
            Self::QueryFailed(source) => Some(source),
        }
    }
}

/// 执行拥有型 SQLite 事务计划的明确终态。
///
/// 携带诊断行的失败会间接持有驱动错误，避免罕见的大型错误源放大每个事务 `Result`；
/// `query_id` 与 `SqliteRow` 仍由错误值直接拥有，调用方可以无损消费。
#[derive(Debug)]
pub(crate) enum ExecuteTransactionError<E> {
    /// 目标主数据库文件不存在；实现没有创建文件。
    NotFound,
    /// 指定事务条件未满足；整个事务已回滚。
    RequirementFailed,
    /// 指定事务条件未满足；整个事务已回滚，并返回条件查询选中的第一行。
    RequirementFailedWithRow { query_id: String, row: SqliteRow },
    /// 条件查询已经命中并保留诊断行，但无法确认回滚终态。
    RequirementFailedWithRowOutcomeUnknown {
        query_id: String,
        row: SqliteRow,
        source: Box<E>,
    },
    /// 事务未提交，驱动确认其修改均未生效。
    NotCommitted(E),
    /// 驱动无法确认提交是否已经生效。
    OutcomeUnknown(E),
}

/// 使用独立短生命周期连接执行最终事务的明确终态。
///
/// 与常驻 SQLite 根不同，该契约把连接显式关闭也纳入一次调用。提交成功但关闭失败
/// 时，提交事实仍然已知，不能降格成 `OutcomeUnknown` 或伪装为回滚。
/// 携带诊断行的变体同样只间接持有驱动错误，不改变诊断行和最终事务终态的所有权。
#[derive(Debug)]
pub(crate) enum ExecuteFinalTransactionError<E> {
    NotFound,
    RequirementFailed,
    RequirementFailedWithRow {
        query_id: String,
        row: SqliteRow,
    },
    RequirementFailedWithRowOutcomeUnknown {
        query_id: String,
        row: SqliteRow,
        source: Box<E>,
    },
    RequirementFailedWithRowAndFinalizationFailed {
        query_id: String,
        row: SqliteRow,
        source: Box<E>,
    },
    NotCommitted(E),
    OutcomeUnknown(E),
    CommittedButFinalizationFailed(E),
}

impl<E: fmt::Display> fmt::Display for ExecuteTransactionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::RequirementFailed => formatter.write_str("事务条件未满足"),
            Self::RequirementFailedWithRow { query_id, row } => write!(
                formatter,
                "事务条件 {query_id} 未满足，并返回 {} 列诊断事实",
                row.values().len()
            ),
            Self::RequirementFailedWithRowOutcomeUnknown {
                query_id,
                row,
                source,
            } => {
                write!(
                    formatter,
                    "事务条件 {query_id} 未满足并返回 {} 列诊断事实，但无法确认回滚终态：{source}",
                    row.values().len()
                )
            }
            Self::NotCommitted(source) => write!(formatter, "数据库事务未提交：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库事务结果未知：{source}"),
        }
    }
}

impl<E: Error + 'static> Error for ExecuteTransactionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound | Self::RequirementFailed | Self::RequirementFailedWithRow { .. } => {
                None
            }
            Self::RequirementFailedWithRowOutcomeUnknown { source, .. } => Some(source.as_ref()),
            Self::NotCommitted(source) | Self::OutcomeUnknown(source) => Some(source),
        }
    }
}

impl<E: fmt::Display> fmt::Display for ExecuteFinalTransactionError<E> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => formatter.write_str("目标数据库不存在"),
            Self::RequirementFailed => formatter.write_str("事务条件未满足"),
            Self::RequirementFailedWithRow { query_id, row } => write!(
                formatter,
                "事务条件 {query_id} 未满足，并返回 {} 列诊断事实",
                row.values().len()
            ),
            Self::RequirementFailedWithRowOutcomeUnknown {
                query_id,
                row,
                source,
            } => {
                write!(
                    formatter,
                    "事务条件 {query_id} 未满足并返回 {} 列诊断事实，但无法确认回滚终态：{source}",
                    row.values().len()
                )
            }
            Self::RequirementFailedWithRowAndFinalizationFailed {
                query_id,
                row,
                source,
            } => {
                write!(
                    formatter,
                    "事务条件 {query_id} 未满足并返回 {} 列诊断事实，事务已回滚但连接收尾失败：{source}",
                    row.values().len()
                )
            }
            Self::NotCommitted(source) => write!(formatter, "数据库事务未提交：{source}"),
            Self::OutcomeUnknown(source) => write!(formatter, "数据库事务结果未知：{source}"),
            Self::CommittedButFinalizationFailed(source) => {
                write!(formatter, "数据库事务已提交，但连接关闭失败：{source}")
            }
        }
    }
}

impl<E: Error + 'static> Error for ExecuteFinalTransactionError<E> {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::NotFound | Self::RequirementFailed | Self::RequirementFailedWithRow { .. } => {
                None
            }
            Self::RequirementFailedWithRowOutcomeUnknown { source, .. }
            | Self::RequirementFailedWithRowAndFinalizationFailed { source, .. } => {
                Some(source.as_ref())
            }
            Self::NotCommitted(source)
            | Self::OutcomeUnknown(source)
            | Self::CommittedButFinalizationFailed(source) => Some(source),
        }
    }
}

/// 以 create-only 语义创建并初始化一个 SQLite 数据库。
///
/// 调用方必须提供位于现存父目录中的目标路径；同一目标的并发调用最多一个成功。
/// 所有命令按给定顺序、使用参数绑定并在同一事务内执行。实现完整拥有连接、伴生
/// 文件和失败清理；返回的 Future 必须为 `Send`，且不得阻塞异步执行器
/// 线程。一旦开始产生副作用，调用方必须持续等待到明确终态。
pub(crate) trait SqliteDatabaseCreator: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn create_new_database(
        &self,
        path: PathBuf,
        commands: Vec<SqliteCommand>,
    ) -> impl Future<Output = Result<(), CreateDatabaseError<Self::Error>>> + Send;
}

/// 使用 SQLite online backup 把现存数据库复制为一个全新的数据库文件。
///
/// 实现必须在开始前原子取得两个连接许可，并以内部固定页批次推进复制；
/// BUSY/LOCKED 期间持续等待，并在批次边界响应取消。源文件缺失时不得创建它，
/// 目标文件存在时不得覆盖。
/// 目标一旦被 create-only 占有，即使调用 Future 被丢弃，实现也必须继续到明确终态。
pub(crate) trait SqliteDatabaseSnapshotter: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn snapshot_database(
        &self,
        source: PathBuf,
        destination: PathBuf,
    ) -> impl Future<Output = Result<(), SnapshotDatabaseError<Self::Error>>> + Send;
}

/// 只读查询一个必须已经存在的 SQLite 数据库。
///
/// 实现不得因数据库缺失而创建主文件；多查询快照必须在同一个连接和只读事务中按
/// 输入顺序执行。查询完成后不向调用方泄漏连接、statement 或行游标。返回的 Future
/// 必须为 `Send`，且不得阻塞异步执行器线程。
pub(crate) trait SqliteQueryExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> impl Future<Output = Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>>> + Send;

    /// 在同一个只读事务快照中按顺序执行多条查询。
    ///
    /// 返回结果与任意非空数量的输入查询一一对应，不设置查询组数、行数或结果字节上限。
    fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> impl Future<Output = Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>>> + Send;
}

/// 在现存数据库中执行一个拥有型事务计划。
///
/// 实现以单一连接和写事务按顺序执行全部步骤。普通批量步骤只准备一次单行语句；
/// 显式 bulk INSERT 按当前连接的真实变量上限自动分块，每块只执行一条多行语句，
/// 不得用内部窗口限制业务总行数。bulk INSERT 不能用于要求逐组影响行数或查询结果的步骤。
/// `RequireNoRows` / `RequireNoRowsReturningFirstRow` / `RequireNoRowsMany`
/// 命中或 `ExecuteManyExactlyOne`
/// 的参数组影响行数不为一时，必须在执行后续步骤前回滚。实现完整拥有连接、TEMP 对象、
/// statement、回滚、关闭、并发预算和背压，不向调用方暴露这些机制。Future 必须
/// 为 `Send` 且不得阻塞异步执行器线程；事务开始后，调用方必须等待到明确终态。
pub(crate) trait SqliteTransactionExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> impl Future<Output = Result<(), ExecuteTransactionError<Self::Error>>> + Send;
}

/// 在独立短生命周期连接中执行最终 SQLite 事务。
///
/// 实现必须只打开已经存在的数据库，完成事务后显式关闭连接，并在 Future 返回前给出
/// 提交、确认未提交、提交终态未知或已提交但连接收尾失败中的精确终态。调用 Future
/// 开始后，编排方必须持续等待到返回，不得通过取消伪造终态。
pub(crate) trait SqliteFinalTransactionExecutor: Send + Sync {
    type Error: Error + Send + Sync + 'static;

    fn execute_final_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> impl Future<Output = Result<(), ExecuteFinalTransactionError<Self::Error>>> + Send;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bulk_insert_keeps_a_single_row_statement_for_simple_fakes() {
        let batch = SqliteBatch::bulk_insert(
            "INSERT INTO rows (owner, id, value)",
            2,
            vec![SqliteValue::Text("builtin".to_owned())],
            vec![
                vec![SqliteValue::Integer(1), SqliteValue::Text("a".to_owned())],
                vec![SqliteValue::Integer(2), SqliteValue::Text("b".to_owned())],
            ],
        );

        assert_eq!(
            batch.statement(),
            "INSERT INTO rows (owner, id, value) VALUES (?1, ?2, ?3)"
        );
        assert_eq!(
            batch.bulk_insert_spec(),
            Some((
                "INSERT INTO rows (owner, id, value)",
                2,
                &[
                    SqliteValue::Integer(1),
                    SqliteValue::Text("a".to_owned()),
                    SqliteValue::Integer(2),
                    SqliteValue::Text("b".to_owned()),
                ][..],
            ))
        );
        let fake_parameter_sets = batch
            .parameter_rows()
            .map(|row| {
                batch
                    .shared_parameters()
                    .iter()
                    .chain(row)
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            fake_parameter_sets,
            vec![
                vec![
                    SqliteValue::Text("builtin".to_owned()),
                    SqliteValue::Integer(1),
                    SqliteValue::Text("a".to_owned()),
                ],
                vec![
                    SqliteValue::Text("builtin".to_owned()),
                    SqliteValue::Integer(2),
                    SqliteValue::Text("b".to_owned()),
                ],
            ]
        );
    }

    #[test]
    fn flat_bulk_insert_exposes_rows_without_per_row_storage() {
        let batch = SqliteBatch::bulk_insert_flat(
            "INSERT INTO rows (owner, id, value)",
            2,
            vec![SqliteValue::Text("builtin".to_owned())],
            vec![
                SqliteValue::Integer(1),
                SqliteValue::Text("a".to_owned()),
                SqliteValue::Integer(2),
                SqliteValue::Text("b".to_owned()),
            ],
        );

        assert_eq!(batch.parameter_set_count(), 2);
        assert_eq!(
            batch.parameter_rows().collect::<Vec<_>>(),
            vec![
                &[SqliteValue::Integer(1), SqliteValue::Text("a".to_owned()),][..],
                &[SqliteValue::Integer(2), SqliteValue::Text("b".to_owned()),][..],
            ]
        );
        assert_eq!(batch.clone(), batch);
        assert!(format!("{batch:?}").contains("BulkInsert"));
    }
}
