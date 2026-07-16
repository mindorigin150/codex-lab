use std::ops::Range;

pub(crate) const MAX_FORMULA_SOURCE_BYTES: usize = 16 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FormulaKind {
    Inline,
    Display,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FormulaMatch<'a> {
    pub source_range: Range<usize>,
    pub body_range: Range<usize>,
    pub body: &'a str,
    pub kind: FormulaKind,
}

/// Finds TeX delimiters in Markdown source while preserving byte offsets into `source`.
///
/// Inline code and fenced code blocks are skipped. Oversized formula bodies are deliberately
/// omitted: formula rendering is an external-content resource boundary.
pub(crate) fn scan_formulas(source: &str) -> Vec<FormulaMatch<'_>> {
    let bytes = source.as_bytes();
    let mut formulas = Vec::new();
    let mut cursor = 0;
    let mut line_start = true;
    let mut fence: Option<(u8, usize)> = None;

    while cursor < bytes.len() {
        if line_start && fence.is_none() && reference_definition_line(bytes, cursor) {
            cursor = line_end(bytes, cursor);
            continue;
        }
        if line_start && fence.is_none() && indented_code_line(bytes, cursor) {
            cursor = line_end(bytes, cursor);
            continue;
        }
        if line_start && let Some((marker, count, end)) = fence_marker(bytes, cursor) {
            if let Some((open_marker, open_count)) = fence {
                if marker == open_marker
                    && count >= open_count
                    && only_space_until_newline(bytes, end)
                {
                    fence = None;
                }
            } else {
                fence = Some((marker, count));
            }
            cursor = end;
            line_start = false;
            continue;
        }

        if bytes[cursor] == b'\n' {
            cursor += 1;
            line_start = true;
            continue;
        }
        line_start = false;
        if fence.is_some() {
            cursor += char_len(source, cursor);
            continue;
        }

        if bytes[cursor] == b'`' {
            let ticks = byte_run(bytes, cursor, b'`');
            cursor = find_backtick_close(bytes, cursor + ticks, ticks).unwrap_or(cursor + ticks);
            continue;
        }

        if bytes[cursor] == b'<' && html_or_autolink_start(bytes, cursor) {
            cursor = bytes[cursor..]
                .iter()
                .position(|byte| *byte == b'>')
                .map_or(cursor + 1, |end| cursor + end + 1);
            continue;
        }

        if bytes[cursor..].starts_with(b"](") {
            cursor = link_destination_end(bytes, cursor + 2);
            continue;
        }
        if bytes[cursor..].starts_with(b"][") {
            cursor = bytes[cursor + 2..]
                .iter()
                .position(|byte| *byte == b']')
                .map_or(cursor + 2, |end| cursor + end + 3);
            continue;
        }

        let delimiter = match delimiter_at(bytes, cursor) {
            Some(delimiter) => delimiter,
            None => {
                cursor += char_len(source, cursor);
                continue;
            }
        };
        let body_start = cursor + delimiter.open_len;
        if delimiter.kind == FormulaKind::Inline
            && bytes.get(body_start).is_none_or(u8::is_ascii_whitespace)
        {
            cursor = body_start;
            continue;
        }

        if let Some(close) = find_formula_close(bytes, body_start, delimiter) {
            let source_end = close + delimiter.close.len();
            if close - body_start <= MAX_FORMULA_SOURCE_BYTES {
                formulas.push(FormulaMatch {
                    source_range: cursor..source_end,
                    body_range: body_start..close,
                    body: &source[body_start..close],
                    kind: delimiter.kind,
                });
            }
            cursor = source_end;
        } else {
            cursor = body_start;
        }
    }

    formulas
}

#[derive(Clone, Copy)]
struct Delimiter {
    open_len: usize,
    close: &'static [u8],
    kind: FormulaKind,
    dollar: bool,
}

fn delimiter_at(bytes: &[u8], cursor: usize) -> Option<Delimiter> {
    if escaped(bytes, cursor) {
        return None;
    }
    if bytes[cursor..].starts_with(b"$$") {
        Some(Delimiter {
            open_len: 2,
            close: b"$$",
            kind: FormulaKind::Display,
            dollar: true,
        })
    } else if bytes[cursor] == b'$' {
        Some(Delimiter {
            open_len: 1,
            close: b"$",
            kind: FormulaKind::Inline,
            dollar: true,
        })
    } else if bytes[cursor..].starts_with(b"\\(") {
        Some(Delimiter {
            open_len: 2,
            close: b"\\)",
            kind: FormulaKind::Inline,
            dollar: false,
        })
    } else if bytes[cursor..].starts_with(b"\\[") {
        Some(Delimiter {
            open_len: 2,
            close: b"\\]",
            kind: FormulaKind::Display,
            dollar: false,
        })
    } else {
        None
    }
}

