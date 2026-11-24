//! HarmonyOS host for the LightCraft editor.
//!
//! Architecture (mirrors the sibling `*-hmos` projects): ArkTS (XComponent + UIAbility) →
//! C++ N-API bridge (`entry/src/main/cpp`) → this Rust worker. The worker owns the single
//! `LightcraftApp`, runs its `logic`/`ui` pass on every frame, and presents through
//! `egui-wgpu` on a `wgpu` GL (GLES) surface created over the HarmonyOS `OHNativeWindow`.
//!
//! LightCraft's `crates/ui-egui` is deliberately eframe-free: the desktop and web apps are the
//! only things that depend on `eframe`, and this host drives `LightcraftApp::logic(ctx)` and
//! `LightcraftApp::ui(&mut Ui)` directly — no upstream patch required for the frame loop.

mod appearance;
mod bridge;
mod input;
mod render;

use appearance::Appearance;
use input::{Input, State as InputState};
use lightcraft_ui_egui::{LightcraftApp, Services};
use render::Render;
use std::sync::mpsc;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static SEND: OnceLock<mpsc::Sender<Message>> = OnceLock::new();

#[derive(Clone)]
enum Message {
    Attach(usize, u32, u32),
    Resize(u32, u32),
    Detach,
    Frame,
    /// The egui repaint callback fired (an animation / state change asked for another frame).
    Wake,
    Active(bool),
    Input(Vec<u8>),
    SystemTheme(String),
    OpenFiles(Vec<u8>),
    SavePreset(String),
    Export(String, Vec<u8>),
    OpenLibrary,
    Close,
}

fn send(message: Message) {
    if let Some(s) = SEND.get() {
        let _ = s.send(message);
    }
}

#[derive(Default)]
struct Config {
    files: String,
    documents: String,
    scale: f32,
    language: String,
    system_theme: String,
    device_type: String,
    width_vp: u32,
}

impl Config {
    fn from_json(json: &[u8]) -> Option<Self> {
        let v: serde_json::Value = serde_json::from_slice(json).ok()?;
        Some(Self {
            files: v["files"].as_str().unwrap_or_default().to_string(),
            documents: v["documents"].as_str().unwrap_or_default().to_string(),
            scale: v["scale"].as_f64().unwrap_or(1.0) as f32,
            language: v["language"].as_str().unwrap_or_default().to_string(),
            system_theme: v["systemTheme"].as_str().unwrap_or_default().to_string(),
            device_type: v["deviceType"].as_str().unwrap_or_default().to_string(),
            width_vp: v["widthVp"].as_u64().unwrap_or(0) as u32,
        })
    }
}

/// Build the platform `Services` the UI crate calls for file I/O and system actions.
fn services(config: &Config, appearance: &Appearance) -> Services {
    let files_dir = config.files.clone();
    let documents_dir = config.documents.clone();
    Services {
        // File ▸ Open Photos… / import: the platform picker returns a list of URIs.
        pick_files: Some(Box::new(move || match bridge::request("open", "", &[]) {
            Ok((json, _)) => parse_paths(&json),
            Err(_) => Vec::new(),
        })),
        // Settings ▸ General ▸ Open Library… and the first-launch library chooser.
        pick_folder: Some(Box::new(move || match bridge::request("open_library", "", &[]) {
            Ok((path, _)) if !path.is_empty() => Some(path),
            _ => None,
        })),
        // Save a developed export to a path the platform picked; bytes travel in `data`.
        write: Some(Box::new(move |name, bytes| {
            bridge::request("export", name, bytes).map(|_| ())
        })),
        // File ▸ Save Preset…
        save_preset_file: Some(Box::new(|suggested| match bridge::request("save_preset", suggested, &[]) {
            Ok((path, _)) if !path.is_empty() => Some(path),
            _ => None,
        })),
        // Help ▸ links / About: open in the system browser.
        open_url: Some(Box::new(|url| {
            bridge::notify(0, "url", url, &[]);
            Ok(())
        })),
        system_theme: Some(appearance.service()),
        ..Default::default()
    }
}

fn parse_paths(json: &str) -> Vec<String> {
    if let Ok(v) = serde_json::from_str::<Vec<String>>(json) {
        return v;
    }
    if !json.is_empty() {
        return vec![json.to_string()];
    }
    Vec::new()
}

// SAFETY: C++ passes a valid pointer/length for the duration of the call; copy immediately.
unsafe fn bytes<'a>(ptr: *const u8, len: usize) -> Vec<u8> {
    if len > isize::MAX as usize || (ptr.is_null() && len != 0) {
        return Vec::new();
    }
    if len == 0 {
        return Vec::new();
    }
    unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec()
}

