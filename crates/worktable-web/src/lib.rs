//! Worktable Web — WASM + WebGPU + WebView target.
//!
//! This crate is the browser/WebView counterpart to `worktable-app` (GPUI).
//! It compiles to `wasm32-unknown-unknown` with a WebGPU canvas and a DOM
//! overlay that reuses `worktable-db` (WASM in-memory + localStorage) and
//! `worktable-events` (tokio broadcast stub). The same crate also exposes a
//! native `wry`/`tao` WebView wrapper (`src/bin/webview.rs`) so the web build
//! can be shipped as a desktop app without Node.

use std::cell::RefCell;
use std::rc::Rc;

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::JsCast;
#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;
#[cfg(target_arch = "wasm32")]
use web_sys::{HtmlCanvasElement, HtmlElement, HtmlInputElement, window};

use worktable_db::{Entry, SqliteStore};

// ---------------------------------------------------------------------------
// Entry helpers
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
fn now_ms() -> i64 {
    (js_sys::Date::now()) as i64
}

#[cfg(not(target_arch = "wasm32"))]
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn new_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Web state (shared between render loop and DOM handlers)
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct WebState {
    store: SqliteStore,
    entries: Rc<RefCell<Vec<Entry>>>,
    selected: Rc<RefCell<Option<String>>>,
    query: Rc<RefCell<String>>,
}

impl WebState {
    fn new(store: SqliteStore) -> Self {
        let entries = store.list_entries(500).unwrap_or_default();
        Self {
            store,
            entries: Rc::new(RefCell::new(entries)),
            selected: Rc::new(RefCell::new(None)),
            query: Rc::new(RefCell::new(String::new())),
        }
    }

    fn refresh(&self) {
        if let Ok(list) = self.store.list_entries(500) {
            *self.entries.borrow_mut() = list;
        }
    }

    fn add_entry(&self, kind: &str, content: String) {
        let entry = Entry {
            id: new_id(),
            kind: kind.to_owned(),
            content,
            title: None,
            source: "Web".to_owned(),
            created_at: now_ms(),
        };
        let _ = self.store.insert_entry(&entry);
        self.entries.borrow_mut().insert(0, entry.clone());
        *self.selected.borrow_mut() = Some(entry.id);
    }

    fn delete_selected(&self) {
        if let Some(id) = self.selected.borrow().clone() {
            let _ = self.store.delete_entry(&id);
            self.entries.borrow_mut().retain(|e| e.id != id);
            *self.selected.borrow_mut() = None;
        }
    }

    fn visible_entries(&self) -> Vec<Entry> {
        let q = self.query.borrow().trim().to_lowercase();
        self.entries
            .borrow()
            .iter()
            .filter(|e| {
                if q.is_empty() {
                    return true;
                }
                e.content.to_lowercase().contains(&q)
                    || e.source.to_lowercase().contains(&q)
                    || e.title.as_deref().unwrap_or("").to_lowercase().contains(&q)
            })
            .cloned()
            .collect()
    }
}

