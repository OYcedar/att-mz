//! 单次 Lua 执行内共享的成功目录列举缓存。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// 只保存成功目录列举；失败始终由真实能力重新观测。
pub(crate) struct SuccessfulDirectoryListCache<T> {
    state: Mutex<DirectoryListCacheState<T>>,
}

struct DirectoryListCacheState<T> {
    mutation_epoch: u64,
    entries: HashMap<PathBuf, Arc<[T]>>,
}

impl<T> Default for SuccessfulDirectoryListCache<T> {
    fn default() -> Self {
        Self {
            state: Mutex::new(DirectoryListCacheState {
                mutation_epoch: 0,
                entries: HashMap::new(),
            }),
        }
    }
}

impl<T> SuccessfulDirectoryListCache<T> {
    pub(crate) fn lookup(&self, path: &Path) -> (Option<Arc<[T]>>, u64) {
        let state = self.state.lock().expect("Lua 目录列举缓存锁不应中毒");
        (state.entries.get(path).cloned(), state.mutation_epoch)
    }

    pub(crate) fn insert_if_unchanged(
        &self,
        path: PathBuf,
        observed_epoch: u64,
        entries: Vec<T>,
    ) -> Arc<[T]> {
        let entries = Arc::<[T]>::from(entries);
        let mut state = self.state.lock().expect("Lua 目录列举缓存锁不应中毒");
        if state.mutation_epoch == observed_epoch {
            state.entries.insert(path, Arc::clone(&entries));
        }
        entries
    }

    pub(crate) fn invalidate(&self, path: &Path) {
        let mut state = self.state.lock().expect("Lua 目录列举缓存锁不应中毒");
        state.mutation_epoch = state.mutation_epoch.wrapping_add(1);
        state.entries.remove(path);
    }

    pub(crate) fn invalidate_subtree(&self, root: &Path) {
        let mut state = self.state.lock().expect("Lua 目录列举缓存锁不应中毒");
        state.mutation_epoch = state.mutation_epoch.wrapping_add(1);
        state.entries.retain(|path, _| !path.starts_with(root));
    }
}

#[cfg(test)]
mod tests {
    use super::SuccessfulDirectoryListCache;
    use std::path::PathBuf;

    #[test]
    fn invalidation_prevents_an_older_observation_from_reentering_the_cache() {
        let cache = SuccessfulDirectoryListCache::default();
        let path = PathBuf::from("data");
        let (cached, observed_epoch) = cache.lookup(&path);
        assert!(cached.is_none());

        cache.invalidate(&path);
        let returned =
            cache.insert_if_unchanged(path.clone(), observed_epoch, vec!["stale".to_owned()]);
        assert_eq!(returned.as_ref(), ["stale"]);
        let (cached, current_epoch) = cache.lookup(&path);
        assert!(cached.is_none(), "失效之后完成的旧列举不得重新进入缓存");

        cache.insert_if_unchanged(path.clone(), current_epoch, vec!["fresh".to_owned()]);
        let (cached, _) = cache.lookup(&path);
        assert_eq!(
            cached
                .expect("同一失效代内完成的成功列举应进入缓存")
                .as_ref(),
            ["fresh"]
        );
    }
}
