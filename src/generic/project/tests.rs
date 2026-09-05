use super::error::GenericProjectResourceError;
use super::error::sqlite_error_is_busy;
use super::initialization::sqlite_sidecar_path;
use super::snapshot::LOAD_UNITS_NATURAL_SQL;
use super::transaction::GenericTransactionFinalizationFailure;
use crate::diagnostic::{
    GenericDiagnosticStage, GenericResourceKind, RelatedFailureRelation, StateEffect,
};
use crate::generic::jsonl::scan_input_tree;
use crate::runtime::sqlite::{
    apply_att_sqlite_cancellable_read_write_policy, suspend_att_sqlite_cancellation,
};
use crate::runtime::windows::pin_path_without_reparse;
use std::fs;
use std::io;

use tempfile::tempdir;

use super::*;

#[test]
fn sqlite_operation_errors_use_the_actual_sqlite_failure() {
    let interrupted = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
        None,
    );
    assert!(matches!(
        sqlite_operation_error("测试查询", interrupted),
        GenericProjectError::Cancelled
    ));

    assert!(matches!(
        sqlite_operation_error("测试查询", rusqlite::Error::QueryReturnedNoRows),
        GenericProjectError::Sqlite {
            operation: "测试查询",
            source: rusqlite::Error::QueryReturnedNoRows,
        }
    ));
}

#[test]
fn initial_candidate_cleanup_preserves_every_failed_path() {
    let directory = tempdir().unwrap();
    let candidate = directory.path().join(".project.db.init.tmp");
    let sidecar = sqlite_sidecar_path(&candidate, SQLITE_SIDECAR_SUFFIXES[0]);
    fs::write(&candidate, "candidate").unwrap();
    fs::write(&sidecar, "sidecar").unwrap();
    let targets = [&candidate, &sidecar]
        .into_iter()
        .map(|path| {
            let file = pin_path_without_reparse(path).unwrap();
            (path.clone(), FileIdentity::of(file.file(), path).unwrap())
        })
        .collect::<Vec<_>>();
    fs::rename(&candidate, directory.path().join("original.db")).unwrap();
    fs::rename(&sidecar, directory.path().join("original-journal")).unwrap();
    fs::write(&candidate, "foreign database").unwrap();
    fs::write(&sidecar, "foreign sidecar").unwrap();

    let cleanup =
        cleanup_initial_database_candidate(&targets).expect_err("清理必须保留被替换后的外来文件");
    assert_eq!(cleanup.len(), 2);
    assert_eq!(fs::read_to_string(&candidate).unwrap(), "foreign database");
    assert_eq!(fs::read_to_string(&sidecar).unwrap(), "foreign sidecar");

    let error = GenericProjectError::InitialCandidateCleanup {
        original: Box::new(GenericProjectError::Io {
            operation: FileSystemOperation::Create,
            path: directory.path().join("project.db"),
            source: io::Error::other("建立初始数据库失败"),
        }),
        cleanup,
    };
    let displayed = error.to_string();
    assert!(displayed.contains(".project.db.init.tmp"));
    assert!(displayed.contains(".project.db.init.tmp-journal"));

    let diagnostic = error.diagnostic_report(
        GenericDiagnosticStage::Init,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(diagnostic.related().len(), 2);
    for related in diagnostic.related() {
        assert_eq!(related.relation(), RelatedFailureRelation::Cleanup);
        assert_eq!(related.report().effect(), StateEffect::RecoveryRequired);
    }
}

fn language(value: &str) -> LanguageId {
    LanguageId::parse(value).expect("测试语言应合法")
}

#[test]
fn current_schema_validation_uses_the_command_cancellation() {
    let connection = Connection::open_in_memory().unwrap();
    create_current_generic_schema_for_test(&connection).unwrap();
    let cancellation = CooperativeCancellation::default();
    cancellation.request();

    assert!(matches!(
        validate_current_generic_schema_with_cancellation(&connection, &cancellation),
        Err(GenericProjectError::Cancelled)
    ));
}

#[test]
fn sqlite_row_text_clone_preserves_conversion_errors() {
    let connection = Connection::open_in_memory().unwrap();
    let mut statement = connection.prepare("SELECT 7 AS value").unwrap();
    let mut rows = statement.query([]).unwrap();
    let row = rows.next().unwrap().unwrap();
    let cancellation = CooperativeCancellation::default();
    assert!(matches!(
        clone_sqlite_text_column_with_cancellation(
            row,
            0,
            "读取测试 TEXT",
            &cancellation,
        ),
        Err(GenericProjectError::Sqlite {
            source: rusqlite::Error::InvalidColumnType(
                0,
                ref column,
                rusqlite::types::Type::Integer,
            ),
            ..
        }) if column == "value"
    ));
    drop(rows);
    drop(statement);

    let mut statement = connection
        .prepare("SELECT CAST(x'80' AS TEXT) AS value")
        .unwrap();
    let mut rows = statement.query([]).unwrap();
    let row = rows.next().unwrap().unwrap();
    let error = clone_sqlite_text_column_with_cancellation(row, 0, "读取测试 TEXT", &cancellation)
        .expect_err("无效 UTF-8 TEXT 必须拒绝");
    assert!(matches!(
        &error,
        GenericProjectError::InvalidDatabase {
            problem: GenericProjectDatabaseProblem::InvalidTextColumnUtf8 {
                column: 0,
                valid_up_to: 0,
                error_len: Some(1),
                ..
            },
            source: None,
        }
    ));
    let wire = serde_json::to_value(error.diagnostic_report(
        GenericDiagnosticStage::ProjectOpening,
        Path::new("project.db"),
        StateEffect::Unchanged,
    ))
    .expect("数据库诊断必须可序列化");
    assert_eq!(
        wire["primary"]["code"],
        "generic.project.database.text_column_invalid_utf8"
    );
    assert_eq!(
        wire["primary"]["issue"]["details"]["problem"]["problem"]["column"],
        0
    );
}

fn init(workspace: &Path, source: &Path) -> GenericProjectStore {
    GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.to_path_buf(),
        source_root: Some(source.to_path_buf()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect("项目应初始化")
    .0
}

fn write_source(source: &Path, content: &str) {
    fs::write(source.join("text.jsonl"), content).expect("测试输入应写入");
}

#[test]
fn write_back_layout_rules_are_persisted_reused_cleared_and_rollback_safe() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).expect("应建立测试来源目录");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let store = init(&temp.path().join("project"), &source);
    store.extract().expect("首次 Extract 应成功");
    let fingerprint = store
        .open()
        .unwrap()
        .extracted_raw_fingerprint()
        .expect("已提取项目必须有来源指纹");

    assert!(
        store
            .load_write_back_layout_rules()
            .expect("尚未设置规则时必须读取空规则")
            .is_empty()
    );
    let selected =
        LayoutRuleSet::parse_toml(b"[[rule]]\nmax_fullwidth_chars = 20\nscopes = ['dialogue']\n")
            .expect("测试规则必须有效");
    store
        .replace_write_back_layout_rules(fingerprint, &selected)
        .expect("有效规则必须原子保存");
    assert_eq!(
        store
            .load_write_back_layout_rules()
            .expect("省略外部文件时必须可复用保存内容")
            .canonical_json(),
        selected.canonical_json()
    );

    assert!(LayoutRuleSet::parse_toml(b"[[rule]]\nmax_fullwidth_chars = 0\n").is_err());
    assert_eq!(
        store
            .load_write_back_layout_rules()
            .expect("无效新文件不得改变旧规则")
            .canonical_json(),
        selected.canonical_json()
    );

    let empty = LayoutRuleSet::parse_toml(b"rule = []").expect("显式空规则必须有效");
    store
        .replace_write_back_layout_rules(fingerprint, &empty)
        .expect("空规则必须清除已保存规则");
    assert!(store.load_write_back_layout_rules().unwrap().is_empty());

    let stale = Sha256Fingerprint::from_bytes([0x7f; 32]);
    assert!(matches!(
        store.replace_write_back_layout_rules(stale, &selected),
        Err(GenericProjectError::TranslationSnapshotChanged)
    ));
    assert!(
        store
            .load_write_back_layout_rules()
            .expect("CAS 失败必须回滚并保留空规则")
            .is_empty()
    );
}

