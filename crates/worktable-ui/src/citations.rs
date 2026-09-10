//! Inline citations: `[n]` marker chips with hover labels and a source footer.
//!
//! Ported from the MIT-licensed [aiCSS](https://www.aicss.dev)
//! `InlineCitations` component. Markers in the text become small numbered
//! chips that open the source in the browser and reveal its title on hover;
//! the reference list is rendered as a compact footer below the prose.
//!
//! The prose flows as word-level inline atoms so a chip sits exactly where its
//! marker sat — a chip never wraps on its own line, and punctuation that
//! follows a marker stays tight against it. Because of that word layout,
//! citation prose is plain text; markdown formatting is not applied to
//! citation-bearing messages.
//!
//! Components are theme-agnostic: the caller passes [`CitationColors`] and a
//! radius from the active theme.

use std::sync::Arc;

use gpui::{
    App, AppContext as _, Div, ElementId, Hsla, InteractiveElement as _, IntoElement,
    ParentElement, Pixels, Point, Refineable as _, Render, RenderOnce, Role, SharedString,
    StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div, px, rems,
};

/// Handler invoked when a citation is activated, receiving the click position
/// so apps can morph in-app sources (e.g. a `worktable-entry:` URL) from
/// where they were clicked. The default opens `url` in the browser.
pub type CitationOpenHandler =
    Arc<dyn Fn(&str, Point<Pixels>, &mut Window, &mut App) + Send + Sync>;

/// One source behind an inline `[n]` marker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CitationRef {
    /// The marker number used in the text.
    pub n: u32,
    /// Human-readable source title.
    pub label: SharedString,
    /// Short excerpt shown in the hover preview under the label.
    pub snippet: SharedString,
    /// Display host, e.g. `arxiv.org`.
    pub host: SharedString,
    /// External URL opened when the marker or footer row is clicked.
    pub url: SharedString,
}

/// One parsed piece of citation prose.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CitationSegment {
    Text(String),
    Marker(u32),
}

/// Split `text` on `[n]` markers. Malformed or unmatched brackets stay in the
/// text, so nothing is lost when a marker has no reference.
pub fn parse_citations(text: &str) -> Vec<CitationSegment> {
    let bytes = text.as_bytes();
    let mut segments = Vec::new();
    let mut last = 0;
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'['
            && let Some((n, end)) = parse_marker(bytes, index)
        {
            if last < index {
                segments.push(CitationSegment::Text(text[last..index].to_owned()));
            }
            segments.push(CitationSegment::Marker(n));
            last = end;
            index = end;
            continue;
        }
        index += 1;
    }
    if last < text.len() {
        segments.push(CitationSegment::Text(text[last..].to_owned()));
    }
    segments
}

fn parse_marker(bytes: &[u8], start: usize) -> Option<(u32, usize)> {
    let mut index = start + 1;
    let mut n: u32 = 0;
    let mut digits = 0;
    while index < bytes.len() && bytes[index].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add((bytes[index] - b'0') as u32);
        index += 1;
        digits += 1;
        if digits > 9 {
            return None;
        }
    }
    if digits > 0 && index < bytes.len() && bytes[index] == b']' {
        Some((n, index + 1))
    } else {
        None
    }
}

/// A renderable inline piece: a word, a marker chip, or a paragraph break.
/// Splitting prose into words is what lets chips flow inside wrapped text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Atom {
    Word(String),
    Marker(u32),
    Break,
}

/// Split prose into paragraphs (blank-line separated), preserving single
/// line breaks inside each paragraph. Each paragraph renders as its own
/// wrapping row, so the answer keeps its shape without full markdown.
fn blocks(text: &str) -> Vec<Vec<Atom>> {
    let mut blocks = Vec::new();
    for paragraph in text.split("\n\n") {
        let mut atoms = Vec::new();
        let mut lines = paragraph.lines().filter(|line| !line.trim().is_empty());
        if let Some(first) = lines.next() {
            atoms.extend(line_atoms(first));
        }
        for line in lines {
            atoms.push(Atom::Break);
            atoms.extend(line_atoms(line));
        }
        if !atoms.is_empty() {
            blocks.push(atoms);
        }
    }
    blocks
}

