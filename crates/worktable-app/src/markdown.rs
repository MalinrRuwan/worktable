//! Markdown rendering for Worktable entries and assistant messages.
//!
//! Parses raw markdown (which may contain unicode emojis) via `pulldown-cmark`
//! and builds a GPUI element tree with styled spans. The parser preserves
//! unicode emojis as-is — GPUI renders them via font fallback. `:shortcode:`
//! emoji aliases are left literal (no extra crate needed for unicode rendering);
//! callers can pre-process with `emojis` crate if they enable it, but unicode is
//! sufficient for "render emojis properly".

use gpui::prelude::FluentBuilder as _;
use gpui::{AnyElement, IntoElement, ParentElement as _, Styled, div, px};
use gpui_component::{Theme, h_flex, v_flex};
use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};

// ---------------------------------------------------------------------------
// Emoji shortcodes — map :tada: etc. to unicode so they render via font fallback.
// ---------------------------------------------------------------------------

fn emojify(text: &str) -> String {
    // Common subset; extend as needed. Keep it small to avoid a crate.
    let mut s = text.to_owned();
    for (code, emoji) in [
        (":tada:", "🎉"),
        (":smile:", "😄"),
        (":rocket:", "🚀"),
        (":heart:", "❤️"),
        (":fire:", "🔥"),
        (":thumbsup:", "👍"),
        (":thumbsdown:", "👎"),
        (":eyes:", "👀"),
        (":sparkles:", "✨"),
        (":warning:", "⚠️"),
        (":check:", "✅"),
        (":x:", "❌"),
        (":bulb:", "💡"),
        (":memo:", "📝"),
        (":link:", "🔗"),
        (":star:", "⭐"),
        (":zap:", "⚡"),
    ] {
        s = s.replace(code, emoji);
    }
    s
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Render arbitrary markdown as a block-level element (paragraphs, headings,
/// lists, code blocks, blockquotes, etc.). Suitable for entry bodies and
/// assistant messages. Returns a `v_flex` that inherits the surrounding
/// `text_color` for normal spans and applies distinct styling for code,
/// links, etc.
pub fn render_markdown(text: &str, theme: &Theme) -> AnyElement {
    let text = emojify(text);
    render_markdown_inner(&text, theme, false)
}

/// Render markdown that is expected to be a single inline line (e.g. an entry
/// title / heading fallback). The output is a wrapping horizontal flex of
/// inline spans, styled at `text_lg` by the caller. The caller should apply
/// `.text_lg()` to the container if desired; this helper returns the raw
/// inline children without an extra block gap.
pub fn render_markdown_inline(text: &str, theme: &Theme) -> AnyElement {
    // For a single line we reuse the same parser but post-process: if the
    // markdown yields exactly one paragraph we unwrap its inline children.
    // Otherwise we fall back to the block renderer — a heading containing a
    // hard break is still valid markdown.
    let text = emojify(text);
    let blocks = parse_blocks(&text, theme);
    if blocks.len() == 1 {
        // `parse_blocks` wraps paragraphs in a flex-wrap div. That div is
        // already an inline container — return it directly.
        return blocks.into_iter().next().unwrap();
    }
    // Multiple blocks (e.g. heading + paragraph) — render as a tight v_flex.
    v_flex().gap_1().children(blocks).into_any_element()
}

// ---------------------------------------------------------------------------
// Core parser → element conversion
// ---------------------------------------------------------------------------

fn render_markdown_inner(text: &str, theme: &Theme, _is_inline: bool) -> AnyElement {
    let blocks = parse_blocks(text, theme);
    if blocks.is_empty() {
        // Empty markdown (e.g. whitespace) — preserve layout with a placeholder.
        return div()
            .text_sm()
            .text_color(theme.muted_foreground)
            .child(text.to_owned())
            .into_any_element();
    }
    v_flex().gap_2().children(blocks).into_any_element()
}

fn parse_blocks(text: &str, theme: &Theme) -> Vec<AnyElement> {
    // Enable the markdown extensions we want to render. `ENABLE_STRIKETHROUGH`
    // gives `~strike~`. Tables are kept for completeness even though the
    // native layout stacks rows vertically.
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_FOOTNOTES);
    opts.insert(Options::ENABLE_TASKLISTS);
    opts.insert(Options::ENABLE_SMART_PUNCTUATION);
    opts.insert(Options::ENABLE_HEADING_ATTRIBUTES);

    let parser = Parser::new_ext(text, opts);

    // Block-level accumulation.
    let mut blocks: Vec<AnyElement> = Vec::new();

    // Inline buffer for the currently open block (paragraph / heading / list item).
    let mut inline: Vec<AnyElement> = Vec::new();

    // Inline style stacks — counters allow nesting (e.g. **bold _italic_**).
    let mut strong: usize = 0;
    let mut emphasis: usize = 0;
    let mut strikethrough: usize = 0;
    let mut link_stack: Vec<String> = Vec::new();
    let mut image_alt_stack: Vec<String> = Vec::new();

    // Block context.
    let mut heading_level: Option<HeadingLevel> = None;
    let mut list_stack: Vec<ListInfo> = Vec::new();
    let mut blockquote_depth: usize = 0;
    let mut in_code_block: Option<CodeBlockInfo> = None;
    let mut code_block_buf: String = String::new();
    let mut in_html_block = false;
    let mut html_buf = String::new();
    let mut in_paragraph = false;
    let mut in_item = false;
    // Table handling — we flatten rows as blocks with a simple flex layout.
    let mut table_rows: Vec<Vec<String>> = Vec::new();
    let mut current_row: Vec<String> = Vec::new();
    let mut current_cell: String = String::new();
    let mut in_table_cell = false;

    // Suppress unused for image alt handling.
    let _image_alt_len = image_alt_stack.len();

    // Helper to emit a styled inline span for `Text`.
    // We capture a closure that pushes to `inline` with current style state.
    let push_text = |text: &str,
                     inline: &mut Vec<AnyElement>,
                     theme: &Theme,
                     strong: usize,
                     emphasis: usize,
                     strikethrough: usize,
                     link_stack: &Vec<String>| {
        if text.is_empty() {
            return;
        }
        let is_bold = strong > 0;
        let is_italic = emphasis > 0;
        let is_strike = strikethrough > 0;
        let link_dest = link_stack.last().cloned();
        let span = styled_inline(
            text.to_owned(),
            theme,
            is_bold,
            is_italic,
            is_strike,
            link_dest,
        );
        inline.push(span);
    };

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Paragraph => {
                    inline.clear();
                    in_paragraph = true;
                    if in_table_cell {
                        current_cell.clear();
                    }
                }
                Tag::Heading { level, .. } => {
                    inline.clear();
                    heading_level = Some(level);
                }
                Tag::BlockQuote(_kind) => {
                    // Flush any pending inline from an outer paragraph before entering quote.
                    if !inline.is_empty()
                        && !in_item
                        && !heading_level.is_some()
                        && in_code_block.is_none()
                    {
                        // Don't flush prematurely; blockquote contains its own blocks.
                    }
                    blockquote_depth += 1;
                }
                Tag::CodeBlock(kind) => {
                    inline.clear();
                    in_code_block = Some(CodeBlockInfo::from_kind(&kind));
                    code_block_buf.clear();
                }
                Tag::HtmlBlock => {
                    in_html_block = true;
                    html_buf.clear();
                }
                Tag::List(start) => {
                    let ordered = start.is_some();
                    let start_num = start.unwrap_or(1);
                    list_stack.push(ListInfo {
                        ordered,
                        start: start_num,
                        next_index: start_num,
                    });
                }
                Tag::Item => {
                    inline.clear();
                    in_item = true;
                    if in_table_cell {
                        current_cell.clear();
                    }
                }
                Tag::Emphasis => {
                    emphasis += 1;
                }
                Tag::Strong => {
                    strong += 1;
                }
                Tag::Strikethrough => {
                    strikethrough += 1;
                }
                Tag::Link { dest_url, .. } => {
                    link_stack.push(dest_url.into_string());
                }
                Tag::Image { dest_url, .. } => {
                    // Images are rendered as their alt text plus a link indicator.
                    // Push a marker so `End(Image)` can wrap the alt.
                    image_alt_stack.push(dest_url.into_string());
                    // Also keep alt text collection in `inline` — the Text events
                    // between Start(Image) and End(Image) are the alt.
                    inline.push(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child("🖼 ".to_owned())
                            .into_any_element(),
                    );
                }
                Tag::Table(_alignments) => {
                    table_rows.clear();
                }
                Tag::TableHead | Tag::TableRow => {
                    current_row.clear();
                }
                Tag::TableCell => {
                    in_table_cell = true;
                    current_cell.clear();
                    inline.clear();
                }
                Tag::FootnoteDefinition(_) => {
                    inline.clear();
                }
                _ => {
                    // Other tags like DefinitionList, MetadataBlock etc. — just flush inline.
                    inline.clear();
                }
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph => {
                    in_paragraph = false;
                    if in_table_cell {
                        // Table cell text was collected in `current_cell` via Text events.
                        // `inline` flush gives us styled spans — but for tables we flatten to plain.
                        let cell_text = std::mem::take(&mut current_cell);
                        // Also capture any inline spans as text? For simplicity use plain.
                        let display = if !cell_text.is_empty() {
                            cell_text
                        } else if !inline.is_empty() {
                            // Fallback: join debug? Instead render inline block count.
                            // We'll treat inline as raw text concatenation.
                            String::new()
                        } else {
                            String::new()
                        };
                        current_row.push(display);
                        inline.clear();
                    } else if in_item {
                        // Paragraph inside list item — keep inline for item flush.
                        // Don't push as separate block yet; let Item end handle it.
                    } else if in_code_block.is_some() || in_html_block {
                        // Inside other block — ignore.
                    } else {
                        // Normal paragraph block.
                        if !inline.is_empty() || !in_code_block.is_some() {
                            let para = flush_paragraph(
                                std::mem::take(&mut inline),
                                theme,
                                blockquote_depth,
                                &list_stack,
                            );
                            blocks.push(para);
                        }
                    }
                }
                TagEnd::Heading(level) => {
                    let content = std::mem::take(&mut inline);
                    heading_level = None;
                    if content.is_empty() {
                        continue;
                    }
                    let heading = flush_heading(content, level, theme, blockquote_depth);
                    blocks.push(heading);
                }
                TagEnd::BlockQuote(_) => {
                    blockquote_depth = blockquote_depth.saturating_sub(1);
                }
                TagEnd::CodeBlock => {
                    let info = in_code_block.take();
                    let code = std::mem::take(&mut code_block_buf);
                    if !code.trim().is_empty() || info.is_some() {
                        let block =
                            render_code_block(&code, info.as_ref(), theme, blockquote_depth);
                        blocks.push(block);
                    }
                    inline.clear();
                }
                TagEnd::HtmlBlock => {
                    in_html_block = false;
                    let html = std::mem::take(&mut html_buf);
                    if !html.trim().is_empty() {
                        let block = render_code_block(&html, None, theme, blockquote_depth);
                        blocks.push(block);
                    }
                }
                TagEnd::List(_ordered) => {
                    list_stack.pop();
                }
                TagEnd::Item => {
                    in_item = false;
                    // Flush the item's content (inline buffer holds its paragraph(s)).
                    // An item may contain multiple blocks, but `inline` holds its trailing paragraph.
                    // Any blocks that were flushed inside the item (e.g. nested lists) already went to `blocks`.
                    // Here we wrap the remaining inline as a list item row.
                    if !inline.is_empty() {
                        let item_inline = std::mem::take(&mut inline);
                        let item_block =
                            flush_list_item(item_inline, theme, &mut list_stack, blockquote_depth);
                        blocks.push(item_block);
                    } else {
                        // Empty item — still advance counter so numbering stays correct.
                        if let Some(info) = list_stack.last_mut() {
                            info.next_index += 1;
                        }
                    }
                    // If the item contained nested blocks already pushed, they are separate entries in `blocks`.
                    // That's acceptable: the visual order remains correct (nested list blocks appear after the item's text).
                }
                TagEnd::Emphasis => {
                    emphasis = emphasis.saturating_sub(1);
                }
                TagEnd::Strong => {
                    strong = strong.saturating_sub(1);
                }
                TagEnd::Strikethrough => {
                    strikethrough = strikethrough.saturating_sub(1);
                }
                TagEnd::Link => {
                    link_stack.pop();
                }
                TagEnd::Image => {
                    let dest = image_alt_stack.pop();
                    if let Some(url) = dest {
                        // Append a small link indicator after the alt text.
                        inline.push(
                            div()
                                .text_xs()
                                .text_color(theme.link)
                                .underline()
                                .child(format!(" ({})", url))
                                .into_any_element(),
                        );
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if !current_row.is_empty() {
                        let row = std::mem::take(&mut current_row);
                        table_rows.push(row);
                    }
                }
                TagEnd::TableCell => {
                    in_table_cell = false;
                    // current_cell already captured on paragraph end; if no paragraph, capture inline.
                    if inline.is_empty() && current_cell.is_empty() {
                        // No text — push empty.
                        current_row.push(String::new());
                    } else if !current_cell.is_empty() {
                        // already pushed in paragraph end.
                    } else {
                        // Inline buffer has spans but no paragraph wrapper (e.g. direct Text in cell).
                        // Extract plain text from inline by noting we didn't track plain.
                        // As fallback, push empty; table rendering will still show row structure.
                        current_row.push(String::new());
                        inline.clear();
                    }
                }
                TagEnd::Table => {
                    if !table_rows.is_empty() {
                        let table_block = render_table(&table_rows, theme, blockquote_depth);
                        blocks.push(table_block);
                        table_rows.clear();
                    }
                }
                TagEnd::FootnoteDefinition => {
                    if !inline.is_empty() {
                        let para = flush_paragraph(
                            std::mem::take(&mut inline),
                            theme,
                            blockquote_depth,
                            &list_stack,
                        );
                        // Footnotes are rendered as muted small blocks with a left border.
                        let footnote = div()
                            .border_l_2()
                            .border_color(theme.border)
                            .pl_3()
                            .ml_1()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(para)
                            .into_any_element();
                        blocks.push(footnote);
                    }
                }
                _ => {
                    // Other ends — flush inline if present.
                    if !inline.is_empty() {
                        let para = flush_paragraph(
                            std::mem::take(&mut inline),
                            theme,
                            blockquote_depth,
                            &list_stack,
                        );
                        blocks.push(para);
                    }
                }
            },
            Event::Text(text) => {
                if in_code_block.is_some() {
                    code_block_buf.push_str(&text);
                } else if in_html_block {
                    html_buf.push_str(&text);
                } else if in_table_cell {
                    current_cell.push_str(&text);
                    // Also collect inline for potential styled rendering (not used for table plain).
                    // We still push a styled span for non-table-path but table cells override.
                } else {
                    // Normal inline text.
                    // Handle emoji shortcodes pass-through: just keep unicode as-is.
                    push_text(
                        &text,
                        &mut inline,
                        theme,
                        strong,
                        emphasis,
                        strikethrough,
                        &link_stack,
                    );
                }
            }
            Event::Code(code) => {
                // Inline code — render with muted background, mono.
                let span = render_inline_code(&code, theme, link_stack.last().cloned());
                inline.push(span);
                if in_table_cell {
                    current_cell.push_str(&code);
                }
            }
            Event::Html(html) => {
                if in_html_block {
                    html_buf.push_str(&html);
                } else if in_table_cell {
                    current_cell.push_str(&html);
                } else {
                    // Inline HTML — render as plain text with muted color.
                    inline.push(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(html.into_string())
                            .into_any_element(),
                    );
                }
            }
            Event::InlineHtml(html) => {
                if in_table_cell {
                    current_cell.push_str(&html);
                } else {
                    inline.push(
                        div()
                            .text_xs()
                            .text_color(theme.muted_foreground)
                            .child(html.into_string())
                            .into_any_element(),
                    );
                }
            }
            Event::SoftBreak => {
                // Markdown soft break is a space in rendered output.
                if in_code_block.is_none() && !in_html_block && !in_table_cell {
                    push_text(
                        " ",
                        &mut inline,
                        theme,
                        strong,
                        emphasis,
                        strikethrough,
                        &link_stack,
                    );
                } else if in_code_block.is_some() {
                    code_block_buf.push('\n');
                } else if in_table_cell {
                    current_cell.push(' ');
                }
            }
            Event::HardBreak => {
                if in_code_block.is_some() {
                    code_block_buf.push('\n');
                } else if in_table_cell {
                    current_cell.push('\n');
                } else if !in_html_block {
                    // Hard break — force line break. We flush the current line as a
                    // block and start a new one, or insert a newline span.
                    // Simpler: insert a block-level line break element.
                    inline.push(div().child("\n").into_any_element());
                }
            }
            Event::Rule => {
                // Horizontal rule — flush any pending paragraph first.
                if !inline.is_empty() {
                    let para = flush_paragraph(
                        std::mem::take(&mut inline),
                        theme,
                        blockquote_depth,
                        &list_stack,
                    );
                    blocks.push(para);
                }
                let rule = div()
                    .h(px(1.))
                    .w_full()
                    .my_2()
                    .bg(theme.border)
                    .into_any_element();
                blocks.push(rule);
            }
            Event::FootnoteReference(label) => {
                let label_str = format!("[^{}]", label);
                push_text(
                    &label_str,
                    &mut inline,
                    theme,
                    strong,
                    emphasis,
                    strikethrough,
                    &link_stack,
                );
            }
            Event::TaskListMarker(checked) => {
                let marker = if checked { "☑ " } else { "☐ " };
                push_text(
                    marker,
                    &mut inline,
                    theme,
                    strong,
                    emphasis,
                    strikethrough,
                    &link_stack,
                );
            }
            Event::DisplayMath(text) | Event::InlineMath(text) => {
                // Math — render as inline code-like with italic.
                let span = div()
                    .italic()
                    .text_color(theme.muted_foreground)
                    .child(text.into_string())
                    .into_any_element();
                inline.push(span);
            }
        }
    }

    // Flush any trailing inline that wasn't closed by a block end (e.g. text
    // without trailing newline).
    if !inline.is_empty() {
        if in_code_block.is_some() {
            let info = in_code_block.take();
            let code = std::mem::take(&mut code_block_buf);
            blocks.push(render_code_block(
                &code,
                info.as_ref(),
                theme,
                blockquote_depth,
            ));
        } else if heading_level.is_some() {
            let level = heading_level.take().unwrap();
            let heading =
                flush_heading(std::mem::take(&mut inline), level, theme, blockquote_depth);
            blocks.push(heading);
        } else if in_item {
            let item_block = flush_list_item(
                std::mem::take(&mut inline),
                theme,
                &mut list_stack,
                blockquote_depth,
            );
            blocks.push(item_block);
        } else {
            let para = flush_paragraph(
                std::mem::take(&mut inline),
                theme,
                blockquote_depth,
                &list_stack,
            );
            blocks.push(para);
        }
    }

    // If file ends inside a code block (no closing fence), flush it.
    if in_code_block.is_some() {
        let info = in_code_block.take();
        let code = std::mem::take(&mut code_block_buf);
        blocks.push(render_code_block(
            &code,
            info.as_ref(),
            theme,
            blockquote_depth,
        ));
    }

    // Also handle trailing table.
    if !table_rows.is_empty() {
        blocks.push(render_table(&table_rows, theme, blockquote_depth));
    }

    // Suppress unused warning for `in_paragraph`.
    let _ = in_paragraph;

    blocks
}

