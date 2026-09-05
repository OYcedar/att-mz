use super::*;
use std::path::PathBuf;

#[cfg(feature = "release-stress")]
#[test]
fn release_stress_path_index_drops_deep_declarations_without_recursive_rust_stack_use() {
    let mut deep = PathBuf::new();
    for _ in 0..20_000 {
        deep.push("d");
    }
    let mut descendant = deep.clone();
    descendant.push("file.json");

    let index = RelativePathIndex::from_paths(&[deep.as_path()]);
    assert_eq!(index.first_strict_ancestor(&descendant), Some(0));
    drop(index);
}

#[test]
fn suffix_path_index_matches_pairwise_overlap_and_earliest_ordinal_semantics() {
    let paths = [
        "z",
        "a/first",
        "other/value",
        "a",
        "z/last",
        "other/value",
        "independent",
    ]
    .map(PathBuf::from);
    let path_refs = paths.iter().map(PathBuf::as_path).collect::<Vec<_>>();
    let expected = path_refs
        .iter()
        .enumerate()
        .map(|(first, path)| {
            path_refs
                .iter()
                .enumerate()
                .skip(first + 1)
                .find_map(|(second, other)| {
                    (path == other || path.starts_with(other) || other.starts_with(path))
                        .then_some(second)
                })
        })
        .collect::<Vec<_>>();

    assert_eq!(overlapping_later_paths(&path_refs), expected);
}
