//! 现行 RPG Maker Markdown 中机器分类 TOML fence 的测试辅助解析。

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ClassifiedExampleKind {
    Valid,
    Invalid,
    Illustrative,
}

impl ClassifiedExampleKind {
    fn parse(line: &str) -> Option<Self> {
        match line.trim() {
            "<!-- att-example: valid -->" => Some(Self::Valid),
            "<!-- att-example: invalid -->" => Some(Self::Invalid),
            "<!-- att-example: illustrative -->" => Some(Self::Illustrative),
            _ => None,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClassifiedTomlFence {
    kind: ClassifiedExampleKind,
    section: String,
    subsection: Option<String>,
    body: String,
    opening_line: usize,
}

impl ClassifiedTomlFence {
    pub(crate) const fn kind(&self) -> ClassifiedExampleKind {
        self.kind
    }

    pub(crate) fn section(&self) -> &str {
        &self.section
    }

    pub(crate) fn subsection(&self) -> Option<&str> {
        self.subsection.as_deref()
    }

    pub(crate) fn body(&self) -> &str {
        &self.body
    }

    pub(crate) const fn opening_line(&self) -> usize {
        self.opening_line
    }
}

pub(crate) fn classified_toml_fences(markdown: &str) -> Vec<ClassifiedTomlFence> {
    let lines = markdown.lines().collect::<Vec<_>>();
    let mut section = String::new();
    let mut subsection = None;
    let mut fences = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let line = lines[index];
        if let Some(heading) = line.strip_prefix("## ") {
            section = heading.to_owned();
            subsection = None;
            index += 1;
            continue;
        }
        if let Some(heading) = line.strip_prefix("### ") {
            subsection = Some(heading.to_owned());
            index += 1;
            continue;
        }
        let Some(kind) = ClassifiedExampleKind::parse(line) else {
            index += 1;
            continue;
        };

        let opening_index = index + 1;
        let opening = lines
            .get(opening_index)
            .unwrap_or_else(|| panic!("第 {} 行的样例标记后缺少 fence", index + 1));
        assert!(
            opening.starts_with("```") && *opening != "```",
            "第 {} 行的样例标记必须紧邻带语言的 fence",
            index + 1
        );
        let language = opening.trim_start_matches('`').trim();
        let body_start = opening_index + 1;
        let mut closing_index = body_start;
        while closing_index < lines.len() && lines[closing_index] != "```" {
            closing_index += 1;
        }
        assert!(
            closing_index < lines.len(),
            "第 {} 行开始的 fence 没有闭合",
            opening_index + 1
        );
        if language == "toml" {
            assert!(!section.is_empty(), "TOML fence 必须位于二级章节内");
            let mut body = lines[body_start..closing_index].join("\n");
            body.push('\n');
            fences.push(ClassifiedTomlFence {
                kind,
                section: section.clone(),
                subsection: subsection.clone(),
                body,
                opening_line: opening_index + 1,
            });
        }
        index = closing_index + 1;
    }

    fences
}
