//! A small markdown parser for citation-bearing answers.
//!
//! `InlineCitations` owns its prose layout (word-level atoms so `[n]` markers
//! can flow as chips), which means it cannot delegate to `TextView`'s
//! markdown renderer. This module supplies just enough structure for the
//! component: block elements (headings, paragraphs, lists, quotes, fenced
//! code) and inline spans (bold, italic, code, links, citation markers).
//!
//! It is intentionally a subset: tables, images, nested lists, and reference
//! links fall back to literal text rather than rendering wrong.

/// One block-level element of an answer.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Block {
    Paragraph(Vec<Inline>),
    Heading {
        level: usize,
        content: Vec<Inline>,
    },
    Bullet(Vec<Inline>),
    Numbered {
        marker: String,
        content: Vec<Inline>,
    },
    Quote(Vec<Inline>),
    Code(String),
}

/// One inline element: styled text or an `[n]` citation marker.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Inline {
    Text(TextSpan),
    Marker(u32),
}

/// A run of text with the inline formatting that applies to it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct TextSpan {
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub code: bool,
    pub link: Option<String>,
}

impl TextSpan {
    #[cfg(test)]
    fn plain(text: String) -> Self {
        Self {
            text,
            bold: false,
            italic: false,
            code: false,
            link: None,
        }
    }
}

/// Parse `text` into block elements.
pub(crate) fn parse_blocks(text: &str) -> Vec<Block> {
    let lines: Vec<&str> = text.lines().collect();
    let mut blocks = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let content = lines[index].trim();
        if content.is_empty() {
            index += 1;
            continue;
        }

        // Fenced code: everything to the closing fence is literal.
        if content.starts_with("```") || content.starts_with("~~~") {
            let fence = &content[..3];
            index += 1;
            let mut code = String::new();
            while index < lines.len() {
                if lines[index].trim_start().starts_with(fence) {
                    index += 1;
                    break;
                }
                if !code.is_empty() {
                    code.push('\n');
                }
                code.push_str(lines[index]);
                index += 1;
            }
            blocks.push(Block::Code(code));
            continue;
        }

        if let Some((level, rest)) = heading(content) {
            blocks.push(Block::Heading {
                level,
                content: parse_inline(rest),
            });
            index += 1;
            continue;
        }
        if let Some(rest) = bullet(content) {
            blocks.push(Block::Bullet(parse_inline(rest)));
            index += 1;
            continue;
        }
        if let Some((marker, rest)) = numbered(content) {
            blocks.push(Block::Numbered {
                marker,
                content: parse_inline(rest),
            });
            index += 1;
            continue;
        }
        if let Some(rest) = content.strip_prefix('>') {
            blocks.push(Block::Quote(parse_inline(rest.trim_start())));
            index += 1;
            continue;
        }
        // Horizontal rule: keep the paragraph flow clean by dropping it.
        if is_rule(content) {
            index += 1;
            continue;
        }

        // Paragraph: join consecutive plain lines (markdown soft wraps) until
        // a blank line or the next block starts.
        let mut joined = String::new();
        while index < lines.len() {
            let line = lines[index].trim();
            if line.is_empty() {
                break;
            }
            if !joined.is_empty() && starts_block(line) {
                break;
            }
            if !joined.is_empty() {
                joined.push(' ');
            }
            joined.push_str(line);
            index += 1;
        }
        blocks.push(Block::Paragraph(parse_inline(&joined)));
    }
    blocks
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&level) && line[level..].starts_with(' ') {
        Some((level, line[level..].trim_start()))
    } else {
        None
    }
}

fn bullet(line: &str) -> Option<&str> {
    for marker in ['-', '*', '+'] {
        if let Some(rest) = line.strip_prefix(marker)
            && rest.starts_with(' ')
        {
            return Some(rest.trim_start());
        }
    }
    None
}

fn numbered(line: &str) -> Option<(String, &str)> {
    let digits = line.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 {
        return None;
    }
    let rest = &line[digits..];
    if let Some(rest) = rest.strip_prefix(". ")
        && let Some(dot) = line.find(". ")
    {
        return Some((line[..dot].to_owned(), rest.trim_start()));
    }
    None
}

