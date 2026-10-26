#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use arboard::Clipboard;

use std::borrow::Cow;
use std::collections::HashMap;

#[derive(Clone, Copy, PartialEq, Eq)]
enum MathKind {
    Block,
    Inline,
}

impl MathKind {
    fn opening(self) -> &'static str {
        match self {
            Self::Block => "$$",
            Self::Inline => "$",
        }
    }
}

const CODE: u8 = 1;
const BOUNDARY: u8 = 2;

// 围栏和缩进代码同时作为公式配对的边界。
fn mask_block_code(text: &str, mask: &mut [u8]) {
    let mut offset = 0;
    let mut fence = None;
    let mut in_paragraph = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start_matches(' ');
        let indent = line.len() - trimmed.len();
        let bytes = trimmed.as_bytes();
        let marker = bytes.first().copied().unwrap_or(0);
        let count = bytes.iter().take_while(|&&b| b == marker).count();
        let was_fenced = fence.is_some();
        if indent <= 3 && matches!(marker, b'`' | b'~') && count >= 3 {
            match fence {
                Some((open_marker, open_count))
                    if marker == open_marker
                        && count >= open_count
                        && trimmed[count..].trim().is_empty() =>
                {
                    fence = None;
                }
                None if marker == b'~' || !trimmed[count..].contains('`') => {
                    fence = Some((marker, count));
                }
                _ => {}
            }
        }
        // 缩进代码不能打断已有段落，尤其不能把公式内缩进的行误认成代码。
        let indented_code = !in_paragraph && (indent >= 4 || line.starts_with('\t'));
        if was_fenced || fence.is_some() || indented_code {
            mask[offset..offset + line.len()].fill(BOUNDARY);
            in_paragraph = false;
        } else {
            in_paragraph = !line.trim().is_empty();
        }
        offset += line.len();
    }
}

// 预先记录各反引号串的下一个等长串，避免未配对标记反复搜索全文。
fn mask_inline_code(text: &str, mask: &mut [u8]) {
    struct Run {
        start: usize,
        end: usize,
        segment: usize,
        escaped: bool,
        next: Option<usize>,
    }
    let bytes = text.as_bytes();
    let mut runs = Vec::<Run>::new();
    let mut segment = 0;
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if line.trim().is_empty() || mask[offset..offset + line.len()].contains(&BOUNDARY) {
            segment += 1;
            offset += line.len();
            continue;
        }
        let mut i = offset;
        let end = offset + line.len();
        let mut slashes = 0;
        while i < end {
            if bytes[i] == b'`' {
                let start = i;
                while i < end && bytes[i] == b'`' {
                    i += 1;
                }
                runs.push(Run {
                    start,
                    end: i,
                    segment,
                    escaped: slashes % 2 == 1,
                    next: None,
                });
                slashes = 0;
            } else {
                slashes = if bytes[i] == b'\\' { slashes + 1 } else { 0 };
                i += 1;
            }
        }
        offset = end;
    }
    let mut next_by_length = HashMap::new();
    let mut previous_segment = None;
    for index in (0..runs.len()).rev() {
        let run = &mut runs[index];
        if previous_segment != Some(run.segment) {
            next_by_length.clear();
            previous_segment = Some(run.segment);
        }
        run.next = next_by_length.insert(run.end - run.start, index);
    }
    let mut index = 0;
    while index < runs.len() {
        let run = &runs[index];
        if !run.escaped
            && let Some(next) = run.next
        {
            mask[run.start..runs[next].end].fill(CODE);
            index = next + 1;
        } else {
            index += 1;
        }
    }
}

// 美元行内公式不跨行，且首尾不能有空白；转义美元与未配对美元保持原样。
fn mask_dollar_math(text: &str, mask: &mut [u8]) {
    let bytes = text.as_bytes();
    let mut open: Option<(usize, usize)> = None;
    let mut i = 0;
    while i < bytes.len() {
        if mask[i] != 0 {
            open = None;
            i += 1;
            continue;
        }
        if matches!(bytes[i], b'\r' | b'\n') && open.is_some_and(|(_, width)| width == 1) {
            open = None;
        }
        if bytes[i] == b'\\' {
            // 一次跳过被转义的 ASCII 字符，包括美元和另一个反斜杠。
            i += 2;
            continue;
        }
        if bytes[i] != b'$' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && mask[i] == 0 && bytes[i] == b'$' {
            i += 1;
        }
        let width = i - start;
        if width > 2 {
            continue;
        }
        if let Some((opening, open_width)) = open {
            let can_close = width == 2
                || (!bytes[start - 1].is_ascii_whitespace()
                    && !bytes.get(i).is_some_and(u8::is_ascii_digit));
            if width == open_width && can_close {
                mask[opening..i].fill(BOUNDARY);
                open = None;
            }
        } else if width == 2 || bytes.get(i).is_some_and(|b| !b.is_ascii_whitespace()) {
            open = Some((start, width));
        }
    }
}

