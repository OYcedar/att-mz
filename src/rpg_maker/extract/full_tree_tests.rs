//! 用根能力测试替身组装完整 Extract 非根依赖树。

use std::error::Error;
use std::fmt;
use std::num::NonZeroUsize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::builtin::BuiltInExtractionService;
use super::document::{RpgMakerDocumentReadingConfig, RpgMakerProjectDocumentReadingService};
use super::lua::{
    LuaExtractionService, LuaInvocation, TrustedLuaExecutionHost, TrustedLuaExecutionOutcome,
};
use super::rules::RulesExtractionService;
use super::service::ExtractService;
use super::store::asset_store::{RpgMakerExtractionAssetStore, RpgMakerExtractionAssetStoreConfig};
use super::{ExtractInput, SelectedRules};
use crate::execution::OperationCompletion;
use crate::execution::cpu::{CpuTaskExecutionError, CpuTaskExecutor};
use crate::fingerprint::Sha256Fingerprint;
use crate::rpg_maker::RpgMakerLayout;
use crate::rpg_maker::project::ExistingProjectOpeningService;
use crate::rpg_maker::project_database::ProjectDatabaseRecordReadingService;
use crate::rpg_maker::project_lease::{
    ProjectCommandLease, ProjectCommandLeaseError, ProjectCommandLeaseProvider,
};
use crate::rpg_maker::{ProjectName, SelectedLua};
use crate::storage::file_system::{
    DirectoryEntry, DirectoryLister, DirectoryTreeFingerprintError,
    DirectoryTreeFingerprintRequest, DirectoryTreeFingerprinter, ExistingDirectoryResolver,
    FileReader, ListDirectoryError, ReadFile, ReadFileError, ResolveDirectoryError,
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

#[derive(Clone, Copy)]
struct FakeProjectLease;

impl ProjectCommandLeaseProvider for FakeProjectLease {
    type Error = FakeRootError;
    type LeaseState = ();

    async fn acquire(
        &self,
        _: &ProjectName,
    ) -> Result<ProjectCommandLease<Self::LeaseState>, ProjectCommandLeaseError<Self::Error>> {
        Ok(ProjectCommandLease::for_test(()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Event {
    BuiltinTransaction,
    RulesTransaction,
    Lua,
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
                "Actors.json" | "Armors.json" | "Classes.json" | "CommonEvents.json"
                | "Enemies.json" | "Skills.json" | "States.json" | "Troops.json" | "Weapons.json",
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
            SqliteValue::Integer(24),
            SqliteValue::Integer(30),
            SqliteValue::Integer(18),
            SqliteValue::Text(r#"{"rules":[]}"#.to_owned()),
        ])])
    }
}

#[derive(Clone)]
struct FakeSqliteTransactionExecutor {
    events: Arc<Mutex<Vec<Event>>>,
    fail_owner: Arc<Mutex<Option<String>>>,
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
        if query.statement().contains("standard_project_definition") {
            return Ok(vec![SqliteRow::new(vec![SqliteValue::Text(
                r#"{"rules":[]}"#.to_owned(),
            )])]);
        }
        assert!(
            query.statement().contains("standard_asset_owner_state")
                || query.statement().contains("UNION ALL"),
            "Store 只应读取当前 owner 快照或新鲜 owner"
        );
        Ok(Vec::new())
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
                .starts_with("INSERT INTO standard_asset_owner_state")
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

#[derive(Clone)]
struct FakeTrustedLuaExecutionHost {
    events: Arc<Mutex<Vec<Event>>>,
    invocations: Arc<Mutex<Vec<RecordedLuaInvocation>>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RecordedLuaInvocation {
    script_path: PathBuf,
    project: crate::rpg_maker::lua::LuaProjectContext,
}

impl TrustedLuaExecutionHost for FakeTrustedLuaExecutionHost {
    type TranslationClient = ();
    type Error = FakeRootError;

    async fn execute(
        &self,
        invocation: LuaInvocation<Self::TranslationClient>,
    ) -> Result<OperationCompletion<TrustedLuaExecutionOutcome>, Self::Error> {
        self.events.lock().expect("事件锁不应中毒").push(Event::Lua);
        let LuaInvocation::Extract {
            script_path,
            project,
        } = invocation
        else {
            panic!("Extract 完整树不应提交 Translate Lua 调用")
        };
        self.invocations
            .lock()
            .expect("Lua 调用锁不应中毒")
            .push(RecordedLuaInvocation {
                script_path,
                project,
            });
        Ok(OperationCompletion::Completed(
            TrustedLuaExecutionOutcome::Empty,
        ))
    }
}

#[tokio::test]
async fn eight_root_fakes_drive_the_complete_non_root_extract_tree() {
    let query_count = Arc::new(AtomicUsize::new(0));
    let cpu_executions = Arc::new(AtomicUsize::new(0));
    let file_max_active = Arc::new(AtomicUsize::new(0));
    let cpu_max_active = Arc::new(AtomicUsize::new(0));
    let events = Arc::new(Mutex::new(Vec::new()));
    let lua_invocations = Arc::new(Mutex::new(Vec::new()));
    let fail_owner = Arc::new(Mutex::new(None));

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
    };

    let document_config = RpgMakerDocumentReadingConfig::new(non_zero(2));
    let store_config = RpgMakerExtractionAssetStoreConfig::new(non_zero(2));

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
    let builtin = BuiltInExtractionService::new(
        RpgMakerProjectDocumentReadingService::new(
            file_reader.clone(),
            directory_lister.clone(),
            cpu.clone(),
            document_config,
        ),
        RpgMakerExtractionAssetStore::new(sqlite_transactions.clone(), cpu.clone(), store_config),
        cpu.clone(),
    );
    let rules = RulesExtractionService::new(
        file_reader.clone(),
        RpgMakerProjectDocumentReadingService::new(
            file_reader,
            directory_lister,
            cpu.clone(),
            document_config,
        ),
        RpgMakerExtractionAssetStore::new(sqlite_transactions.clone(), cpu.clone(), store_config),
        cpu.clone(),
    );
    let lua = LuaExtractionService::new(
        FakeTrustedLuaExecutionHost {
            events: Arc::clone(&events),
            invocations: Arc::clone(&lua_invocations),
        },
        RpgMakerExtractionAssetStore::new(sqlite_transactions, cpu.clone(), store_config),
    );
    let extract = ExtractService::new(
        opener,
        Some(builtin),
        Some(SelectedRules::new(PathBuf::from("rules.toml"), rules)),
        Some(SelectedLua::new(PathBuf::from("extract.lua"), lua)),
        FakeProjectLease,
        crate::execution::CooperativeCancellation::default(),
    );

    let name: ProjectName = "demo".parse().expect("测试项目名应该合法");
    let output = extract
        .execute(ExtractInput { name: name.clone() })
        .await
        .expect("完整非根树应该成功完成组合提取");

    let OperationCompletion::Completed(output) = output else {
        panic!("完整非根树应正常完成")
    };
    assert_eq!(output.name, name);
    assert_eq!(query_count.load(Ordering::Relaxed), 1, "项目只应开启一次");
    assert!(
        cpu_executions.load(Ordering::Relaxed) > 0,
        "文档、提取和 Store 的 CPU 工作应通过根执行器"
    );
    assert_eq!(file_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(cpu_max_active.load(Ordering::SeqCst), 2);
    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[
            Event::BuiltinTransaction,
            Event::RulesTransaction,
            Event::Lua,
        ]
    );

    {
        let invocations = lua_invocations.lock().expect("Lua 调用锁不应中毒");
        assert_eq!(invocations.len(), 1);
        assert_eq!(invocations[0].script_path, PathBuf::from("extract.lua"));
        assert_eq!(invocations[0].project.name(), output.name.as_str());
    }

    events.lock().expect("事件锁不应中毒").clear();
    lua_invocations.lock().expect("Lua 调用锁不应中毒").clear();
    *fail_owner.lock().expect("SQLite 失败配置锁不应中毒") = Some("builtin".to_owned());

    extract
        .execute(ExtractInput { name })
        .await
        .expect_err("Builtin 根事务失败必须停止 Rules 与 Lua");

    assert_eq!(
        events.lock().expect("事件锁不应中毒").as_slice(),
        &[Event::BuiltinTransaction]
    );
    assert!(
        lua_invocations
            .lock()
            .expect("Lua 调用锁不应中毒")
            .is_empty()
    );
}

fn non_zero(value: usize) -> NonZeroUsize {
    NonZeroUsize::new(value).expect("测试配置必须显式提供非零值")
}
