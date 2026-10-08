//! Inline citations: `[n]` marker chips with hover labels and a source footer.
//!
//! Ported from the MIT-licensed [aiCSS](https://www.aicss.dev)
//! `InlineCitations` component. Markers in the text become small numbered
//! chips that open the source in the browser and reveal its title on hover;
//! the reference list is rendered as a compact footer below the prose.
//!
//! The prose flows as word-level inline atoms so a chip sits exactly where its
//! marker sat — a chip never wraps on its own line, and punctuation that
//! follows a marker stays tight against it. Markdown blocks keep their styling,
//! while their text runs share one native, window-scoped selection participant.
//!
//! Components are theme-agnostic: the caller passes [`CitationColors`] and a
//! radius from the active theme.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;

use gpui::{
    App, AppContext as _, ClickEvent, Div, ElementId, FontWeight, Hsla, InteractiveElement as _,
    IntoElement, ParentElement, Pixels, Point, Refineable as _, Render, RenderOnce, Role,
    SharedString, StatefulInteractiveElement as _, StyleRefinement, Styled, Window, div, px, rems,
};

use crate::markdown::{Block, Inline, needs_space, parse_blocks};

// A cited answer is one selection participant, not one per word. The pinned
// SelectableText::with_handle overwrites the handle's geometry/runs for each
// child, so aggregate them here before projecting the native selection. Runs
// reuse the existing word/chip layout; the clipboard alone adds word spaces
// and block breaks. Chips and source footer rows remain interactive links.
pub(crate) mod selection {
    use super::*;
    use gpui::{
        AnyElement, Bounds, Element, GlobalElementId, HitboxBehavior, InspectorElementId, LayoutId,
        PaintQuad, StyledText, transparent_black,
    };
    use gpui_base::{TextSelectionHandle, TextSelectionRegistration, TextSelectionRun};

    #[derive(Default)]
    pub(crate) struct Runs {
        runs: Vec<TextSelectionRun>,
        separators: Vec<String>,
        ranges: Vec<Option<std::ops::Range<usize>>>,
        copy_ends: Vec<usize>,
    }

    pub(crate) type Document = Rc<RefCell<Runs>>;

    pub(crate) fn text(document: &Document, text: impl Into<SharedString>, after: &str) -> Span {
        let text = text.into();
        Span {
            styled: StyledText::new(text.clone()),
            text,
            after: after.to_owned(),
            document: document.clone(),
            index: 0,
            copy_end: None,
        }
    }

    pub(crate) struct Span {
        styled: StyledText,
        text: SharedString,
        after: String,
        document: Document,
        index: usize,
        copy_end: Option<usize>,
    }

    impl Span {
        pub(crate) fn copy_end(mut self, end: usize) -> Self {
            self.copy_end = Some(end);
            self
        }
    }

    impl IntoElement for Span {
        type Element = Self;
        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Span {
        type RequestLayoutState = ();
        type PrepaintState = ();
        fn id(&self) -> Option<ElementId> {
            None
        }
        fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
            None
        }
        fn request_layout(
            &mut self,
            id: Option<&GlobalElementId>,
            inspector: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, ()) {
            self.styled.request_layout(id, inspector, window, cx)
        }
        fn prepaint(
            &mut self,
            id: Option<&GlobalElementId>,
            inspector: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) {
            self.styled
                .prepaint(id, inspector, bounds, &mut (), window, cx);
            let mut document = self.document.borrow_mut();
            self.index = document.runs.len();
            document.runs.push(
                TextSelectionRun::new(self.text.clone(), self.styled.layout().clone(), bounds)
                    .with_document_order(self.index as u64),
            );
            document.separators.push(self.after.clone());
            document
                .copy_ends
                .push(self.copy_end.unwrap_or(self.text.len()));
        }
        fn paint(
            &mut self,
            id: Option<&GlobalElementId>,
            inspector: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            _: &mut (),
            _: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) {
            let document = self.document.borrow();
            if let Some(Some(range)) = document.ranges.get(self.index) {
                let layout = self.styled.layout();
                if let (Some(start), Some(end)) = (
                    layout.position_for_index(range.start),
                    layout.position_for_index(range.end),
                ) {
                    let mut row = start.y;
                    while row <= end.y {
                        let left = if row == start.y {
                            start.x
                        } else {
                            bounds.left()
                        };
                        let right = if row == end.y { end.x } else { bounds.right() };
                        window.paint_quad(PaintQuad {
                            bounds: Bounds::from_corners(
                                Point::new(left, row),
                                Point::new(right, row + layout.line_height()),
                            ),
                            background: gpui_base::Theme::global(cx).tokens.colors.selection.into(),
                            corner_radii: Default::default(),
                            border_widths: Default::default(),
                            border_color: transparent_black(),
                            border_style: Default::default(),
                        });
                        row += layout.line_height();
                    }
                }
            }
            drop(document);
            self.styled
                .paint(id, inspector, bounds, &mut (), &mut (), window, cx);
        }
    }

