use std::{collections::BTreeMap, env, path::Path, sync::Arc};

use anyhow::Context as _;
use gpui::{
    App, Application, Bounds, Context, Render, Subscription, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, rgb, size,
};
use tokio::runtime::{Builder, Runtime};
use worktable_ai::{WorktableEntry, WorktableRuntime};

const DEFAULT_WORKER_PATH: &str = "agent/dist/worker.js";

struct AppBootstrap {
    tokio: Arc<Runtime>,
    service: Option<Arc<WorktableRuntime>>,
    entries: Vec<WorktableEntry>,
}

struct WorktableView {
    _tokio: Arc<Runtime>,
    _service: Option<Arc<WorktableRuntime>>,
    _quit_subscription: Option<Subscription>,
    entries: Vec<WorktableEntry>,
}

impl WorktableView {
    fn new(bootstrap: AppBootstrap, quit_subscription: Option<Subscription>) -> Self {
        Self {
            _tokio: bootstrap.tokio,
            _service: bootstrap.service,
            _quit_subscription: quit_subscription,
            entries: bootstrap.entries,
        }
    }
}

impl Render for WorktableView {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .flex()
            .flex_col()
            .bg(rgb(0x111312))
            .p_8()
            .text_color(rgb(0xf0f2ee))
            .child(
                div()
                    .flex()
                    .justify_between()
                    .items_end()
                    .border_b_1()
                    .border_color(rgb(0x2a302c))
                    .pb_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap_2()
                            .child(
                                div()
                                    .text_size(px(30.))
                                    .font_weight(gpui::FontWeight::BOLD)
                                    .child("Worktable"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(rgb(0x8d968f))
                                    .child("Your saved entries"),
                            ),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(rgb(0x8d968f))
                            .child(entry_count(self.entries.len())),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .pt_6()
                    .child(render_entry_list(&self.entries)),
            )
    }
}

fn render_entry_list(entries: &[WorktableEntry]) -> impl IntoElement {
    let mut list = div().flex().flex_col();

    if entries.is_empty() {
        return list
            .flex_1()
            .items_center()
            .justify_center()
            .text_color(rgb(0x8d968f))
            .child("No entries yet");
    }

    for entry in entries {
        list = list.child(render_entry(entry));
    }

    list
}

fn render_entry(entry: &WorktableEntry) -> impl IntoElement {
    let heading = entry.title.as_deref().unwrap_or(&entry.content);
    let kind = entry_kind(&entry.kind);

    let mut row = div()
        .flex()
        .flex_col()
        .gap_2()
        .border_b_1()
        .border_color(rgb(0x2a302c))
        .py_5()
        .child(
            div()
                .flex()
                .items_center()
                .justify_between()
                .child(div().text_xs().text_color(rgb(0xa8cbb4)).child(kind))
                .child(
                    div()
                        .text_xs()
                        .text_color(rgb(0x657068))
                        .child(entry.source.clone()),
                ),
        )
        .child(
            div()
                .text_lg()
                .text_color(rgb(0xf0f2ee))
                .child(heading.to_owned()),
        );

    if entry.title.is_some() {
        row = row.child(
            div()
                .text_sm()
                .text_color(rgb(0x9ba59d))
                .child(entry.content.clone()),
        );
    }

    row
}

fn entry_kind(kind: &str) -> &'static str {
    match kind {
        "text" => "TEXT",
        "link" => "LINK",
        "image" => "IMAGE",
        _ => "ENTRY",
    }
}

fn entry_count(count: usize) -> String {
    match count {
        1 => "1 entry".to_owned(),
        count => format!("{count} entries"),
    }
}

fn main() -> anyhow::Result<()> {
    let tokio = Arc::new(
        Builder::new_multi_thread()
            .enable_all()
            .build()
            .context("failed to create Worktable Tokio runtime")?,
    );
    let bootstrap = bootstrap(tokio);

    Application::new().run(move |cx: &mut App| {
        let quit_subscription = bootstrap.service.clone().map(|service| {
            let tokio = bootstrap.tokio.clone();
            cx.on_app_quit(move |_| {
                let service = service.clone();
                let tokio = tokio.clone();
                async move {
                    let _ = tokio.block_on(service.shutdown());
                }
            })
        });

        let bounds = Bounds::centered(None, size(px(980.), px(680.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..WindowOptions::default()
            },
            move |_, cx| cx.new(|_| WorktableView::new(bootstrap, quit_subscription)),
        )
        .expect("failed to open Worktable window");
        cx.activate(true);
    });

    Ok(())
}

fn bootstrap(tokio: Arc<Runtime>) -> AppBootstrap {
    let database_url = env::var("TURSO_DATABASE_URL");
    let auth_token = env::var("TURSO_AUTH_TOKEN");

    let (database_url, auth_token) = match (database_url, auth_token) {
        (Ok(database_url), Ok(auth_token)) => (database_url, auth_token),
        (Err(_), _) | (_, Err(_)) => {
            eprintln!("Turso is not configured; Worktable started without persisted entries");
            return AppBootstrap {
                tokio,
                service: None,
                entries: Vec::new(),
            };
        }
    };

    let service = match tokio.block_on(WorktableRuntime::connect(&database_url, &auth_token)) {
        Ok(service) => Arc::new(service),
        Err(error) => {
            eprintln!("failed to connect Worktable to Turso: {error:#}");
            return AppBootstrap {
                tokio,
                service: None,
                entries: Vec::new(),
            };
        }
    };

    let entries = match tokio.block_on(service.list_entries(100)) {
        Ok(entries) => entries,
        Err(error) => {
            eprintln!("failed to load Worktable entries: {error:#}");
            Vec::new()
        }
    };

    let worker_path =
        env::var("WORKTABLE_PI_WORKER").unwrap_or_else(|_| DEFAULT_WORKER_PATH.to_owned());
    if !Path::new(&worker_path).exists() {
        eprintln!(
            "AI worker is not built; run `cd agent && npm run build` or set WORKTABLE_PI_WORKER (currently `{worker_path}`)"
        );
    } else {
        let worker_command =
            env::var("WORKTABLE_PI_WORKER_COMMAND").unwrap_or_else(|_| "node".to_owned());
        let environment = worker_environment();
        if let Err(error) =
            tokio.block_on(service.start_ai_worker(&worker_command, vec![worker_path], environment))
        {
            eprintln!("failed to start the AgentOS Pi worker: {error:#}");
        }
    }

    AppBootstrap {
        tokio,
        service: Some(service),
        entries,
    }
}

fn worker_environment() -> BTreeMap<String, String> {
    const FORWARDED_VARS: &[&str] = &[
        "PI_PROVIDER",
        "PI_MODEL",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GOOGLE_API_KEY",
        "GEMINI_API_KEY",
        "OPENROUTER_API_KEY",
        "MISTRAL_API_KEY",
        "XAI_API_KEY",
        "GROQ_API_KEY",
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_REGION",
    ];

    FORWARDED_VARS
        .iter()
        .filter_map(|name| env::var(name).ok().map(|value| ((*name).to_owned(), value)))
        .collect()
}
