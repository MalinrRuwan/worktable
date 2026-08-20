//! Native WebView wrapper for the Worktable Web build.
//!
//! `cargo run -p worktable-web --bin worktable-webview --features webview`
//! hosts `dist/web/index.html` (the wasm-pack output) in a `wry` WebView.
//! This gives you a desktop app without Node, using the same WASM/WebGPU code
//! that runs in the browser. Falls back to a helpful message if `dist/web`
//! hasn't been built yet.

#[cfg(feature = "webview")]
fn main() {
    use tao::{
        dpi::LogicalSize,
        event::{Event, WindowEvent},
        event_loop::{ControlFlow, EventLoop},
        window::WindowBuilder,
    };
    use wry::WebViewBuilder;

    let dist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../dist/web/index.html");
    let url = if dist.exists() {
        format!("file://{}", dist.canonicalize().unwrap().display())
    } else {
        // Fallback: inline HTML that tells the user to build the web target
        let fallback = r#"data:text/html,
            <html><head><meta charset="utf-8"><style>body{font-family:system-ui;padding:40px;background:#f8fafc;color:#0f172a}</style></head>
            <body><h1>Worktable WebView</h1><p>Run <code>scripts/package-web.sh</code> first to build <code>dist/web</code>.</p>
            <p>Fallback: <code>wasm-pack build crates/worktable-web --target web --out-dir dist/web/pkg</code></p></body></html>"#;
        fallback.to_owned()
    };

    let event_loop = EventLoop::new();
    let window = WindowBuilder::new()
        .with_title("Worktable — WebView (WASM/WebGPU)")
        .with_inner_size(LogicalSize::new(1080, 720))
        .build(&event_loop)
        .unwrap();

    let _webview = WebViewBuilder::new()
        .with_url(url)
        .with_devtools(true)
        .build(&window)
        .unwrap();

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        if let Event::WindowEvent {
            event: WindowEvent::CloseRequested,
            ..
        } = event
        {
            *control_flow = ControlFlow::Exit;
        }
    });
}

#[cfg(not(feature = "webview"))]
fn main() {
    eprintln!("Rebuild with --features webview:");
    eprintln!("  cargo run -p worktable-web --bin worktable-webview --features webview");
    eprintln!("Or build the browser target:");
    eprintln!("  scripts/package-web.sh");
    std::process::exit(1);
}