fn find_formula_close(bytes: &[u8], mut cursor: usize, delimiter: Delimiter) -> Option<usize> {
    while cursor + delimiter.close.len() <= bytes.len() {
        if bytes[cursor..].starts_with(delimiter.close) && !escaped(bytes, cursor) {
            if delimiter.dollar && delimiter.kind == FormulaKind::Inline {
                let previous_is_space = cursor == 0 || bytes[cursor - 1].is_ascii_whitespace();
                let followed_by_dollar = bytes.get(cursor + 1) == Some(&b'$');
                if previous_is_space || followed_by_dollar {
                    cursor += 1;
                    continue;
                }
            }
            return Some(cursor);
        }
        cursor += 1;
    }
    None
}

fn escaped(bytes: &[u8], cursor: usize) -> bool {
    let mut slash_count = 0;
    let mut index = cursor;
    while index > 0 && bytes[index - 1] == b'\\' {
        slash_count += 1;
        index -= 1;
    }
    slash_count % 2 == 1
}

fn byte_run(bytes: &[u8], start: usize, byte: u8) -> usize {
    bytes[start..]
        .iter()
        .take_while(|candidate| **candidate == byte)
        .count()
}

fn find_backtick_close(bytes: &[u8], mut cursor: usize, ticks: usize) -> Option<usize> {
    while cursor < bytes.len() {
        if bytes[cursor] == b'`' {
            let run = byte_run(bytes, cursor, b'`');
            if run == ticks {
                return Some(cursor + run);
            }
            cursor += run;
        } else {
            cursor += 1;
        }
    }
    None
}

fn fence_marker(bytes: &[u8], line_start: usize) -> Option<(u8, usize, usize)> {
    let mut cursor = blockquote_content_start(bytes, line_start);
    let content_start = cursor;
    while cursor < bytes.len() && cursor - content_start < 3 && bytes[cursor] == b' ' {
        cursor += 1;
    }
    let marker = *bytes.get(cursor)?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let count = byte_run(bytes, cursor, marker);
    (count >= 3).then_some((marker, count, cursor + count))
}

fn indented_code_line(bytes: &[u8], line_start: usize) -> bool {
    let mut spaces = 0usize;
    let mut cursor = blockquote_content_start(bytes, line_start);
    while let Some(byte) = bytes.get(cursor) {
        match byte {
            b' ' => {
                spaces += 1;
                cursor += 1;
                if spaces == 4 {
                    return true;
                }
            }
            b'\t' => return true,
            _ => return false,
        }
    }
    false
}

fn blockquote_content_start(bytes: &[u8], line_start: usize) -> usize {
    let mut cursor = line_start;
    loop {
        let prefix_start = cursor;
        let mut marker = cursor;
        while marker < bytes.len() && marker - prefix_start < 3 && bytes[marker] == b' ' {
            marker += 1;
        }
        if bytes.get(marker) != Some(&b'>') {
            return cursor;
        }
        cursor = marker + 1;
        if bytes.get(cursor) == Some(&b' ') {
            cursor += 1;
        }
    }
}

fn reference_definition_line(bytes: &[u8], line_start: usize) -> bool {
    let mut cursor = blockquote_content_start(bytes, line_start);
    let content_start = cursor;
    while cursor < bytes.len() && cursor - content_start < 3 && bytes[cursor] == b' ' {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'[') {
        return false;
    }
    let line_end = line_end(bytes, cursor);
    bytes[cursor..line_end]
        .windows(2)
        .any(|window| window == b"]:")
}

fn html_or_autolink_start(bytes: &[u8], cursor: usize) -> bool {
    bytes
        .get(cursor + 1)
        .is_some_and(|byte| byte.is_ascii_alphabetic() || matches!(byte, b'/' | b'!' | b'?'))
}

fn line_end(bytes: &[u8], cursor: usize) -> usize {
    bytes[cursor..]
        .iter()
        .position(|byte| *byte == b'\n')
        .map_or(bytes.len(), |end| cursor + end)
}

