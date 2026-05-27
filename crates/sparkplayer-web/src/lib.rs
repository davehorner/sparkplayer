//! Browser/WASM entry point for SparkPlayer, rendered with Ratzilla.
//!
//! Wiring:
//! - One hidden `<video>` element decodes/plays the media; its audio routes
//!   through a Web Audio graph ([`audio`]) that taps samples for the visualizer,
//!   and it doubles as the on-screen picture, floated over the ratzilla canvas.
//! - An `<img>` element does the same for album art.
//! - Media comes from a fetched `manifest.json` (web-playlist mode) or from
//!   user-picked local files (the `#file-input` element).

mod album_art;
mod audio;
mod library;
mod video;

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use wasm_bindgen::prelude::*;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    Document, Event, HtmlElement, HtmlImageElement, HtmlInputElement, HtmlVideoElement, Url,
    Window,
};

use ratzilla::event::{KeyCode as RKeyCode, KeyEvent as RKeyEvent};
use ratzilla::{DomBackend, WebRenderer};

use sparkplayer_core::backend::{CoreKey, CoreKeyEvent};
use sparkplayer_core::library::Track;
use sparkplayer_core::ratatui::layout::Rect;
use sparkplayer_core::ratatui::Terminal;
use sparkplayer_core::App;

use crate::album_art::WebAlbumArt;
use crate::audio::WebAudioBackend;
use crate::library::{LocalStorageConfig, WebLibrary};
use crate::video::WebVideoBackend;

const ROOT_ID: &str = "sparkplayer-root";
const MANIFEST_URL: &str = "manifest.json";

type SharedApp = Rc<std::cell::RefCell<App>>;

/// Convert any `Display` error (ratzilla / io) into a `JsValue` for `?`.
fn to_js<E: std::fmt::Display>(e: E) -> JsValue {
    JsValue::from_str(&e.to_string())
}

#[wasm_bindgen(start)]
pub fn start() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();

    let window = web_sys::window().ok_or("no window")?;
    let document = window.document().ok_or("no document")?;

    // The shared media element: plays audio (routed through the analyser) and
    // shows video when floated over the canvas.
    let video_el: HtmlVideoElement = document
        .create_element("video")?
        .dyn_into::<HtmlVideoElement>()?;
    let img_el: HtmlImageElement = document
        .create_element("img")?
        .dyn_into::<HtmlImageElement>()?;
    init_overlay(&video_el)?;
    init_overlay(&img_el)?;
    if let Some(body) = document.body() {
        body.append_child(&video_el)?;
        body.append_child(&img_el)?;
    }

    let audio = WebAudioBackend::new(video_el.clone())?;
    let video = WebVideoBackend::new(video_el.clone());
    let art = WebAlbumArt::new(img_el.clone());
    let config = LocalStorageConfig;
    let cfg = sparkplayer_core::backend::ConfigStore::load(&config);

    let app = App::new(
        Box::new(audio),
        Box::new(video),
        Box::new(WebLibrary),
        Box::new(config),
        Box::new(art),
        Vec::new(),
        PathBuf::new(),
        &cfg,
    );
    let app: SharedApp = Rc::new(std::cell::RefCell::new(app));
    app.borrow_mut().status =
        String::from("Press any key (or pick files) to start — browser audio needs a gesture");

    // Fetch the manifest; if it lists tracks, switch to web-playlist mode.
    spawn_local(load_manifest(app.clone()));

    // Wire the local-file picker, if present.
    wire_file_input(&document, app.clone());

    // Build the ratzilla terminal and run.
    let backend = DomBackend::new_by_id(ROOT_ID).map_err(to_js)?;
    let terminal = Terminal::new(backend).map_err(to_js)?;

    {
        let app = app.clone();
        let first_gesture = Rc::new(Cell::new(true));
        terminal.on_key_event(move |ev: RKeyEvent| {
            let mut a = app.borrow_mut();
            if first_gesture.replace(false) {
                a.audio.on_user_gesture();
                if a.playing_index.is_none() && !a.tracks.is_empty() {
                    let _ = a.play_index(0);
                }
            }
            let _ = a.handle_key(map_key(ev));
        });
    }

    {
        let app = app.clone();
        let video_el = video_el.clone();
        let img_el = img_el.clone();
        let document = document.clone();
        let perf = window.performance();
        terminal.draw_web(move |frame| {
            let mut a = app.borrow_mut();
            let now = perf.as_ref().map(|p| p.now() / 1000.0).unwrap_or(0.0);
            a.set_clock(now);
            a.audio.pump();
            if a.current_duration.is_none() {
                a.current_duration = a.audio.duration();
            }
            let _ = a.check_advance();
            a.tick_video();
            sparkplayer_core::ui::draw(frame, &mut a);

            let term = frame.area();
            position_overlays(&document, &a, &video_el, &img_el, term);
        });
    }

    Ok(())
}

/// Map a ratzilla key event to the platform-neutral [`CoreKeyEvent`].
fn map_key(ev: RKeyEvent) -> CoreKeyEvent {
    let code = match ev.code {
        RKeyCode::Char(c) => CoreKey::Char(c),
        RKeyCode::Up => CoreKey::Up,
        RKeyCode::Down => CoreKey::Down,
        RKeyCode::Left => CoreKey::Left,
        RKeyCode::Right => CoreKey::Right,
        RKeyCode::PageUp => CoreKey::PageUp,
        RKeyCode::PageDown => CoreKey::PageDown,
        RKeyCode::Home => CoreKey::Home,
        RKeyCode::End => CoreKey::End,
        RKeyCode::Tab => CoreKey::Tab,
        RKeyCode::Enter => CoreKey::Enter,
        RKeyCode::Esc => CoreKey::Esc,
        _ => CoreKey::Other,
    };
    CoreKeyEvent::with_ctrl(code, ev.ctrl)
}