#[test]
fn project_diagnostics_preserve_io_sqlite_and_state_facts() {
    let io_error = GenericProjectError::Io {
        operation: FileSystemOperation::Read,
        path: PathBuf::from("nested/input.jsonl"),
        source: io::Error::from_raw_os_error(5),
    };
    let io_diagnostic = io_error.diagnostic_report(
        GenericDiagnosticStage::Extract,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(io_diagnostic.primary().code(), "filesystem.io");
    let wire = serde_json::to_string(&io_diagnostic).expect("I/O 诊断必须可序列化");
    assert!(wire.contains("nested/input.jsonl"));
    assert!(wire.contains("\"raw_os_code\":5"));

    let connection = Connection::open_in_memory().unwrap();
    let source = connection
        .execute("INSERT INTO missing_table VALUES (1)", [])
        .expect_err("不存在的表必须产生 SQLite driver 错误");
    let sqlite_error = GenericProjectError::Sqlite {
        operation: "写入 Generic 测试数据库",
        source,
    };
    let sqlite_diagnostic = sqlite_error.diagnostic_report(
        GenericDiagnosticStage::Translate,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(sqlite_diagnostic.primary().code(), "sqlite.driver");
    let wire = serde_json::to_string(&sqlite_diagnostic).expect("SQLite 诊断必须可序列化");
    assert!(wire.contains("\"primary_code\":1"));
    assert!(wire.contains("\"extended_code\":1"));

    let state_diagnostic = GenericProjectError::ExtractRequired.diagnostic_report(
        GenericDiagnosticStage::Translate,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(
        state_diagnostic.primary().code(),
        "generic.project.extract_required"
    );
}

#[test]
fn first_init_requires_all_values_and_does_not_extract() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let (store, project) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace,
        source_root: Some(source.canonicalize().unwrap()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect("首次 Init 应成功");

    assert_eq!(project.project_name().as_str(), "game");
    assert_eq!(project.extracted_raw_fingerprint(), None);
    assert!(matches!(
        store.load_snapshot(),
        Err(GenericProjectError::ExtractRequired)
    ));
}

#[test]
fn generic_init_records_its_sqlite_transaction() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let performance = Arc::new(RunPerformanceCounters::default());

    GenericProjectStore::initialize_with_cancellation(
        GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: temp.path().join("project"),
            source_root: Some(source),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        },
        CooperativeCancellation::default(),
        Arc::clone(&performance),
    )
    .expect("首次 Init 应成功");

    let transactions = performance
        .snapshot()
        .sqlite_transactions
        .database_initialization;
    assert_eq!(transactions.begin.attempted, 1);
    assert_eq!(transactions.begin.succeeded, 1);
    assert_eq!(transactions.commit.attempted, 1);
    assert_eq!(transactions.commit.succeeded, 1);
    assert_eq!(transactions.rollback.attempted, 0);
}

#[test]
fn initial_database_and_connections_use_the_common_sqlite_policy() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    let project = store.open().expect("Generic 项目应可打开");
    let connection = open_sqlite_connection(
        project.database_path(),
        false,
        CooperativeCancellation::default(),
    )
    .expect("项目数据库应可重开");

    let page_size: i64 = connection
        .query_row("PRAGMA page_size", [], |row| row.get(0))
        .expect("应可读取 page_size");
    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("应可读取 journal_mode");
    let synchronous: i64 = connection
        .query_row("PRAGMA synchronous", [], |row| row.get(0))
        .expect("应可读取 synchronous");
    let cache_size: i64 = connection
        .query_row("PRAGMA cache_size", [], |row| row.get(0))
        .expect("应可读取 cache_size");
    let temp_store: i64 = connection
        .query_row("PRAGMA temp_store", [], |row| row.get(0))
        .expect("应可读取 temp_store");

    assert_eq!(page_size, 64 * 1024);
    assert_eq!(journal_mode, "wal");
    assert_eq!(synchronous, 2, "SQLite FULL synchronous 应返回枚举值 2");
    assert_eq!(cache_size, -(3 * 1024 * 1024));
    assert_eq!(temp_store, 2, "SQLite MEMORY TEMP 模式应返回枚举值 2");
}

#[test]
fn initial_database_matches_the_current_exact_generic_schema() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let _store = init(&workspace, &source);
    let connection =
        Connection::open(workspace.join(DATABASE_FILE_NAME)).expect("应打开 Generic 数据库");

    validate_current_generic_schema(&connection)
        .expect("新建数据库必须与当前唯一 Generic schema 完全一致");
}

#[test]
fn current_generic_schema_rejects_definition_drift_and_attached_objects() {
    let connection = Connection::open_in_memory().unwrap();
    create_current_generic_schema_for_test(&connection).unwrap();
    connection
        .execute_batch(
            "ALTER TABLE translation_resource RENAME TO translation_resource_old;
             CREATE TABLE translation_resource (
                 resource_kind TEXT PRIMARY KEY,
                 canonical_json TEXT NOT NULL
             ) STRICT;
             INSERT INTO translation_resource
             SELECT * FROM translation_resource_old;
             DROP TABLE translation_resource_old;",
        )
        .unwrap();

    let definition_error =
        validate_current_generic_schema(&connection).expect_err("约束变化必须拒绝");
    assert!(matches!(
        definition_error,
        GenericProjectError::InvalidDatabase {
            problem: GenericProjectDatabaseProblem::SchemaMismatch {
                ref definition_mismatches,
                ..
            },
            ..
        } if definition_mismatches
            .iter()
            .any(|object| object.as_str() == "table/translation_resource")
    ));

    connection
        .execute(
            "CREATE INDEX unexpected_generic_unit_index
             ON generic_unit(source_text)",
            [],
        )
        .unwrap();
    let attached_object_error =
        validate_current_generic_schema(&connection).expect_err("附加受管索引必须拒绝");
    assert!(matches!(
        attached_object_error,
        GenericProjectError::InvalidDatabase {
            problem: GenericProjectDatabaseProblem::SchemaMismatch {
                ref unexpected,
                ..
            },
            ..
        } if unexpected
            .iter()
            .any(|object| object.as_str() == "index/unexpected_generic_unit_index")
    ));
}

#[test]
fn normal_project_open_rejects_generic_schema_drift() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    Connection::open(workspace.join(DATABASE_FILE_NAME))
        .unwrap()
        .execute_batch(
            "ALTER TABLE translation_resource RENAME TO translation_resource_old;
             CREATE TABLE translation_resource (
                 resource_kind TEXT PRIMARY KEY,
                 canonical_json TEXT NOT NULL
             ) STRICT;
             INSERT INTO translation_resource
             SELECT * FROM translation_resource_old;
             DROP TABLE translation_resource_old;",
        )
        .unwrap();

    assert!(matches!(
        store.open(),
        Err(GenericProjectError::InvalidDatabase {
            problem: GenericProjectDatabaseProblem::SchemaMismatch {
                ref definition_mismatches,
                ..
            },
            ..
        }) if definition_mismatches
            .iter()
            .any(|object| object.as_str() == "table/translation_resource")
    ));
}

#[test]
fn normal_project_open_rejects_typed_invalid_terminology_resource() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    Connection::open(workspace.join(DATABASE_FILE_NAME))
        .unwrap()
        .execute(
            "UPDATE translation_resource
             SET canonical_json = '[1]'
             WHERE resource_kind = 'terminology'",
            [],
        )
        .unwrap();

    assert!(matches!(
        store.open(),
        Err(GenericProjectError::InvalidResource(
            GenericProjectResourceError::InvalidSnapshot {
                resource: GenericResourceKind::Terminology,
                ..
            }
        ))
    ));
}

#[test]
fn extract_accepts_nul_in_stable_ids_and_kind() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    write_source(
        &source,
        r#"{"id":"\u0000group","kind":"\u0000kind","units":[{"id":"\u0000unit","text":"本文"}]}
"#,
    );

    store.extract().expect("NUL 是稳定身份和 kind 的合法内容");
    let snapshot = store.load_snapshot().expect("应读取已提取资产");
    let group = &snapshot.files()[0].groups()[0];

    assert_eq!(group.id(), "\0group");
    assert_eq!(group.kind(), "\0kind");
    assert_eq!(group.units()[0].id(), "\0unit");
}

#[test]
fn failed_first_init_leaves_no_database_and_can_retry_same_workspace() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");

    let error = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(source.clone()),
        source_language: Some(language("ja")),
        target_language: None,
    })
    .expect_err("首次 Init 缺少目标语言时应失败");

    assert!(matches!(
        error,
        GenericProjectError::MissingInitialField("target-language")
    ));
    assert!(!workspace.join(DATABASE_FILE_NAME).exists());
    assert!(!workspace.exists());

    let (_, project) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(source.canonicalize().unwrap()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect("补齐参数后应能在同一路径成功 Init");

    assert_eq!(project.source_root(), source.canonicalize().unwrap());
    assert!(workspace.join(DATABASE_FILE_NAME).is_file());
    assert!(
        fs::read_dir(&workspace).unwrap().all(|entry| {
            entry.unwrap().file_name().to_string_lossy() != ".project.db.init.tmp"
        })
    );
}

#[test]
fn first_init_publishes_a_self_contained_database_file() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let (_, initialized) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(source.canonicalize().unwrap()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect("首次 Init 应发布数据库");
    let published = workspace.join(DATABASE_FILE_NAME);
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        assert!(
            !sqlite_sidecar_path(&published, suffix).exists(),
            "首次 Init 成功后不得依赖 SQLite sidecar"
        );
    }

    let isolated_workspace = temp.path().join("isolated-project");
    fs::create_dir(&isolated_workspace).unwrap();
    let isolated_database = isolated_workspace.join(DATABASE_FILE_NAME);
    fs::copy(&published, &isolated_database).expect("应只复制已发布的主数据库文件");
    let isolated_connection =
        Connection::open(&isolated_database).expect("独立主数据库文件应可直接重开");
    validate_current_generic_schema(&isolated_connection)
        .expect("独立主数据库文件应包含完整的当前 schema");
    let singleton_count: i64 = isolated_connection
        .query_row(
            "SELECT count(*) FROM generic_project WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("应可读取独立主数据库中的项目单例");
    assert_eq!(singleton_count, 1);
    let journal_mode: String = isolated_connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .expect("应可读取独立主数据库的日志模式");
    assert_eq!(journal_mode, "delete");
    isolated_connection
        .close()
        .expect("独立主数据库验证连接应可显式关闭");

    let reopened = GenericProjectStore::for_workspace(isolated_workspace)
        .open()
        .expect("只复制主数据库文件后仍应能通过生产读取路径打开");
    assert_eq!(reopened.project_name(), initialized.project_name());
    assert_eq!(reopened.source_root(), initialized.source_root());
    assert_eq!(reopened.language_pair(), initialized.language_pair());
}