fn link_destination_end(bytes: &[u8], mut cursor: usize) -> usize {
    let mut depth = 1usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\\' => cursor = (cursor + 2).min(bytes.len()),
            b'(' => {
                depth += 1;
                cursor += 1;
            }
            b')' => {
                depth -= 1;
                cursor += 1;
                if depth == 0 {
                    return cursor;
                }
            }
            b'\n' => return cursor,
            _ => cursor += 1,
        }
    }
    cursor
}

fn only_space_until_newline(bytes: &[u8], mut cursor: usize) -> bool {
    while cursor < bytes.len() && bytes[cursor] != b'\n' {
        if !bytes[cursor].is_ascii_whitespace() {
            return false;
        }
        cursor += 1;
    }
    true
}

fn char_len(source: &str, cursor: usize) -> usize {
    let Some(character) = source[cursor..].chars().next() else {
        unreachable!("formula scanner cursor must point inside the source");
    };
    character.len_utf8()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scans_all_delimiters_with_source_offsets() {
        let source = "a $x+1$ b $$y<z$$ c \\(q\\) d \\[r\\]";
        let found = scan_formulas(source);
        assert_eq!(
            found.iter().map(|m| m.body).collect::<Vec<_>>(),
            ["x+1", "y<z", "q", "r"]
        );
        assert_eq!(
            found.iter().map(|m| m.kind).collect::<Vec<_>>(),
            [
                FormulaKind::Inline,
                FormulaKind::Display,
                FormulaKind::Inline,
                FormulaKind::Display
            ]
        );
        for formula in found {
            assert_eq!(&source[formula.body_range.clone()], formula.body);
            assert!(formula.source_range.start < formula.body_range.start);
            assert!(formula.source_range.end > formula.body_range.end);
        }
    }

    #[test]
    fn scans_vscode_replayed_formula_source() {
        let source = "# Fit \\(\\Delta\\)\n\n\\[\n\\Delta =\naction\\_ready\\_ms-worker\\_service\\_ms\n\\]\n\nRuntime: \\(action\\_ready=L_r+R_r+\\Delta\\)";
        let found = scan_formulas(source);

        assert_eq!(
            found.iter().map(|formula| formula.body).collect::<Vec<_>>(),
            [
                "\\Delta",
                "\n\\Delta =\naction\\_ready\\_ms-worker\\_service\\_ms\n",
                "action\\_ready=L_r+R_r+\\Delta"
            ]
        );
        assert_eq!(
            found.iter().map(|formula| formula.kind).collect::<Vec<_>>(),
            [
                FormulaKind::Inline,
                FormulaKind::Display,
                FormulaKind::Inline
            ]
        );
    }

    #[test]
    fn skips_escaped_delimiters_and_code() {
        let source = r#"\$escaped$ `$inline$`
```rust
let x = "$fenced$";
```
real $x$
~~~
$$also_fenced$$
~~~"#;
        let found = scan_formulas(source);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].body, "x");
    }

    #[test]
    fn skips_indented_and_blockquoted_fenced_code() {
        let source =
            "    $indented$\n>     $quoted_indented$\n\n> ```text\n> $quoted$\n> ```\n\nreal $x$";
        let found = scan_formulas(source);
        assert_eq!(
            found.iter().map(|formula| formula.body).collect::<Vec<_>>(),
            ["x"]
        );
    }

    #[test]
    fn skips_link_destinations_and_html_tags() {
        let source = "[label $x$](https://example.test/$link$) [ref][target$skip$]\n[target]: https://example.test/$definition$\n<span title=\"$html$\"> then $y$";
        let found = scan_formulas(source);
        assert_eq!(
            found.iter().map(|formula| formula.body).collect::<Vec<_>>(),
            ["x", "y"]
        );
    }

    #[test]
    fn unmatched_backticks_remain_literal_markdown() {
        let found = scan_formulas("unmatched ` then $x$");
        assert_eq!(found[0].body, "x");
    }

    #[test]
    fn literal_angle_brackets_do_not_hide_math() {
        let found = scan_formulas("a < use $x$ here > b");
        assert_eq!(found[0].body, "x");
    }

    #[test]
    fn does_not_treat_currency_as_math() {
        assert!(scan_formulas("Costs $5 and $10 today").is_empty());
    }

    #[test]
    fn accepts_even_backslash_before_delimiter() {
        let found = scan_formulas(r"\\$x$");
        assert_eq!(found[0].body, "x");
    }

    #[test]
    fn omits_formula_over_source_limit() {
        let source = format!("${}$", "x".repeat(MAX_FORMULA_SOURCE_BYTES + 1));
        assert!(scan_formulas(&source).is_empty());
    }
}
