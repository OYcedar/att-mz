//! RPG Maker 标准 NW.js 启动文档的冻结读取与派生标题同步。

use std::ffi::OsStr;
use std::ops::Range;
use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::value::RawValue;

use crate::fingerprint::{Sha256Fingerprint, Sha256FramedHasher};
use crate::rpg_maker::project_database::SourceSnapshotFingerprint;
use crate::storage::file_system::{ReadFile, ReadFileError, SnapshotFileReader};
use crate::storage::scoped_path::ScopedDirectoryPath;

const PACKAGE_FILE_NAME: &str = "package.json";
const HTML_OPEN_TITLE: &str = "<title>";
const HTML_CLOSE_TITLE: &str = "</title>";

/// 把 data/js 树和可选标准启动壳收敛为当前唯一来源快照身份。
pub(crate) fn source_snapshot_fingerprint(
    data_js_fingerprint: Sha256Fingerprint,
    bootstrap: Option<&RpgMakerBootstrapFiles>,
) -> SourceSnapshotFingerprint {
    let Some(bootstrap) = bootstrap else {
        return SourceSnapshotFingerprint::from_bytes(data_js_fingerprint.into_bytes());
    };
    let mut fingerprint = Sha256FramedHasher::new(b"att.rpg-maker.source-snapshot");
    fingerprint.frame(1, data_js_fingerprint.as_bytes());
    fingerprint.frame(2, b"bootstrap");
    fingerprint.frame(
        3,
        bootstrap
            .main_relative()
            .to_str()
            .expect("package.main 已由 UTF-8 JSON string 建立")
            .as_bytes(),
    );
    fingerprint.frame(4, bootstrap.package_bytes());
    fingerprint.frame(5, bootstrap.main_html_bytes());
    SourceSnapshotFingerprint::from_bytes(fingerprint.finish().into_bytes())
}

/// 已从游戏根读取的标准 NW.js 启动文档。
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpgMakerBootstrapFiles {
    main_relative: PathBuf,
    package_bytes: Vec<u8>,
    html_bytes: Vec<u8>,
}

impl RpgMakerBootstrapFiles {
    /// 保留活动 main 身份，以确定的 package 与 HTML 字节建立候选启动壳。
    pub(crate) fn with_document_bytes(&self, package_bytes: Vec<u8>, html_bytes: Vec<u8>) -> Self {
        Self {
            main_relative: self.main_relative.clone(),
            package_bytes,
            html_bytes,
        }
    }

    /// 返回 `package.main` 指向的游戏根相对 HTML 路径。
    pub(crate) fn main_relative(&self) -> &Path {
        &self.main_relative
    }

    /// 返回根 `package.json` 的冻结原始字节。
    pub(crate) fn package_bytes(&self) -> &[u8] {
        &self.package_bytes
    }

    /// 返回活动 `package.main` HTML 的冻结原始字节。
    pub(crate) fn main_html_bytes(&self) -> &[u8] {
        &self.html_bytes
    }

    /// 取得启动文档所有权，供候选目录按原路径写入。
    #[cfg(test)]
    pub(crate) fn into_parts(self) -> (PathBuf, Vec<u8>, Vec<u8>) {
        (self.main_relative, self.package_bytes, self.html_bytes)
    }

    /// 当 `window.title` 仍逐值等于原始游戏标题时，返回只替换该 JSON string token 的字节。
    pub(crate) fn rewritten_package_title(
        &self,
        original_game_title: &str,
        translated_game_title: &str,
    ) -> Option<Vec<u8>> {
        rewrite_package_window_title(
            &self.package_bytes,
            original_game_title,
            translated_game_title,
        )
    }

    /// 当唯一标准 `<title>` 内容仍逐字等于原始游戏标题时，返回只替换内容的字节。
    pub(crate) fn rewritten_html_title(
        &self,
        original_game_title: &str,
        translated_game_title: &str,
    ) -> Option<Vec<u8>> {
        rewrite_html_title(&self.html_bytes, original_game_title, translated_game_title)
    }
}