#[test]
fn first_init_rejects_and_preserves_every_stale_target_sidecar() {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let workspace = temp.path().join("project");
        fs::create_dir(&workspace).unwrap();
        let published = workspace.join(DATABASE_FILE_NAME);
        let stale_sidecar = sqlite_sidecar_path(&published, suffix);
        fs::write(&stale_sidecar, b"stale-sidecar").expect("应建立遗留 sidecar");

        let error = GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace.clone(),
            source_root: Some(source.canonicalize().unwrap()),
            source_language: Some(language("ja")),
            target_language: Some(language("zh-Hans")),
        })
        .expect_err("发布目标旁存在遗留 sidecar 时不得建立新项目");

        assert!(matches!(
            error,
            GenericProjectError::Io {
                operation: FileSystemOperation::Metadata,
                ref path,
                ref source,
            } if path == &stale_sidecar && source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert!(!published.exists(), "拒绝遗留 sidecar 时不得发布主数据库");
        assert_eq!(
            fs::read(&stale_sidecar).expect("遗留 sidecar 必须保留"),
            b"stale-sidecar"
        );
        assert!(
            fs::read_dir(&workspace).unwrap().all(|entry| {
                entry.unwrap().file_name().to_string_lossy() != ".project.db.init.tmp"
            }),
            "拒绝遗留 sidecar 后不得留下候选数据库"
        );
    }
}

#[test]
fn target_sidecar_appearing_during_init_still_blocks_publish() {
    for suffix in SQLITE_SIDECAR_SUFFIXES {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        let candidate = temp.path().join("candidate.db");
        let published = temp.path().join(DATABASE_FILE_NAME);
        let stale_sidecar = sqlite_sidecar_path(&published, suffix);
        let cancellation = CooperativeCancellation::default();
        let candidate_file = create_new_pinned_database_file(&candidate).unwrap();
        let identity = FileIdentity::of(&candidate_file, &candidate).unwrap();
        let mut connection =
            open_sqlite_connection(&candidate, true, cancellation.clone()).expect("应打开候选库");
        create_initial_schema(
            &mut connection,
            &"game".parse().unwrap(),
            &source.canonicalize().unwrap(),
            &language("ja"),
            &language("zh-Hans"),
            &cancellation,
            &RunPerformanceCounters::default(),
        )
        .expect("应建立候选 schema");
        fs::write(&stale_sidecar, b"appeared-during-init")
            .expect("应模拟候选建立后出现的目标 sidecar");

        let error = publish_initial_database_candidate(
            connection,
            candidate_file,
            identity,
            &candidate,
            &published,
            &cancellation,
        )
        .expect_err("最终 rename 前发现目标 sidecar 时不得发布");

        assert!(matches!(
            error,
            GenericProjectError::Io {
                operation: FileSystemOperation::Metadata,
                ref path,
                ref source,
            } if path == &stale_sidecar && source.kind() == io::ErrorKind::AlreadyExists
        ));
        assert!(!published.exists(), "目标 sidecar 竞争时不得发布主数据库");
        assert_eq!(
            fs::read(&stale_sidecar).expect("竞争产生的 sidecar 必须保留"),
            b"appeared-during-init"
        );
        cleanup_initial_database_candidate(&[(candidate, identity)]).expect("应清理未发布候选");
    }
}

#[test]
fn occupied_wal_checkpoint_does_not_publish_initial_database() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let candidate = temp.path().join("candidate.db");
    let published = temp.path().join(DATABASE_FILE_NAME);
    let cancellation = CooperativeCancellation::default();
    let candidate_file = create_new_pinned_database_file(&candidate).unwrap();
    let identity = FileIdentity::of(&candidate_file, &candidate).unwrap();
    let mut writer =
        open_sqlite_connection(&candidate, true, cancellation.clone()).expect("应打开候选库");
    create_initial_schema(
        &mut writer,
        &"game".parse().unwrap(),
        &source.canonicalize().unwrap(),
        &language("ja"),
        &language("zh-Hans"),
        &cancellation,
        &RunPerformanceCounters::default(),
    )
    .expect("应建立候选 schema");

    let blocker = Connection::open(&candidate).expect("应打开 checkpoint 阻塞连接");
    blocker.execute_batch("BEGIN").expect("应开始读取事务");
    let initial_profile: Option<String> = blocker
        .query_row(
            "SELECT last_profile_id FROM generic_project WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .expect("读取事务应固定 WAL 快照");
    assert_eq!(initial_profile, None);
    writer
        .execute(
            "UPDATE generic_project SET last_profile_id = 'primary' WHERE singleton = 1",
            [],
        )
        .expect("应在读取快照之后追加 WAL frame");
    drop(writer);
    assert!(
        sqlite_sidecar_path(&candidate, "-wal").exists(),
        "读取快照必须让候选 WAL 保持存在"
    );

    let publisher = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open(&candidate).expect("应重开候选库"),
        || true,
    )
    .expect("应安装立即停止 busy wait 的测试策略");
    publisher
        .progress_handler(0, None::<fn() -> bool>)
        .expect("测试只应由 busy handler 停止 checkpoint");
    let error = publish_initial_database_candidate(
        publisher,
        candidate_file,
        identity,
        &candidate,
        &published,
        &cancellation,
    )
    .expect_err("checkpoint 被读取快照占用时不得发布主数据库");
    assert!(matches!(
        error,
        GenericProjectError::Sqlite {
            operation: "收束 Generic 初始数据库 WAL",
            ref source,
        } if sqlite_error_is_busy(source)
    ));
    assert!(!published.exists(), "checkpoint 未完成不得出现已发布数据库");

    blocker.execute_batch("ROLLBACK").expect("应释放读取快照");
    drop(blocker);
    let mut cleanup_targets = vec![(candidate.clone(), identity)];
    observe_initial_database_sidecars(&candidate, &mut cleanup_targets).unwrap();
    cleanup_initial_database_candidate(&cleanup_targets).expect("应清理测试候选及 sidecar");
}

#[test]
fn initial_schema_and_project_singleton_roll_back_together() {
    use rusqlite::hooks::{AuthAction, Authorization};

    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let cancellation = CooperativeCancellation::default();
    let mut connection = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open_in_memory().unwrap(),
        || false,
    )
    .unwrap();
    connection
        .authorizer(Some(
            |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Insert {
                    table_name: "generic_project",
                } => Authorization::Deny,
                _ => Authorization::Allow,
            },
        ))
        .unwrap();

    let result = create_initial_schema(
        &mut connection,
        &"game".parse().unwrap(),
        &source,
        &language("ja"),
        &language("zh-Hans"),
        &cancellation,
        &RunPerformanceCounters::default(),
    );
    assert!(result.is_err(), "单例写入失败时 Init 事务应失败");

    connection
        .authorizer(None::<fn(rusqlite::hooks::AuthContext<'_>) -> Authorization>)
        .unwrap();
    let table_count: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_schema
             WHERE type = 'table'
               AND name IN (
                   'generic_project', 'generic_file', 'generic_group',
                   'generic_unit', 'translation_resource'
               )",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(table_count, 0);
}

#[test]
fn init_rejects_source_that_contains_write_back_root() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = source.join("project");

    let error = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(source.canonicalize().unwrap()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect_err("输入包含写回目录时应拒绝");

    assert!(matches!(
        error,
        GenericProjectError::SourceWriteBackOverlap { .. }
    ));
    assert!(!workspace.join(DATABASE_FILE_NAME).exists());
}

#[test]
fn reinit_rejects_source_inside_write_back_and_preserves_project() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    let original_source = store.open().unwrap().source_root().to_path_buf();
    let overlapping_source = workspace.join("write_back").join("input");
    fs::create_dir_all(&overlapping_source).unwrap();

    let error = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(overlapping_source),
        source_language: None,
        target_language: None,
    })
    .expect_err("输入位于写回目录内时应拒绝");

    assert!(matches!(
        error,
        GenericProjectError::SourceWriteBackOverlap { .. }
    ));
    assert_eq!(store.open().unwrap().source_root(), original_source);
}

#[test]
fn init_allows_source_and_write_back_as_sibling_directories() {
    let temp = tempdir().unwrap();
    let workspace = temp.path().join("project");
    let source = workspace.join("input");
    fs::create_dir_all(&source).unwrap();

    let (_, project) = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace,
        source_root: Some(source.clone()),
        source_language: Some(language("ja")),
        target_language: Some(language("zh-Hans")),
    })
    .expect("输入与写回目录为兄弟目录时应允许");

    assert_eq!(project.source_root(), source.canonicalize().unwrap());
}

#[test]
fn first_init_rejects_identical_languages_without_creating_database() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");

    let error = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(source),
        source_language: Some(language("JA")),
        target_language: Some(language("ja")),
    })
    .expect_err("相同源语言和目标语言应拒绝");

    assert!(matches!(
        error,
        GenericProjectError::SameSourceAndTargetLanguage { .. }
    ));
    assert!(!workspace.join(DATABASE_FILE_NAME).exists());
}

#[test]
fn reinit_rejects_identical_languages_without_changing_project() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);

    let error = GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace,
        source_root: None,
        source_language: Some(language("ja")),
        target_language: Some(language("ja")),
    })
    .expect_err("再次 Init 也应拒绝相同语言");

    assert!(matches!(
        error,
        GenericProjectError::SameSourceAndTargetLanguage { .. }
    ));
    assert_eq!(
        store.open().unwrap().language_pair(),
        &LanguagePair::new(language("ja"), language("zh-Hans"))
    );
}

#[test]
fn open_rejects_database_with_identical_languages() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    let connection = store.open_connection(false).unwrap();
    connection
        .execute(
            "UPDATE main.generic_project
             SET target_language = source_language
             WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        store.open(),
        Err(GenericProjectError::SameSourceAndTargetLanguage { .. })
    ));
}

