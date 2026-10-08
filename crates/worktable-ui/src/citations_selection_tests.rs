use gpui::{
    AppContext as _, Context, FocusHandle, InteractiveElement as _, IntoElement, Modifiers,
    MouseButton, ParentElement as _, Render, Styled as _, TestAppContext, VisualTestContext,
    Window, div, point, px,
};
use gpui_base::TextSelection;
use gpui_component::Root;

use super::{CitationRef, InlineCitations};
use crate::StreamingText;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

struct Answer {
    text: String,
    streaming: bool,
    focus: FocusHandle,
    opened: Arc<AtomicUsize>,
}

impl Render for Answer {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus)
            .w(px(400.))
            .child(if self.streaming {
                StreamingText::new("answer", self.text.clone())
                    .streaming(true)
                    .into_any_element()
            } else {
                let opened = self.opened.clone();
                InlineCitations::new(
                    "answer",
                    self.text.clone(),
                    vec![CitationRef {
                        n: 1,
                        label: "Source".into(),
                        snippet: "Preview".into(),
                        host: "example.com".into(),
                        url: "https://example.com".into(),
                    }],
                )
                .on_open(Arc::new(move |_, _, _, _| {
                    opened.fetch_add(1, Ordering::Relaxed);
                }))
                .into_any_element()
            })
    }
}

fn setup<'a>(
    cx: &'a mut TestAppContext,
    text: &str,
    streaming: bool,
) -> (gpui::Entity<Answer>, &'a mut VisualTestContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        crate::set_reduced_motion(cx, true);
    });
    let mut answer = None;
    let (_, cx) = cx.add_window_view(|window, cx| {
        let view = cx.new(|cx| Answer {
            text: text.to_owned(),
            streaming,
            focus: cx.focus_handle(),
            opened: Arc::new(AtomicUsize::new(0)),
        });
        view.read(cx).focus.clone().focus(window, cx);
        answer = Some(view.clone());
        Root::new(view, window, cx)
    });
    cx.run_until_parked();
    (answer.unwrap(), cx)
}

fn drag(cx: &mut VisualTestContext, first: &'static str, last: &'static str) -> String {
    let first = cx.debug_bounds(first).unwrap();
    let last = cx.debug_bounds(last).unwrap();
    let start = point(first.left() + px(0.1), first.center().y);
    let end = point(last.right() - px(0.1), last.center().y);
    cx.simulate_mouse_down(start, MouseButton::Left, Modifiers::default());
    cx.simulate_mouse_move(end, Some(MouseButton::Left), Modifiers::default());
    cx.simulate_mouse_up(end, MouseButton::Left, Modifiers::default());
    cx.run_until_parked();
    cx.update(|window, cx| {
        let _ = window.draw(cx);
        TextSelection::selected_text(window, cx)
    })
}

#[gpui::test]
fn cited_prose_selection_keeps_spaces_across_atoms_and_blocks(cx: &mut TestAppContext) {
    let (_, cx) = setup(cx, "alpha **beta** gamma\n\nsecond line", false);
    assert_eq!(
        drag(cx, "answer-text-0-0", "answer-text-1-1"),
        "alpha beta gamma\nsecond line"
    );
    cx.simulate_keystrokes("cmd-c");
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "alpha beta gamma\nsecond line"
        )
    });
}

#[gpui::test]
fn streaming_selection_excludes_caret_and_updates_with_text(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, "alpha beta", true);
    assert_eq!(drag(cx, "streaming-text", "streaming-text"), "alpha beta");
    view.update(cx, |view, cx| {
        view.text = "updated answer".into();
        cx.notify();
    });
    cx.run_until_parked();
    assert_eq!(
        drag(cx, "streaming-text", "streaming-text"),
        "updated answer"
    );
}

#[gpui::test]
fn code_and_list_text_participate_in_answer_selection(cx: &mut TestAppContext) {
    let (_, cx) = setup(cx, "- first item\n\n```\nlet value = 1;\n```", false);
    assert_eq!(
        drag(cx, "answer-text-0-0", "answer-code-1"),
        "first item\nlet value = 1;"
    );
}

#[gpui::test]
fn citation_adjacent_prose_copies_and_repeated_chips_stay_interactive(cx: &mut TestAppContext) {
    let (view, cx) = setup(cx, "alpha [1] beta [1] gamma", false);
    assert_eq!(
        drag(cx, "answer-text-0-0", "answer-text-0-4"),
        "alpha beta gamma"
    );
    cx.simulate_keystrokes("cmd-c");
    cx.run_until_parked();
    cx.update(|_, cx| {
        assert_eq!(
            cx.read_from_clipboard().unwrap().text().unwrap(),
            "alpha beta gamma"
        )
    });
    for selector in ["answer-cite-1", "answer-cite-1-1", "answer-ref-1"] {
        let bounds = cx.debug_bounds(selector).unwrap();
        cx.simulate_click(bounds.center(), Modifiers::default());
        cx.run_until_parked();
    }
    view.read_with(cx, |view, _| {
        assert_eq!(view.opened.load(Ordering::Relaxed), 3)
    });
}

#[gpui::test]
fn selection_can_copy_a_single_styled_span_and_wrapped_unicode_prose(cx: &mut TestAppContext) {
    let (_, cx) = setup(
        cx,
        "déjà **κόσμος** and several more words that make this paragraph wrap onto another visual line",
        false,
    );
    assert_eq!(drag(cx, "answer-text-0-1", "answer-text-0-1"), "κόσμος");
    assert_eq!(
        drag(cx, "answer-text-0-0", "answer-text-0-14"),
        "déjà κόσμος and several more words that make this paragraph wrap onto another visual line"
    );
}