/// 读取游戏根的可选标准 NW.js 启动文档。
///
/// 标准启动文档由根 `package.json`、其中安全的相对 `.html` `main` 路径及对应普通文件组成。
/// 缺失或不符合该结构时返回 `None`；真实文件系统 I/O 失败保留原始错误。
pub(crate) async fn read_optional_bootstrap_files<F>(
    file_reader: &F,
    game_root: &Path,
) -> Result<Option<RpgMakerBootstrapFiles>, ReadFileError<F::Error>>
where
    F: SnapshotFileReader,
{
    let Some(package) = read_optional_file(file_reader, game_root.join(PACKAGE_FILE_NAME)).await?
    else {
        return Ok(None);
    };
    let package_bytes = package.into_bytes();
    let Some(main_relative) = package_main_relative(&package_bytes) else {
        return Ok(None);
    };
    let Some(html) = read_optional_file(file_reader, game_root.join(&main_relative)).await? else {
        return Ok(None);
    };

    Ok(Some(RpgMakerBootstrapFiles {
        main_relative,
        package_bytes,
        html_bytes: html.into_bytes(),
    }))
}

async fn read_optional_file<F>(
    file_reader: &F,
    path: PathBuf,
) -> Result<Option<ReadFile>, ReadFileError<F::Error>>
where
    F: SnapshotFileReader,
{
    match file_reader.read_snapshot_file(path).await {
        Ok(file) => Ok(Some(file)),
        Err(ReadFileError::NotFound { .. } | ReadFileError::NotFile { .. }) => Ok(None),
        Err(error @ ReadFileError::Io { .. }) => Err(error),
    }
}

#[derive(Deserialize)]
struct PackageFields<'a> {
    #[serde(borrow)]
    main: Option<&'a RawValue>,
    #[serde(borrow)]
    window: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct WindowFields<'a> {
    #[serde(borrow)]
    title: Option<&'a RawValue>,
}

fn package_main_relative(package_bytes: &[u8]) -> Option<PathBuf> {
    let package = std::str::from_utf8(package_bytes).ok()?;
    let fields: PackageFields<'_> = serde_json::from_str(package).ok()?;
    let main = decode_json_string(fields.main?)?;
    let main = ScopedDirectoryPath::new(PathBuf::from(main)).ok()?;
    if main.as_path().extension() != Some(OsStr::new("html")) {
        return None;
    }
    Some(main.as_path().to_path_buf())
}

fn rewrite_package_window_title(
    package_bytes: &[u8],
    original_game_title: &str,
    translated_game_title: &str,
) -> Option<Vec<u8>> {
    if original_game_title.is_empty() || translated_game_title == original_game_title {
        return None;
    }
    let package = std::str::from_utf8(package_bytes).ok()?;
    let package_fields: PackageFields<'_> = serde_json::from_str(package).ok()?;
    let window: WindowFields<'_> = serde_json::from_str(package_fields.window?.get()).ok()?;
    let title = window.title?;
    if decode_json_string(title)?.as_str() != original_game_title {
        return None;
    }

    let span = borrowed_raw_span(package, title)?;
    let replacement = serde_json::to_vec(translated_game_title).ok()?;
    Some(replace_bytes(package_bytes, span, &replacement))
}

fn decode_json_string(raw: &RawValue) -> Option<String> {
    serde_json::from_str(raw.get()).ok()
}

fn borrowed_raw_span(document: &str, raw: &RawValue) -> Option<Range<usize>> {
    let document_start = document.as_ptr() as usize;
    let raw_start = raw.get().as_ptr() as usize;
    let start = raw_start.checked_sub(document_start)?;
    let end = start.checked_add(raw.get().len())?;
    if document.as_bytes().get(start..end)? != raw.get().as_bytes() {
        return None;
    }
    Some(start..end)
}

fn rewrite_html_title(
    html_bytes: &[u8],
    original_game_title: &str,
    translated_game_title: &str,
) -> Option<Vec<u8>> {
    if original_game_title.is_empty() || translated_game_title == original_game_title {
        return None;
    }
    let html = std::str::from_utf8(html_bytes).ok()?;
    let span = unique_html_title_content_span(html)?;
    if html.get(span.clone())? != original_game_title {
        return None;
    }
    let replacement = escape_html_text(translated_game_title);
    Some(replace_bytes(html_bytes, span, replacement.as_bytes()))
}