#[test]
fn extract_preserves_stable_units_and_retains_bodies_across_context_changes() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"甲\"},{\"id\":\"b\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().expect("首次 Extract 应成功");
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let writes = group
        .units()
        .iter()
        .map(|unit| TranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            expected_source_text: unit.source_text().to_owned(),
            expected_group_context: group.context_fingerprint(),
            translation: format!("译{}", unit.source_text()),
            state_fingerprint: Sha256Fingerprint::from_bytes([7; 32]),
            expected_translation: None,
            was_current_rejected: false,
        })
        .collect::<Vec<_>>();
    store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &writes,
        )
        .unwrap();

    fs::rename(source.join("text.jsonl"), source.join("moved.jsonl")).unwrap();
    store.extract().expect("移动文件应成功");
    assert_eq!(
        store.load_snapshot().unwrap().files()[0].groups()[0]
            .units()
            .iter()
            .filter(|unit| unit.translation().is_some())
            .count(),
        3
    );

    fs::write(
        source.join("moved.jsonl"),
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"renamed\",\"text\":\"甲\"},{\"id\":\"b\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
    )
    .unwrap();
    store.extract().expect("只改 Unit ID 应成功");
    let units = store.load_snapshot().unwrap().files()[0].groups()[0]
        .units()
        .to_vec();
    assert!(units[0].translation().is_none());
    assert!(units[1].translation().is_some());
    assert!(units[2].translation().is_some());

    fs::write(
        source.join("moved.jsonl"),
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"renamed-again\",\"text\":\"甲\"},{\"id\":\"b-renamed\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
    )
    .unwrap();
    store.extract().expect("同组多个 Unit 只改 ID 应成功");
    let units = store.load_snapshot().unwrap().files()[0].groups()[0]
        .units()
        .to_vec();
    assert!(units[0].translation().is_none());
    assert!(units[1].translation().is_none());
    assert!(units[2].translation().is_some());

    fs::write(
        source.join("moved.jsonl"),
        "{\"id\":\"g\",\"kind\":\"name\",\"units\":[{\"id\":\"renamed-again\",\"text\":\"甲\"},{\"id\":\"b-renamed\",\"text\":\"乙\"},{\"id\":\"c\",\"text\":\"丙\"}]}\n",
    )
    .unwrap();
    store.extract().expect("kind 修改应成功");
    let changed = store.load_snapshot().unwrap();
    let changed_group = &changed.files()[0].groups()[0];
    assert!(changed_group.units()[0].translation().is_none());
    assert!(changed_group.units()[1].translation().is_none());
    let retained = changed_group.units()[2]
        .translation()
        .expect("稳定 Unit 的正文不应因 kind 变化而删除");
    assert_eq!(retained.translation(), "译丙");
    assert_eq!(
        retained.state_fingerprint(),
        Sha256Fingerprint::from_bytes([7; 32]),
        "Extract 只能保留正文和原状态，不能把失配状态改写成当前适用性"
    );
    assert_eq!(
        crate::generic::current_translation_for_stored_with_cancellation(
            changed.project(),
            changed_group,
            &changed_group.units()[2],
            &CooperativeCancellation::default(),
        )
        .unwrap(),
        None,
        "旧 kind 语境的正文只保留为可逆旧值，不得继续作为 Current"
    );
}

#[test]
fn load_snapshot_preserves_file_group_and_unit_natural_order() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir_all(source.join("nested")).unwrap();
    fs::write(
        source.join("a.jsonl"),
        concat!(
            "{\"id\":\"group-z\",\"kind\":\"dialogue\",\"units\":[",
            "{\"id\":\"unit-z\",\"text\":\"甲\"},{\"id\":\"unit-a\",\"text\":\"乙\"}]}\n",
            "{\"id\":\"group-a\",\"kind\":\"name\",\"units\":[",
            "{\"id\":\"unit-only\",\"text\":\"丙\"}]}\n"
        ),
    )
    .unwrap();
    fs::write(
        source.join("nested").join("b.jsonl"),
        "{\"id\":\"group-nested\",\"kind\":\"description\",\"units\":[{\"id\":\"unit-nested\",\"text\":\"丁\"}]}\n",
    )
    .unwrap();
    fs::write(source.join("z-empty.jsonl"), "").unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);

    store.extract().expect("多文件输入应该可提取");
    let snapshot = store.load_snapshot().expect("多层快照应该可读取");

    assert_eq!(
        snapshot
            .files()
            .iter()
            .map(|file| file.relative_path().to_path_buf())
            .collect::<Vec<_>>(),
        [
            PathBuf::from("a.jsonl"),
            PathBuf::from("nested").join("b.jsonl"),
            PathBuf::from("z-empty.jsonl"),
        ]
    );
    assert_eq!(
        snapshot.files()[0]
            .groups()
            .iter()
            .map(GenericStoredGroup::id)
            .collect::<Vec<_>>(),
        ["group-z", "group-a"]
    );
    assert_eq!(
        snapshot.files()[0].groups()[0]
            .units()
            .iter()
            .map(GenericStoredUnit::id)
            .collect::<Vec<_>>(),
        ["unit-z", "unit-a"]
    );
    assert_eq!(
        snapshot.files()[1].groups()[0].units()[0].source_text(),
        "丁"
    );
    assert!(snapshot.files()[2].groups().is_empty());
}

#[test]
fn unit_snapshot_query_uses_natural_order_indexes_without_a_temp_sort() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let connection = store.open_connection(false).unwrap();
    let mut statement = connection
        .prepare(&format!("EXPLAIN QUERY PLAN {LOAD_UNITS_NATURAL_SQL}"))
        .unwrap();
    let plan = statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();

    assert!(
        plan.iter().all(|step| !step.contains("TEMP B-TREE")),
        "Unit 快照读取不应建立临时排序树：{plan:?}"
    );
    assert!(plan.iter().any(|step| step.starts_with("SCAN f")));
    assert!(plan.iter().any(|step| step.starts_with("SEARCH g")));
    assert!(plan.iter().any(|step| step.starts_with("SEARCH u")));
}

#[test]
fn live_changes_require_an_explicit_extract() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"old\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    assert!(store.ensure_input_current().is_ok());

    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"new\"}]}\n",
    );
    assert!(matches!(
        store.ensure_input_current(),
        Err(GenericProjectError::ExtractRequired)
    ));
}

#[test]
fn publish_recheck_compares_live_raw_and_asset_fingerprints() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    let original = "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"old\"}]}\n";
    write_source(&source, original);
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let project = store.open().unwrap();
    ensure_input_fingerprints_current(&project).expect("未变化的输入应该通过发布前复查");

    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"new\"}]}\n",
    );
    assert!(matches!(
        ensure_input_fingerprints_current(&project),
        Err(GenericProjectError::ExtractRequired)
    ));

    write_source(&source, original);
    let mut wrong_asset = project;
    wrong_asset.extracted_asset_fingerprint = Some(Sha256Fingerprint::from_bytes([0_u8; 32]));
    assert!(matches!(
        ensure_input_fingerprints_current(&wrong_asset),
        Err(GenericProjectError::ExtractRequired)
    ));
}

#[test]
fn successful_translation_write_remembers_profile_in_the_same_transaction() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let write = TranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: unit.id().to_owned(),
        expected_source_text: unit.source_text().to_owned(),
        expected_group_context: group.context_fingerprint(),
        translation: "译文".to_owned(),
        state_fingerprint: Sha256Fingerprint::from_bytes([42; 32]),
        expected_translation: None,
        was_current_rejected: false,
    };

    let outcome = store
        .commit_translations_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            std::slice::from_ref(&write),
            "primary",
        )
        .unwrap();
    assert_eq!(outcome.committed, 1);
    assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));

    let conflict = store
        .commit_translations_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[TranslationWrite {
                translation: "另一译文".to_owned(),
                ..write
            }],
            "secondary",
        )
        .unwrap();
    assert_eq!(conflict.committed, 0);
    assert_eq!(conflict.conflicts.len(), 1);
    assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));
}