    pub(crate) struct Surface {
        pub id: ElementId,
        pub child: AnyElement,
        pub document: Document,
    }

    impl IntoElement for Surface {
        type Element = Self;
        fn into_element(self) -> Self {
            self
        }
    }

    impl Element for Surface {
        type RequestLayoutState = Rc<TextSelectionHandle>;
        type PrepaintState = ();
        fn id(&self) -> Option<ElementId> {
            Some(self.id.clone())
        }
        fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
            None
        }
        fn request_layout(
            &mut self,
            id: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            window: &mut Window,
            cx: &mut App,
        ) -> (LayoutId, Rc<TextSelectionHandle>) {
            let handle = window.with_element_state(
                id.expect("answer selection has a stable id"),
                |retained: Option<Rc<TextSelectionHandle>>, _| {
                    let handle =
                        retained.unwrap_or_else(|| Rc::new(TextSelectionHandle::new("", cx)));
                    (handle.clone(), handle)
                },
            );
            let document = self.document.clone();
            let selection = Rc::downgrade(&handle);
            handle.copy_with(
                move |cx| {
                    let Some(selection) = selection.upgrade() else {
                        return String::new();
                    };
                    let document = document.borrow();
                    let projection = selection.update_runs(&document.runs, cx);
                    copy_runs(&document, projection.ranges())
                },
                cx,
            );
            (self.child.request_layout(window, cx), handle)
        }
        fn prepaint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            bounds: Bounds<Pixels>,
            handle: &mut Rc<TextSelectionHandle>,
            window: &mut Window,
            cx: &mut App,
        ) {
            *self.document.borrow_mut() = Runs::default();
            self.child.prepaint(window, cx);
            let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
            let text_bounds = self
                .document
                .borrow()
                .runs
                .iter()
                .map(TextSelectionRun::bounds)
                .collect();
            handle.register(
                TextSelectionRegistration::new(hitbox, bounds)
                    .with_text_bounds(text_bounds)
                    .with_rendered_element(handle, window, cx),
                window,
                cx,
            );
        }
        fn paint(
            &mut self,
            _: Option<&GlobalElementId>,
            _: Option<&InspectorElementId>,
            _: Bounds<Pixels>,
            handle: &mut Rc<TextSelectionHandle>,
            _: &mut (),
            window: &mut Window,
            cx: &mut App,
        ) {
            {
                let mut document = self.document.borrow_mut();
                document.ranges = handle.update_runs(&document.runs, cx).ranges().to_vec();
            }
            self.child.paint(window, cx);
        }
    }

    fn copy_runs(document: &Runs, ranges: &[Option<std::ops::Range<usize>>]) -> String {
        let mut copied = String::new();
        let last = ranges.iter().rposition(Option::is_some);
        for (index, (run, range)) in document.runs.iter().zip(ranges).enumerate() {
            if let Some(range) = range {
                let end = range.end.min(document.copy_ends[index]);
                if range.start < end {
                    copied.push_str(&run.text()[range.start..end]);
                }
                if range.end == run.text().len() && Some(index) != last {
                    copied.push_str(&document.separators[index]);
                }
            }
        }
        copied
    }
}

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
    /// Chip stroke, tooltip edge, and footer separator.
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
    /// Monospace family for code spans/blocks (the caller's theme owns it).
    mono_font: Option<SharedString>,
    open: Option<CitationOpenHandler>,
    style: StyleRefinement,
    /// Markers emitted so far in this render. A sentence can cite the same
    /// source twice (`[1] … [1]`), and two elements sharing one id also share
    /// their tooltip state — the second marker would then show no preview.
    marker_seq: Cell<usize>,
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
                border: Hsla::default(),
            },
            radius: px(4.0),
            mono_font: None,
            open: None,
            style: StyleRefinement::default(),
            marker_seq: Cell::new(0),
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

    /// Monospace family for code spans and fenced code blocks.
    pub fn mono_font(mut self, family: impl Into<SharedString>) -> Self {
        self.mono_font = Some(family.into());
        self
    }

    /// Route activation through `handler` instead of opening the URL. The
    /// handler receives the citation's URL (possibly an in-app scheme).
    pub fn on_open(mut self, handler: CitationOpenHandler) -> Self {
        self.open = Some(handler);
        self
    }

    /// One wrapping row of inline atoms: a paragraph, heading, or list body.
    fn atom_row(
        &self,
        atoms: &[Inline],
        document: &selection::Document,
        block_index: usize,
    ) -> Div {
        let colors = self.colors;
        let mut row = div().flex().flex_wrap().items_center().w_full().min_w_0();
        for (index, atom) in atoms.iter().enumerate() {
            let space_after = atoms
                .get(index + 1)
                .is_some_and(|next| needs_space(atom, next));
            // Adjacent markers read as one cluster: the later chip tucks under
            // the earlier one instead of sitting a gap apart.
            let grouped = matches!(atom, Inline::Marker(_))
                && index > 0
                && matches!(atoms.get(index - 1), Some(Inline::Marker(_)));
            let element: gpui::AnyElement = match atom {
                Inline::Marker(n) => {
                    let occurrence = self.marker_seq.get();
                    self.marker_seq.set(occurrence + 1);
                    self.chip("cite", *n, occurrence, self.reference(*n), true, grouped)
                        .into_any_element()
                }
                Inline::Text(span) => {
                    let next_text = atoms[index + 1..]
                        .iter()
                        .find(|next| matches!(next, Inline::Text(_)));
                    let after = match next_text {
                        None => "\n",
                        Some(next) if needs_space(atom, next) => " ",
                        Some(_) => "",
                    };
                    let selector = format!("{}-text-{block_index}-{index}", self.id);
                    let mut text = div()
                        .id(ElementId::Name(selector.clone().into()))
                        .debug_selector(move || selector.clone())
                        .min_w_0()
                        .child(selection::text(document, span.text.clone(), after));
                    if span.bold {
                        text = text.font_weight(FontWeight::SEMIBOLD);
                    }
                    if span.italic {
                        text = text.italic();
                    }
                    if span.code {
                        text = text
                            .px_1()
                            .py_0p5()
                            .rounded(self.radius)
                            .bg(colors.chip_background);
                        if let Some(mono) = self.mono_font.clone() {
                            text = text.font_family(mono);
                        }
                    }
                    match &span.link {
                        Some(url) => {
                            let selector = format!("{}-link-{block_index}-{index}", self.id);
                            let url = url.clone();
                            let open = self.open.clone();
                            text.id(ElementId::Name(selector.into()))
                                .cursor_pointer()
                                .underline()
                                .on_click(move |event: &ClickEvent, window, cx| {
                                    let position = event.position();
                                    match &open {
                                        Some(handler) => handler(&url, position, window, cx),
                                        None => cx.open_url(&url),
                                    }
                                })
                                .into_any_element()
                        }
                        None => text.into_any_element(),
                    }
                }
            };
            row = if space_after {
                row.child(div().mr_1().child(element))
            } else {
                row.child(element)
            };
        }
        row
    }

    /// Render one markdown block (paragraph, heading, list, quote, code).
    fn render_block(
        &self,
        block: &Block,
        document: &selection::Document,
        block_index: usize,
    ) -> gpui::AnyElement {
        let colors = self.colors;
        match block {
            Block::Paragraph(atoms) => self
                .atom_row(atoms, document, block_index)
                .into_any_element(),
            Block::Heading { level, content } => {
                let mut heading = div().w_full().min_w_0().font_weight(FontWeight::SEMIBOLD);
                heading = match level {
                    1 => heading.text_lg(),
                    2 => heading.text_base(),
                    _ => heading.text_sm(),
                };
                heading
                    .child(self.atom_row(content, document, block_index))
                    .into_any_element()
            }
            Block::Bullet(atoms) => div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(colors.muted)
                        .child(selection::text(document, "•", " ")),
                )
                .child(self.atom_row(atoms, document, block_index))
                .into_any_element(),
            Block::Numbered { marker, content } => div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .text_color(colors.muted)
                        .child(selection::text(document, marker.clone(), " ")),
                )
                .child(self.atom_row(content, document, block_index))
                .into_any_element(),
            Block::Quote(atoms) => div()
                .flex()
                .flex_row()
                .items_start()
                .gap_2()
                .w_full()
                .min_w_0()
                .child(
                    div()
                        .flex_shrink_0()
                        .self_stretch()
                        .w(px(2.0))
                        .rounded_full()
                        .bg(colors.border),
                )
                .child(self.atom_row(atoms, document, block_index).italic())
                .into_any_element(),
            Block::Code(code) => {
                let mut block = div()
                    .w_full()
                    .min_w_0()
                    .overflow_hidden()
                    .p_2()
                    .rounded(self.radius)
                    .bg(colors.chip_background)
                    .text_xs();
                if let Some(mono) = self.mono_font.clone() {
                    block = block.font_family(mono);
                }
                let selector = format!("{}-code-{block_index}", self.id);
                block = block.debug_selector(move || selector.clone());
                block
                    .child(selection::text(document, code.clone(), "\n"))
                    .into_any_element()
            }
        }
    }

    fn reference(&self, n: u32) -> Option<CitationRef> {
        self.refs.iter().find(|reference| reference.n == n).cloned()
    }

    fn chip(
        &self,
        suffix: &str,
        n: u32,
        occurrence: usize,
        reference: Option<CitationRef>,
        superscript: bool,
        grouped: bool,
    ) -> impl IntoElement {
        let colors = self.colors;
        // Only the first mention of a source keeps the plain selector, so an
        // answer that never repeats a marker has the same element ids as
        // before; repeats get their own identity (and therefore their own
        // tooltip) from the occurrence counter.
        let selector = if occurrence == 0 {
            format!("{}-{suffix}-{n}", self.id)
        } else {
            format!("{}-{suffix}-{n}-{occurrence}", self.id)
        };
        let mut chip = number_chip(colors, self.radius, n, superscript)
            .id(ElementId::Name(selector.clone().into()))
            .debug_selector(move || selector);
        if grouped {
            // Same stroke and baseline on each chip in an overlapping cluster.
            chip = chip.ml(rems(-0.125));
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
            .child(self.chip("ref", reference.n, 0, None, false, false))
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

/// The numbered circle/pill shared by inline markers and every source footer.
fn number_chip(colors: CitationColors, radius: Pixels, n: u32, superscript: bool) -> Div {
    // Keep padding for every number, not just the first nine sources.
    let digits = n.checked_ilog10().unwrap_or(0) + 1;
    let width = rems(0.625 + 0.375 * digits as f32);
    let mut chip = div()
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .h_4()
        .min_w(width)
        .px(rems(0.1875))
        .rounded_full()
        .bg(colors.chip_background)
        .border_1()
        .border_color(colors.border)
        .text_color(colors.muted)
        .text_size(rems(0.6875))
        .font_weight(gpui::FontWeight::SEMIBOLD)
        .line_height(rems(0.875))
        .child(n.to_string());
    if radius == Pixels::ZERO {
        chip = chip.rounded(radius);
    }
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
        let colors = self.colors;
        let document = selection::Document::default();
        let mut root = div()
            .flex()
            .flex_col()
            .gap_2()
            .w_full()
            .min_w_0()
            .text_color(colors.foreground);
        for (index, block) in parse_blocks(&self.text).iter().enumerate() {
            root = root.child(self.render_block(block, &document, index));
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
        selection::Surface {
            id: ElementId::Name(format!("{}-selection", self.id).into()),
            child: root.into_any_element(),
            document,
        }
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
        // A preview is a hint, not the source: every line is clamped to the
        // tooltip's measure, so a long label, an unbreakable URL, or a wall of
        // snippet text can never paint outside the card (or grow it to the
        // window width).
        let mut card = div()
            .debug_selector(|| "citation-tooltip".into())
            .flex()
            .flex_col()
            .gap_1()
            .w(rems(16.0))
            .max_w_full()
            .min_w_0()
            .overflow_hidden()
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
                    .debug_selector(|| "citation-tooltip-label".into())
                    .w_full()
                    .min_w_0()
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .line_clamp(2)
                    .child(self.label.clone()),
            );
        if !self.snippet.is_empty() {
            card = card.child(
                div()
                    .debug_selector(|| "citation-tooltip-snippet".into())
                    .w_full()
                    .min_w_0()
                    .text_color(self.colors.muted)
                    .line_clamp(4)
                    .child(self.snippet.clone()),
            );
        }
        if !self.host.is_empty() {
            card = card.child(
                div()
                    .debug_selector(|| "citation-tooltip-host".into())
                    .w_full()
                    .min_w_0()
                    .text_color(self.colors.muted)
                    .opacity(0.8)
                    .line_clamp(1)
                    .child(self.host.clone()),
            );
        }
        card
    }
}

#[cfg(test)]
#[path = "citations_selection_tests.rs"]
mod selection_tests;

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
    fn markdown_answers_keep_their_markers() {
        // The block parser drives the component's layout; markers survive
        // headings, list items, and emphasis without becoming literal text.
        let blocks = parse_blocks("## Findings\n\n- compute[1], though\n- **bold** point");
        assert!(matches!(blocks[0], Block::Heading { level: 2, .. }));
        match &blocks[1] {
            Block::Bullet(atoms) => {
                assert!(atoms.contains(&Inline::Marker(1)));
            }
            other => panic!("expected a bullet, got {other:?}"),
        }
        match &blocks[2] {
            Block::Bullet(atoms) => assert!(atoms.iter().any(|atom| matches!(
                atom,
                Inline::Text(text) if text.text == "bold" && text.bold
            ))),
            other => panic!("expected a bullet, got {other:?}"),
        }
    }
}