fn line_atoms(line: &str) -> Vec<Atom> {
    let mut atoms = Vec::new();
    for segment in parse_citations(line) {
        match segment {
            CitationSegment::Text(text) => {
                atoms.extend(
                    text.split_whitespace()
                        .map(|word| Atom::Word(word.to_owned())),
                );
            }
            CitationSegment::Marker(n) => atoms.push(Atom::Marker(n)),
        }
    }
    atoms
}

/// Flatten parsed segments into inline atoms (paragraphs joined by breaks).
#[cfg(test)]
fn atoms(text: &str) -> Vec<Atom> {
    let mut atoms = Vec::new();
    for (index, block) in blocks(text).into_iter().enumerate() {
        if index > 0 {
            atoms.push(Atom::Break);
        }
        atoms.extend(block);
    }
    atoms
}

/// Whether a space belongs between two adjacent atoms. Spaces sit between
/// words, never before punctuation, and never inside a marker.
fn needs_space(previous: &Atom, next: &Atom) -> bool {
    let Atom::Word(next_word) = next else {
        return false;
    };
    let Some(first) = next_word.chars().next() else {
        return false;
    };
    if !first.is_alphanumeric() {
        return false;
    }
    match previous {
        Atom::Break => false,
        Atom::Marker(_) => true,
        Atom::Word(previous_word) => !matches!(
            previous_word.chars().last(),
            None | Some('(' | '[' | '{' | '"' | '\'' | '“' | '‘' | '—' | '–')
        ),
    }
}

/// Theme-derived colors for the citation chips, tooltip, and footer.
#[derive(Clone, Copy, Debug)]
pub struct CitationColors {
    /// Primary text (footer label, hovered chip text).
    pub foreground: Hsla,
    /// Secondary text (resting chip text, host, separator).
    pub muted: Hsla,
    /// Resting chip surface.
    pub chip_background: Hsla,
    /// Hovered chip surface.
    pub chip_hover_background: Hsla,
    /// Background behind tooltip text (the inverse of `foreground`).
    pub background: Hsla,
    /// Footer separator hairline.
    pub border: Hsla,
}

/// Citation prose with numbered marker chips and a source footer.
///
/// ```ignore
/// InlineCitations::new("answer", text.clone(), citations)
///     .colors(CitationColors { .. })
///     .radius(theme.radius_tokens().sm);
/// ```
#[derive(IntoElement)]
pub struct InlineCitations {
    id: SharedString,
    text: SharedString,
    refs: Vec<CitationRef>,
    colors: CitationColors,
    radius: Pixels,
    open: Option<CitationOpenHandler>,
    style: StyleRefinement,
}