#[test]
fn rejected_candidate_round_trips_and_valid_translation_clears_it_atomically() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let state = Sha256Fingerprint::from_bytes([42; 32]);
    let source_lines = vec![unit.source_text().to_owned()];
    let rejected = RejectedTranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: unit.id().to_owned(),
        readable_id: "input.jsonl:line1:unit1:text".to_owned(),
        origin: TranslationOrigin::Automatic,
        expected_source_text: unit.source_text().to_owned(),
        source: source_lines.clone(),
        expected_group_context: group.context_fingerprint(),
        expected_manual_applicability: crate::manual::generic_manual_applicability(
            group.id(),
            unit.id(),
            "text.jsonl",
            group.kind(),
            "ja",
            "zh-Hans",
            &source_lines,
        ),
        candidate_json: "{\"wrong\":true}".to_owned(),
        translation: None,
        violation: ProvenInvariantViolation::InvalidCandidateShape,
        planning_state: state,
        expected_translation: None,
        was_current_rejected: false,
    };

    let outcome = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[],
            std::slice::from_ref(&rejected),
            "primary",
        )
        .unwrap();
    assert_eq!(outcome.rejected, 1);
    assert_eq!(outcome.newly_rejected, 1);
    assert_eq!(outcome.resolved_rejected, 0);
    let snapshot = store.load_snapshot().unwrap();
    let stored = snapshot.files()[0].groups()[0].units()[0]
        .rejected()
        .expect("当前硬拒绝必须可以重读");
    assert_eq!(stored.readable_id(), rejected.readable_id);
    assert_eq!(stored.translation(), None);
    assert_eq!(
        stored.violation(),
        &ProvenInvariantViolation::InvalidCandidateShape
    );

    let no_result = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[],
            &[],
            "primary",
        )
        .unwrap();
    assert_eq!(no_result.committed, 0);
    assert_eq!(no_result.rejected, 0);
    assert_eq!(no_result.newly_rejected, 0);
    assert_eq!(no_result.resolved_rejected, 0);
    let unchanged = store.load_snapshot().unwrap();
    assert_eq!(
        unchanged.files()[0].groups()[0].units()[0]
            .rejected()
            .expect("取消、Unavailable 或无法映射时不得请求前清除旧 Rejected")
            .candidate_json,
        rejected.candidate_json
    );

    let repeated = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[],
            &[RejectedTranslationWrite {
                was_current_rejected: true,
                ..rejected.clone()
            }],
            "primary",
        )
        .unwrap();
    assert_eq!(repeated.rejected, 1);
    assert_eq!(repeated.newly_rejected, 0);
    assert_eq!(repeated.resolved_rejected, 0);

    let write = TranslationWrite {
        group_id: rejected.group_id,
        unit_id: rejected.unit_id,
        expected_source_text: rejected.expected_source_text,
        expected_group_context: rejected.expected_group_context,
        translation: "译文".to_owned(),
        state_fingerprint: state,
        expected_translation: None,
        was_current_rejected: true,
    };
    let outcome = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            std::slice::from_ref(&write),
            &[],
            "primary",
        )
        .unwrap();
    assert_eq!(outcome.committed, 1);
    assert_eq!(outcome.resolved_rejected, 1);
    assert_eq!(outcome.newly_rejected, 0);
    let snapshot = store.load_snapshot().unwrap();
    let unit = &snapshot.files()[0].groups()[0].units()[0];
    assert_eq!(unit.translation().unwrap().translation(), "译文");
    assert!(unit.rejected().is_none());
}

#[test]
fn rejected_candidate_cas_accepts_exact_stale_body_and_rejects_a_changed_body() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let old_state = crate::generic::applicability::generic_automatic_applicability(
        "ja",
        "zh-Hans",
        group.id(),
        unit.id(),
        unit.source_text(),
        Sha256Fingerprint::from_bytes([91; 32]),
    );
    store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: "旧语境译文".to_owned(),
                state_fingerprint: old_state,
                expected_translation: None,
                was_current_rejected: false,
            }],
        )
        .unwrap();
    let stale = store.load_snapshot().unwrap();
    let group = &stale.files()[0].groups()[0];
    let unit = &group.units()[0];
    let previous = unit.translation().expect("旧正文必须保留").clone();
    let source_lines = vec![unit.source_text().to_owned()];
    let current_state = crate::generic::applicability::generic_automatic_applicability(
        "ja",
        "zh-Hans",
        group.id(),
        unit.id(),
        unit.source_text(),
        group.context_fingerprint(),
    );
    let rejection = RejectedTranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: unit.id().to_owned(),
        readable_id: "text.jsonl:line1:unit1:text".to_owned(),
        origin: TranslationOrigin::Automatic,
        expected_source_text: unit.source_text().to_owned(),
        source: source_lines.clone(),
        expected_group_context: group.context_fingerprint(),
        expected_manual_applicability: crate::manual::generic_manual_applicability(
            group.id(),
            unit.id(),
            "text.jsonl",
            group.kind(),
            "ja",
            "zh-Hans",
            &source_lines,
        ),
        candidate_json: "true".to_owned(),
        translation: None,
        violation: ProvenInvariantViolation::InvalidCandidateShape,
        planning_state: current_state,
        expected_translation: Some(previous.clone()),
        was_current_rejected: false,
    };

    let saved = store
        .commit_translation_results_for_profile(
            stale.project().extracted_raw_fingerprint().unwrap(),
            &[],
            std::slice::from_ref(&rejection),
            "primary",
        )
        .unwrap();
    assert_eq!(saved.rejected, 1);
    assert!(saved.conflicts.is_empty());
    let retained = store.load_snapshot().unwrap();
    let retained_unit = &retained.files()[0].groups()[0].units()[0];
    assert_eq!(retained_unit.translation(), Some(&previous));
    assert!(retained_unit.rejected().is_some());

    let replacement = TranslationWrite {
        group_id: rejection.group_id.clone(),
        unit_id: rejection.unit_id.clone(),
        expected_source_text: rejection.expected_source_text.clone(),
        expected_group_context: rejection.expected_group_context,
        translation: "新语境译文".to_owned(),
        state_fingerprint: current_state,
        expected_translation: Some(previous),
        was_current_rejected: false,
    };
    assert_eq!(
        store
            .commit_translations(
                retained.project().extracted_raw_fingerprint().unwrap(),
                &[replacement],
            )
            .unwrap()
            .committed,
        1
    );
    let conflict = store
        .commit_translation_results_for_profile(
            retained.project().extracted_raw_fingerprint().unwrap(),
            &[],
            &[rejection],
            "primary",
        )
        .unwrap();
    assert_eq!(conflict.rejected, 0);
    assert_eq!(conflict.conflicts, [("g".to_owned(), "u".to_owned())]);
    let final_snapshot = store.load_snapshot().unwrap();
    let final_unit = &final_snapshot.files()[0].groups()[0].units()[0];
    assert_eq!(
        final_unit
            .translation()
            .map(GenericStoredTranslation::translation),
        Some("新语境译文")
    );
    assert!(final_unit.rejected().is_none());
}

#[test]
fn stale_manual_translation_does_not_block_current_rejected_candidate() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"旧原文\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let old_source = vec![unit.source_text().to_owned()];
    let old_applicability = crate::manual::generic_manual_applicability(
        group.id(),
        unit.id(),
        "text.jsonl",
        group.kind(),
        "ja",
        "zh-Hans",
        &old_source,
    );
    let connection = Connection::open(&store.database_path).unwrap();
    crate::manual::apply_generic_manual_translations(
        &connection,
        &[crate::manual::ValidatedManualTranslation {
            id: "text.jsonl:line1:unit1:text".to_owned(),
            kind: crate::manual::ManualTranslationType::Free,
            source: old_source,
            translation: vec!["旧译文".to_owned()],
            locator: crate::manual::ManualTranslationLocator::Generic {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
            },
            applicability: old_applicability,
        }],
    )
    .unwrap();
    drop(connection);

    let current_rejection = RejectedTranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: unit.id().to_owned(),
        readable_id: "text.jsonl:line1:unit1:text".to_owned(),
        origin: TranslationOrigin::Automatic,
        expected_source_text: unit.source_text().to_owned(),
        source: vec![unit.source_text().to_owned()],
        expected_group_context: group.context_fingerprint(),
        expected_manual_applicability: old_applicability,
        candidate_json: "{\"wrong\":true}".to_owned(),
        translation: None,
        violation: ProvenInvariantViolation::InvalidCandidateShape,
        planning_state: Sha256Fingerprint::from_bytes([41; 32]),
        expected_translation: None,
        was_current_rejected: false,
    };
    let current_outcome = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[],
            &[current_rejection],
            "primary",
        )
        .unwrap();
    assert_eq!(current_outcome.rejected, 0);
    assert_eq!(
        current_outcome.conflicts,
        [(group.id().to_owned(), unit.id().to_owned())],
        "当前人工译文仍必须阻止模型候选覆盖该 Unit"
    );

    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"k\",\"units\":[{\"id\":\"u\",\"text\":\"新原文\"}]}\n",
    );
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    assert!(unit.translation().is_none(), "旧人工译文必须已经过期");
    let source_lines = vec![unit.source_text().to_owned()];
    let rejected = RejectedTranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: unit.id().to_owned(),
        readable_id: "text.jsonl:line1:unit1:text".to_owned(),
        origin: TranslationOrigin::Automatic,
        expected_source_text: unit.source_text().to_owned(),
        source: source_lines.clone(),
        expected_group_context: group.context_fingerprint(),
        expected_manual_applicability: crate::manual::generic_manual_applicability(
            group.id(),
            unit.id(),
            "text.jsonl",
            group.kind(),
            "ja",
            "zh-Hans",
            &source_lines,
        ),
        candidate_json: "{\"wrong\":true}".to_owned(),
        translation: None,
        violation: ProvenInvariantViolation::InvalidCandidateShape,
        planning_state: Sha256Fingerprint::from_bytes([42; 32]),
        expected_translation: None,
        was_current_rejected: false,
    };

    let outcome = store
        .commit_translation_results_for_profile(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[],
            &[rejected],
            "primary",
        )
        .unwrap();
    assert_eq!(outcome.rejected, 1);
    assert!(outcome.conflicts.is_empty());
    let snapshot = store.load_snapshot().unwrap();
    assert!(
        snapshot.files()[0].groups()[0].units()[0]
            .rejected()
            .is_some(),
        "当前 Rejected 候选必须在过期人工记录存在时仍可保存"
    );
}