fn is_rule(line: &str) -> bool {
    let dashes = line.chars().all(|c| c == '-') && line.chars().count() >= 3;
    let stars = line.chars().all(|c| c == '*') && line.chars().count() >= 3;
    dashes || stars
}

fn starts_block(line: &str) -> bool {
    line.starts_with("```")
        || line.starts_with("~~~")
        || heading(line).is_some()
        || bullet(line).is_some()
        || numbered(line).is_some()
        || line.starts_with('>')
}

/// Parse inline formatting: `**bold**`, `*italic*`/`_italic_`, `` `code` ``,
/// `[text](url)` links, and `[n]` citation markers.
pub(crate) fn parse_inline(text: &str) -> Vec<Inline> {
    let chars: Vec<char> = text.chars().collect();
    let mut spans: Vec<Inline> = Vec::new();
    let mut plain = String::new();
    let mut bold = false;
    let mut italic = false;
    let mut index = 0;

    while index < chars.len() {
        let current = chars[index];

        if current == '`'
            && let Some(end) = find_char(&chars, index + 1, '`')
        {
            flush_plain(&mut spans, &mut plain, bold, italic, None);
            spans.push(Inline::Text(TextSpan {
                text: chars[index + 1..end].iter().collect(),
                bold: false,
                italic: false,
                code: true,
                link: None,
            }));
            index = end + 1;
            continue;
        }

        if current == '*' || current == '_' {
            let doubled = index + 1 < chars.len() && chars[index + 1] == current;
            flush_plain(&mut spans, &mut plain, bold, italic, None);
            if doubled {
                bold = !bold;
                index += 2;
            } else {
                italic = !italic;
                index += 1;
            }
            continue;
        }

        if current == '['
            && let Some((n, end)) = marker_at(&chars, index)
        {
            flush_plain(&mut spans, &mut plain, bold, italic, None);
            spans.push(Inline::Marker(n));
            index = end;
            continue;
        }

        if current == '['
            && let Some((label, url, end)) = link_at(&chars, index)
        {
            flush_plain(&mut spans, &mut plain, bold, italic, None);
            spans.push(Inline::Text(TextSpan {
                text: label,
                bold,
                italic,
                code: false,
                link: Some(url),
            }));
            index = end;
            continue;
        }

        plain.push(current);
        index += 1;
    }

    flush_plain(&mut spans, &mut plain, bold, italic, None);
    spans
}

fn find_char(chars: &[char], from: usize, needle: char) -> Option<usize> {
    chars[from..]
        .iter()
        .position(|c| *c == needle)
        .map(|offset| from + offset)
}

/// `[123]` → `(123, index after the bracket)`.
fn marker_at(chars: &[char], start: usize) -> Option<(u32, usize)> {
    let mut index = start + 1;
    let mut n: u32 = 0;
    let mut digits = 0;
    while index < chars.len() && chars[index].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add((chars[index] as u8 - b'0') as u32);
        index += 1;
        digits += 1;
        if digits > 9 {
            return None;
        }
    }
    if digits > 0 && index < chars.len() && chars[index] == ']' {
        Some((n, index + 1))
    } else {
        None
    }
}

/// `[label](https://…)` → `(label, url, index after the paren)`.
fn link_at(chars: &[char], start: usize) -> Option<(String, String, usize)> {
    let close_bracket = find_char(chars, start + 1, ']')?;
    if chars.get(close_bracket + 1) != Some(&'(') {
        return None;
    }
    let close_paren = find_char(chars, close_bracket + 2, ')')?;
    let label: String = chars[start + 1..close_bracket].iter().collect();
    let url: String = chars[close_bracket + 2..close_paren].iter().collect();
    if label.is_empty() || url.is_empty() {
        return None;
    }
    Some((label, url, close_paren + 1))
}

/// Emit the accumulated plain text as word atoms, tagging each with the
/// active style. Words (not runs) are the atoms because the renderer flows
/// them in a wrapping flex row.
fn flush_plain(
    spans: &mut Vec<Inline>,
    plain: &mut String,
    bold: bool,
    italic: bool,
    link: Option<String>,
) {
    if plain.is_empty() {
        return;
    }
    for word in plain.split_whitespace() {
        spans.push(Inline::Text(TextSpan {
            text: word.to_owned(),
            bold,
            italic,
            code: false,
            link: link.clone(),
        }));
    }
    plain.clear();
}