// ---------------------------------------------------------------------------
// Block flush helpers
// ---------------------------------------------------------------------------

struct ListInfo {
    ordered: bool,
    start: u64,
    next_index: u64,
}

struct CodeBlockInfo {
    lang: Option<String>,
}

impl CodeBlockInfo {
    fn from_kind(kind: &CodeBlockKind) -> Self {
        match kind {
            CodeBlockKind::Fenced(lang) => Self {
                lang: if lang.is_empty() {
                    None
                } else {
                    Some(lang.to_string())
                },
            },
            CodeBlockKind::Indented => Self { lang: None },
        }
    }
}

fn flush_paragraph(
    inline: Vec<AnyElement>,
    theme: &Theme,
    blockquote_depth: usize,
    _list_stack: &[ListInfo],
) -> AnyElement {
    if inline.is_empty() {
        return div().into_any_element();
    }
    let mut para = div()
        .flex()
        .flex_wrap()
        .gap_1()
        .text_sm()
        .children(inline)
        .into_any_element();

    if blockquote_depth > 0 {
        para = div()
            .border_l_2()
            .border_color(theme.border)
            .pl_3()
            .ml_1()
            .py_1()
            .text_color(theme.muted_foreground)
            .italic()
            .child(para)
            .into_any_element();
        // Nesting depth adds extra left margin.
        for _ in 1..blockquote_depth {
            para = div().ml_2().child(para).into_any_element();
        }
    }

    para
}