// ---------------------------------------------------------------------------
// DOM rendering (pure web-sys, no framework) — WASM only
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
fn render_app(state: &WebState) {
    let document = window().unwrap().document().unwrap();
    let app = document
        .get_element_by_id("worktable-root")
        .unwrap()
        .dyn_into::<HtmlElement>()
        .unwrap();

    // Clear
    while let Some(child) = app.first_child() {
        app.remove_child(&child).unwrap();
    }

    // Header + toolbar
    let header = document.create_element("div").unwrap();
    header.set_attribute("style", "display:flex;align-items:center;justify-content:space-between;padding:12px 16px;border-bottom:1px solid #e2e8f0;background:rgba(255,255,255,0.8);backdrop-filter:blur(8px);position:sticky;top:0;z-index:10;").unwrap();
    header.set_inner_html(
        r#"<div style="display:flex;align-items:center;gap:10px;">
            <div style="width:28px;height:28px;border-radius:8px;background:#0f172a;color:#fff;display:flex;align-items:center;justify-content:center;font-weight:700;">W</div>
            <div><div style="font-weight:600;">Worktable</div><div style="font-size:12px;color:#64748b;">WebGPU • WASM • WebView</div></div>
        </div>
        <div style="font-size:12px;color:#64748b;">pure Rust • no Node</div>"#,
    );
    app.append_child(&header).unwrap();

    // Toolbar: search + new
    let toolbar = document.create_element("div").unwrap();
    toolbar.set_attribute("style", "display:flex;gap:8px;padding:12px 16px;align-items:center;border-bottom:1px solid #f1f5f9;").unwrap();

    let search = document
        .create_element("input")
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    search.set_placeholder("Search entries…");
    search.set_value(&state.query.borrow().clone());
    search
        .set_attribute(
            "style",
            "flex:1;padding:8px 12px;border:1px solid #e2e8f0;border-radius:8px;outline:none;",
        )
        .unwrap();
    {
        let state = state.clone();
        let app_state = state.clone();
        let closure = Closure::wrap(Box::new(move |e: web_sys::Event| {
            let target = e.target().unwrap().dyn_into::<HtmlInputElement>().unwrap();
            *app_state.query.borrow_mut() = target.value();
            render_app(&app_state);
        }) as Box<dyn FnMut(_)>);
        search
            .add_event_listener_with_callback("input", closure.as_ref().unchecked_ref())
            .unwrap();
        closure.forget();
    }
    toolbar.append_child(&search).unwrap();

    for (label, kind) in [("New Note", "text"), ("New Link", "link")] {
        let btn = document.create_element("button").unwrap();
        btn.set_text_content(Some(label));
        btn.set_attribute("style", if kind == "text" {
            "padding:8px 12px;background:#0f172a;color:#fff;border-radius:8px;border:none;cursor:pointer;"
        } else {
            "padding:8px 12px;background:#fff;color:#0f172a;border:1px solid #e2e8f0;border-radius:8px;cursor:pointer;"
        }).unwrap();
        let state = state.clone();
        let closure = Closure::wrap(Box::new(move |_: web_sys::Event| {
            let document = window().unwrap().document().unwrap();
            let input = document
                .get_element_by_id("composer-input")
                .unwrap()
                .dyn_into::<HtmlInputElement>()
                .unwrap();
            let value = input.value().trim().to_owned();
            if value.is_empty() {
                return;
            }
            state.add_entry(kind, value);
            input.set_value("");
            render_app(&state);
        }) as Box<dyn FnMut(_)>);
        btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
            .unwrap();
        closure.forget();
        toolbar.append_child(&btn).unwrap();
    }
    app.append_child(&toolbar).unwrap();

    // Composer
    let composer = document.create_element("div").unwrap();
    composer.set_attribute("style", "display:flex;gap:8px;padding:12px 16px;border-bottom:1px solid #f1f5f9;background:#f8fafc;").unwrap();
    let composer_input = document
        .create_element("input")
        .unwrap()
        .dyn_into::<HtmlInputElement>()
        .unwrap();
    composer_input.set_id("composer-input");
    composer_input.set_placeholder("Write a note or paste a link… (Enter to add)");
    composer_input
        .set_attribute(
            "style",
            "flex:1;padding:8px 12px;border:1px solid #e2e8f0;border-radius:8px;",
        )
        .unwrap();
    {
        let state = state.clone();
        let closure = Closure::wrap(Box::new(move |e: web_sys::KeyboardEvent| {
            if e.key() == "Enter" {
                let target = e.target().unwrap().dyn_into::<HtmlInputElement>().unwrap();
                let value = target.value().trim().to_owned();
                if value.is_empty() {
                    return;
                }
                let kind = if value.starts_with("http://") || value.starts_with("https://") {
                    "link"
                } else {
                    "text"
                };
                state.add_entry(kind, value);
                target.set_value("");
                render_app(&state);
            }
        }) as Box<dyn FnMut(_)>);
        composer_input
            .add_event_listener_with_callback("keydown", closure.as_ref().unchecked_ref())
            .unwrap();
        closure.forget();
    }
    composer.append_child(&composer_input).unwrap();
    app.append_child(&composer).unwrap();

    // Entries
    let list = document.create_element("div").unwrap();
    list.set_attribute("style", "padding:12px 16px;display:flex;flex-direction:column;gap:8px;max-height:60vh;overflow:auto;").unwrap();

    let visible = state.visible_entries();
    if visible.is_empty() {
        let empty = document.create_element("div").unwrap();
        empty
            .set_attribute(
                "style",
                "padding:32px;text-align:center;color:#94a3b8;font-size:14px;",
            )
            .unwrap();
        empty.set_text_content(Some("No entries yet — add a note or link above."));
        list.append_child(&empty).unwrap();
    } else {
        for entry in visible {
            let is_selected = state.selected.borrow().as_deref() == Some(&entry.id);
            let row = document.create_element("div").unwrap();
            row.set_attribute(
                "style",
                &format!(
                    "padding:12px;border-radius:10px;border:1px solid {};background:{};cursor:pointer;display:flex;justify-content:space-between;gap:12px;",
                    if is_selected { "#0f172a" } else { "#e2e8f0" },
                    if is_selected { "#f8fafc" } else { "#fff" }
                ),
            )
            .unwrap();
            let meta = if entry.kind == "link" {
                "🔗 link"
            } else {
                "📝 note"
            };
            row.set_inner_html(&format!(
                r#"<div style="min-width:0;flex:1;">
                    <div style="font-size:12px;color:#64748b;">{} • {} • {}</div>
                    <div style="white-space:pre-wrap;word-break:break-word;font-size:14px;margin-top:4px;">{}</div>
                </div>
                <div style="display:flex;gap:6px;align-items:start;">
                    <button data-act="copy" style="padding:6px 8px;border:1px solid #e2e8f0;background:#fff;border-radius:6px;cursor:pointer;font-size:12px;">Copy</button>
                    <button data-act="del" style="padding:6px 8px;background:#fee2e2;border:1px solid #fecaca;border-radius:6px;cursor:pointer;font-size:12px;">Delete</button>
                </div>"#,
                meta,
                entry.source,
                format_relative(entry.created_at),
                html_escape(&entry.content)
            ));

            // select
            {
                let state = state.clone();
                let id = entry.id.clone();
                let closure = Closure::wrap(Box::new(move |_: web_sys::MouseEvent| {
                    *state.selected.borrow_mut() = Some(id.clone());
                    render_app(&state);
                }) as Box<dyn FnMut(_)>);
                row.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                    .unwrap();
                closure.forget();
            }
            // copy
            if let Some(btn) = row.query_selector("button[data-act=\"copy\"]").unwrap() {
                let content = entry.content.clone();
                let closure = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
                    e.stop_propagation();
                    if let Some(win) = window() {
                        let _ = win.navigator().clipboard().write_text(&content);
                    }
                }) as Box<dyn FnMut(_)>);
                btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                    .unwrap();
                closure.forget();
            }
            // delete
            if let Some(btn) = row.query_selector("button[data-act=\"del\"]").unwrap() {
                let state = state.clone();
                let id = entry.id.clone();
                let closure = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
                    e.stop_propagation();
                    let _ = state.store.delete_entry(&id);
                    state.entries.borrow_mut().retain(|x| x.id != id);
                    if state.selected.borrow().as_deref() == Some(&id) {
                        *state.selected.borrow_mut() = None;
                    }
                    render_app(&state);
                }) as Box<dyn FnMut(_)>);
                btn.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
                    .unwrap();
                closure.forget();
            }

            list.append_child(&row).unwrap();
        }
    }
    app.append_child(&list).unwrap();

    // Footer / WebGPU canvas info
    let footer = document.create_element("div").unwrap();
    footer.set_attribute("style", "padding:12px 16px;border-top:1px solid #f1f5f9;display:flex;justify-content:space-between;font-size:12px;color:#64748b;").unwrap();
    footer.set_inner_html(&format!(
        r##"<span>{} entries • WebGPU canvas below • <a href="#" id="clear-all" style="color:#0f172a;">Clear all</a></span><span>worktable-web v0.1.0</span>"##,
        state.entries.borrow().len()
    ));
    app.append_child(&footer).unwrap();
    if let Some(link) = footer
        .query_selector("#clear-all")
        .unwrap()
        .and_then(|el| el.dyn_into::<HtmlElement>().ok())
    {
        let state = state.clone();
        let closure = Closure::wrap(Box::new(move |e: web_sys::MouseEvent| {
            e.prevent_default();
            for entry in state.entries.borrow().clone() {
                let _ = state.store.delete_entry(&entry.id);
            }
            state.entries.borrow_mut().clear();
            *state.selected.borrow_mut() = None;
            render_app(&state);
        }) as Box<dyn FnMut(_)>);
        link.add_event_listener_with_callback("click", closure.as_ref().unchecked_ref())
            .unwrap();
        closure.forget();
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(target_arch = "wasm32")]
fn format_relative(created_at_ms: i64) -> String {
    let now = now_ms();
    let delta = ((now - created_at_ms).max(0) / 1000) as i64;
    if delta < 10 {
        "just now".to_owned()
    } else if delta < 60 {
        format!("{delta}s ago")
    } else if delta < 3600 {
        format!("{}m ago", delta / 60)
    } else if delta < 86400 {
        format!("{}h ago", delta / 3600)
    } else {
        format!("{}d ago", delta / 86400)
    }
}