#[test]
fn batch_translation_commit_preserves_per_unit_cas_and_conflict_order() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        concat!(
            "{\"id\":\"g\",\"kind\":\"k\",\"units\":[",
            "{\"id\":\"a\",\"text\":\"甲\"},",
            "{\"id\":\"b\",\"text\":\"乙\"},",
            "{\"id\":\"c\",\"text\":\"丙\"}]}\n"
        ),
    );
    let store = init(&workspace, &source);
    store.extract().unwrap();
    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    let state = Sha256Fingerprint::from_bytes([42; 32]);
    let mut writes = group
        .units()
        .iter()
        .map(|unit| TranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            expected_source_text: unit.source_text().to_owned(),
            expected_group_context: group.context_fingerprint(),
            translation: format!("译文-{}", unit.id()),
            state_fingerprint: state,
            expected_translation: None,
            was_current_rejected: false,
        })
        .collect::<Vec<_>>();
    writes[1].expected_source_text = "错误原文".to_owned();
    writes[2].expected_group_context = Sha256Fingerprint::from_bytes([7; 32]);

    let outcome = store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &writes,
        )
        .unwrap();
    assert_eq!(outcome.committed, 1);
    assert_eq!(
        outcome.conflicts,
        [
            ("g".to_owned(), "b".to_owned()),
            ("g".to_owned(), "c".to_owned())
        ],
        "冲突必须保持调用方提供的自然顺序"
    );

    let snapshot = store.load_snapshot().unwrap();
    let group = &snapshot.files()[0].groups()[0];
    assert!(group.units()[0].translation().is_some());
    assert!(group.units()[1].translation().is_none());
    assert!(group.units()[2].translation().is_none());
    let previous = group.units()[0].translation().unwrap().clone();
    let update = TranslationWrite {
        group_id: group.id().to_owned(),
        unit_id: group.units()[0].id().to_owned(),
        expected_source_text: group.units()[0].source_text().to_owned(),
        expected_group_context: group.context_fingerprint(),
        translation: "人工并发修改后的新正文".to_owned(),
        state_fingerprint: Sha256Fingerprint::from_bytes([43; 32]),
        expected_translation: Some(previous),
        was_current_rejected: false,
    };
    let outcome = store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[update],
        )
        .unwrap();
    assert_eq!(outcome.committed, 1);
    assert!(outcome.conflicts.is_empty());
}

#[test]
fn reinit_preserves_the_last_successful_profile() {
    let temp = tempdir().unwrap();
    let first_source = temp.path().join("source-a");
    let second_source = temp.path().join("source-b");
    fs::create_dir(&first_source).unwrap();
    fs::create_dir(&second_source).unwrap();
    let workspace = temp.path().join("project");
    let store = init(&workspace, &first_source);
    store.remember_profile("primary").unwrap();
    store.extract().expect("空输入也应建立 Extract 快照");
    let extracted_before_move = store.open().unwrap().extracted_raw_fingerprint();

    GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace.clone(),
        source_root: Some(second_source),
        source_language: None,
        target_language: None,
    })
    .expect("改变输入根应成功");
    let after_source_change = store.open().unwrap();
    assert_eq!(after_source_change.last_profile_id(), Some("primary"));
    assert_eq!(
        after_source_change.extracted_raw_fingerprint(),
        extracted_before_move,
        "只改变绑定路径不应删除最近一次成功 Extract 的事实"
    );

    GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace,
        source_root: None,
        source_language: None,
        target_language: Some(language("zh-Hant")),
    })
    .expect("改变语言应成功");
    assert_eq!(store.open().unwrap().last_profile_id(), Some("primary"));
}

#[test]
fn changing_either_language_preserves_extract_and_translation_bodies() {
    for (source_language, target_language) in [(Some("en"), None), (None, Some("zh-Hant"))] {
        let temp = tempdir().unwrap();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        write_source(
            &source,
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
        );
        let workspace = temp.path().join("project");
        let store = init(&workspace, &source);
        store.extract().expect("首次 Extract 应成功");
        let snapshot = store.load_snapshot().expect("应该可读取 Extract 快照");
        let group = &snapshot.files()[0].groups()[0];
        let unit = &group.units()[0];
        let current_state = crate::generic::applicability::generic_automatic_applicability(
            snapshot.project().language_pair().source().as_str(),
            snapshot.project().language_pair().target().as_str(),
            group.id(),
            unit.id(),
            unit.source_text(),
            group.context_fingerprint(),
        );
        store
            .commit_translations(
                snapshot.project().extracted_raw_fingerprint().unwrap(),
                &[TranslationWrite {
                    group_id: group.id().to_owned(),
                    unit_id: unit.id().to_owned(),
                    expected_source_text: unit.source_text().to_owned(),
                    expected_group_context: group.context_fingerprint(),
                    translation: "译文".to_owned(),
                    state_fingerprint: current_state,
                    expected_translation: None,
                    was_current_rejected: false,
                }],
            )
            .expect("测试译文应该可提交");

        GenericProjectStore::initialize(GenericInitRequest {
            project_name: "game".parse().unwrap(),
            workspace_root: workspace,
            source_root: None,
            source_language: source_language.map(language),
            target_language: target_language.map(language),
        })
        .expect("改变语言应成功");

        let project = store.open().unwrap();
        assert_eq!(
            project.extracted_raw_fingerprint(),
            snapshot.project().extracted_raw_fingerprint()
        );
        assert_eq!(
            project.extracted_asset_fingerprint(),
            snapshot.project().extracted_asset_fingerprint()
        );
        let connection = store.open_connection(false).unwrap();
        let asset_rows: i64 = connection
            .query_row(
                "SELECT
                     (SELECT count(*) FROM generic_file)
                   + (SELECT count(*) FROM generic_group)
                   + (SELECT count(*) FROM generic_unit)",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(asset_rows, 3, "语言变化不应销毁仍可核对的 Extract 事实");
        let retained_translation: Option<String> = connection
            .query_row(
                "SELECT translation FROM generic_unit WHERE group_id = 'g' AND unit_id = 'u'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(retained_translation.as_deref(), Some("译文"));
        drop(connection);
        let changed = store.load_snapshot().unwrap();
        let changed_group = &changed.files()[0].groups()[0];
        let changed_unit = &changed_group.units()[0];
        assert_eq!(
            changed_unit.translation().unwrap().state_fingerprint(),
            current_state,
            "语言变化只能改变当前适用性，不能重写已有状态"
        );
        assert_eq!(
            crate::generic::current_translation_for_stored_with_cancellation(
                changed.project(),
                changed_group,
                changed_unit,
                &CooperativeCancellation::default(),
            )
            .unwrap(),
            None
        );
        let connection = store.open_connection(false).unwrap();
        let terminology_json: String = connection
            .query_row(
                "SELECT canonical_json FROM translation_resource
                 WHERE resource_kind = 'terminology'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(terminology_json, "[]");
    }
}

#[test]
fn moving_to_an_identical_source_root_preserves_the_current_snapshot_and_translation() {
    let temp = tempdir().unwrap();
    let first_source = temp.path().join("source-a");
    let second_source = temp.path().join("source-b");
    fs::create_dir(&first_source).unwrap();
    fs::create_dir(&second_source).unwrap();
    let input =
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n";
    write_source(&first_source, input);
    write_source(&second_source, input);
    let workspace = temp.path().join("project");
    let store = init(&workspace, &first_source);
    store.extract().expect("首次 Extract 应成功");
    let snapshot = store.load_snapshot().expect("应该可读取 Extract 快照");
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &[TranslationWrite {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                translation: "译文".to_owned(),
                state_fingerprint: Sha256Fingerprint::from_bytes([31; 32]),
                expected_translation: None,
                was_current_rejected: false,
            }],
        )
        .expect("测试译文应该可提交");

    GenericProjectStore::initialize(GenericInitRequest {
        project_name: "game".parse().unwrap(),
        workspace_root: workspace,
        source_root: Some(second_source),
        source_language: None,
        target_language: None,
    })
    .expect("移动到相同内容的输入根应成功");

    let (moved, _) = store
        .ensure_input_current()
        .expect("相同内容的新根应继续匹配既有 Extract 快照");
    assert_eq!(
        moved.files()[0].groups()[0].units()[0]
            .translation()
            .map(GenericStoredTranslation::translation),
        Some("译文")
    );
}

#[test]
fn sqlite_busy_wait_stops_promptly_when_the_command_is_cancelled() {
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    init(&workspace, &source);

    let blocker = Connection::open(workspace.join(DATABASE_FILE_NAME)).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

    let cancellation = CooperativeCancellation::default();
    let cancellable_store = GenericProjectStore::for_workspace_with_cancellation(
        workspace.clone(),
        cancellation.clone(),
        Arc::new(RunPerformanceCounters::default()),
    );
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let (sender, receiver) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        worker_barrier.wait();
        sender
            .send(cancellable_store.remember_profile("blocked"))
            .unwrap();
    });
    barrier.wait();
    match receiver.recv_timeout(Duration::from_millis(100)) {
        Err(mpsc::RecvTimeoutError::Timeout) => {}
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            panic!("SQLite 等待线程不得在返回结果前断开")
        }
        Ok(result) => panic!("外部写锁存在时操作必须继续等待，而不是提前返回：{result:?}"),
    }

    cancellation.request();
    let result = receiver
        .recv_timeout(Duration::from_secs(1))
        .expect("取消后不应继续等待 rusqlite 默认的约五秒超时");
    assert!(matches!(result, Err(GenericProjectError::Cancelled)));
    assert!(result.unwrap_err().is_cancelled());

    blocker.execute_batch("ROLLBACK").unwrap();
    worker.join().unwrap();
    let verification = Connection::open(workspace.join(DATABASE_FILE_NAME)).unwrap();
    let remembered: Option<String> = verification
        .query_row(
            "SELECT last_profile_id FROM generic_project WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        remembered, None,
        "Busy 取消只能在显式回滚确认成功后返回 Cancelled"
    );
}

