//! 用根能力测试替身组装完整 Extract 非根依赖树。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::SelectedRules;
use super::builtin::BuiltInExtractionService;
use super::document::{RpgMakerDocumentReadingConfig, RpgMakerProjectDocumentReadingService};
use super::rules::RulesExtractionService;
use super::service::ExtractService;
use super::store::asset_store::RpgMakerExtractionAssetStore;
use crate::execution::OperationCompletion;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::project_name::ProjectName;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::asset::RpgMakerAssetOwner;
use crate::rpg_maker::project::{ExistingProjectOpener, ExistingProjectOpeningService};
use crate::rpg_maker::project_database::ProjectDatabaseRecordReadingService;
use crate::storage::file_system::{
    DirectoryEntry, DirectoryLister, DirectoryTreeFingerprintError,
    DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFile, ReadFileError, ResolveDirectoryError,
    SnapshotFileReader,
};
use crate::storage::sqlite::{
    ExecuteTransactionError, QueryExistingDatabaseError, SqliteQuery, SqliteQueryExecutor,
    SqliteRow, SqliteTransactionExecutor, SqliteTransactionPlan, SqliteTransactionStep,
    SqliteValue,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FakeRootError;

impl fmt::Display for FakeRootError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("根能力测试替身失败")
    }
}

impl Error for FakeRootError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    BuiltinTransaction,
    RulesTransaction,
}

#[derive(Clone)]
struct FakeDirectoryResolver;

impl ExistingDirectoryResolver for FakeDirectoryResolver {
    type Error = FakeRootError;

    async fn resolve_existing_directory(
        &self,
        _: PathBuf,
    ) -> Result<PathBuf, ResolveDirectoryError<Self::Error>> {
        Ok(PathBuf::from("C:/Games/Demo"))
    }
}

#[derive(Clone)]
struct FakeDirectoryTreeFingerprinter;

impl DirectoryTreeFingerprinter for FakeDirectoryTreeFingerprinter {
    type Error = FakeRootError;

    async fn fingerprint_directory_tree(
        &self,
        _: DirectoryTreeFingerprintRequest,
    ) -> Result<Sha256Fingerprint, DirectoryTreeFingerprintError<Self::Error>> {
        Ok(Sha256Fingerprint::from_bytes([0x5a; 32]))
    }
}

impl FileReader for FakeDirectoryTreeFingerprinter {
    type Error = FakeRootError;

    async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
        Err(ReadFileError::NotFound { path })
    }
}

impl SnapshotFileReader for FakeDirectoryTreeFingerprinter {
    async fn read_snapshot_file(
        &self,
        path: PathBuf,
    ) -> Result<ReadFile, ReadFileError<Self::Error>> {
        self.read_file(path).await
    }
}

#[derive(Clone)]
struct FakeFileReader {
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl FileReader for FakeFileReader {
    type Error = FakeRootError;

    async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        let bytes = match path.file_name().and_then(|name| name.to_str()) {
            Some("rules.toml") => br#"
[[rule]]
file = "Items.json"
path = '[].customRule'
"#
            .to_vec(),
            Some("Items.json") => r#"[null,{"name":"","description":"","customRule":"规则文本"}]"#
                .as_bytes()
                .to_vec(),
            Some("CommonEvents.json") => r#"[
                null,
                {"list":[
                    {"code":999,"parameters":[17]},
                    {"code":999,"parameters":["命令规则文本"]}
                ]}
            ]"#
            .as_bytes()
            .to_vec(),
            Some("System.json") => br#"{
                "gameTitle":"",
                "currencyUnit":"",
                "terms":{"basic":[],"commands":[],"params":[],"messages":{}},
                "elements":[],
                "skillTypes":[],
                "weaponTypes":[],
                "armorTypes":[],
                "equipTypes":[]
            }"#
            .to_vec(),
            Some(
                "Actors.json" | "Armors.json" | "Classes.json" | "Enemies.json" | "Skills.json"
                | "States.json" | "Troops.json" | "Weapons.json",
            ) => b"[null]".to_vec(),
            _ => {
                return Err(ReadFileError::NotFound { path });
            }
        };

        Ok(ReadFile::new(path, bytes))
    }
}

#[derive(Clone)]
struct FakeDirectoryLister;

impl DirectoryLister for FakeDirectoryLister {
    type Error = FakeRootError;

    async fn list_directory(
        &self,
        _: PathBuf,
    ) -> Result<Vec<DirectoryEntry>, ListDirectoryError<Self::Error>> {
        Ok(Vec::new())
    }
}

#[derive(Clone)]
struct FakeCpuTaskExecutor {
    executions: Arc<AtomicUsize>,
    active: Arc<AtomicUsize>,
    max_active: Arc<AtomicUsize>,
}

impl CpuTaskExecutor for FakeCpuTaskExecutor {
    type Error = FakeRootError;

    async fn execute<T, F>(&self, task: F) -> Result<T, CpuTaskExecutionError<Self::Error>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(active, Ordering::SeqCst);
        tokio::task::yield_now().await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(task())
    }
}

