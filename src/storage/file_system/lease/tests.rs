use super::*;

#[test]
fn exclusive_file_lease_request_requires_a_directory_and_identity() {
    assert!(matches!(
        ExclusiveFileLeaseRequest::new(PathBuf::new(), OsString::from("game")),
        Err(ExclusiveFileLeaseRequestError::EmptyLockDirectory)
    ));
    assert!(matches!(
        ExclusiveFileLeaseRequest::new(
            PathBuf::from("C:/workspaces/locks/leases"),
            OsString::new(),
        ),
        Err(ExclusiveFileLeaseRequestError::EmptyIdentity)
    ));

    let request = ExclusiveFileLeaseRequest::new(
        PathBuf::from("C:/workspaces/locks/leases"),
        OsString::from("游戏 一"),
    )
    .expect("Unicode 文件租约身份应该合法");
    assert_eq!(
        request.lock_directory(),
        Path::new("C:/workspaces/locks/leases")
    );
    assert_eq!(request.identity(), OsStr::new("游戏 一"));
}
