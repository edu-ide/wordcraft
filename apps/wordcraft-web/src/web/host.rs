//! The `postMessage` bridge to a same-origin parent page (`?host=parent`), for pages that embed
//! WordCraft in an `<iframe>`. Messages are plain objects with a `type`:
//!
//! - `wordcraft:ready` (to the parent): `{ version }`, the app is listening; send a file now.
//! - `wordcraft:open` (from the parent): `{ name, bytes }`, bytes an `ArrayBuffer` or
//!   `Uint8Array`. It opens like a document picked with File › Open or dropped on the window.
//! - `wordcraft:save` (to the parent): `{ name, bytes }` (a transferred `ArrayBuffer`) for every
//!   file WordCraft would otherwise download: Save and Save As (the document counts as saved
//!   once it is handed over, in a format that keeps everything) and the exports. The parent
//!   stores it and reports failures itself.
//! - `wordcraft:dirty` (to the parent): `{ dirty }` whenever unsaved changes appear or go away.
//! - `wordcraft:failed` (to the parent): `{ error }` when the app couldn't start.
//! - `wordcraft:command` (from the parent): `{ request, id, params }` runs a registry command
//!   (the ids the CLI, the control channel and MCP use, such as `text.insert` or
//!   `document.text`) through the app, as the control channel's `engine.execute` does: it never
//!   asks first.
//! - `wordcraft:result` (to the parent): `{ request, ok, result }` or `{ request, ok: false,
//!   error }` for each command, `request` echoed as it came.
//!
//! Only the parent window on the page's own origin is heard or answered. In host mode the page
//! starts on a blank document (no start screen, no sample) and the parent guards leaving the
//! page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use serde_json::Value as Json;
use wasm_bindgen::JsCast as _;
use wasm_bindgen::JsValue;
use wasm_bindgen::closure::Closure;
use wordcraft_ui_egui::{Inbox, Services, WordApp};

use crate::host_protocol::{self as protocol, Inbound};

#[derive(Clone)]
pub struct Bridge {
    window: web_sys::Window,
    parent: web_sys::Window,
    origin: String,
    /// The unsaved-changes state last reported to the parent.
    reported: Rc<Cell<Option<bool>>>,
    /// Commands from the parent, run at the next frame (they need the app).
    commands: Rc<RefCell<Vec<Inbound>>>,
}

impl Bridge {
    /// `None` when the page isn't framed (there is no parent to talk to).
    pub fn new() -> Option<Self> {
        let window = web_sys::window()?;
        let parent = window.parent().ok().flatten()?;
        if js_sys::Object::is(&parent, &window) {
            return None;
        }
        let origin = window.location().origin().ok()?;
        Some(Self { window, parent, origin, reported: Rc::default(), commands: Rc::default() })
    }

    /// Send saved and exported files to the parent instead of downloading them, tell it about
    /// unsaved changes, and hear files and commands from it.
    pub fn connect(&self, services: &mut Services, inbox: Inbox, ctx: egui::Context) {
        self.listen(inbox, ctx);
        let bridge = self.clone();
        services.download = Some(Box::new(move |name: &str, bytes: &[u8]| bridge.post_save(name, bytes)));
        let bridge = self.clone();
        let previous = services.on_dirty.take();
        services.on_dirty = Some(Box::new(move |dirty: bool| {
            if let Some(f) = &previous {
                f(dirty);
            }
            bridge.report_dirty(dirty);
        }));
    }

    pub fn post_ready(&self) {
        self.send(&[("type", protocol::READY.into()), ("version", env!("CARGO_PKG_VERSION").into())]);
    }

    pub fn post_failed(&self, error: &str) {
        self.send(&[("type", protocol::FAILED.into()), ("error", error.into())]);
    }

    /// After the app has opened what arrived this frame: run the parent's commands through the
    /// app and answer each one.
    pub fn run_commands(&self, app: &mut WordApp) {
        let pending = std::mem::take(&mut *self.commands.borrow_mut());
        for inbound in pending {
            let answer = match inbound {
                Inbound::Command(command) => protocol::run(command, |id, params| app.execute(id, params)),
                Inbound::BadCommand { request, error } => protocol::result(request, Err(error)),
                Inbound::Open { .. } => continue,
            };
            if let Err(e) = self.post_json(&answer) {
                log::error!("couldn't answer the page: {e}");
            }
        }
    }

