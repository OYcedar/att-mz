//! 候选目标与指纹逻辑根共用的路径重叠索引。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path};

#[derive(Default)]
pub(super) struct RelativePathIndex {
    root: RelativePathIndexNode,
}

#[derive(Default)]
struct RelativePathIndexNode {
    children: HashMap<OsString, Self>,
    terminal_min_ordinal: Option<usize>,
    subtree_min_ordinal: Option<usize>,
}

impl Drop for RelativePathIndex {
    fn drop(&mut self) {
        // 路径深度只受真实文件系统约束；显式排空堆上节点，避免深声明在析构时递归
        // 穿过 HashMap 子节点并耗尽 Rust 调用栈。
        let mut pending = self
            .root
            .children
            .drain()
            .map(|(_, child)| child)
            .collect::<Vec<_>>();
        while let Some(mut node) = pending.pop() {
            pending.extend(node.children.drain().map(|(_, child)| child));
        }
    }
}

impl RelativePathIndex {
    pub(super) fn from_paths(paths: &[&Path]) -> Self {
        let mut index = Self::default();
        for (ordinal, path) in paths.iter().enumerate() {
            index.insert(path, ordinal);
        }
        index
    }

    fn insert(&mut self, path: &Path, ordinal: usize) {
        let mut node = &mut self.root;
        node.subtree_min_ordinal = min_ordinal(node.subtree_min_ordinal, ordinal);
        for component in path.components() {
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            node = node.children.entry(component.to_os_string()).or_default();
            node.subtree_min_ordinal = min_ordinal(node.subtree_min_ordinal, ordinal);
        }
        node.terminal_min_ordinal = min_ordinal(node.terminal_min_ordinal, ordinal);
    }

    /// 返回输入顺序最早、与 `path` 相同或互为祖先的声明。
    pub(super) fn first_overlapping(&self, path: &Path) -> Option<usize> {
        let mut node = &self.root;
        let mut candidate = node.terminal_min_ordinal;
        for component in path.components() {
            candidate = min_optional_ordinal(candidate, node.terminal_min_ordinal);
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            let Some(child) = node.children.get(component) else {
                return candidate;
            };
            node = child;
        }
        min_optional_ordinal(candidate, node.subtree_min_ordinal)
    }

    /// 返回输入顺序最早、且是 `path` 严格祖先的声明。
    pub(super) fn first_strict_ancestor(&self, path: &Path) -> Option<usize> {
        let mut node = &self.root;
        let mut candidate = node.terminal_min_ordinal;
        for component in path.components() {
            candidate = min_optional_ordinal(candidate, node.terminal_min_ordinal);
            let Component::Normal(component) = component else {
                unreachable!("候选相对路径已经过结构校验")
            };
            let Some(child) = node.children.get(component) else {
                return candidate;
            };
            node = child;
        }
        candidate
    }
}

fn min_ordinal(current: Option<usize>, ordinal: usize) -> Option<usize> {
    Some(current.map_or(ordinal, |current| current.min(ordinal)))
}

fn min_optional_ordinal(first: Option<usize>, second: Option<usize>) -> Option<usize> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (Some(ordinal), None) | (None, Some(ordinal)) => Some(ordinal),
        (None, None) => None,
    }
}

/// 为每条路径找到输入顺序更晚、且最早与它重叠的路径。
///
/// 反向建立后缀索引，既保持旧契约“先比较较早输入”的错误选择，又把两两互扫
/// 降为与路径组件总数近似线性的工作量。
pub(super) fn overlapping_later_paths(paths: &[&Path]) -> Vec<Option<usize>> {
    let mut suffix = RelativePathIndex::default();
    let mut overlaps = vec![None; paths.len()];
    for ordinal in (0..paths.len()).rev() {
        overlaps[ordinal] = suffix.first_overlapping(paths[ordinal]);
        suffix.insert(paths[ordinal], ordinal);
    }
    overlaps
}

#[cfg(test)]
mod tests;
