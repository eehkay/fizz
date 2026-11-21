//! `fizz bridge`: Fizz without its interface, for a status-bar widget.
//!
//! It keeps the same state the full app does and prints what a small panel
//! needs as one line of JSON whenever that changes. Requests come in as
//! lines of JSON on standard input, so message text never appears in a
//! command line. It ends when its input closes.
//!
//! Out, one line per change:
//!
//! ```json
//! {"v":1,"demo":false,"online":true,"loading":false,"error":null,"unread":2,
//!  "chats":[{"id":"…","title":"Maya","group":false,"unread":true,
//!            "time":1790000000000,"preview":"See you at 6","from_me":false}],
//!  "open":{"id":"…","title":"Maya","group":false,"loading":false,"error":null,
//!          "messages":[{"id":"…","from_me":false,"sender":"Maya","text":"…",
//!                       "time":1790000000000,"pending":false,"failed":null,
//!                       "note":false,"attachments":[],"reactions":[]}]}}
//! ```
//!
//! In, one line per request:
//!
//! ```json
//! {"cmd":"open","chat":"…"}            show a conversation, and mark it read
//! {"cmd":"close"}                      stop showing it
//! {"cmd":"send","chat":"…","text":"…"} reply in a conversation that exists
//! {"cmd":"retry","chat":"…","message":"…"}  send a failed message again
//! {"cmd":"refresh"}                    ask for the chat list again
//! ```
//!
//! It never notifies, plays a sound or downloads pictures, and it can only
//! reply: starting a conversation is the full app's job.

use anyhow::Result;
use ratatui_image::picker::Picker;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;

use crate::api::Api;
use crate::app::App;
use crate::config::Config;
use crate::model::{Chat, Message};

/// Bumped when a field changes meaning, so a widget can tell.
const VERSION: u32 = 1;
/// How many conversations and how much of the open one are sent.
const CHATS: usize = 12;
const MESSAGES: usize = 30;

#[derive(Serialize)]
struct State {
    v: u32,
    demo: bool,
    online: Option<bool>,
    loading: bool,
    error: Option<String>,
    /// Unread conversations, counting those below the ones listed.
    unread: usize,
    chats: Vec<ChatRow>,
    open: Option<Open>,
}

#[derive(Serialize)]
struct ChatRow {
    id: String,
    title: String,
    group: bool,
    unread: bool,
    time: i64,
    preview: String,
    from_me: bool,
}

#[derive(Serialize)]
struct Open {
    id: String,
    title: String,
    group: bool,
    loading: bool,
    error: Option<String>,
    messages: Vec<Row>,
}

#[derive(Serialize)]
struct Row {
    id: String,
    from_me: bool,
    /// Who sent it, for a group; empty for our own.
    sender: String,
    text: String,
    time: i64,
    pending: bool,
    failed: Option<String>,
    /// Not something anyone said: a renamed group, an unsent message.
    note: bool,
    attachments: Vec<String>,
    reactions: Vec<String>,
}

#[derive(Deserialize, Debug, PartialEq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
enum Request {
    Open { chat: String },
    Close,
    Send { chat: String, text: String },
    Retry { chat: String, message: String },
    Refresh,
}

fn chat_row(app: &App, chat: &Chat) -> ChatRow {
    let last = chat.last_message.as_ref();
    ChatRow {
        id: chat.guid.clone(),
        title: app.chat_title(chat),
        group: chat.is_group(),
        unread: app.is_unread(chat),
        time: chat.last_activity(),
        preview: last.map(Message::summary).unwrap_or_default(),
        from_me: last.is_some_and(|m| m.is_from_me),
    }
}