fn unique_html_title_content_span(html: &str) -> Option<Range<usize>> {
    let bytes = html.as_bytes();
    let mut cursor = 0;
    let mut title_starts = 0_usize;
    let mut standard_title = None;
    while let Some(start) = find_next_html_open(bytes, cursor) {
        if bytes[start..].starts_with(b"<!--") {
            let Some(end) = find_html_bytes(bytes, start + 4, b"-->") else {
                break;
            };
            cursor = end + 3;
            continue;
        }
        let tag = match parse_html_tag(bytes, start) {
            HtmlTagParse::Tag(tag) => tag,
            HtmlTagParse::NotTag => {
                cursor = start + 1;
                continue;
            }
            HtmlTagParse::Unterminated => break,
        };
        if !tag.closing && tag.name_is(bytes, b"plaintext") {
            break;
        }
        if !tag.closing && tag.name_in(bytes, HTML_RAW_TEXT_ELEMENTS) {
            let name = &bytes[tag.name_start..tag.name_end];
            cursor = find_html_end_tag(bytes, tag.end, name).map_or(bytes.len(), |end| end.end);
            continue;
        }
        if !tag.closing
            && (tag.name_is(bytes, b"template")
                || (!tag.self_closing && tag.name_in(bytes, HTML_FOREIGN_ELEMENTS)))
        {
            cursor = skip_html_inert_element(bytes, tag);
            continue;
        }
        if !tag.closing && tag.name_is(bytes, b"title") {
            title_starts = title_starts.checked_add(1)?;
            if tag.self_closing {
                cursor = tag.end;
                continue;
            }
            let Some(closing) = find_html_end_tag(bytes, tag.end, b"title") else {
                break;
            };
            if &bytes[tag.start..tag.end] == HTML_OPEN_TITLE.as_bytes()
                && &bytes[closing.start..closing.end] == HTML_CLOSE_TITLE.as_bytes()
            {
                standard_title = Some(tag.end..closing.start);
            }
            cursor = closing.end;
            continue;
        }
        cursor = tag.end;
    }
    (title_starts == 1).then_some(standard_title).flatten()
}

#[derive(Clone, Copy)]
struct HtmlTag {
    start: usize,
    end: usize,
    name_start: usize,
    name_end: usize,
    closing: bool,
    self_closing: bool,
}

enum HtmlTagParse {
    NotTag,
    Unterminated,
    Tag(HtmlTag),
}

impl HtmlTag {
    fn name_is(self, html: &[u8], expected: &[u8]) -> bool {
        html[self.name_start..self.name_end].eq_ignore_ascii_case(expected)
    }

    fn name_in(self, html: &[u8], expected: &[&[u8]]) -> bool {
        expected.iter().any(|name| self.name_is(html, name))
    }
}

const HTML_RAW_TEXT_ELEMENTS: &[&[u8]] = &[
    b"script",
    b"style",
    b"textarea",
    b"xmp",
    b"iframe",
    b"noembed",
    b"noframes",
    b"noscript",
];
const HTML_FOREIGN_ELEMENTS: &[&[u8]] = &[b"svg", b"math"];

fn find_next_html_open(html: &[u8], start: usize) -> Option<usize> {
    html.get(start..)?
        .iter()
        .position(|byte| *byte == b'<')
        .map(|offset| start + offset)
}

fn find_html_bytes(html: &[u8], start: usize, expected: &[u8]) -> Option<usize> {
    html.get(start..)?
        .windows(expected.len())
        .position(|candidate| candidate == expected)
        .map(|offset| start + offset)
}

fn parse_html_tag(html: &[u8], start: usize) -> HtmlTagParse {
    if html.get(start) != Some(&b'<') {
        return HtmlTagParse::NotTag;
    }
    let Some(mut cursor) = start.checked_add(1) else {
        return HtmlTagParse::NotTag;
    };
    let closing = html.get(cursor) == Some(&b'/');
    cursor += usize::from(closing);
    let name_start = cursor;
    while html.get(cursor).is_some_and(|byte| html_name_byte(*byte)) {
        cursor += 1;
    }
    if cursor == name_start {
        return HtmlTagParse::NotTag;
    }
    let name_end = cursor;
    let Some(end) = html_tag_end(html, cursor) else {
        return HtmlTagParse::Unterminated;
    };
    let mut suffix = end - 1;
    while suffix > name_end && html[suffix - 1].is_ascii_whitespace() {
        suffix -= 1;
    }
    HtmlTagParse::Tag(HtmlTag {
        start,
        end,
        name_start,
        name_end,
        closing,
        self_closing: !closing && suffix > name_end && html[suffix - 1] == b'/',
    })
}

fn html_name_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b':')
}

