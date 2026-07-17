//! RPG Maker MZ 纵向切片。
//!
//! 命令行解析、生产根构造和进程呈现属于 `application`；本模块只拥有 MZ
//! 业务输入、输出与用例实现。

pub mod extract;
pub mod init;
pub(crate) mod location_codec;
pub(crate) mod lua;
pub(crate) mod placeholder_token;
pub(crate) mod project;
mod project_name;
pub(crate) mod standard_asset;
pub(crate) mod tag;
pub(crate) mod text;
pub mod translate;
pub mod write_back;

pub use project::{MaxFullwidthChars, MaxFullwidthCharsError, MzWriteBackLayoutProfile};
pub use project_name::ProjectName;