#[test]
fn cancellation_rolls_back_a_partially_written_extract_batch() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    write_source(
        &source,
        concat!(
            "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[",
            "{\"id\":\"u1\",\"text\":\"第一项\"},",
            "{\"id\":\"u2\",\"text\":\"第二项\"}",
            "]}\n"
        ),
    );
    let workspace = temp.path().join("project");
    init(&workspace, &source);

    let cancellation = CooperativeCancellation::default();
    let store = GenericProjectStore::for_workspace_with_cancellation(
        workspace.clone(),
        cancellation.clone(),
        Arc::new(RunPerformanceCounters::default()),
    );
    let project = store.open().expect("应打开 Generic 项目");
    let scanned = scan_input_tree(&source).expect("应扫描测试输入");
    let previous = GenericStoredSnapshot {
        project,
        files: Vec::new(),
    };
    let reconciled =
        reconcile_snapshot(&previous, &scanned, &cancellation).expect("应建立待写快照");

    let mut connection = store.open_connection(false).expect("应打开项目数据库");
    let hook_cancellation = cancellation.clone();
    connection
        .update_hook(Some(
            move |_action: rusqlite::hooks::Action, _database: &str, table: &str, _row_id: i64| {
                if table == "generic_unit" {
                    hook_cancellation.request();
                }
            },
        ))
        .expect("应安装测试更新 hook");
    let result = store.finish_cancellable(run_cancellable_transaction(
        &mut connection,
        &cancellation,
        &RunPerformanceCounters::default(),
        SqliteTransactionScope::WritePlan,
        "开始测试 Extract 事务",
        "提交测试 Extract 事务",
        "回滚测试 Extract 事务",
        |transaction| replace_snapshot(transaction, &scanned, &reconciled.files, &cancellation),
    ));
    assert!(matches!(result, Err(GenericProjectError::Cancelled)));
    assert!(connection.is_autocommit(), "取消返回前必须确认事务已回滚");
    drop(connection);

    let verification =
        Connection::open(workspace.join(DATABASE_FILE_NAME)).expect("应重开项目数据库");
    let asset_rows: i64 = verification
        .query_row(
            "SELECT
                 (SELECT count(*) FROM generic_file)
               + (SELECT count(*) FROM generic_group)
               + (SELECT count(*) FROM generic_unit)",
            [],
            |row| row.get(0),
        )
        .expect("应检查回滚后的资产");
    let fingerprints: (Option<Vec<u8>>, Option<Vec<u8>>) = verification
        .query_row(
            "SELECT extracted_raw_fingerprint, extracted_asset_fingerprint
             FROM generic_project WHERE singleton = 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("应检查回滚后的 Extract 指纹");
    assert_eq!(asset_rows, 0);
    assert_eq!(fingerprints, (None, None));
}

#[test]
fn rollback_failure_is_outcome_unknown_and_preserves_both_failures() {
    use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

    let cancellation = CooperativeCancellation::default();
    let wait_cancellation = cancellation.clone();
    let mut connection = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open_in_memory().unwrap(),
        move || wait_cancellation.is_requested(),
    )
    .unwrap();
    connection
        .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
        .unwrap();
    connection
        .authorizer(Some(
            |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            },
        ))
        .unwrap();

    let result: Result<(), GenericProjectError> = run_cancellable_transaction(
        &mut connection,
        &cancellation,
        &RunPerformanceCounters::default(),
        SqliteTransactionScope::WritePlan,
        "开始回滚失败测试事务",
        "提交回滚失败测试事务",
        "回滚失败测试事务",
        |transaction| {
            transaction
                .execute("INSERT INTO changed VALUES (1)", [])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入回滚失败测试数据",
                    source,
                })?;
            cancellation.request();
            Err(GenericProjectError::Cancelled)
        },
    );
    let classifier = GenericProjectStore::for_workspace_with_cancellation(
        PathBuf::new(),
        cancellation.clone(),
        Arc::new(RunPerformanceCounters::default()),
    );
    let result = classifier.finish_cancellable(result);
    let error = result.expect_err("ROLLBACK 被拒绝时不得报告干净取消");
    match &error {
        GenericProjectError::TransactionOutcomeUnknown {
            primary: Some(primary),
            finalization: GenericTransactionFinalizationFailure::Sqlite { operation, .. },
            ..
        } => {
            assert!(matches!(primary.as_ref(), GenericProjectError::Cancelled));
            assert_eq!(*operation, "回滚失败测试事务");
        }
        other => panic!("应保留主取消与回滚失败，实际为 {other:?}"),
    }
    assert!(!error.is_cancelled());
    let diagnostic = error.diagnostic_report(
        GenericDiagnosticStage::Extract,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
    assert_eq!(diagnostic.related().len(), 1);
    assert_eq!(
        diagnostic.related()[0].relation(),
        RelatedFailureRelation::Finalization
    );
    assert_eq!(
        diagnostic.related()[0].report().effect(),
        StateEffect::OutcomeUnknown
    );
    assert_eq!(
        diagnostic.related()[0].report().primary().code(),
        "sqlite.driver"
    );
}

#[test]
fn cancellation_requested_during_commit_does_not_interrupt_a_successful_commit() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

    let cancellation = CooperativeCancellation::default();
    let wait_cancellation = cancellation.clone();
    let mut connection = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open_in_memory().unwrap(),
        move || wait_cancellation.is_requested(),
    )
    .unwrap();
    connection
        .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
        .unwrap();
    let commit_seen = Arc::new(AtomicBool::new(false));
    let hook_commit_seen = Arc::clone(&commit_seen);
    let hook_cancellation = cancellation.clone();
    connection
        .authorizer(Some(
            move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Unknown,
                } => {
                    hook_commit_seen.store(true, Ordering::Release);
                    hook_cancellation.request();
                    Authorization::Allow
                }
                _ => Authorization::Allow,
            },
        ))
        .unwrap();

    let result = run_cancellable_transaction(
        &mut connection,
        &cancellation,
        &RunPerformanceCounters::default(),
        SqliteTransactionScope::WritePlan,
        "开始提交取消测试事务",
        "提交取消测试事务",
        "回滚提交取消测试事务",
        |transaction| {
            transaction
                .execute("INSERT INTO changed VALUES (1)", [])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入提交取消测试数据",
                    source,
                })?;
            Ok(())
        },
    );
    assert!(result.is_ok(), "COMMIT 开始后到达的取消不得改写成功终态");
    assert!(commit_seen.load(Ordering::Acquire));
    assert!(cancellation.is_requested());
    assert!(connection.is_autocommit());
    let finalization_cancellation = connection.cancellation_handle();
    let finalization = suspend_att_sqlite_cancellation(&finalization_cancellation);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM changed", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    drop(finalization);
}

#[test]
fn failed_commit_with_confirmed_rollback_is_not_mapped_to_cancelled() {
    use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

    let cancellation = CooperativeCancellation::default();
    let wait_cancellation = cancellation.clone();
    let mut connection = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open_in_memory().unwrap(),
        move || wait_cancellation.is_requested(),
    )
    .unwrap();
    connection
        .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
        .unwrap();
    let hook_cancellation = cancellation.clone();
    connection
        .authorizer(Some(
            move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Unknown,
                } => {
                    hook_cancellation.request();
                    Authorization::Deny
                }
                _ => Authorization::Allow,
            },
        ))
        .unwrap();

    let result = run_cancellable_transaction(
        &mut connection,
        &cancellation,
        &RunPerformanceCounters::default(),
        SqliteTransactionScope::WritePlan,
        "开始提交失败测试事务",
        "提交失败测试事务",
        "回滚提交失败测试事务",
        |transaction| {
            transaction
                .execute("INSERT INTO changed VALUES (1)", [])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入提交失败测试数据",
                    source,
                })?;
            Ok(())
        },
    );
    let classifier = GenericProjectStore::for_workspace_with_cancellation(
        PathBuf::new(),
        cancellation.clone(),
        Arc::new(RunPerformanceCounters::default()),
    );
    let result = classifier.finish_cancellable(result);
    let error = result.expect_err("COMMIT 被拒绝后必须报告确认未提交");
    assert!(matches!(
        &error,
        GenericProjectError::TransactionNotCommitted { .. }
    ));
    assert!(!error.is_cancelled());
    assert!(connection.is_autocommit());
    let finalization_cancellation = connection.cancellation_handle();
    let finalization = suspend_att_sqlite_cancellation(&finalization_cancellation);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM changed", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(finalization);
    let diagnostic = error.diagnostic_report(
        GenericDiagnosticStage::Translate,
        Path::new("project.db"),
        StateEffect::OutcomeUnknown,
    );
    assert_eq!(diagnostic.effect(), StateEffect::Unchanged);
    assert!(diagnostic.related().is_empty());
    let wire = serde_json::to_string(&diagnostic).expect("回滚终态诊断必须可序列化");
    assert!(wire.contains("\"transaction\":\"rolled_back\""));
}