fn html_tag_end(html: &[u8], mut cursor: usize) -> Option<usize> {
    let mut quote = None;
    while let Some(byte) = html.get(cursor).copied() {
        match (quote, byte) {
            (Some(expected), actual) if actual == expected => quote = None,
            (None, b'\'' | b'"') => quote = Some(byte),
            (None, b'>') => return cursor.checked_add(1),
            _ => {}
        }
        cursor += 1;
    }
    None
}

fn find_html_end_tag(html: &[u8], mut cursor: usize, name: &[u8]) -> Option<HtmlTag> {
    while let Some(start) = find_next_html_open(html, cursor) {
        match parse_html_tag(html, start) {
            HtmlTagParse::Tag(tag) if tag.closing && tag.name_is(html, name) => {
                return Some(tag);
            }
            HtmlTagParse::Unterminated => return None,
            HtmlTagParse::NotTag | HtmlTagParse::Tag(_) => {}
        }
        cursor = start + 1;
    }
    None
}

fn skip_html_inert_element(html: &[u8], opening: HtmlTag) -> usize {
    let mut stack = Vec::new();
    stack.push(opening.name_start..opening.name_end);
    let mut cursor = opening.end;
    while let Some(start) = find_next_html_open(html, cursor) {
        if html[start..].starts_with(b"<!--") {
            let Some(end) = find_html_bytes(html, start + 4, b"-->") else {
                return html.len();
            };
            cursor = end + 3;
            continue;
        }
        let tag = match parse_html_tag(html, start) {
            HtmlTagParse::Tag(tag) => tag,
            HtmlTagParse::NotTag => {
                cursor = start + 1;
                continue;
            }
            HtmlTagParse::Unterminated => return html.len(),
        };
        if !tag.closing && tag.name_is(html, b"plaintext") {
            return html.len();
        }
        if !tag.closing && tag.name_in(html, HTML_RAW_TEXT_ELEMENTS) {
            let name = &html[tag.name_start..tag.name_end];
            cursor = find_html_end_tag(html, tag.end, name).map_or(html.len(), |end| end.end);
            continue;
        }
        if !tag.closing
            && (tag.name_is(html, b"template")
                || (!tag.self_closing && tag.name_in(html, HTML_FOREIGN_ELEMENTS)))
        {
            stack.push(tag.name_start..tag.name_end);
            cursor = tag.end;
            continue;
        }
        if tag.closing
            && stack.last().is_some_and(|name| {
                html[name.clone()].eq_ignore_ascii_case(&html[tag.name_start..tag.name_end])
            })
        {
            stack.pop();
            if stack.is_empty() {
                return tag.end;
            }
        }
        cursor = tag.end;
    }
    html.len()
}