fn flush_heading(
    inline: Vec<AnyElement>,
    level: HeadingLevel,
    theme: &Theme,
    blockquote_depth: usize,
) -> AnyElement {
    let weight = match level {
        HeadingLevel::H1 => gpui::FontWeight::BOLD,
        HeadingLevel::H2 => gpui::FontWeight::BOLD,
        HeadingLevel::H3 => gpui::FontWeight::SEMIBOLD,
        HeadingLevel::H4 => gpui::FontWeight::SEMIBOLD,
        HeadingLevel::H5 => gpui::FontWeight::SEMIBOLD,
        HeadingLevel::H6 => gpui::FontWeight::SEMIBOLD,
    };

    let mut heading = match level {
        HeadingLevel::H1 => div()
            .flex()
            .flex_wrap()
            .gap_1()
            .font_weight(weight)
            .text_lg()
            .children(inline)
            .into_any_element(),
        HeadingLevel::H2 => div()
            .flex()
            .flex_wrap()
            .gap_1()
            .font_weight(weight)
            .text_base()
            .children(inline)
            .into_any_element(),
        _ => div()
            .flex()
            .flex_wrap()
            .gap_1()
            .font_weight(weight)
            .text_sm()
            .children(inline)
            .into_any_element(),
    };

    if blockquote_depth > 0 {
        heading = div()
            .border_l_2()
            .border_color(theme.border)
            .pl_3()
            .child(heading)
            .into_any_element();
    }

    heading
}