// ---------------------------------------------------------------------------
// WebGPU — WASM only, distinct from DOM, runs on the same canvas
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
struct WgpuState {
    surface: wgpu::Surface<'static>,
    device: wgpu::Device,
    queue: wgpu::Queue,
    config: wgpu::SurfaceConfiguration,
    render_pipeline: wgpu::RenderPipeline,
    size: (u32, u32),
}

#[cfg(target_arch = "wasm32")]
impl WgpuState {
    async fn new(canvas: HtmlCanvasElement) -> Result<Self, JsValue> {
        let width = canvas.width().max(1);
        let height = canvas.height().max(1);

        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::all(),
            ..Default::default()
        });

        // SAFETY: canvas lives as long as the WgpuState (leaked to 'static via surface)
        let surface = instance
            .create_surface(wgpu::SurfaceTarget::Canvas(canvas.clone()))
            .map_err(|e| JsValue::from_str(&format!("surface: {e:?}")))?;

        let adapter = instance
            .request_adapter(&wgpu::RequestAdapterOptions {
                power_preference: wgpu::PowerPreference::HighPerformance,
                compatible_surface: Some(&surface),
                force_fallback_adapter: false,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("adapter: {e:?}")))?;

        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("worktable-web-device"),
                required_features: wgpu::Features::empty(),
                required_limits: wgpu::Limits::default(),
                memory_hints: wgpu::MemoryHints::default(),
                trace: wgpu::Trace::Off,
            })
            .await
            .map_err(|e| JsValue::from_str(&format!("device: {e:?}")))?;

        let caps = surface.get_capabilities(&adapter);
        let format = caps
            .formats
            .iter()
            .find(|f| f.is_srgb())
            .copied()
            .unwrap_or(caps.formats[0]);

        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format,
            width,
            height,
            present_mode: wgpu::PresentMode::Fifo,
            alpha_mode: caps.alpha_modes[0],
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("worktable-shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("shader.wgsl").into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("worktable-pipeline-layout"),
            bind_group_layouts: &[],
            push_constant_ranges: &[],
        });

        let render_pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("worktable-pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            surface,
            device,
            queue,
            config,
            render_pipeline,
            size: (width, height),
        })
    }

    fn resize(&mut self, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.size = (width, height);
        self.config.width = width;
        self.config.height = height;
        self.surface.configure(&self.device, &self.config);
    }

    fn render(&mut self) -> Result<(), wgpu::SurfaceError> {
        let frame = self.surface.get_current_texture()?;
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("worktable-encoder"),
            });
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("worktable-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: 0.06,
                            g: 0.09,
                            b: 0.16,
                            a: 1.0,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });
            pass.set_pipeline(&self.render_pipeline);
            pass.draw(0..3, 0..1);
        }
        self.queue.submit(Some(encoder.finish()));
        frame.present();
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
fn setup_wgpu(canvas: HtmlCanvasElement) {
    wasm_bindgen_futures::spawn_local(async move {
        match WgpuState::new(canvas.clone()).await {
            Ok(mut state) => {
                log::info!("WebGPU initialized {}x{}", state.size.0, state.size.1);

                // Handle resize via window resize + devicePixelRatio
                let win = window().unwrap();
                let canvas_clone = canvas.clone();
                let closure = Closure::wrap(Box::new(move || {
                    let dpr = win.device_pixel_ratio();
                    let w = (canvas_clone.client_width() as f64 * dpr) as u32;
                    let h = (canvas_clone.client_height() as f64 * dpr) as u32;
                    // state resize will be handled in render loop polling; keep canvas size in sync
                    canvas_clone.set_width(w.max(1));
                    canvas_clone.set_height(h.max(1));
                }) as Box<dyn FnMut()>);
                window()
                    .unwrap()
                    .add_event_listener_with_callback("resize", closure.as_ref().unchecked_ref())
                    .unwrap();
                closure.forget();

                // Render loop via requestAnimationFrame
                let f: Rc<RefCell<Option<Closure<dyn FnMut()>>>> = Rc::new(RefCell::new(None));
                let g = f.clone();
                let mut wgpu_state = state;
                let canvas_for_resize = canvas.clone();
                *g.borrow_mut() = Some(Closure::wrap(Box::new(move || {
                    // sync canvas size to CSS size
                    let dpr = window().unwrap().device_pixel_ratio();
                    let css_w = canvas_for_resize.client_width() as u32;
                    let css_h = canvas_for_resize.client_height() as u32;
                    let w = ((css_w as f64) * dpr) as u32;
                    let h = ((css_h as f64) * dpr) as u32;
                    if w != wgpu_state.size.0 || h != wgpu_state.size.1 {
                        wgpu_state.resize(w.max(1), h.max(1));
                        canvas_for_resize.set_width(w.max(1));
                        canvas_for_resize.set_height(h.max(1));
                    }
                    match wgpu_state.render() {
                        Ok(_) => {}
                        Err(wgpu::SurfaceError::Lost | wgpu::SurfaceError::Outdated) => {
                            wgpu_state.resize(wgpu_state.size.0, wgpu_state.size.1)
                        }
                        Err(e) => log::warn!("wgpu render error: {e:?}"),
                    }
                    // schedule next frame
                    let _ = window().unwrap().request_animation_frame(
                        f.borrow().as_ref().unwrap().as_ref().unchecked_ref(),
                    );
                }) as Box<dyn FnMut()>));
                let _ = window()
                    .unwrap()
                    .request_animation_frame(g.borrow().as_ref().unwrap().as_ref().unchecked_ref());
            }
            Err(e) => {
                log::error!("WebGPU init failed: {e:?} — falling back to 2D canvas");
                let ctx = canvas
                    .get_context("2d")
                    .unwrap()
                    .unwrap()
                    .dyn_into::<web_sys::CanvasRenderingContext2d>()
                    .unwrap();
                ctx.set_fill_style_str("#0f172a");
                ctx.fill_rect(0.0, 0.0, canvas.width() as f64, canvas.height() as f64);
                ctx.set_fill_style_str("#38bdf8");
                ctx.set_font("600 28px system-ui");
                ctx.fill_text("Worktable — WebGPU unavailable", 24.0, 48.0)
                    .unwrap();
                ctx.set_fill_style_str("#e2e8f0");
                ctx.set_font("14px system-ui");
                ctx.fill_text(
                    "Enable WebGPU in your browser or try Chrome/Edge 113+",
                    24.0,
                    72.0,
                )
                .unwrap();
            }
        }
    });
}

