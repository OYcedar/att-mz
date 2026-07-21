//! 用户控制文本进入终端或持久化可观测输出前的统一净化边界。

/// 把用户控制文本收敛为单行纯文本。
///
/// 调用方可以在此结果外层增加自身的方向隔离；输入中已有的双向控制符、终端控制符
/// 和换行一律不保留，避免文本伪装成额外终端行或改变日志查看器的显示顺序。
pub(crate) fn sanitize_user_text(value: &str) -> String {
    let mut sanitized = String::with_capacity(value.len());
    let mut previous_was_space = false;
    for character in value.chars() {
        if is_bidi_control(character) {
            continue;
        }
        if matches!(
            character,
            '\r' | '\n' | '\u{0085}' | '\u{2028}' | '\u{2029}'
        ) {
            if !previous_was_space && !sanitized.is_empty() {
                sanitized.push(' ');
                previous_was_space = true;
            }
            continue;
        }
        if character.is_control() {
            continue;
        }
        if character.is_whitespace() && previous_was_space {
            continue;
        }
        sanitized.push(character);
        previous_was_space = character.is_whitespace();
    }
    sanitized
}

const fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'
            | '\u{202b}'
            | '\u{202c}'
            | '\u{202d}'
            | '\u{202e}'
            | '\u{2066}'
            | '\u{2067}'
            | '\u{2068}'
            | '\u{2069}'
    )
}
