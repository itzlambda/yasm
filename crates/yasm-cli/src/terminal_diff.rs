use similar::{ChangeTag, TextDiff};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

pub(super) fn terminal_width() -> usize {
    terminal_size::terminal_size()
        .map(|(terminal_size::Width(width), _)| usize::from(width))
        .filter(|width| *width > 0)
        .or_else(|| {
            std::env::var("COLUMNS")
                .ok()?
                .parse()
                .ok()
                .filter(|width| *width > 0)
        })
        .unwrap_or(100)
}

pub(super) fn render(
    diff: &TextDiff<'_, '_, str>,
    name: &str,
    old: &str,
    new: &str,
    width: usize,
) -> String {
    let mut output = format!("\x1b[1m{name}\x1b[0m\n");
    let old_width = line_count_width(old);
    let new_width = line_count_width(new);
    for (index, group) in diff.grouped_ops(3).iter().enumerate() {
        if index > 0 {
            output.push_str("\x1b[90m…\x1b[0m\n");
        }
        for op in group {
            for change in diff.iter_inline_changes(op) {
                output.push_str(&format_inline_change(change, old_width, new_width, width));
            }
        }
    }
    output
}

fn line_count_width(text: &str) -> usize {
    text.lines().count().max(1).to_string().len()
}

fn format_inline_change(
    change: similar::InlineChange<'_, str>,
    old_width: usize,
    new_width: usize,
    width: usize,
) -> String {
    let old = change
        .old_index()
        .map(|index| (index + 1).to_string())
        .unwrap_or_default();
    let new = change
        .new_index()
        .map(|index| (index + 1).to_string())
        .unwrap_or_default();
    let sign = match change.tag() {
        ChangeTag::Equal => " ",
        ChangeTag::Delete => "-",
        ChangeTag::Insert => "+",
    };
    let prefix = format!(
        "\x1b[90m{old:>old_width$} {new:>new_width$} | \x1b[0m{}{} ",
        line_color(change.tag()),
        sign
    );
    let continuation_prefix = format!(
        "\x1b[90m{old:>old_width$} {new:>new_width$} | \x1b[0m{}  ",
        line_color(change.tag()),
        old = "",
        new = ""
    );
    let prefix_width = old_width + 1 + new_width + 3 + 2;
    let content_width = width.saturating_sub(prefix_width + 1).max(1);
    let mut output = String::new();
    output.push_str(&prefix);
    let mut column = 0;
    for (emphasized, value) in change.iter_strings_lossy() {
        if emphasized {
            output.push_str(emphasis_color(change.tag()));
        }
        push_wrapped_text(
            &mut output,
            value.trim_end_matches('\n'),
            &mut column,
            content_width,
            &format!(
                "{continuation_prefix}{}",
                if emphasized {
                    emphasis_color(change.tag())
                } else {
                    ""
                }
            ),
        );
        if emphasized {
            output.push_str("\x1b[0m");
            output.push_str(line_color(change.tag()));
        }
    }
    output.push_str("\x1b[0m\n");
    output
}

fn push_wrapped_text(
    output: &mut String,
    text: &str,
    column: &mut usize,
    content_width: usize,
    continuation_prefix: &str,
) {
    for token in wrap_tokens(text) {
        let token_width = UnicodeWidthStr::width(token);
        if *column > 0
            && *column + token_width > content_width
            && !token.chars().all(char::is_whitespace)
        {
            output.push_str("\x1b[0m\n");
            output.push_str(continuation_prefix);
            *column = 0;
        }
        for ch in token.chars() {
            let char_width = ch.width().unwrap_or(0);
            if *column > 0 && *column + char_width > content_width {
                output.push_str("\x1b[0m\n");
                output.push_str(continuation_prefix);
                *column = 0;
            }
            output.push(ch);
            *column += char_width;
        }
    }
}

fn wrap_tokens(text: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut start = 0;
    let mut in_whitespace = None;
    for (index, ch) in text.char_indices() {
        let whitespace = ch.is_whitespace();
        match in_whitespace {
            None => in_whitespace = Some(whitespace),
            Some(current) if current != whitespace => {
                tokens.push(&text[start..index]);
                start = index;
                in_whitespace = Some(whitespace);
            }
            _ => {}
        }
    }
    if start < text.len() {
        tokens.push(&text[start..]);
    }
    tokens
}

fn line_color(tag: ChangeTag) -> &'static str {
    match tag {
        ChangeTag::Equal => "\x1b[90m",
        ChangeTag::Delete => "\x1b[31m",
        ChangeTag::Insert => "\x1b[32m",
    }
}

fn emphasis_color(tag: ChangeTag) -> &'static str {
    match tag {
        ChangeTag::Equal => "\x1b[7;90m",
        ChangeTag::Delete => "\x1b[7;31m",
        ChangeTag::Insert => "\x1b[7;32m",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_styles(text: &str) -> String {
        let mut chars = text.chars();
        let mut output = String::new();
        while let Some(ch) = chars.next() {
            if ch == '\x1b' {
                for ch in chars.by_ref() {
                    if ch == 'm' {
                        break;
                    }
                }
            } else {
                output.push(ch);
            }
        }
        output
    }

    #[test]
    fn wraps_added_markdown_with_aligned_gutters_and_unicode_width() {
        let new = "A long paragraph with 界界界 and more words that must wrap cleanly.\n";
        let diff = TextDiff::from_lines("", new);
        let output = render(&diff, "SKILL.md", "", new, 32);
        assert!(output.contains("\x1b[32m"));
        let plain = strip_styles(&output);
        let lines: Vec<_> = plain.lines().skip(1).collect();
        assert!(lines.len() > 2);
        assert!(lines[0].starts_with("  1 | + "));
        assert!(lines[1..].iter().all(|line| line.starts_with("    |   ")));
        assert!(lines.iter().all(|line| UnicodeWidthStr::width(*line) < 32));
        let reconstructed = lines.iter().map(|line| &line[8..]).collect::<String>();
        assert_eq!(reconstructed, new.trim_end_matches('\n'));
    }

    #[test]
    fn highlights_changed_words_and_resets_emphasis_before_unchanged_suffix() {
        let old = "Keep the old word here.\n";
        let new = "Keep the new word here.\n";
        let diff = TextDiff::from_lines(old, new);
        let output = render(&diff, "SKILL.md", old, new, 80);
        assert!(output.contains("\x1b[7;31mold\x1b[0m\x1b[31m"));
        assert!(output.contains("\x1b[7;32mnew\x1b[0m\x1b[32m"));
        let plain = strip_styles(&output);
        assert!(plain.contains("1   | - Keep the old word here."));
        assert!(plain.contains("  1 | + Keep the new word here."));
    }
}