fn worker(config: Config, rx: mpsc::Receiver<Message>) {
    bridge::notify(0, "stage", "Rust worker entered", &[]);
    let appearance = appearance::Appearance::from_token(&config.system_theme);

    // System-language auto-adaptation. The ArkTS host already sends the device language
    // (i18n.System.getSystemLanguage(), e.g. "zh-Hans-CN" / "en-US") in `config.language`.
    // Resolve it to a shipped locale and install it as the "Auto" default BEFORE the UI state is
    // first constructed — so a Chinese device starts in 简体中文 and an English one stays English.
    // A stored user choice (Settings ▸ Language) is persisted to disk and takes precedence over
    // this default on later launches. An empty / unrecognised tag leaves the default (English).
    let lang_tag = config.language.trim();
    if !lang_tag.is_empty() {
        match lightcraft_ui_egui::i18n::Locale::parse_tag(lang_tag) {
            Some(locale) => {
                bridge::notify(0, "stage", &format!("language tag '{}' -> {:?}", lang_tag, locale), &[]);
                lightcraft_ui_egui::i18n::set_default_language(locale);
            }
            None => bridge::notify(0, "stage", &format!("language tag '{}' not a shipped locale, using default", lang_tag), &[]),
        }
    }

    let mut app = LightcraftApp::new(lightcraft_engine::Session::new(), services(&config, &appearance));

    let ctx = egui::Context::default();
    // egui asks for another frame (animation / state change); route it to a vsync request.
    ctx.set_request_repaint_callback(|_info| send(Message::Wake));
    let mut render: Option<Render> = None;
    let mut input_state = InputState::default();
    let mut active = true;
    let mut started = false;

    loop {
        let message = match rx.recv() {
            Ok(m) => m,
            Err(_) => break,
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(
            || -> Result<(), String> {
                match message {
                    Message::Attach(ptr, w, h) => {
                        if render.is_none() {
                            bridge::notify(0, "stage", "creating native GPU surface", &[]);
                            let r = Render::new(std::sync::Arc::new(render::Window(ptr)), w, h)?;
                            started = true;
                            render = Some(r);
                        } else if let Some(r) = render.as_mut() {
                            let _ = r.attach(std::sync::Arc::new(render::Window(ptr)), w, h);
                        }
                        bridge::resume();
                        request_frame();
                        bridge::notify(0, "ready", "LightCraft native egui / wgpu GLES", &[]);
                    }
                    Message::Resize(w, h) => {
                        if let Some(r) = render.as_mut() {
                            r.resize(w, h)?;
                            request_frame();
                        }
                    }
                    Message::Wake => {
                        request_frame();
                    }
                    Message::Detach => {
                        if let Some(r) = render.as_mut() {
                            r.detach();
                        }
                    }
                    Message::Frame => {
                        static FRAME: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                        let n = FRAME.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        if n < 5 || n % 60 == 0 {
                            bridge::notify(0, "stage", &format!(
                                "frame #{}: active={} started={} render={}",
                                n, active, started, render.is_some()
                            ), &[]);
                        }
                        if !active || !started || render.is_none() {
                            return Ok(());
                        }
                        let Some(r) = render.as_mut() else {
                            return Ok(());
                        };
                        let scale = config.scale;
                        let mut raw = egui::RawInput {
                            // Cap the font atlas at the GPU's real 2D limit (8192 here); 16384
                            // exceeds it and makes egui-wgpu's `create_texture` fail validation.
                            max_texture_side: Some(r.max_texture_side as usize),
                            ..Default::default()
                        };
                        raw.events.extend(input_state.take_pending());
                        let info = raw.viewports.entry(egui::ViewportId::ROOT).or_default();
                        info.native_pixels_per_point = Some(scale);
                        // Give egui the real available area (points). AniCraft (working) sets this;
                        // without it egui falls back to a "very big area" default that can push
                        // panels out of the composited surface. points = physical_px / ppp.
                        let s = scale.max(0.01);
                        raw.screen_rect = Some(egui::Rect::from_min_max(
                            egui::pos2(0.0, 0.0),
                            egui::pos2(
                                r.config.width.max(1) as f32 / s,
                                r.config.height.max(1) as f32 / s,
                            ),
                        ));
                        let app = &mut app;
                        app.raw_input_hook(&ctx, &mut raw);
                        let mut full = ctx.run_ui(raw, |ui| {
                            app.logic(ui.ctx());
                            app.ui(ui);
                        });
                        let (w, h) = (r.config.width, r.config.height);
                        let shape_count = full.shapes.len();
                        let tex_count = full.textures_delta.set.len();
                        let result = r.draw(&ctx, &full.textures_delta, full.shapes, scale)?;
                        if n < 5 || n % 60 == 0 {
                            bridge::notify(0, "stage", &format!(
                                "frame #{} draw -> {} ({}x{}) shapes={} textures={}",
                                n,
                                match result {
                                    render::Present::Presented => "Presented",
                                    render::Present::NeedsReconfigure => "NeedsReconfigure",
                                    render::Present::Skipped => "Skipped",
                                },
                                w, h, shape_count, tex_count
                            ), &[]);
                        }
                        if result == render::Present::NeedsReconfigure {
                            r.resize(w.max(1), h.max(1))?;
                        }
                        // Keep the render loop alive: a presented frame schedules the next vsync
                        // (the bridge de-duplicates), and egui's repaint callback does the same.
                        request_frame();
                        let commands = full
                            .viewport_output
                            .remove(&egui::ViewportId::ROOT)
                            .map(|v| v.commands)
                            .unwrap_or_default();
                        if commands.iter().any(|c| matches!(c, egui::ViewportCommand::Close)) {
                            return Ok(());
                        }
                    }
                    Message::Active(a) => {
                        active = a;
                        if a {
                            request_frame();
                        }
                    }
                    Message::Input(json) => {
                        if let Ok(json) = std::str::from_utf8(&json) {
                            if let Ok(parsed) = serde_json::from_str::<Input>(json) {
                                if parsed.kind == "system_theme" && !parsed.theme.is_empty() {
                                    appearance.publish_token(&parsed.theme);
                                    request_frame();
                                } else {
                                    input_state.apply(&parsed);
                                    request_frame();
                                }
                            }
                        }
                    }
                    Message::SystemTheme(token) => {
                        appearance.publish_token(&token);
                    }
                    Message::OpenFiles(json) => {
                        // Legacy: not used; open files now go through the "open" request.
                        let _ = json;
                    }
                    Message::SavePreset(_) => {}
                    Message::Export(_, _) => {}
                    Message::OpenLibrary => {}
                    Message::Close => {
                        if !app.shutdown(Duration::from_secs(2)) {
                            bridge::notify(0, "diagnostic", "shutdown: background work still running", &[]);
                        }
                        bridge::cancel();
                        return Err("closed".into());
                    }
                }
                Ok(())
            },
        ));
        let outcome: Result<(), String> = match result {
            Ok(inner) => inner,
            Err(pay) => {
                let msg = if let Some(s) = pay.downcast_ref::<&str>() {
                    (*s).to_string()
                } else if let Some(s) = pay.downcast_ref::<String>() {
                    s.clone()
                } else {
                    "unknown panic payload".to_string()
                };
                let full = format!("panic in worker: {msg}");
                // hilog splits on newlines and truncates; collapse to one line so the real
                // wgpu "Caused by" detail is visible, and also dump to the sandbox.
                let oneline = full.replace('\n', " | ").replace('\r', "");
                bridge::notify(0, "error", &oneline, &[]);
                let _ = std::fs::write(format!("{}/lightcraft_panic.txt", config.files), &full);
                Err(full)
            }
        };
        if let Err(e) = outcome {
            if e != "closed" {
                bridge::notify(0, "error", &e, &[]);
            }
            break;
        }
    }
}