fn convert(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    if !bytes
        .windows(2)
        .any(|pair| pair[0] == b'\\' && matches!(pair[1], b'[' | b'('))
    {
        return Cow::Borrowed(text);
    }
    let mut mask = vec![0; bytes.len()];
    mask_block_code(text, &mut mask);
    mask_inline_code(text, &mut mask);
    mask_dollar_math(text, &mut mask);

    // 所有开始标记按出现顺序记录。未闭合外层不会阻止有效内层被转换。
    struct Pair {
        start: usize,
        kind: MathKind,
        end: Option<usize>,
    }
    let mut pairs = Vec::<Pair>::new();
    let mut stack = Vec::<usize>::new();
    let mut first_inline = None;
    let mut i = 0;
    while i < bytes.len() {
        if matches!(bytes[i], b'\r' | b'\n')
            && let Some(depth) = first_inline.take()
        {
            stack.truncate(depth);
        }
        if mask[i] != 0 {
            if mask[i] == BOUNDARY {
                stack.clear();
                first_inline = None;
            }
            i += 1;
            continue;
        }
        if bytes[i] != b'\\' || i + 1 >= bytes.len() || mask[i + 1] != 0 {
            i += 1;
            continue;
        }
        // 两个反斜杠互相转义，无需向前回溯。
        if bytes[i + 1] == b'\\' {
            i += 2;
            continue;
        }
        let (kind, opening) = match bytes[i + 1] {
            b'[' => (MathKind::Block, true),
            b']' => (MathKind::Block, false),
            b'(' => (MathKind::Inline, true),
            b')' => (MathKind::Inline, false),
            _ => {
                i += 1;
                continue;
            }
        };
        if opening {
            if kind == MathKind::Inline && first_inline.is_none() {
                first_inline = Some(stack.len());
            }
            stack.push(pairs.len());
            pairs.push(Pair {
                start: i,
                kind,
                end: None,
            });
        } else if let Some(&index) = stack.last()
            && pairs[index].kind == kind
        {
            pairs[index].end = Some(i);
            stack.pop();
            if first_inline == Some(stack.len()) {
                first_inline = None;
            }
        }
        i += 2;
    }

    let mut result = None;
    let mut last = 0;
    for pair in pairs {
        if pair.start < last {
            continue;
        }
        if let Some(end) = pair.end {
            let output = result.get_or_insert_with(|| String::with_capacity(text.len()));
            output.push_str(&text[last..pair.start]);
            output.push_str(pair.kind.opening());
            output.push_str(&text[pair.start + 2..end]);
            output.push_str(pair.kind.opening());
            last = end + 2;
        }
    }
    match result {
        Some(mut output) => {
            output.push_str(&text[last..]);
            Cow::Owned(output)
        }
        None => Cow::Borrowed(text),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut clipboard = Clipboard::new()?;
    let original = clipboard.get_text()?;
    let converted = convert(&original);

    if converted != original {
        clipboard.set_text(converted.into_owned())?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic() {
        assert_eq!(convert(r"\(x+y\)"), "$x+y$");
        assert_eq!(convert(r"\[x+y\]"), "$$x+y$$");
    }

    #[test]
    fn multiline() {
        assert_eq!(convert("\\[a\n+b\\]"), "$$a\n+b$$");
        assert_eq!(convert("\\(a\n+b\\)"), "\\(a\n+b\\)");
    }

    #[test]
    fn nested() {
        assert_eq!(convert(r"\[a \[b\] c\]"), r"$$a \[b\] c$$");
    }

    #[test]
    fn unmatched() {
        assert_eq!(convert(r"\(abc"), r"\(abc");
        assert_eq!(convert(r"abc\]"), r"abc\]");
    }

    #[test]
    fn code_protection() {
        let text = "`\\(x\\)` and \\(y\\)";
        assert_eq!(convert(text), "`\\(x\\)` and $y$");

        let text = "```latex\n\\[x\\]\n```\n\\[y\\]";
        assert_eq!(convert(text), "```latex\n\\[x\\]\n```\n$$y$$");
    }

    #[test]
    fn escaped_delimiters_and_unicode() {
        assert_eq!(convert(r"\\(x\\)"), r"\\(x\\)");
        assert_eq!(convert(r"\\\(中文🙂\\\)"), r"\\$中文🙂\\$");
        assert_eq!(convert("前🙂\\(α+β\\)后"), "前🙂$α+β$后");
    }

    #[test]
    fn block_code_boundaries() {
        for text in [
            "    \\(x\\)",
            "\t\\(x\\)",
            "\\[before\n```\ncode\n```\nafter\\]",
            "~~~latex\n\\(x\\)\n~~~",
            "````latex\n```\n\\(x\\)\n````",
            "```latex\n\\(x\\)",
        ] {
            assert_eq!(convert(text), text);
        }
        assert_eq!(convert("\\[\n    x+y\n\\]"), "$$\n    x+y\n$$");
        assert_eq!(
            convert("text\n\n    \\(x\\)\n\n\\(y\\)"),
            "text\n\n    \\(x\\)\n\n$y$"
        );
    }

    #[test]
    fn inline_code_boundaries_and_escaping() {
        assert_eq!(convert(r"\` text \(x\) `"), r"\` text $x$ `");
        assert_eq!(
            convert("`before\n\n\\(x\\)\n\nafter`"),
            "`before\n\n$x$\n\nafter`"
        );
        let text = "`before\n```\ncode\n```\n\\(x\\) after`";
        assert_eq!(convert(text), "`before\n```\ncode\n```\n$x$ after`");
        assert_eq!(convert("``a ` \\(x\\)`` \\(y\\)"), "``a ` \\(x\\)`` $y$");
        assert_eq!(convert("`a\n\\(x\\)` \\(y\\)"), "`a\n\\(x\\)` $y$");
        // 代码内部的反斜杠不转义结束反引号。
        assert_eq!(convert(r"`a\` \(x\)"), r"`a\` $x$");
        let text = "\\(a `b\nc` d\\)";
        assert_eq!(convert(text), text);
    }

    #[test]
    fn existing_dollar_math() {
        for text in [r"$\(x\)$", r"$$\[x\]$$", "$$a\n\\(x\\)\n$$"] {
            assert_eq!(convert(text), text);
        }
        assert_eq!(convert(r"\$ \(x\) \$"), r"\$ $x$ \$");
        assert_eq!(convert(r"$5 and $10; \(x\)"), r"$5 and $10; $x$");
        assert_eq!(convert("$a\n\\(x\\)$"), "$a\n$x$$");
        assert_eq!(convert(r"$a$ \(x\) $$b$$"), r"$a$ $x$ $$b$$");
    }

    #[test]
    fn unmatched_outer_and_multiple_pairs() {
        assert_eq!(convert(r"\[bad \(x\)"), r"\[bad $x$");
        assert_eq!(convert(r"\(a\) + \[b\] + \(c\)"), "$a$ + $$b$$ + $c$");
        assert_eq!(convert("\\(bad\n\\[x\\]"), "\\(bad\n$$x$$");
        assert_eq!(convert(r"\[a \(b\) c\]"), r"$$a \(b\) c$$");
    }

    #[test]
    fn unchanged_text_is_borrowed() {
        for text in [
            "",
            "中文🙂 plain text",
            r"\[unclosed",
            r"`\(x\)`",
            r"$\(x\)$",
        ] {
            assert!(matches!(convert(text), Cow::Borrowed(_)));
        }
    }

    #[test]
    fn repeated_conversion_is_stable() {
        for text in [r"\(x\)", r"\[a \(b\) c\]", "`code` \\(中文\\) \\[y\\]"] {
            let converted = convert(text);
            assert_eq!(convert(&converted), converted);
        }
    }

    #[test]
    fn large_unmatched_input() {
        let text = r"\[".repeat(100_000);
        assert_eq!(convert(&text), text);
        let text = format!("{}{}", "\\".repeat(100_000), r"\(x\)");
        assert!(convert(&text).ends_with("$x$"));
    }
}
