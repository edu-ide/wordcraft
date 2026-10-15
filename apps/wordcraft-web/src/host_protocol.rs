//! The messages of the `?host=parent` bridge (`host.rs`) as plain data, so they are tested off
//! the browser: which messages are heard, what they ask for, and the answer to a command.
//!
//! `host.rs` reads a message's fields from the `MessageEvent` into JSON (the file's `bytes`
//! separately) and posts the JSON answers back.

use serde_json::{Value as Json, json};

pub const READY: &str = "wordcraft:ready";
pub const OPEN: &str = "wordcraft:open";
pub const SAVE: &str = "wordcraft:save";
pub const DIRTY: &str = "wordcraft:dirty";
pub const FAILED: &str = "wordcraft:failed";
pub const COMMAND: &str = "wordcraft:command";
pub const RESULT: &str = "wordcraft:result";

/// The name of a document the parent sends without one.
pub const DEFAULT_NAME: &str = "Document.docx";

/// The fields of a message `host.rs` reads (all but `bytes`).
pub const FIELDS: &[&str] = &["type", "name", "request", "id", "params"];

/// What the parent asks for.
#[derive(Debug, Clone, PartialEq)]
pub enum Inbound {
    /// `wordcraft:open {name, bytes}`: open the document.
    Open { name: String, bytes: Vec<u8> },
    /// `wordcraft:command {request, id, params}`: run a registry command.
    Command(Command),
    /// A `wordcraft:command` that can't run; the parent still gets an answer.
    BadCommand { request: Json, error: String },
}

/// A registry command for the app (the ids the CLI, the control channel and MCP use).
#[derive(Debug, Clone, PartialEq)]
pub struct Command {
    /// The parent's tag for this request, echoed in the answer as it came.
    pub request: Json,
    pub id: String,
    pub params: Json,
}

/// Only the parent window, on the page's own origin, is heard. An opaque origin (`"null"`)
/// matches no other page.
pub fn accept(event_origin: &str, own_origin: &str, from_parent: bool) -> bool {
    from_parent && event_origin == own_origin && own_origin != "null"
}

/// The message `data` (the [`FIELDS`] that are present) with the `bytes` it carried: `None` for
/// messages that aren't for the page (another type, or an open without bytes).
pub fn parse(data: &Json, bytes: Option<Vec<u8>>) -> Option<Inbound> {
    match data.get("type").and_then(Json::as_str)? {
        OPEN => {
            let name = data.get("name").and_then(Json::as_str).map(str::trim).filter(|n| !n.is_empty()).unwrap_or(DEFAULT_NAME);
            Some(Inbound::Open { name: name.to_string(), bytes: bytes? })
        }
        COMMAND => {
            let request = data.get("request").cloned().unwrap_or(Json::Null);
            let Some(id) = data.get("id").and_then(Json::as_str).map(str::trim).filter(|id| !id.is_empty()) else {
                return Some(Inbound::BadCommand { request, error: "missing `id` (a command id such as `text.insert`)".into() });
            };
            let params = match data.get("params") {
                None | Some(Json::Null) => json!({}),
                Some(p @ Json::Object(_)) => p.clone(),
                Some(_) => return Some(Inbound::BadCommand { request, error: format!("`params` of `{id}` must be an object") }),
            };
            Some(Inbound::Command(Command { request, id: id.to_string(), params }))
        }
        _ => None,
    }
}

/// Run `command` through `dispatch` (the app's own command entry) and build the answer.
pub fn run(command: Command, dispatch: impl FnOnce(&str, Json) -> Result<Json, String>) -> Json {
    let outcome = dispatch(&command.id, command.params);
    result(command.request, outcome)
}

