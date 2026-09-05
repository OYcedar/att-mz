use super::*;

#[test]
fn tree_fingerprint_request_requires_non_overlapping_safe_logical_roots() {
    assert!(matches!(
        DirectoryTreeFingerprintRequest::new(Vec::new()),
        Err(DirectoryTreeFingerprintRequestError::EmptyRoots)
    ));
    for logical in [
        "",
        ".",
        "../assets",
        "assets/../scripts",
        "/assets",
        "C:/assets",
    ] {
        assert!(matches!(
            DirectoryTreeRoot::new(PathBuf::from("physical"), PathBuf::from(logical)),
            Err(DirectoryTreeFingerprintRequestError::InvalidLogicalRoot { .. })
        ));
    }
    assert!(matches!(
        DirectoryTreeFingerprintRequest::new(vec![
            DirectoryTreeRoot::new(PathBuf::from("physical/assets"), PathBuf::from("assets"))
                .expect("资源逻辑根应该合法"),
            DirectoryTreeRoot::new(
                PathBuf::from("physical/catalog"),
                PathBuf::from("assets/catalog"),
            )
            .expect("资源子逻辑根应该合法"),
        ]),
        Err(DirectoryTreeFingerprintRequestError::OverlappingLogicalRoots { .. })
    ));

    let request = DirectoryTreeFingerprintRequest::new(vec![
        DirectoryTreeRoot::new(PathBuf::from("physical/assets"), PathBuf::from("assets"))
            .expect("资源逻辑根应该合法"),
        DirectoryTreeRoot::new(PathBuf::from("physical/scripts"), PathBuf::from("scripts"))
            .expect("脚本逻辑根应该合法"),
    ])
    .expect("资源与脚本逻辑根互不重叠");
    assert_eq!(request.roots().len(), 2);
}