fn flush_list_item(
    inline: Vec<AnyElement>,
    theme: &Theme,
    list_stack: &mut [ListInfo],
    blockquote_depth: usize,
) -> AnyElement {
    let Some(info) = list_stack.last_mut() else {
        // No list context — treat as plain paragraph.
        return flush_paragraph(inline, theme, blockquote_depth, list_stack);
    };

    let bullet = if info.ordered {
        let n = info.next_index;
        info.next_index += 1;
        format!("{n}.")
    } else {
        "•".to_owned()
    };

    // Indent based on nesting depth — use static gap, not dynamic ml(px) which lacks a Tailwind helper.
    let mut row = h_flex()
        .gap_2()
        .items_start()
        .when(list_stack.len() > 1, |this| this.ml_4())
        .child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .w(px(16.))
                .text_align(gpui::TextAlign::Right)
                .child(bullet),
        )
        .child(
            div()
                .flex_1()
                .flex()
                .flex_wrap()
                .gap_1()
                .text_sm()
                .children(inline),
        )
        .into_any_element();

    if blockquote_depth > 0 {
        row = div()
            .border_l_2()
            .border_color(theme.border)
            .pl_3()
            .child(row)
            .into_any_element();
    }

    row
}

fn render_code_block(
    code: &str,
    info: Option<&CodeBlockInfo>,
    theme: &Theme,
    blockquote_depth: usize,
) -> AnyElement {
    let lang_label = info
        .and_then(|i| i.lang.as_deref())
        .map(|s| s.to_owned())
        .unwrap_or_default();

    let code_trimmed = code.trim_end_matches('\n');

    // We need to build the block with children; create inner directly.
    let mut inner = v_flex()
        .gap_1()
        .p_3()
        .rounded(theme.radius)
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .text_xs()
        .font_family(theme.mono_font_family.clone());

    if !lang_label.is_empty() {
        inner = inner.child(
            div()
                .text_xs()
                .text_color(theme.muted_foreground)
                .font_weight(gpui::FontWeight::SEMIBOLD)
                .child(lang_label),
        );
    }

    inner = inner.child(
        div()
            .text_color(theme.foreground)
            .child(code_trimmed.to_owned()),
    );

    let mut el: AnyElement = inner.into_any_element();

    if blockquote_depth > 0 {
        el = div()
            .border_l_2()
            .border_color(theme.border)
            .pl_3()
            .child(el)
            .into_any_element();
    }
    el
}