#[test]
fn commit_and_rollback_failures_report_outcome_unknown_with_both_causes() {
    use rusqlite::hooks::{AuthAction, Authorization, TransactionOperation};

    let cancellation = CooperativeCancellation::default();
    let wait_cancellation = cancellation.clone();
    let mut connection = apply_att_sqlite_cancellable_read_write_policy(
        Connection::open_in_memory().unwrap(),
        move || wait_cancellation.is_requested(),
    )
    .unwrap();
    connection
        .execute_batch("CREATE TABLE changed(value INTEGER NOT NULL)")
        .unwrap();
    let hook_cancellation = cancellation.clone();
    connection
        .authorizer(Some(
            move |context: rusqlite::hooks::AuthContext<'_>| match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Unknown,
                } => {
                    hook_cancellation.request();
                    Authorization::Deny
                }
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => Authorization::Deny,
                _ => Authorization::Allow,
            },
        ))
        .unwrap();

    let result = run_cancellable_transaction(
        &mut connection,
        &cancellation,
        &RunPerformanceCounters::default(),
        SqliteTransactionScope::WritePlan,
        "开始提交终态未知测试事务",
        "提交终态未知测试事务",
        "回滚提交终态未知测试事务",
        |transaction| {
            transaction
                .execute("INSERT INTO changed VALUES (1)", [])
                .map_err(|source| GenericProjectError::Sqlite {
                    operation: "写入提交终态未知测试数据",
                    source,
                })?;
            Ok(())
        },
    );
    let classifier = GenericProjectStore::for_workspace_with_cancellation(
        PathBuf::new(),
        cancellation.clone(),
        Arc::new(RunPerformanceCounters::default()),
    );
    let result = classifier.finish_cancellable(result);
    let error = result.expect_err("COMMIT 与 ROLLBACK 都失败时结果必须未知");
    match &error {
        GenericProjectError::TransactionOutcomeUnknown {
            primary: Some(primary),
            finalization: GenericTransactionFinalizationFailure::Sqlite { operation, .. },
            ..
        } => {
            assert!(matches!(
                primary.as_ref(),
                GenericProjectError::Sqlite {
                    operation: "提交终态未知测试事务",
                    ..
                }
            ));
            assert_eq!(*operation, "回滚提交终态未知测试事务");
        }
        other => panic!("应保留 COMMIT 与 ROLLBACK 两个失败，实际为 {other:?}"),
    }
    assert!(!error.is_cancelled());
    let diagnostic = error.diagnostic_report(
        GenericDiagnosticStage::Translate,
        Path::new("project.db"),
        StateEffect::Unchanged,
    );
    assert_eq!(diagnostic.effect(), StateEffect::OutcomeUnknown);
    assert_eq!(diagnostic.primary().code(), "sqlite.driver");
    assert_eq!(diagnostic.related().len(), 1);
    assert_eq!(
        diagnostic.related()[0].relation(),
        RelatedFailureRelation::Finalization
    );
    assert_eq!(
        diagnostic.related()[0].report().effect(),
        StateEffect::OutcomeUnknown
    );
    let wire = serde_json::to_string(&diagnostic).expect("事务未知诊断必须可序列化");
    assert_eq!(wire.matches("sqlite.driver").count(), 2);
    assert!(wire.contains("\"transaction\":\"active\""));
    assert!(wire.contains("\"transaction\":\"outcome_unknown\""));
}

#[test]
fn extract_preserves_logical_units_when_equal_text_siblings_reorder() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"a\",\"text\":\"同文\"},{\"id\":\"b\",\"text\":\"同文\"},{\"id\":\"c\",\"text\":\"未移动\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().expect("首次 Extract 应成功");
    let snapshot = store.load_snapshot().expect("应该可读取首次快照");
    let group = &snapshot.files()[0].groups()[0];
    let writes = group
        .units()
        .iter()
        .map(|unit| TranslationWrite {
            group_id: group.id().to_owned(),
            unit_id: unit.id().to_owned(),
            expected_source_text: unit.source_text().to_owned(),
            expected_group_context: group.context_fingerprint(),
            translation: format!("译文-{}", unit.id()),
            state_fingerprint: Sha256Fingerprint::from_bytes([8; 32]),
            expected_translation: None,
            was_current_rejected: false,
        })
        .collect::<Vec<_>>();
    store
        .commit_translations(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            &writes,
        )
        .expect("测试译文应该可提交");

    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"b\",\"text\":\"同文\"},{\"id\":\"a\",\"text\":\"同文\"},{\"id\":\"c\",\"text\":\"未移动\"}]}\n",
    );
    store.extract().expect("重排后的输入应该可重新提取");

    let moved = store.load_snapshot().unwrap();
    let units = moved.files()[0].groups()[0].units();
    assert_eq!(units[0].id(), "b");
    assert_eq!(units[0].translation().unwrap().translation(), "译文-b");
    assert_eq!(units[1].id(), "a");
    assert_eq!(units[1].translation().unwrap().translation(), "译文-a");
    assert_eq!(units[2].translation().unwrap().translation(), "译文-c");
}

#[test]
fn applying_resources_rejects_invalid_terminology_and_preserves_valid_raw_text() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"原文\"}]}\n",
    );
    let workspace = temp.path().join("project");
    let store = init(&workspace, &source);
    store.extract().expect("首次 Extract 应成功");
    let expected_raw_fingerprint = store
        .open()
        .expect("应打开已提取项目")
        .extracted_raw_fingerprint()
        .expect("已提取项目应保存原始指纹");

    for (case, terminology_json, expected_code) in [
        (
            "条目类型错误",
            "[1]",
            "translation.terminology.invalid_snapshot_json",
        ),
        (
            "术语重复",
            r#"[{"term":"同名","translation":"译文一","triggers":["触发一"]},{"term":"同名","translation":"译文二","triggers":["触发二"]}]"#,
            "translation.terminology.duplicate_term",
        ),
    ] {
        let error = store
            .apply_translation_resources(expected_raw_fingerprint, terminology_json, "[]", &[])
            .expect_err(case);
        assert!(matches!(&error, GenericProjectError::InvalidResource(_)));
        assert!(std::error::Error::source(&error).is_some());
        let diagnostic = error.diagnostic_report(
            GenericDiagnosticStage::Translate,
            store.database_path(),
            StateEffect::Unchanged,
        );
        assert_eq!(diagnostic.primary().code(), expected_code);

        let resources = store
            .load_translation_resources()
            .expect("拒绝无效术语后项目资源仍应可读取");
        assert_eq!(resources.terminology_json(), "[]");
        assert_eq!(resources.placeholder_rules_json(), "[]");
    }

    let terminology_with_whitespace =
        r#"[{"term":" 原文 ","translation":" 译文 ","triggers":[" 原文 "]}]"#;
    store
        .apply_translation_resources(
            expected_raw_fingerprint,
            terminology_with_whitespace,
            "[]",
            &[],
        )
        .expect("术语原值中的首尾空白应由项目资源边界原样接受");
    let resources = store
        .load_translation_resources()
        .expect("合法术语应保存到当前项目");
    assert_eq!(resources.terminology_json(), terminology_with_whitespace);
}

#[test]
fn applying_new_resources_moves_invalid_manual_translation_to_rejected_atomically() {
    let temp = tempdir().unwrap();
    let source = temp.path().join("source");
    fs::create_dir(&source).unwrap();
    let workspace = temp.path().join("project");
    write_source(
        &source,
        "{\"id\":\"g\",\"kind\":\"dialogue\",\"units\":[{\"id\":\"u\",\"text\":\"Open [A].\"}]}\n",
    );
    let store = init(&workspace, &source);
    store.extract().expect("首次 Extract 应成功");
    let snapshot = store.load_snapshot().expect("应该可读取首次快照");
    let group = &snapshot.files()[0].groups()[0];
    let unit = &group.units()[0];
    let source_lines = vec![unit.source_text().to_owned()];
    let applicability = crate::manual::generic_manual_applicability(
        group.id(),
        unit.id(),
        "text.jsonl",
        group.kind(),
        "ja",
        "zh-Hans",
        &source_lines,
    );
    let connection = Connection::open(&store.database_path).expect("应该可打开项目数据库");
    crate::manual::apply_generic_manual_translations(
        &connection,
        &[crate::manual::ValidatedManualTranslation {
            id: "text.jsonl:line1:unit1:text".to_owned(),
            kind: crate::manual::ManualTranslationType::Free,
            source: source_lines,
            translation: vec!["打开 [B]。".to_owned()],
            locator: crate::manual::ManualTranslationLocator::Generic {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
            },
            applicability,
        }],
    )
    .expect("人工译文应该可保存");
    drop(connection);

    let snapshot = store.load_snapshot().expect("应该可读取人工译文");
    let unit = &snapshot.files()[0].groups()[0].units()[0];
    let previous = unit.translation().expect("人工译文应该是当前译文").clone();
    assert_eq!(previous.origin(), TranslationOrigin::Manual);

    let outcome = store
        .apply_translation_resources(
            snapshot.project().extracted_raw_fingerprint().unwrap(),
            "[]",
            "[]",
            &[TranslationClear {
                group_id: group.id().to_owned(),
                unit_id: unit.id().to_owned(),
                readable_id: "text.jsonl:line1:unit1:text".to_owned(),
                expected_source_text: unit.source_text().to_owned(),
                expected_group_context: group.context_fingerprint(),
                expected_translation: previous,
                violation: ProvenInvariantViolation::PlaceholderMismatch,
                rejection_planning_state: Sha256Fingerprint::from_bytes([8; 32]),
            }],
        )
        .expect("资源和失效人工译文应该可原子更新");

    assert_eq!(outcome.committed, 1);
    assert!(outcome.conflicts.is_empty());
    let snapshot = store.load_snapshot().expect("应该可读取失效后的项目状态");
    let unit = &snapshot.files()[0].groups()[0].units()[0];
    assert!(
        unit.translation().is_none(),
        "已转入 Rejected 的人工译文不得继续作为 Current"
    );
    let rejected = unit.rejected().expect("失效人工译文必须保存在 Rejected");
    assert_eq!(rejected.origin(), TranslationOrigin::Manual);
    assert_eq!(
        rejected.translation(),
        Some(["打开 [B]。".to_owned()].as_slice())
    );
    assert_eq!(
        rejected.violation(),
        &ProvenInvariantViolation::PlaceholderMismatch
    );
}
