use super::*;

fn mapping(source: &str, target: &str) -> DirectorySourceMapping {
    DirectorySourceMapping::new(PathBuf::from(source), PathBuf::from(target))
        .expect("测试来源映射应该合法")
}

fn overlay(path: &str) -> DirectoryFileOverlay {
    DirectoryFileOverlay::new(PathBuf::from(path), vec![1, 2, 3]).expect("测试文件覆盖应该合法")
}

#[test]
fn every_candidate_relative_path_rejects_empty_absolute_and_escape_forms() {
    for path in [
        ".",
        "../assets",
        "assets/../scripts",
        "assets/./catalog.json",
        "/outside",
        "C:/outside",
    ] {
        assert!(matches!(
            DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::from(path)),
            Err(DirectoryStageRequestError::InvalidRelativePath { .. })
        ));
        assert!(matches!(
            DirectoryFileOverlay::new(PathBuf::from(path), Vec::new()),
            Err(DirectoryStageRequestError::InvalidRelativePath { .. })
        ));
        assert!(matches!(
            DirectoryStageRequest::new(
                PathBuf::from("out"),
                DirectoryPublishIntent::CreateNew,
                vec![mapping("source", "source")],
                Vec::new(),
                vec![PathBuf::from(path)],
            ),
            Err(DirectoryStageRequestError::InvalidRelativePath { .. })
        ));
    }
    assert!(
        DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::new()).is_ok(),
        "来源映射可以精确声明候选根"
    );
    assert!(matches!(
        DirectoryFileOverlay::new(PathBuf::new(), Vec::new()),
        Err(DirectoryStageRequestError::InvalidRelativePath { .. })
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source", "source")],
            Vec::new(),
            vec![PathBuf::new()],
        ),
        Err(DirectoryStageRequestError::InvalidRelativePath { .. })
    ));
}

#[test]
fn root_source_mapping_owns_the_whole_candidate_and_must_be_unique() {
    let request = DirectoryStageRequest::new(
        PathBuf::from("out"),
        DirectoryPublishIntent::ReplaceExisting,
        vec![
            DirectorySourceMapping::new(PathBuf::from("source"), PathBuf::new())
                .expect("根来源映射应合法"),
        ],
        vec![overlay("dialogue.jsonl"), overlay("nested/name.jsonl")],
        Vec::new(),
    )
    .expect("根来源映射应覆盖全部文件");
    assert!(
        request.source_mappings()[0]
            .relative_target()
            .as_os_str()
            .is_empty()
    );

    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::ReplaceExisting,
            vec![
                DirectorySourceMapping::new(PathBuf::from("source/root"), PathBuf::new(),)
                    .expect("根来源映射应合法"),
                mapping("source/nested", "nested"),
            ],
            Vec::new(),
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::OverlappingSourceTargets { .. })
    ));
}

#[test]
fn stage_request_rejects_missing_roots_and_sources() {
    assert!(matches!(
        DirectorySourceMapping::new(PathBuf::new(), PathBuf::from("assets")),
        Err(DirectoryStageRequestError::EmptySourceDirectory)
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::new(),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source", "assets")],
            Vec::new(),
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::EmptyTargetRoot)
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::EmptySourceMappings)
    ));
}

#[test]
fn stage_request_rejects_overlapping_targets_overlays_and_empty_directories() {
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![
                mapping("source/assets", "assets"),
                mapping("source/catalog", "assets/catalog")
            ],
            Vec::new(),
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::OverlappingSourceTargets { .. })
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            vec![
                overlay("assets/catalog.json"),
                overlay("assets/catalog.json")
            ],
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::OverlappingOverlays { .. })
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            Vec::new(),
            vec![PathBuf::from("empty"), PathBuf::from("empty/child")],
        ),
        Err(DirectoryStageRequestError::OverlappingEmptyDirectories { .. })
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            Vec::new(),
            vec![PathBuf::from("assets/empty")],
        ),
        Err(DirectoryStageRequestError::EmptyDirectoryOverlapsSourceTarget { .. })
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            vec![overlay("assets/catalog.json")],
            vec![PathBuf::from("assets/catalog.json/child")],
        ),
        Err(DirectoryStageRequestError::EmptyDirectoryOverlapsOverlay { .. })
    ));
}

#[test]
fn overlay_can_replace_a_source_file_or_create_a_disjoint_file() {
    DirectoryStageRequest::new(
        PathBuf::from("out"),
        DirectoryPublishIntent::CreateNew,
        vec![mapping("source/assets", "assets")],
        vec![overlay("assets/catalog.json"), overlay("scripts/main.lua")],
        Vec::new(),
    )
    .expect("来源内替换与独立文件覆盖都应合法");
}

#[test]
fn overlay_rejects_a_source_target_or_its_ancestor() {
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/assets", "assets")],
            vec![overlay("assets")],
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::OverlayOverlapsSourceTarget {
            overlay,
            source_target,
        }) if overlay == Path::new("assets") && source_target == Path::new("assets")
    ));
    assert!(matches!(
        DirectoryStageRequest::new(
            PathBuf::from("out"),
            DirectoryPublishIntent::CreateNew,
            vec![mapping("source/images", "assets/images")],
            vec![overlay("assets")],
            Vec::new(),
        ),
        Err(DirectoryStageRequestError::OverlayOverlapsSourceTarget {
            overlay,
            source_target,
        }) if overlay == Path::new("assets") && source_target == Path::new("assets/images")
    ));
}

#[test]
fn stage_request_prefix_index_preserves_input_order_error_selection() {
    let error = DirectoryStageRequest::new(
        PathBuf::from("out"),
        DirectoryPublishIntent::CreateNew,
        vec![
            mapping("source/z", "z"),
            mapping("source/a", "a"),
            mapping("source/a-child", "a/child"),
            mapping("source/z-child", "z/child"),
        ],
        Vec::new(),
        Vec::new(),
    )
    .expect_err("输入顺序最早的重叠声明必须失败");
    assert_eq!(
        error,
        DirectoryStageRequestError::OverlappingSourceTargets {
            first: PathBuf::from("z"),
            second: PathBuf::from("z/child"),
        }
    );

    let error = DirectoryStageRequest::new(
        PathBuf::from("out"),
        DirectoryPublishIntent::CreateNew,
        vec![mapping("source/assets", "assets")],
        vec![
            overlay("assets"),
            overlay("scripts/catalog.json"),
            overlay("scripts/catalog.json/child"),
        ],
        Vec::new(),
    )
    .expect_err("较早覆盖的来源目标冲突必须先于较晚覆盖间冲突");
    assert_eq!(
        error,
        DirectoryStageRequestError::OverlayOverlapsSourceTarget {
            overlay: PathBuf::from("assets"),
            source_target: PathBuf::from("assets"),
        }
    );
}

#[cfg(feature = "release-stress")]
#[test]
fn release_stress_stage_request_accepts_many_disjoint_overlays_without_pairwise_scanning() {
    let overlays = (0..10_000)
        .map(|ordinal| overlay(&format!("assets/file-{ordinal:05}.json")))
        .collect();
    let request = DirectoryStageRequest::new(
        PathBuf::from("out"),
        DirectoryPublishIntent::CreateNew,
        vec![mapping("source/assets", "assets")],
        overlays,
        Vec::new(),
    )
    .expect("互不重叠的大型覆盖 manifest 应通过前缀索引一次校验");
    assert_eq!(request.overlays().len(), 10_000);
}