impl InlineCitations {
    pub fn new(
        id: impl Into<SharedString>,
        text: impl Into<SharedString>,
        refs: Vec<CitationRef>,
    ) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
            refs,
            colors: CitationColors {
                foreground: Hsla::default(),
                muted: Hsla::default(),
                chip_background: Hsla::default(),
                chip_hover_background: Hsla::default(),
                background: Hsla::default(),
                border: Hsla::default(),
            },
            radius: px(4.0),
            open: None,
            style: StyleRefinement::default(),
        }
    }

    pub fn colors(mut self, colors: CitationColors) -> Self {
        self.colors = colors;
        self
    }

    /// Chip and tooltip corner radius (theme `radius_tokens().sm`).
    pub fn radius(mut self, radius: Pixels) -> Self {
        self.radius = radius;
        self
    }

    /// Route activation through `handler` instead of opening the URL. The
    /// handler receives the citation's URL (possibly an in-app scheme).
    pub fn on_open(mut self, handler: CitationOpenHandler) -> Self {
        self.open = Some(handler);
        self
    }

    fn reference(&self, n: u32) -> Option<CitationRef> {
        self.refs.iter().find(|reference| reference.n == n).cloned()
    }

    fn chip(
        &self,
        suffix: &str,
        n: u32,
        reference: Option<CitationRef>,
        superscript: bool,
    ) -> impl IntoElement {
        let colors = self.colors;
        let selector = format!("{}-{suffix}-{n}", self.id);
        let mut chip = div()
            .id(ElementId::Name(selector.clone().into()))
            .debug_selector(move || selector)
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .size(rems(0.75))
            .rounded(self.radius)
            .bg(colors.chip_background)
            .text_color(colors.muted)
            .text_size(rems(0.5625))
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .line_height(rems(0.5625))
            .child(n.to_string());
        if superscript {
            // The reference raises the marker like a superscript.
            chip = chip.relative().top(rems(-0.25));
        }
        let Some(reference) = reference else {
            return chip;
        };
        let tooltip_label = reference.label.clone();
        let tooltip_host = reference.host.clone();
        let tooltip_snippet = reference.snippet.clone();
        let tooltip_colors = colors;
        let tooltip_radius = self.radius;
        let url = reference.url;
        let open = self.open.clone();
        let accessible = format!("{} {}", reference.n, reference.label);
        chip = chip
            .cursor_pointer()
            .role(Role::Link)
            .aria_label(accessible)
            .hover(move |style| {
                style
                    .bg(colors.chip_hover_background)
                    .text_color(colors.foreground)
            })
            .on_click(move |event, window, cx| {
                let position = event.position();
                match &open {
                    Some(handler) => handler(&url, position, window, cx),
                    None => cx.open_url(&url),
                }
            })
            .tooltip(move |_window, cx| {
                let label = tooltip_label.clone();
                let host = tooltip_host.clone();
                let snippet = tooltip_snippet.clone();
                cx.new(|_| CitationTooltip {
                    label,
                    host,
                    snippet,
                    colors: tooltip_colors,
                    radius: tooltip_radius,
                })
                .into()
            });
        chip
    }

    fn footer_row(&self, reference: &CitationRef) -> impl IntoElement {
        let colors = self.colors;
        let group = SharedString::from(format!("{}-ref-{}", self.id, reference.n));
        let url = reference.url.clone();
        let open = self.open.clone();
        let accessible = format!("{} {} — {}", reference.n, reference.label, reference.host);
        let selector = format!("{}-ref-{}", self.id, reference.n);
        let row_selector = selector.clone();
        div()
            .id(ElementId::Name(selector.into()))
            .debug_selector(move || row_selector)
            .group(group.clone())
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .w_full()
            .min_w_0()
            .cursor_pointer()
            .role(Role::Link)
            .aria_label(accessible)
            .on_click(move |event, window, cx| {
                let position = event.position();
                match &open {
                    Some(handler) => handler(&url, position, window, cx),
                    None => cx.open_url(&url),
                }
            })
            .child(self.chip("ref", reference.n, None, false))
            .child(
                div()
                    .min_w_0()
                    .text_ellipsis()
                    .text_color(colors.foreground)
                    .child(reference.label.clone()),
            )
            .child(div().flex_shrink_0().text_color(colors.muted).child("·"))
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colors.muted)
                    .group_hover(group.clone(), move |style| {
                        style.text_color(colors.foreground)
                    })
                    .child(reference.host.clone()),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(colors.muted)
                    .opacity(0.45)
                    .group_hover(group, |style| style.opacity(1.0))
                    .child("\u{2197}"),
            )
    }
}

/// The source list under a citation-bearing answer: one numbered row per
/// source with its label and host. Each row routes through the same
/// [`CitationOpenHandler`] as inline chips.
///
/// Use it with a markdown body whose `[n]` markers were rewritten to links
/// (`[n](url)`), so the prose keeps its formatting while markers stay
/// clickable.
#[derive(IntoElement)]
pub struct CitationFooter {
    refs: Vec<CitationRef>,
    colors: CitationColors,
    radius: Pixels,
    open: Option<CitationOpenHandler>,
    style: StyleRefinement,
}

impl CitationFooter {
    pub fn new(refs: Vec<CitationRef>) -> Self {
        Self {
            refs,
            colors: CitationColors {
                foreground: Hsla::default(),
                muted: Hsla::default(),
                chip_background: Hsla::default(),
                chip_hover_background: Hsla::default(),
                background: Hsla::default(),
                border: Hsla::default(),
            },
            radius: px(4.0),
            open: None,
            style: StyleRefinement::default(),
        }
    }