fn row(app: &App, message: &Message, reactions: Vec<String>) -> Row {
    let unsent = message.date_retracted.is_some();
    let note = unsent || message.item_type != 0;
    let text = if unsent {
        "Unsent a message".to_string()
    } else if message.item_type != 0 {
        message.summary()
    } else {
        message.body()
    };
    Row {
        id: message.guid.clone(),
        from_me: message.is_from_me,
        sender: match &message.handle {
            Some(handle) if !message.is_from_me => app.short_name_for(&handle.address),
            _ => String::new(),
        },
        text,
        time: message.date_created,
        pending: message.pending,
        failed: message.failed.clone(),
        note,
        attachments: if unsent {
            Vec::new()
        } else {
            message
                .attachments
                .iter()
                .filter(|a| !a.is_plugin_payload())
                .map(|a| {
                    if a.is_gif() {
                        "GIF".to_string()
                    } else if a.is_image() {
                        "Photo".to_string()
                    } else if a.is_video() {
                        "Video".to_string()
                    } else {
                        a.name().to_string()
                    }
                })
                .collect()
        },
        reactions,
    }
}

fn state(app: &App, demo: bool) -> State {
    let open = app.open.as_ref().and_then(|guid| {
        let chat = app.chats.iter().find(|c| &c.guid == guid)?;
        let convo = app.convos.get(guid);
        let messages = convo.map_or(Vec::new(), |convo| {
            let from = convo.messages.len().saturating_sub(MESSAGES);
            convo.messages[from..]
                .iter()
                .map(|m| {
                    let reactions = convo
                        .reactions
                        .get(&m.guid)
                        .map(|all| all.iter().map(|(_, glyph)| glyph.clone()).collect())
                        .unwrap_or_default();
                    row(app, m, reactions)
                })
                .collect()
        });
        Some(Open {
            id: guid.clone(),
            title: app.chat_title(chat),
            group: chat.is_group(),
            loading: convo.is_some_and(|c| c.loading && !c.loaded),
            error: convo.and_then(|c| c.error.clone()),
            messages,
        })
    });
    State {
        v: VERSION,
        demo,
        online: app.online,
        loading: app.chats_loading && app.chats.is_empty(),
        error: app.chats_error.clone(),
        unread: app.chats.iter().filter(|c| app.is_unread(c)).count(),
        chats: app
            .chats
            .iter()
            .filter(|c| !c.is_draft())
            .take(CHATS)
            .map(|c| chat_row(app, c))
            .collect(),
        open,
    }
}

fn handle(app: &mut App, line: &str) {
    let line = line.trim();
    if line.is_empty() {
        return;
    }
    match serde_json::from_str::<Request>(line) {
        Ok(Request::Open { chat }) => {
            if app.chats.iter().any(|c| c.guid == chat) {
                // Another Fizz may have read things since; do not undo that.
                app.reload_seen();
                app.open_chat(&chat);
            }
        }
        Ok(Request::Close) => app.close_chat(),
        Ok(Request::Send { chat, text }) => {
            if text.trim().is_empty() || !app.send_to(&chat, &text) {
                eprintln!("fizz bridge: nothing sent (empty text or unknown conversation)");
            }
        }
        Ok(Request::Retry { chat, message }) => app.retry(&chat, &message),
        Ok(Request::Refresh) => app.refresh_chats(),
        // Never echo the line: it may hold the text of a message.
        Err(e) => eprintln!("fizz bridge: request not understood ({:?})", e.classify()),
    }
}