    /// The parent hears when unsaved changes appear or go away.
    fn report_dirty(&self, dirty: bool) {
        if self.reported.get() != Some(dirty) && self.send(&[("type", protocol::DIRTY.into()), ("dirty", dirty.into())]) {
            self.reported.set(Some(dirty));
        }
    }

    fn post_save(&self, path: &str, bytes: &[u8]) -> Result<(), String> {
        let name = std::path::Path::new(path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| path.to_string());
        let buffer = js_sys::Uint8Array::from(bytes).buffer();
        let message = message(&[("type", protocol::SAVE.into()), ("name", name.as_str().into()), ("bytes", buffer.clone().into())])
            .ok_or_else(|| "couldn't build the message".to_string())?;
        self.parent
            .post_message_with_transfer(&message, &self.origin, &js_sys::Array::of1(&buffer))
            .map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    fn listen(&self, inbox: Inbox, ctx: egui::Context) {
        let parent = self.parent.clone();
        let origin = self.origin.clone();
        let commands = self.commands.clone();
        let on_message = Closure::<dyn FnMut(web_sys::MessageEvent)>::new(move |event: web_sys::MessageEvent| {
            let from_parent = event.source().is_some_and(|s| js_sys::Object::is(&s, &parent));
            if !protocol::accept(&event.origin(), &origin, from_parent) {
                return;
            }
            let data = event.data();
            // Only an open carries a file: don't copy bytes out of anything else.
            let file = if string(&data, "type").as_deref() == Some(protocol::OPEN) { bytes(&data) } else { None };
            match protocol::parse(&fields(&data), file) {
                Some(Inbound::Open { name, bytes }) => inbox.lock().unwrap_or_else(|e| e.into_inner()).push((name, bytes)),
                Some(command) => commands.borrow_mut().push(command),
                None => return,
            }
            ctx.request_repaint();
        });
        if self.window.add_event_listener_with_callback("message", on_message.as_ref().unchecked_ref()).is_ok() {
            // The listener lives as long as the page.
            on_message.forget();
        }
    }

    /// Post a JSON message to the parent (as a plain object).
    fn post_json(&self, message: &Json) -> Result<(), String> {
        let text = serde_json::to_string(message).map_err(|e| e.to_string())?;
        let value = js_sys::JSON::parse(&text).map_err(|e| format!("{e:?}"))?;
        self.parent.post_message(&value, &self.origin).map_err(|e| e.as_string().unwrap_or_else(|| format!("{e:?}")))
    }

    /// Post a message to the parent; false if it couldn't be sent.
    fn send(&self, fields: &[(&str, JsValue)]) -> bool {
        message(fields).is_some_and(|m| self.parent.post_message(&m, &self.origin).is_ok())
    }
}

fn message(fields: &[(&str, JsValue)]) -> Option<js_sys::Object> {
    let m = js_sys::Object::new();
    for (key, value) in fields {
        js_sys::Reflect::set(&m, &JsValue::from_str(key), value).ok()?;
    }
    Some(m)
}

fn string(data: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(data, &JsValue::from_str(key)).ok()?.as_string()
}

/// The message's [`protocol::FIELDS`] that are present, as JSON.
fn fields(data: &JsValue) -> Json {
    let mut out = serde_json::Map::new();
    for key in protocol::FIELDS {
        let Ok(value) = js_sys::Reflect::get(data, &JsValue::from_str(key)) else { continue };
        if value.is_undefined() {
            continue;
        }
        let text = js_sys::JSON::stringify(&value).ok().and_then(|t| t.as_string());
        if let Some(value) = text.and_then(|t| serde_json::from_str(&t).ok()) {
            out.insert((*key).to_string(), value);
        }
    }
    Json::Object(out)
}

/// `bytes` as an `ArrayBuffer` or `Uint8Array`.
fn bytes(data: &JsValue) -> Option<Vec<u8>> {
    let bytes = js_sys::Reflect::get(data, &JsValue::from_str("bytes")).ok()?;
    if let Some(array) = bytes.dyn_ref::<js_sys::Uint8Array>() {
        Some(array.to_vec())
    } else if bytes.is_instance_of::<js_sys::ArrayBuffer>() {
        Some(js_sys::Uint8Array::new(&bytes).to_vec())
    } else {
        None
    }
}