    pub fn colors(mut self, colors: CitationColors) -> Self {
        self.colors = colors;
        self
    }

    pub fn radius(mut self, radius: Pixels) -> Self {
        self.radius = radius;
        self
    }

    pub fn on_open(mut self, handler: CitationOpenHandler) -> Self {
        self.open = Some(handler);
        self
    }
}

impl Styled for CitationFooter {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for CitationFooter {
    fn render(self, _window: &mut Window, cx: &mut App) -> impl IntoElement {
        let colors = self.colors;
        let radius = self.radius;
        let mut column = div()
            .flex()
            .flex_col()
            .w_full()
            .min_w_0()
            .gap_1()
            .pt_2()
            .border_t_1()
            .border_color(colors.border.opacity(0.6))
            .child(
                div()
                    .text_xs()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(colors.muted)
                    .child("Sources"),
            );
        for reference in self.refs.iter().cloned() {
            let open = self.open.clone();
            let url = reference.url.clone();
            let selector = format!("citation-footer-{}", reference.n);
            let row_selector = selector.clone();
            let group = SharedString::from(format!("citation-footer-group-{}", reference.n));
            let accessible = format!("{} {} — {}", reference.n, reference.label, reference.host);
            column = column.child(
                div()
                    .id(ElementId::Name(selector.into()))
                    .debug_selector(move || row_selector)
                    .group(group.clone())
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .w_full()
                    .min_w_0()
                    .cursor_pointer()
                    .role(Role::Link)
                    .aria_label(accessible)
                    .on_click(move |event, window, cx| {
                        let position = event.position();
                        match &open {
                            Some(handler) => handler(&url, position, window, cx),
                            None => cx.open_url(&url),
                        }
                    })
                    .child(number_chip(colors, radius, reference.n, false))
                    .child(
                        div()
                            .min_w_0()
                            .text_ellipsis()
                            .text_color(colors.foreground)
                            .child(reference.label.clone()),
                    )
                    .child(div().flex_shrink_0().text_color(colors.muted).child("·"))
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(colors.muted)
                            .group_hover(group.clone(), move |style| {
                                style.text_color(colors.foreground)
                            })
                            .child(reference.host.clone()),
                    )
                    .child(
                        div()
                            .flex_shrink_0()
                            .text_color(colors.muted)
                            .opacity(0.45)
                            .group_hover(group, |style| style.opacity(1.0))
                            .child("\u{2197}"),
                    ),
            );
        }
        let _ = cx;
        column.style().refine(&self.style);
        column
    }
}

/// The square numbered chip shared by inline markers and footer rows.
fn number_chip(colors: CitationColors, radius: Pixels, n: u32, superscript: bool) -> Div {
    let mut chip = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .size(rems(0.75))
        .rounded(radius)
        .bg(colors.chip_background)
        .text_color(colors.muted)
        .text_size(rems(0.5625))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .line_height(rems(0.5625))
        .child(n.to_string());
    if superscript {
        chip = chip.relative().top(rems(-0.25));
    }
    chip
}

impl Styled for InlineCitations {
    fn style(&mut self) -> &mut StyleRefinement {
        &mut self.style
    }
}

impl RenderOnce for InlineCitations {
    fn render(self, _window: &mut Window, _cx: &mut App) -> impl IntoElement {
        let mut root = div().flex().flex_col().gap_1().w_full().min_w_0();
        for block in blocks(&self.text) {
            let mut prose = div().flex().flex_wrap().items_center().w_full().min_w_0();
            for (index, atom) in block.iter().enumerate() {
                let space_after = block
                    .get(index + 1)
                    .is_some_and(|next| needs_space(atom, next));
                let element: gpui::AnyElement = match atom {
                    Atom::Word(word) => div().min_w_0().child(word.clone()).into_any_element(),
                    Atom::Break => div().w_full().h_0().into_any_element(),
                    Atom::Marker(n) => self
                        .chip("cite", *n, self.reference(*n), true)
                        .into_any_element(),
                };
                prose = if space_after {
                    prose.child(div().mr_1().child(element))
                } else {
                    prose.child(element)
                };
            }
            root = root.child(prose);
        }
        if !self.refs.is_empty() {
            let mut footer = div()
                .flex()
                .flex_col()
                .gap_1()
                .mt_1()
                .pt_2()
                .w_full()
                .min_w_0()
                .border_t_1()
                .border_color(self.colors.border);
            for reference in &self.refs {
                footer = footer.child(self.footer_row(reference));
            }
            root = root.child(footer);
        }
        root.style().refine(&self.style);
        root
    }
}