/// `config` is `None` for the demo, which talks to no server.
pub async fn run(config: Option<Config>) -> Result<()> {
    let demo = config.is_none();
    let api = config.as_ref().map(Api::new).transpose()?;
    let (tx, mut rx) = unbounded_channel();
    let mut app = App::new(api.clone(), tx.clone(), Picker::halfblocks());
    match (api, config) {
        (Some(api), Some(config)) => {
            app.start_headless();
            crate::live::start(api, config, tx);
        }
        _ => {
            app.load_demo();
            app.start_headless();
        }
    }

    // Requests arrive on a plain thread: reading standard input blocks.
    let (requests_tx, mut requests) = unbounded_channel::<String>();
    std::thread::spawn(move || {
        for line in std::io::stdin().lines() {
            let Ok(line) = line else { break };
            if requests_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut marks = tokio::time::interval(Duration::from_secs(5));
    let mut was_online = None;
    let mut last = String::new();
    loop {
        let now = serde_json::to_string(&state(&app, demo))?;
        if now != last {
            let mut out = std::io::stdout().lock();
            // Whoever was listening has gone.
            if writeln!(out, "{now}").and_then(|()| out.flush()).is_err() {
                return Ok(());
            }
            last = now;
        }
        tokio::select! {
            request = requests.recv() => match request {
                Some(line) => handle(&mut app, &line),
                None => return Ok(()),
            },
            Some(event) = rx.recv() => {
                app.on_event(event);
                while let Ok(event) = rx.try_recv() {
                    app.on_event(event);
                }
                // Back in touch after starting without the server: the
                // list was never fetched, so fetch it now.
                if app.online == Some(true) && was_online != Some(true) && app.chats_error.is_some() {
                    app.refresh_chats();
                }
                was_online = app.online;
            }
            _ = marks.tick(), if !demo => {
                app.reload_seen();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> App {
        let (tx, _rx) = unbounded_channel();
        let mut app = App::new(None, tx, Picker::halfblocks());
        app.load_demo();
        app.start_headless();
        app
    }

    #[test]
    fn nothing_is_open_until_asked() {
        let app = demo();
        let state = state(&app, true);
        assert!(state.open.is_none());
        assert!(!state.chats.is_empty());
        assert!(state.chats.len() <= CHATS);
        assert_eq!(state.unread, state.chats.iter().filter(|c| c.unread).count());
    }

    #[test]
    fn opening_marks_read_and_closing_stops_that() {
        let mut app = demo();
        let unread = state(&app, true)
            .chats
            .into_iter()
            .find(|c| c.unread)
            .expect("the demo has an unread conversation");
        handle(&mut app, &format!(r#"{{"cmd":"open","chat":"{}"}}"#, unread.id));
        let after = state(&app, true);
        let open = after.open.expect("it is open now");
        assert_eq!(open.id, unread.id);
        assert!(!open.messages.is_empty());
        assert!(!after.chats.iter().find(|c| c.id == unread.id).unwrap().unread);
        handle(&mut app, r#"{"cmd":"close"}"#);
        assert!(state(&app, true).open.is_none());
    }

    #[test]
    fn a_reply_lands_in_its_conversation() {
        let mut app = demo();
        let chat = state(&app, true).chats[1].id.clone();
        handle(
            &mut app,
            &serde_json::json!({"cmd": "send", "chat": chat, "text": "On my way"}).to_string(),
        );
        let after = state(&app, true);
        let open = after.open.expect("sending opens the conversation");
        let last = open.messages.last().unwrap();
        assert_eq!((last.text.as_str(), last.from_me), ("On my way", true));
        // It is the newest conversation now, with the reply as its preview.
        assert_eq!(after.chats[0].id, chat);
        assert_eq!(after.chats[0].preview, "On my way");
    }

    #[test]
    fn only_conversations_that_exist_can_be_written_to() {
        let mut app = demo();
        let before = serde_json::to_string(&state(&app, true)).unwrap();
        for request in [
            r#"{"cmd":"send","chat":"new:+15550109999","text":"hello"}"#,
            r#"{"cmd":"send","chat":"no-such-chat","text":"hello"}"#,
            r#"{"cmd":"open","chat":"no-such-chat"}"#,
            r#"{"cmd":"send","chat":"no-such-chat"}"#,
            r#"{"cmd":"start","to":"+15550109999","text":"hello"}"#,
            "not json",
        ] {
            handle(&mut app, request);
        }
        assert_eq!(serde_json::to_string(&state(&app, true)).unwrap(), before);
    }

    #[test]
    fn a_failed_send_can_be_retried() {
        let mut app = demo();
        let chat = state(&app, true).chats[0].id.clone();
        handle(
            &mut app,
            &serde_json::json!({"cmd": "send", "chat": chat, "text": "try this !fail"}).to_string(),
        );
        let open = state(&app, true).open.unwrap();
        let failed = open.messages.last().unwrap();
        assert!(failed.failed.is_some());
        handle(
            &mut app,
            &serde_json::json!({"cmd": "retry", "chat": chat, "message": failed.id}).to_string(),
        );
        let open = state(&app, true).open.unwrap();
        let again = open.messages.last().unwrap();
        assert_eq!(again.text, "try this !fail");
        assert!(again.failed.is_none());
        assert_eq!(open.messages.iter().filter(|m| m.text == "try this !fail").count(), 1);
    }
}