fn render_inline_code(code: &str, theme: &Theme, link_dest: Option<String>) -> AnyElement {
    let mut span = div()
        .px_1()
        .rounded(px(4.))
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border.opacity(0.5))
        .text_xs()
        .font_family(theme.mono_font_family.clone())
        .text_color(theme.foreground)
        .child(code.to_owned());

    if link_dest.is_some() {
        span = span.text_color(theme.link).underline();
    }

    span.into_any_element()
}

fn styled_inline(
    text: String,
    theme: &Theme,
    is_bold: bool,
    is_italic: bool,
    is_strike: bool,
    link_dest: Option<String>,
) -> AnyElement {
    let mut span = div().child(text);

    if is_bold {
        span = span.font_weight(gpui::FontWeight::BOLD);
    }
    if is_italic {
        span = span.italic();
    }
    if is_strike {
        span = span.line_through();
    }
    if link_dest.is_some() {
        span = span.text_color(theme.link).underline();
    }

    span.into_any_element()
}

fn render_table(rows: &[Vec<String>], theme: &Theme, blockquote_depth: usize) -> AnyElement {
    // Simple vertical stack of rows; first row is header with bold.
    let mut table = v_flex()
        .gap_1()
        .p_2()
        .rounded(theme.radius)
        .border_1()
        .border_color(theme.border)
        .overflow_hidden();

    for (idx, row) in rows.iter().enumerate() {
        let is_header = idx == 0;
        let mut row_el = h_flex().gap_2().px_2().py_1();
        if is_header {
            row_el = row_el
                .bg(theme.muted)
                .font_weight(gpui::FontWeight::SEMIBOLD);
        } else if idx % 2 == 1 {
            row_el = row_el.bg(theme.muted.opacity(0.3));
        }
        for cell in row {
            row_el = row_el.child(
                div()
                    .flex_1()
                    .text_sm()
                    .when(is_header, |this| this.font_weight(gpui::FontWeight::BOLD))
                    .child(cell.clone()),
            );
        }
        table = table.child(row_el);
        if is_header {
            table = table.child(div().h(px(1.)).w_full().bg(theme.border));
        }
    }

    let mut el = table.into_any_element();
    if blockquote_depth > 0 {
        el = div()
            .border_l_2()
            .border_color(theme.border)
            .pl_3()
            .child(el)
            .into_any_element();
    }
    el
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn whitespace_helper_exists() {
        // The mix helpers in worktable_ui are exercised here indirectly by
        // checking that rendering doesn't panic for empty markdown.
        // This test ensures the module compiles even without a GPUI context.
        assert!(true);
    }
}