/// The hover preview shown above a citation chip: the source label, an
/// excerpt of the entry, and its host.
struct CitationTooltip {
    label: SharedString,
    host: SharedString,
    snippet: SharedString,
    colors: CitationColors,
    radius: Pixels,
}

impl Render for CitationTooltip {
    fn render(&mut self, _window: &mut Window, _cx: &mut gpui::Context<Self>) -> impl IntoElement {
        let mut card = div()
            .flex()
            .flex_col()
            .gap_1()
            .max_w(rems(16.0))
            .px_2()
            .py_1p5()
            .rounded(self.radius)
            .border_1()
            .border_color(self.colors.border)
            .bg(self.colors.chip_background)
            .text_color(self.colors.foreground)
            .text_xs()
            .child(
                div()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .child(self.label.clone()),
            );
        if !self.snippet.is_empty() {
            card = card.child(
                div()
                    .text_color(self.colors.muted)
                    .child(self.snippet.clone()),
            );
        }
        if !self.host.is_empty() {
            card = card.child(
                div()
                    .text_color(self.colors.muted)
                    .opacity(0.8)
                    .child(self.host.clone()),
            );
        }
        card
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_markers_into_text_and_number_segments() {
        let segments = parse_citations("Alpha[1] beta[22].");
        assert_eq!(
            segments,
            vec![
                CitationSegment::Text("Alpha".to_owned()),
                CitationSegment::Marker(1),
                CitationSegment::Text(" beta".to_owned()),
                CitationSegment::Marker(22),
                CitationSegment::Text(".".to_owned()),
            ]
        );
    }

    #[test]
    fn malformed_or_empty_markers_stay_literal() {
        assert_eq!(
            parse_citations("no markers here"),
            vec![CitationSegment::Text("no markers here".to_owned())]
        );
        assert_eq!(
            parse_citations("keep [x] and [] and [12x]"),
            vec![CitationSegment::Text(
                "keep [x] and [] and [12x]".to_owned()
            )]
        );
        assert_eq!(parse_citations(""), Vec::new());
    }

    #[test]
    fn unicode_text_splits_on_char_boundaries() {
        let segments = parse_citations("naïve[1] — ok");
        assert_eq!(segments[0], CitationSegment::Text("naïve".to_owned()));
        assert_eq!(segments[1], CitationSegment::Marker(1));
        assert_eq!(segments[2], CitationSegment::Text(" — ok".to_owned()));
    }

    #[test]
    fn atoms_keep_punctuation_tight_around_markers() {
        // "compute[1], though": no space between the word and the chip, no
        // space between the chip and the comma, a space after the comma.
        let atoms = atoms("compute[1], though");
        assert_eq!(
            atoms,
            vec![
                Atom::Word("compute".to_owned()),
                Atom::Marker(1),
                Atom::Word(",".to_owned()),
                Atom::Word("though".to_owned()),
            ]
        );
        assert!(!needs_space(&atoms[0], &atoms[1]), "word → chip is tight");
        assert!(!needs_space(&atoms[1], &atoms[2]), "chip → comma is tight");
        assert!(
            needs_space(&atoms[2], &atoms[3]),
            "comma → word needs a space"
        );
        assert!(
            needs_space(&Atom::Marker(1), &Atom::Word("word".to_owned())),
            "chip → word needs a space"
        );
        assert!(!needs_space(&Atom::Word("(".to_owned()), &atoms[3]));
    }
}