/// Whether a space belongs between two adjacent atoms. Spaces sit between
/// words, never before punctuation, and never inside a marker.
pub(crate) fn needs_space(previous: &Inline, next: &Inline) -> bool {
    let Inline::Text(next) = next else {
        return false;
    };
    let Some(first) = next.text.chars().next() else {
        return false;
    };
    if !first.is_alphanumeric() {
        return false;
    }
    match previous {
        Inline::Marker(_) => true,
        Inline::Text(previous) => !matches!(
            previous.text.chars().last(),
            None | Some('(' | '[' | '{' | '"' | '\'' | '“' | '‘' | '—' | '–')
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(block: &Block) -> Vec<Inline> {
        match block {
            Block::Paragraph(content)
            | Block::Bullet(content)
            | Block::Quote(content)
            | Block::Heading { content, .. }
            | Block::Numbered { content, .. } => content.clone(),
            Block::Code(_) => Vec::new(),
        }
    }

    #[test]
    fn paragraphs_split_on_blank_lines_and_join_soft_wraps() {
        let blocks = parse_blocks("first line\ncontinues here\n\nsecond paragraph");
        assert_eq!(blocks.len(), 2);
        let first = words(&blocks[0]);
        assert_eq!(first.len(), 4, "two lines join into one flow: {first:?}");
        assert!(matches!(blocks[1], Block::Paragraph(_)));
    }

    #[test]
    fn headings_lists_quotes_and_code_get_their_blocks() {
        let blocks =
            parse_blocks("# Title\n\n- one\n- two\n\n1. first\n> quoted\n\n```\nlet x = 1;\n```");
        assert!(matches!(blocks[0], Block::Heading { level: 1, .. }));
        assert!(matches!(blocks[1], Block::Bullet(_)));
        assert!(matches!(blocks[2], Block::Bullet(_)));
        assert!(matches!(blocks[3], Block::Numbered { .. }));
        assert!(matches!(blocks[4], Block::Quote(_)));
        assert_eq!(blocks[5], Block::Code("let x = 1;".to_owned()));
    }

    #[test]
    fn inline_styles_and_links_are_tracked() {
        let spans = parse_inline("a **bold** and *italic* and `code` [docs](https://x.dev)");
        let bold = spans.iter().find_map(|span| match span {
            Inline::Text(text) if text.text == "bold" => Some(text),
            _ => None,
        });
        assert!(bold.is_some_and(|text| text.bold));
        let italic = spans.iter().find_map(|span| match span {
            Inline::Text(text) if text.text == "italic" => Some(text),
            _ => None,
        });
        assert!(italic.is_some_and(|text| text.italic));
        let code = spans.iter().find_map(|span| match span {
            Inline::Text(text) if text.text == "code" => Some(text),
            _ => None,
        });
        assert!(code.is_some_and(|text| text.code));
        let link = spans.iter().find_map(|span| match span {
            Inline::Text(text) if text.text == "docs" => Some(text),
            _ => None,
        });
        assert_eq!(
            link.and_then(|text| text.link.clone()),
            Some("https://x.dev".to_owned())
        );
    }

    #[test]
    fn citation_markers_survive_next_to_punctuation() {
        let spans = parse_inline("compute[1], though");
        assert_eq!(spans[0], Inline::Text(TextSpan::plain("compute".into())));
        assert_eq!(spans[1], Inline::Marker(1));
        assert_eq!(spans[2], Inline::Text(TextSpan::plain(",".into())));
        assert_eq!(spans[3], Inline::Text(TextSpan::plain("though".into())));
        assert!(!needs_space(&spans[0], &spans[1]), "word → chip is tight");
        assert!(!needs_space(&spans[1], &spans[2]), "chip → comma is tight");
        assert!(
            needs_space(&spans[2], &spans[3]),
            "comma → word needs a space"
        );
    }
}