/// Ask the C++ bridge for the next vsync-driven frame. The bridge de-duplicates pending
/// requests, so calling it from several sources (attach, input, repaint callback) is safe.
#[cfg(target_env = "ohos")]
unsafe extern "C" {
    fn lc_request_vsync();
}
#[cfg(target_env = "ohos")]
fn request_frame() {
    unsafe { lc_request_vsync(); }
}
#[cfg(not(target_env = "ohos"))]
fn request_frame() {}

// ---- extern "C" entry points called from the C++ bridge ----
// `#[unsafe(no_mangle)]` is required: `extern "C"` only sets the calling ABI, it does NOT control the
// symbol name. Without it the symbols are Rust-mangled (e.g. `_RNv...7lc_init`) and the C++
// bridge's bare `lc_init` reference can never resolve at link time.

#[unsafe(no_mangle)]
pub extern "C" fn lc_init(config_json: *const u8, config_len: usize, notify: bridge::Notify) -> bool {
    bridge::init(notify);
    let json = unsafe { bytes(config_json, config_len) };
    let Some(config) = Config::from_json(&json) else {
        return false;
    };
    let (tx, rx) = mpsc::channel();
    let ok = SEND.set(tx).is_ok();
    if !ok {
        return false;
    }
    std::thread::Builder::new().name("lightcraft-worker".into()).spawn(move || worker(config, rx)).is_ok()
}

#[unsafe(no_mangle)]
pub extern "C" fn lc_attach(ptr: usize, w: u32, h: u32) {
    send(Message::Attach(ptr, w, h));
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_resize(w: u32, h: u32) {
    send(Message::Resize(w, h));
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_detach() {
    send(Message::Detach);
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_frame() {
    send(Message::Frame);
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_input(json: *const u8, len: usize) {
    send(Message::Input(unsafe { bytes(json, len) }));
}
/// Deliver an ArkTS answer straight to the worker waiting inside `bridge::request` (which is
/// blocked on its own reply channel, so this must NOT round-trip through the worker queue).
#[unsafe(no_mangle)]
pub extern "C" fn lc_reply(id: u64, ok: bool, name: *const u8, name_len: usize, data: *const u8, data_len: usize) {
    let name = unsafe { bytes(name, name_len) };
    let data = unsafe { bytes(data, data_len) };
    if ok {
        bridge::reply(id, Ok((String::from_utf8_lossy(&name).into(), data)));
    } else {
        bridge::reply(id, Err(String::from_utf8_lossy(&name).into()));
    }
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_active(active: bool) {
    send(Message::Active(active));
}
#[unsafe(no_mangle)]
pub extern "C" fn lc_close() {
    send(Message::Close);
}
