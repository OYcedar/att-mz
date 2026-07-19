//! 面向上层职责的底层存储契约。
//!
//! 本模块只定义当前已经出现真实消费方的能力，不提供文件系统或 SQLite
//! 的生产实现。

pub(crate) mod file_system;
pub(crate) mod sqlite;
pub(crate) mod sqlite_session;