// ---------------------------------------------------------------------------
// WASM entry — only on wasm32
// ---------------------------------------------------------------------------

#[cfg(target_arch = "wasm32")]
#[wasm_bindgen(start)]
pub fn main() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();
    wasm_logger::init(wasm_logger::Config::default());
    log::info!("worktable-web v0.1.0 — WASM/WebGPU booting");

    let window = window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;

    // Ensure root + canvas exist (index.html provides them, but create fallback)
    if document.get_element_by_id("worktable-root").is_none() {
        let root = document.create_element("div").unwrap();
        root.set_id("worktable-root");
        document.body().unwrap().append_child(&root).unwrap();
    }
    if document.get_element_by_id("worktable-canvas").is_none() {
        let canvas = document
            .create_element("canvas")
            .unwrap()
            .dyn_into::<HtmlCanvasElement>()
            .unwrap();
        canvas.set_id("worktable-canvas");
        canvas
            .set_attribute(
                "style",
                "width:100%;height:220px;display:block;background:#0f172a;",
            )
            .unwrap();
        document
            .get_element_by_id("worktable-root")
            .unwrap()
            .append_child(&canvas)
            .unwrap();
    }

    // Style injection
    if document.get_element_by_id("worktable-style").is_none() {
        let style = document.create_element("style").unwrap();
        style.set_id("worktable-style");
        style.set_inner_html(include_str!("style.css"));
        document.head().unwrap().append_child(&style).unwrap();
    }

    let canvas = document
        .get_element_by_id("worktable-canvas")
        .unwrap()
        .dyn_into::<HtmlCanvasElement>()
        .unwrap();

    // Make canvas crisp
    let dpr = window.device_pixel_ratio();
    let css_w = canvas.client_width().max(800) as f64;
    let css_h = canvas.client_height().max(220) as f64;
    canvas.set_width((css_w * dpr) as u32);
    canvas.set_height((css_h * dpr) as u32);

    setup_wgpu(canvas);

    // Init storage (WASM in-memory + localStorage)
    let store = SqliteStore::connect("worktable-web").expect("wasm store");
    let _ = store.migrate();
    let state = WebState::new(store);
    render_app(&state);

    // Expose for console debugging
    // SAFETY: keep state alive
    std::mem::forget(state);

    Ok(())
}

// Allow `cargo run -p worktable-web` natively to print help
#[cfg(not(target_arch = "wasm32"))]
pub fn run_native_help() {
    println!("worktable-web is a WASM/WebGPU target.");
    println!("  wasm: wasm-pack build crates/worktable-web --target web --out-dir pkg");
    println!("  web : scripts/package-web.sh");
    println!("  webview: cargo run -p worktable-web --bin worktable-webview --features webview");
}