/// `wordcraft:result {request, ok, result}` or `{request, ok: false, error}`.
pub fn result(request: Json, outcome: Result<Json, String>) -> Json {
    match outcome {
        Ok(value) => json!({"type": RESULT, "request": request, "ok": true, "result": value}),
        Err(error) => json!({"type": RESULT, "request": request, "ok": false, "error": error}),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;
    use wordcraft_doc::Document;
    use wordcraft_engine::Session;
    use wordcraft_ui_egui::{Services, WordApp};

    fn app(services: Services) -> WordApp {
        WordApp::new(Session::new(Document::from_text("Memo")), services)
    }

    #[test]
    fn every_message_type_is_namespaced_by_the_engine() {
        for t in [READY, OPEN, SAVE, DIRTY, FAILED, COMMAND, RESULT] {
            assert!(t.strip_prefix("wordcraft:").is_some_and(|rest| !rest.is_empty()), "{t}");
        }
        assert_eq!(FIELDS, ["type", "name", "request", "id", "params"]);
    }

    #[test]
    fn only_the_parent_on_the_same_origin_is_heard() {
        assert!(accept("https://space.local", "https://space.local", true));
        assert!(!accept("https://evil.example", "https://space.local", true), "another origin");
        assert!(!accept("https://space.local", "https://space.local", false), "another window on the same origin");
        assert!(!accept("null", "null", true), "opaque origins match nothing");
    }

    #[test]
    fn open_takes_the_name_and_bytes() {
        let open = parse(&json!({"type": OPEN, "name": "memo.docx"}), Some(vec![1, 2, 3]));
        assert_eq!(open, Some(Inbound::Open { name: "memo.docx".into(), bytes: vec![1, 2, 3] }));
        let unnamed = parse(&json!({"type": OPEN, "name": 5}), Some(vec![]));
        assert_eq!(unnamed, Some(Inbound::Open { name: DEFAULT_NAME.into(), bytes: vec![] }));
        assert_eq!(parse(&json!({"type": OPEN, "name": "memo.docx"}), None), None, "an open without bytes is ignored");
    }

    #[test]
    fn other_messages_are_ignored() {
        assert_eq!(parse(&json!({"type": "deckcraft:open", "name": "a.pptx"}), Some(vec![1])), None);
        assert_eq!(parse(&json!({"type": SAVE, "name": "a.docx"}), Some(vec![1])), None, "the page's own messages");
        assert_eq!(parse(&json!({"name": "a.docx"}), Some(vec![1])), None);
        assert_eq!(parse(&json!([OPEN]), Some(vec![1])), None);
    }

    #[test]
    fn command_keeps_the_request_tag_as_it_came() {
        let parsed = parse(&json!({"type": COMMAND, "request": [1, "a"], "id": "text.insert", "params": {"text": "x"}}), None);
        assert_eq!(parsed, Some(Inbound::Command(Command { request: json!([1, "a"]), id: "text.insert".into(), params: json!({"text": "x"}) })));
        let Some(Inbound::Command(c)) = parse(&json!({"type": COMMAND, "id": "document.text"}), None) else { panic!() };
        assert_eq!((c.request, c.params), (Json::Null, json!({})), "no request is null, no params is {{}}");
    }

    #[test]
    fn a_command_that_cannot_run_is_still_answered() {
        let no_id = parse(&json!({"type": COMMAND, "request": 3, "id": 12}), None);
        assert!(matches!(no_id, Some(Inbound::BadCommand { request, .. }) if request == json!(3)));
        let bad_params = parse(&json!({"type": COMMAND, "request": 4, "id": "text.insert", "params": true}), None);
        assert!(matches!(bad_params, Some(Inbound::BadCommand { request, error }) if request == json!(4) && error.contains("object")));
    }

    #[test]
    fn a_registry_command_runs_in_the_app_and_answers() {
        let mut app = app(Services::default());
        let answer = run(Command { request: json!(1), id: "text.insert".into(), params: json!({"text": "Hello "}) }, |id, p| app.execute(id, p));
        assert_eq!((answer["type"].as_str(), answer["request"].as_i64(), answer["ok"].as_bool()), (Some(RESULT), Some(1), Some(true)), "{answer}");
        assert!(answer.get("error").is_none());
        let answer = run(Command { request: json!("t"), id: "document.text".into(), params: json!({}) }, |id, p| app.execute(id, p));
        assert_eq!(answer["ok"], true, "{answer}");
        assert!(answer["result"]["text"].as_str().is_some_and(|t| t.contains("Hello")), "{answer}");
    }

    #[test]
    fn an_unknown_command_answers_with_the_error() {
        let mut app = app(Services::default());
        let answer = run(Command { request: json!(9), id: "no.such.command".into(), params: json!({}) }, |id, p| app.execute(id, p));
        assert_eq!((answer["ok"].as_bool(), answer["request"].as_i64()), (Some(false), Some(9)), "{answer}");
        assert!(answer["error"].as_str().is_some_and(|e| e.contains("no.such.command")), "{answer}");
        assert!(answer.get("result").is_none());
    }

    /// A save the parent asks for goes where the user's Save goes in the browser: the bytes go
    /// to the page (`download`, which the bridge turns into `wordcraft:save`), and the document
    /// counts as saved.
    #[test]
    fn a_save_command_hands_the_document_over() {
        let saved: Rc<RefCell<Vec<(String, Vec<u8>)>>> = Rc::default();
        let sink = saved.clone();
        let services = Services {
            download: Some(Box::new(move |name: &str, bytes: &[u8]| {
                sink.borrow_mut().push((name.into(), bytes.into()));
                Ok(())
            })),
            ..Default::default()
        };
        let mut app = app(services);
        let _ = app.execute("text.insert", json!({"text": "edited "}));
        assert!(app.session.dirty);
        let answer = run(Command { request: json!(2), id: "file.saveAs".into(), params: json!({"path": "memo.docx"}) }, |id, p| app.execute(id, p));
        assert_eq!(answer["ok"], true, "{answer}");
        let saved = saved.borrow();
        let (name, bytes) = saved.first().expect("the save handed the file over");
        assert_eq!(name, "memo.docx");
        assert!(bytes.starts_with(b"PK"), "a Word package");
        assert!(!app.session.dirty);
    }
}