#[derive(Clone)]
struct FakeSqliteQueryExecutor {
    queries: Arc<AtomicUsize>,
}

impl SqliteQueryExecutor for FakeSqliteQueryExecutor {
    type Error = FakeRootError;

    async fn query_existing_database(
        &self,
        path: PathBuf,
        query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/Projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        assert!(query.statement().contains("FROM metadata"));
        assert!(query.parameters().is_empty());
        self.queries.fetch_add(1, Ordering::Relaxed);
        Ok(vec![SqliteRow::new(vec![
            SqliteValue::Text("demo".to_owned()),
            SqliteValue::Text("ja".to_owned()),
            SqliteValue::Text("zh-Hans".to_owned()),
            SqliteValue::Blob(vec![0x5a; 32]),
            SqliteValue::Text(r#"{"rules":[]}"#.to_owned()),
        ])])
    }

    async fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
        let mut results = Vec::with_capacity(queries.len());
        for query in queries {
            results.push(self.query_existing_database(path.clone(), query).await?);
        }
        Ok(results)
    }
}

#[derive(Clone)]
struct FakeSqliteTransactionExecutor {
    events: Arc<Mutex<Vec<Event>>>,
    fail_owner: Arc<Mutex<Option<String>>>,
    cancel_after_owner: Arc<Mutex<Option<String>>>,
    cancellation: crate::execution::CooperativeCancellation,
}

impl SqliteTransactionExecutor for FakeSqliteTransactionExecutor {
    type Error = FakeRootError;

    async fn execute_transaction(
        &self,
        path: PathBuf,
        plan: SqliteTransactionPlan,
    ) -> Result<(), ExecuteTransactionError<Self::Error>> {
        assert_eq!(
            path,
            PathBuf::from("C:/Projects")
                .join("mz")
                .join("demo")
                .join("project.db")
        );
        let owner = transaction_owner(&plan);
        let event = match owner {
            "builtin" => Event::BuiltinTransaction,
            "rules" => Event::RulesTransaction,
            owner => panic!("未预期的快照所有者：{owner}"),
        };
        self.events.lock().expect("事件锁不应中毒").push(event);
        if self
            .cancel_after_owner
            .lock()
            .expect("SQLite 取消配置锁不应中毒")
            .as_deref()
            == Some(owner)
        {
            self.cancellation.request();
        }
        if self
            .fail_owner
            .lock()
            .expect("SQLite 失败配置锁不应中毒")
            .as_deref()
            == Some(owner)
        {
            Err(ExecuteTransactionError::NotCommitted(FakeRootError))
        } else {
            Ok(())
        }
    }
}

impl SqliteQueryExecutor for FakeSqliteTransactionExecutor {
    type Error = FakeRootError;

    async fn query_existing_database(
        &self,
        _: PathBuf,
        query: SqliteQuery,
    ) -> Result<Vec<SqliteRow>, QueryExistingDatabaseError<Self::Error>> {
        if query.statement().contains("rpg_maker_project_definition") {
            return Ok(vec![SqliteRow::new(vec![SqliteValue::Text(
                r#"{"rules":[]}"#.to_owned(),
            )])]);
        }
        assert!(
            query.statement().contains("rpg_maker_asset_owner_state")
                || query.statement().contains("rpg_maker_text_group")
                || query.statement().contains("rpg_maker_text_unit")
                || query.statement().contains("rpg_maker_mutation_claim"),
            "Store 只应读取当前 owner 快照或新鲜 owner"
        );
        Ok(Vec::new())
    }

    async fn query_existing_database_snapshot(
        &self,
        path: PathBuf,
        queries: Vec<SqliteQuery>,
    ) -> Result<Vec<Vec<SqliteRow>>, QueryExistingDatabaseError<Self::Error>> {
        let mut results = Vec::with_capacity(queries.len());
        for query in queries {
            results.push(self.query_existing_database(path.clone(), query).await?);
        }
        Ok(results)
    }
}

fn transaction_owner(plan: &SqliteTransactionPlan) -> &str {
    plan.steps()
        .iter()
        .find_map(|step| {
            let SqliteTransactionStep::Execute(command) = step else {
                return None;
            };
            if !command
                .statement()
                .starts_with("INSERT INTO rpg_maker_asset_owner_state")
            {
                return None;
            }
            match command.parameters().first() {
                Some(SqliteValue::Text(owner)) => Some(owner.as_str()),
                _ => None,
            }
        })
        .expect("资产事务必须明确快照所有者")
}