fn escape_html_text(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&#39;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn replace_bytes(source: &[u8], span: Range<usize>, replacement: &[u8]) -> Vec<u8> {
    let mut rewritten = Vec::with_capacity(source.len() - span.len() + replacement.len());
    rewritten.extend_from_slice(&source[..span.start]);
    rewritten.extend_from_slice(replacement);
    rewritten.extend_from_slice(&source[span.end..]);
    rewritten
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::error::Error;
    use std::fmt;
    use std::fs;

    use super::*;
    use crate::diagnostic::FileSystemPathViolation;
    use crate::runtime::filesystem::{
        SystemFileSystem, SystemFileSystemConfig, SystemFileSystemError,
    };

    #[derive(Clone)]
    enum FakeEntry {
        File(Vec<u8>),
        NotFile,
        Io,
    }

    #[derive(Clone, Default)]
    struct FakeFileReader {
        entries: BTreeMap<PathBuf, FakeEntry>,
    }

    impl FakeFileReader {
        fn with(mut self, path: impl Into<PathBuf>, entry: FakeEntry) -> Self {
            self.entries.insert(path.into(), entry);
            self
        }
    }

    impl crate::storage::file_system::FileReader for FakeFileReader {
        type Error = FakeIoError;

        async fn read_file(&self, path: PathBuf) -> Result<ReadFile, ReadFileError<Self::Error>> {
            match self.entries.get(&path) {
                Some(FakeEntry::File(bytes)) => Ok(ReadFile::new(path, bytes.clone())),
                Some(FakeEntry::NotFile) => Err(ReadFileError::NotFile { path }),
                Some(FakeEntry::Io) => Err(ReadFileError::Io {
                    path,
                    source: FakeIoError,
                }),
                None => Err(ReadFileError::NotFound { path }),
            }
        }
    }

    impl SnapshotFileReader for FakeFileReader {
        async fn read_snapshot_file(
            &self,
            path: PathBuf,
        ) -> Result<ReadFile, ReadFileError<Self::Error>> {
            <Self as crate::storage::file_system::FileReader>::read_file(self, path).await
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct FakeIoError;

    impl fmt::Display for FakeIoError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("read failed")
        }
    }

    impl Error for FakeIoError {}

    #[tokio::test]
    async fn reads_standard_bootstrap_and_preserves_both_documents() {
        let root = Path::new("C:/game");
        let package = r#"{
  "name": "sample",
  "main": "www/index.html",
  "window": { "title": "原题" }
}"#
        .as_bytes();
        let html = b"<!doctype html>\r\n<title>\xe5\x8e\x9f\xe9\xa2\x98</title>\r\n";
        let reader = FakeFileReader::default()
            .with(
                root.join(PACKAGE_FILE_NAME),
                FakeEntry::File(package.to_vec()),
            )
            .with(root.join("www/index.html"), FakeEntry::File(html.to_vec()));

        let bootstrap = read_optional_bootstrap_files(&reader, root)
            .await
            .expect("标准 bootstrap 读取不得失败")
            .expect("标准 bootstrap 应存在");

        assert_eq!(bootstrap.main_relative(), Path::new("www/index.html"));
        assert_eq!(bootstrap.package_bytes(), package);
        assert_eq!(bootstrap.main_html_bytes(), html);
        assert_eq!(
            bootstrap.clone().into_parts(),
            (
                PathBuf::from("www/index.html"),
                package.to_vec(),
                html.to_vec()
            )
        );
    }

    #[test]
    fn main_accepts_only_safe_relative_html_paths() {
        for main in ["index.html", "www/index.html", "界面/入口.html"] {
            let package = format!(r#"{{"main":{}}}"#, serde_json::to_string(main).unwrap());
            assert_eq!(
                package_main_relative(package.as_bytes()),
                Some(PathBuf::from(main)),
                "{main:?}"
            );
        }
        for main in [
            "",
            "/index.html",
            "../index.html",
            "www/../index.html",
            r"www\index.html",
            "C:/index.html",
            "index.htm",
            "index.html?debug=1",
        ] {
            let package = format!(r#"{{"main":{}}}"#, serde_json::to_string(main).unwrap());
            assert_eq!(package_main_relative(package.as_bytes()), None, "{main:?}");
        }
        assert_eq!(package_main_relative(b"not json"), None);
        assert_eq!(package_main_relative(br#"{"main":42}"#), None);
    }

    #[tokio::test]
    async fn missing_non_file_and_invalid_documents_are_absent() {
        let root = Path::new("C:/game");
        let package_path = root.join(PACKAGE_FILE_NAME);
        let valid_package = br#"{"main":"index.html"}"#;

        let readers = [
            FakeFileReader::default(),
            FakeFileReader::default().with(package_path.clone(), FakeEntry::NotFile),
            FakeFileReader::default()
                .with(package_path.clone(), FakeEntry::File(b"not json".to_vec())),
            FakeFileReader::default().with(
                package_path.clone(),
                FakeEntry::File(br#"{"main":"../index.html"}"#.to_vec()),
            ),
            FakeFileReader::default().with(
                package_path.clone(),
                FakeEntry::File(valid_package.to_vec()),
            ),
            FakeFileReader::default()
                .with(package_path, FakeEntry::File(valid_package.to_vec()))
                .with(root.join("index.html"), FakeEntry::NotFile),
        ];

        for reader in readers {
            assert_eq!(
                read_optional_bootstrap_files(&reader, root)
                    .await
                    .expect("非标准 bootstrap 应按缺席处理"),
                None
            );
        }
    }

    #[tokio::test]
    async fn package_and_html_io_failures_are_preserved() {
        let root = Path::new("C:/game");
        let package_path = root.join(PACKAGE_FILE_NAME);
        let package_error = read_optional_bootstrap_files(
            &FakeFileReader::default().with(package_path.clone(), FakeEntry::Io),
            root,
        )
        .await
        .expect_err("package I/O 失败必须上抛");
        assert!(matches!(
            package_error,
            ReadFileError::Io { path, source: FakeIoError } if path == package_path
        ));

        let html_path = root.join("index.html");
        let html_error = read_optional_bootstrap_files(
            &FakeFileReader::default()
                .with(
                    root.join(PACKAGE_FILE_NAME),
                    FakeEntry::File(br#"{"main":"index.html"}"#.to_vec()),
                )
                .with(html_path.clone(), FakeEntry::Io),
            root,
        )
        .await
        .expect_err("HTML I/O 失败必须上抛");
        assert!(matches!(
            html_error,
            ReadFileError::Io { path, source: FakeIoError } if path == html_path
        ));
    }

    #[tokio::test]
    async fn system_snapshot_reader_rejects_hardlinked_package_and_active_html() {
        let temporary = tempfile::tempdir().expect("应建立真实启动壳测试目录");
        let package_linked_root = temporary.path().join("package-linked");
        fs::create_dir(&package_linked_root).expect("应建立 package 硬链接游戏根");
        let package_origin = package_linked_root.join("package-origin.json");
        let package_path = package_linked_root.join(PACKAGE_FILE_NAME);
        fs::write(&package_origin, br#"{"main":"index.html"}"#).expect("应建立 package 硬链接来源");
        fs::hard_link(&package_origin, &package_path).expect("本地 NTFS 应支持 package 硬链接");

        let html_linked_root = temporary.path().join("html-linked");
        fs::create_dir(&html_linked_root).expect("应建立 HTML 硬链接游戏根");
        fs::write(
            html_linked_root.join(PACKAGE_FILE_NAME),
            br#"{"main":"index.html"}"#,
        )
        .expect("应建立普通 package");
        let html_origin = html_linked_root.join("index-origin.html");
        let html_path = html_linked_root.join("index.html");
        fs::write(&html_origin, b"<title>source</title>").expect("应建立 HTML 硬链接来源");
        fs::hard_link(&html_origin, &html_path).expect("本地 NTFS 应支持 HTML 硬链接");

        let file_system = SystemFileSystem::new(SystemFileSystemConfig::production())
            .expect("应建立真实文件系统根");
        for (root, rejected) in [
            (package_linked_root.as_path(), package_path.as_path()),
            (html_linked_root.as_path(), html_path.as_path()),
        ] {
            let error = read_optional_bootstrap_files(&file_system, root)
                .await
                .expect_err("冻结启动壳必须拒绝硬链接文件");
            assert!(matches!(
                error,
                ReadFileError::Io {
                    path,
                    source: SystemFileSystemError::InvalidPath {
                        violation: FileSystemPathViolation::HardLink,
                        ..
                    },
                } if path == rejected
            ));
        }
        file_system.shutdown().await.expect("文件系统根应干净终结");
    }

    #[test]
    fn package_title_rewrite_changes_only_the_borrowed_string_token() {
        let package =
            r#"{"main":"index.html","window" : {"title" : "原\u9898", "width":816},"title":"原题"}"#
                .as_bytes();
        let bootstrap = RpgMakerBootstrapFiles {
            main_relative: PathBuf::from("index.html"),
            package_bytes: package.to_vec(),
            html_bytes: Vec::new(),
        };

        assert_eq!(
            bootstrap.rewritten_package_title("原题", "译\"题"),
            Some(
                r#"{"main":"index.html","window" : {"title" : "译\"题", "width":816},"title":"原题"}"#
                    .as_bytes()
                    .to_vec()
            )
        );
    }

    #[test]
    fn html_title_rewrite_requires_one_standard_equal_title_and_escapes_translation() {
        let bootstrap = RpgMakerBootstrapFiles {
            main_relative: PathBuf::from("index.html"),
            package_bytes: Vec::new(),
            html_bytes: b"<head>\r\n<title>\xe5\x8e\x9f\xe9\xa2\x98</title>\r\n</head>".to_vec(),
        };

        assert_eq!(
            bootstrap.rewritten_html_title("原题", "译<&\"'>题"),
            Some(
                "<head>\r\n<title>译&lt;&amp;&quot;&#39;&gt;题</title>\r\n</head>"
                    .as_bytes()
                    .to_vec()
            )
        );
        for html in [
            "<title>原题</title><title>原题</title>",
            "<TITLE>原题</TITLE>",
            "<title class=\"game\">原题</title>",
            "<title>其他题</title>",
        ] {
            assert_eq!(rewrite_html_title(html.as_bytes(), "原题", "译题"), None);
        }
    }

    #[test]
    fn html_title_rewrite_uses_the_actual_element_and_ignores_raw_text_decoys() {
        let html = concat!(
            "<head>\n",
            "<!-- <title>原题</title> -->\n",
            "<script>const decoy = '<title>原题</title>';</script>\n",
            "<style>.decoy::after { content: '<title>原题</title>'; }</style>\n",
            "<title>原题</title>\n",
            "</head>\n",
        );
        assert_eq!(
            rewrite_html_title(html.as_bytes(), "原题", "译题"),
            Some(
                html.replacen(
                    "<title>原题</title>\n</head>",
                    "<title>译题</title>\n</head>",
                    1,
                )
                .into_bytes(),
            )
        );
        for decoys in [
            "<!-- <title>原题</title> -->",
            "<script>const decoy = '<title>原题</title>';</script>",
            "<style>.decoy { content: '<title>原题</title>'; }</style>",
            "<div data=\"<title>原题</title>",
        ] {
            assert_eq!(
                rewrite_html_title(decoys.as_bytes(), "原题", "译题"),
                None,
                "原始文本中的诱饵不能成为启动标题"
            );
        }
    }

    #[test]
    fn html_title_rewrite_ignores_inert_and_foreign_title_decoys() {
        let html = concat!(
            "<textarea><title>原题</title></textarea>\n",
            "<template><title>原题</title></template>\n",
            "<svg><title>原题</title></svg>\n",
            "<math><title>原题</title></math>\n",
            "<title>原题</title>\n",
        );
        assert_eq!(
            rewrite_html_title(html.as_bytes(), "原题", "译题"),
            Some(
                concat!(
                    "<textarea><title>原题</title></textarea>\n",
                    "<template><title>原题</title></template>\n",
                    "<svg><title>原题</title></svg>\n",
                    "<math><title>原题</title></math>\n",
                    "<title>译题</title>\n",
                )
                .as_bytes()
                .to_vec()
            )
        );

        for decoy in [
            "<textarea><title>原题</title></textarea>",
            "<textarea/><title>原题</title>",
            "<template><title>原题</title></template>",
            "<template><template/><title>原题</title></template><title>原题</title></template>",
            "<svg><title>原题</title></svg>",
            "<math><title>原题</title></math>",
        ] {
            assert_eq!(rewrite_html_title(decoy.as_bytes(), "原题", "译题"), None);
        }
    }

    #[test]
    fn empty_and_different_consumers_remain_unchanged() {
        for package in [
            br#"{"window":{"title":""}}"#.as_slice(),
            r#"{"window":{"title":"自定义题"}}"#.as_bytes(),
        ] {
            assert_eq!(rewrite_package_window_title(package, "原题", "译题"), None);
        }
        for html in ["<title></title>", "<title>自定义题</title>"] {
            assert_eq!(rewrite_html_title(html.as_bytes(), "原题", "译题"), None);
        }
        assert_eq!(
            rewrite_package_window_title(br#"{"window":{"title":""}}"#, "", "译题"),
            None
        );
        assert_eq!(rewrite_html_title(b"<title></title>", "", "译题"), None);
    }

    #[test]
    fn source_snapshot_identity_includes_the_frozen_bootstrap_documents() {
        let tree = Sha256Fingerprint::from_bytes([0x5a; 32]);
        assert_eq!(
            source_snapshot_fingerprint(tree, None),
            SourceSnapshotFingerprint::from_bytes([0x5a; 32])
        );
        let bootstrap = RpgMakerBootstrapFiles {
            main_relative: PathBuf::from("index.html"),
            package_bytes: br#"{"main":"index.html"}"#.to_vec(),
            html_bytes: b"<title>demo</title>".to_vec(),
        };
        let baseline = source_snapshot_fingerprint(tree, Some(&bootstrap));
        for changed in [
            RpgMakerBootstrapFiles {
                main_relative: PathBuf::from("start.html"),
                ..bootstrap.clone()
            },
            RpgMakerBootstrapFiles {
                package_bytes: br#"{"main":"index.html","name":"changed"}"#.to_vec(),
                ..bootstrap.clone()
            },
            RpgMakerBootstrapFiles {
                html_bytes: b"<title>changed</title>".to_vec(),
                ..bootstrap.clone()
            },
        ] {
            assert_ne!(source_snapshot_fingerprint(tree, Some(&changed)), baseline);
        }
    }
}