/// Common style for a floating overlay element (hidden until positioned).
fn init_overlay(el: &HtmlElement) -> Result<(), JsValue> {
    let style = el.style();
    style.set_property("position", "fixed")?;
    style.set_property("display", "none")?;
    style.set_property("z-index", "10")?;
    style.set_property("object-fit", "contain")?;
    style.set_property("background", "#000")?;
    style.set_property("pointer-events", "none")?;
    Ok(())
}

/// Place (or hide) the `<video>` / `<img>` overlays to match the cell rects the
/// UI recorded this frame. Cell→pixel scale comes from the root container's
/// on-screen size divided by the terminal's column/row count.
fn position_overlays(
    document: &Document,
    app: &App,
    video_el: &HtmlVideoElement,
    img_el: &HtmlImageElement,
    term: Rect,
) {
    let root = document.get_element_by_id(ROOT_ID);
    let rect = root.as_ref().map(|r| r.get_bounding_client_rect());
    place(video_el, app.last_video_rect, term, rect.as_ref());
    place(img_el, app.last_art_rect, term, rect.as_ref());
}

fn place(
    el: &HtmlElement,
    cell: Option<Rect>,
    term: Rect,
    container: Option<&web_sys::DomRect>,
) {
    let style = el.style();
    let (Some(cell), Some(container)) = (cell, container) else {
        let _ = style.set_property("display", "none");
        return;
    };
    if term.width == 0 || term.height == 0 || container.width() <= 0.0 {
        let _ = style.set_property("display", "none");
        return;
    }
    let cw = container.width() / term.width as f64;
    let ch = container.height() / term.height as f64;
    let left = container.left() + cell.x as f64 * cw;
    let top = container.top() + cell.y as f64 * ch;
    let w = cell.width as f64 * cw;
    let h = cell.height as f64 * ch;
    let _ = style.set_property("left", &format!("{left}px"));
    let _ = style.set_property("top", &format!("{top}px"));
    let _ = style.set_property("width", &format!("{w}px"));
    let _ = style.set_property("height", &format!("{h}px"));
    let _ = style.set_property("display", "block");
}

/// Hide the `HtmlElement` overlay (used when no rect was recorded this frame).
#[allow(dead_code)]
fn hide(el: &HtmlElement) {
    let _ = el.style().set_property("display", "none");
}

/// Fetch `manifest.json` and, if it lists tracks, load them into the playlist.
async fn load_manifest(app: SharedApp) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let tracks = match fetch_manifest_tracks(&window).await {
        Ok(t) => t,
        Err(_) => Vec::new(),
    };
    if tracks.is_empty() {
        app.borrow_mut().status =
            String::from("No manifest — use the file picker to add local files");
        return;
    }
    let mut a = app.borrow_mut();
    let n = tracks.len();
    a.tracks = tracks;
    a.selected = 0;
    a.playlist_state.select(Some(0));
    a.status = format!("Loaded {n} track(s) — press any key to start");
}

async fn fetch_manifest_tracks(window: &Window) -> Result<Vec<Track>, JsValue> {
    let resp_val = JsFuture::from(window.fetch_with_str(MANIFEST_URL)).await?;
    let resp: web_sys::Response = resp_val.dyn_into()?;
    if !resp.ok() {
        return Ok(Vec::new());
    }
    let text = JsFuture::from(resp.text()?).await?;
    let text = text.as_string().unwrap_or_default();
    Ok(parse_manifest(&text))
}

/// Parse `{ "tracks": [ {"url": "...", "title": "..."} ] }`.
fn parse_manifest(text: &str) -> Vec<Track> {
    let mut out = Vec::new();
    let Ok(value) = js_sys::JSON::parse(text) else {
        return out;
    };
    let Ok(tracks) = js_sys::Reflect::get(&value, &JsValue::from_str("tracks")) else {
        return out;
    };
    if !tracks.is_array() {
        return out;
    }
    let arr = js_sys::Array::from(&tracks);
    for entry in arr.iter() {
        let url = js_sys::Reflect::get(&entry, &JsValue::from_str("url"))
            .ok()
            .and_then(|v| v.as_string());
        let Some(url) = url else { continue };
        let title = js_sys::Reflect::get(&entry, &JsValue::from_str("title"))
            .ok()
            .and_then(|v| v.as_string())
            .unwrap_or_else(|| url.clone());
        out.push(Track::from_url(url, title));
    }
    out
}

/// Attach a change handler to `#file-input` that turns picked files into
/// object-URL tracks and starts playing.
fn wire_file_input(document: &Document, app: SharedApp) {
    let Some(input) = document
        .get_element_by_id("file-input")
        .and_then(|e| e.dyn_into::<HtmlInputElement>().ok())
    else {
        return;
    };
    let input_for_handler = input.clone();
    let handler = Closure::<dyn FnMut(Event)>::new(move |_ev: Event| {
        let Some(files) = input_for_handler.files() else {
            return;
        };
        let mut a = app.borrow_mut();
        let start = a.tracks.len();
        for i in 0..files.length() {
            let Some(file) = files.item(i) else { continue };
            let Ok(url) = Url::create_object_url_with_blob(&file) else {
                continue;
            };
            a.tracks.push(Track::from_url(url, file.name()));
        }
        if a.tracks.len() > start {
            // A file pick is a user gesture; start playback of the first added.
            a.audio.on_user_gesture();
            let _ = a.play_index(start);
        }
    });
    let _ = input
        .add_event_listener_with_callback("change", handler.as_ref().unchecked_ref());
    handler.forget();
}