#[tokio::test]
async fn root_fakes_drive_the_complete_non_root_extract_tree() {
    let query_count = Arc::new(AtomicUsize::new(0));
    let cpu_executions = Arc::new(AtomicUsize::new(0));
    let file_max_active = Arc::new(AtomicUsize::new(0));
    let cpu_max_active = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let fail_owner = Arc::new(Mutex::new(None));
    let cancel_after_owner = Arc::new(Mutex::new(None));
    let cancellation = crate::execution::CooperativeCancellation::default();

    let file_reader = FakeFileReader {
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::clone(&file_max_active),
    };
    let directory_lister = FakeDirectoryLister;
    let cpu = FakeCpuTaskExecutor {
        executions: Arc::clone(&cpu_executions),
        active: Arc::new(AtomicUsize::new(0)),
        max_active: Arc::clone(&cpu_max_active),
    };
    let sqlite_transactions = FakeSqliteTransactionExecutor {
        events: Arc::clone(&events),
        fail_owner: Arc::clone(&fail_owner),
        cancel_after_owner: Arc::clone(&cancel_after_owner),
        cancellation: cancellation.clone(),
    };

    let document_config = RpgMakerDocumentReadingConfig::new(non_zero(2));

    let opener = ExistingProjectOpeningService::new(
        ProjectDatabaseRecordReadingService::new(
            PathBuf::from("C:/Projects"),
            RpgMakerLayout::MZ,
            FakeSqliteQueryExecutor {
                queries: Arc::clone(&query_count),
            },
        ),
        FakeDirectoryResolver,
        FakeDirectoryTreeFingerprinter,
    );
    let name: ProjectName = "demo".parse().expect("测试项目名应该合法");
    let project = opener.open(&name).await.expect("测试项目应可打开");
    let builtin = BuiltInExtractionService::new(
        RpgMakerProjectDocumentReadingService::new(
            file_reader.clone(),
            directory_lister.clone(),
            cpu.clone(),
            document_config,
        ),
        RpgMakerExtractionAssetStore::new(sqlite_transactions.clone(), cpu.clone()),
        cpu.clone(),
    );
    let rules = RulesExtractionService::new(
        RpgMakerProjectDocumentReadingService::new(
            file_reader,
            directory_lister,
            cpu.clone(),
            document_config,
        ),
        RpgMakerExtractionAssetStore::new(sqlite_transactions.clone(), cpu.clone()),
        cpu.clone(),
    );
    let extract = ExtractService::new(
        Some(builtin),
        Some(SelectedRules::new(
            crate::rpg_maker::extract::rules::RulesProgram::from_toml(
                PathBuf::from("rules.toml"),
                br#"
[[rule]]
file = "Items.json"
path = '[].customRule'

[[rule]]
code = 999
parameter = 0
"#
                .to_vec(),
            )
            .expect("测试 Rules 应合法"),
            rules,
        )),
        cancellation,
    );

    let output = extract
        .execute(&project)
        .await
        .expect("完整非根树应该成功完成组合提取");

    let OperationCompletion::Completed(output) = output else {
        panic!("完整非根树应正常完成")
    };
    assert_eq!(output.name, name);
    assert_eq!(output.rules_warnings.len(), 1);
    assert_eq!(output.rules_warnings[0].rule_number, 2);
    assert_eq!(output.rules_warnings[0].source_file, "CommonEvents.json");
    assert_eq!(output.rules_warnings[0].command_code, 999);
    assert_eq!(output.rules_warnings[0].parameter, 0);
    assert_eq!(output.rules_warnings[0].actual_type.as_str(), "number");
    assert_eq!(output.rules_warnings[0].skipped_count, 1);
    assert_eq!(query_count.load(Ordering::Relaxed), 1, "项目只应开启一次");
    assert!(
        cpu_executions.load(Ordering::Relaxed) > 0,
        "文档、提取和 Store 的 CPU 工作应通过根执行器"
    );
    assert_eq!(file_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(cpu_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[Event::BuiltinTransaction, Event::RulesTransaction]
    );

    events.lock().expect("事件锁不应中毒").clear();
    *fail_owner.lock().expect("SQLite 失败配置锁不应中毒") = Some("builtin".to_owned());

    extract
        .execute(&project)
        .await
        .expect_err("Builtin 根事务失败必须停止 Rules");

    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[Event::BuiltinTransaction]
    );

    events.lock().expect("事件锁不应中毒").clear();
    *fail_owner.lock().expect("SQLite 失败配置锁不应中毒") = Some("rules".to_owned());

    let error = extract
        .execute(&project)
        .await
        .expect_err("Rules 根事务失败必须保留已经提交的 Builtin 事实");
    let super::service::ExtractServiceError::Rules {
        completed_owners, ..
    } = error
    else {
        panic!("Rules 事务失败应保持 Rules 阶段错误")
    };
    assert_eq!(completed_owners, vec![RpgMakerAssetOwner::Builtin]);
    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[Event::BuiltinTransaction, Event::RulesTransaction]
    );

    events.lock().expect("事件锁不应中毒").clear();
    *fail_owner.lock().expect("SQLite 失败配置锁不应中毒") = None;
    *cancel_after_owner
        .lock()
        .expect("SQLite 取消配置锁不应中毒") = Some("rules".to_owned());

    let completion = extract
        .execute(&project)
        .await
        .expect("Rules 提交后到达的取消不得抹掉已完成结果");
    let OperationCompletion::Completed(output) = completion else {
        panic!("Rules 提交后到达的取消应保留完成结果与警告")
    };
    assert_eq!(output.rules_warnings.len(), 1);
    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[Event::BuiltinTransaction, Event::RulesTransaction]
    );
}

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("测试配置必须显式提供非零值")
}
