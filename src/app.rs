//! Application state and everything that changes it.

use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use ratatui_image::picker::Picker;
use ratatui_image::sliced::SlicedProtocol;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::Api;
use crate::emoji;
use crate::files::{self, Then};
use crate::images::{self, ImageState};
use crate::input::Input;
use crate::metrics::{self, Usage};
use crate::model::{
    Attachment, Chat, Contact, Message, Reaction, ServerInfo, TAPBACKS, normalize_address,
    sendable_address,
};
use crate::theme::Theme;

/// The largest box, in terminal cells, a picture may take in the timeline.


/// The picker row that finishes a group, told apart from a person by this
/// in place of an address.
pub const START_GROUP: &str = "\u{1}start-group";

/// How many GIFs play at once in a conversation.
const PLAYING_GIFS: usize = 6;

/// How many results of each kind the search layer shows.
const SEARCH_CONVERSATIONS: usize = 8;
const SEARCH_MESSAGES: usize = 80;
/// How many of the conversations a search's words appear in are listed.
const SEARCH_MENTIONS: usize = 6;

/// How far back the first look for conversations goes: three months.
const RECENT_WINDOW_MS: i64 = 90 * 24 * 3600 * 1000;

/// How many conversations and how much of each are remembered between runs.
const REMEMBERED_CHATS: usize = 80;
const REMEMBERED_CONVERSATIONS: usize = 8;
const REMEMBERED_MESSAGES: usize = 60;

/// How many of the newest messages to scan per step when building the list.
const SCAN_PAGE: usize = 400;
const MESSAGE_PAGE: usize = 50;

pub enum AppEvent {
    /// A slice of the newest messages, from which the chat list is built.
    Recent {
        /// Whether this was a request for older history (scrolling down
        /// the list) rather than the first, most recent page.
        more: bool,
        result: Result<Vec<Message>, String>,
    },
    ChatDetail(Chat),
    /// How far the local copy of message text has got.
    Indexing(crate::index::Progress),
    /// Messages found for a search; `generation` says which search.
    Found {
        generation: u64,
        hits: Vec<crate::index::Hit>,
        /// Every conversation with a match, the one with the most first.
        mentions: Vec<crate::index::Mention>,
    },
    /// What the server says about itself, including whether its Private
    /// API helper is connected.
    Info(ServerInfo),
    /// The result of an update check: the newer commit, if there is one.
    Update(Option<String>),
    /// The other side started or stopped typing in a chat.
    Typing {
        chat: String,
        on: bool,
    },
    /// What the server holds as a conversation's newest received message,
    /// asked for to find out whether it has been read on another device.
    Newest {
        chat: String,
        message: Message,
    },
    /// The server says a conversation was marked read, or unread, elsewhere.
    ReadStatus {
        chat: String,
        read: bool,
    },
    /// A conversation has been deleted on the server (or it refused).
    Deleted {
        chat: String,
        result: Result<(), String>,
    },
    /// An unsend or edit has been accepted (or refused).
    Changed {
        chat: String,
        what: &'static str,
        /// The new text, for an edit.
        text: Option<String>,
        result: Result<Message, String>,
    },
    /// A tapback we sent has been accepted (or refused).
    Reacted {
        chat: String,
        result: Result<Message, String>,
    },
    /// A slice of the server's full chat list, for searching by name.
    Index(Vec<Chat>),
    Contacts(Vec<Contact>),
    Messages {
        chat: String,
        page: Page,
        result: Result<Vec<Message>, String>,
        /// What putting the page in the database found out, when it was.
        stored: Option<crate::store::Stored>,
    },
    Incoming(Vec<Message>),
    Online(bool),
    Sent {
        chat: String,
        temp: String,
        result: Result<Message, String>,
    },
    Image {
        guid: String,
        result: Result<images::Loaded, String>,
    },
    Theme(Theme),
    /// The answer to "is there already a chat with this person?".
    Resolved {
        address: String,
        chat: Option<Chat>,
    },
    /// A brand-new conversation's first message has gone out (or failed).
    Started {
        draft: String,
        temp: String,
        result: Result<Chat, String>,
    },
    /// Files to attach to the message being written.
    Attach(Vec<PathBuf>),
    /// Text to insert into the message being written.
    PasteText(String),
    /// A line for the status bar, such as download progress.
    Status(String),
    FileDone {
        guid: String,
        result: Result<String, String>,
    },
}

/// What is kept between runs so the next start can show something at once.
const SNAPSHOT_VERSION: u32 = 1;

#[derive(serde::Serialize, serde::Deserialize)]
struct Snapshot {
    version: u32,
    chats: Vec<Chat>,
    contacts: HashMap<String, String>,
    people: Vec<(String, String)>,
    open: Option<String>,
    conversations: Vec<(String, Vec<Message>)>,
}

/// Which stretch of a conversation's history a fetch brought back.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Page {
    /// Older than anything loaded (or the first, most recent page).
    Older,
    /// Newer than anything loaded, when the view is not at the present.
    Newer,
    /// The newest page, fetched to bring a remembered conversation up to
    /// date.
    Latest,
    /// Both sides of one message, after jumping to it from search.
    /// `more_newer` says whether there is more history after this window.
    Around { more_newer: bool },
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Compose,
    /// Stepping through the open conversation's messages, pictures and files.
    Browse,
}

/// The id of the chat-list row that stands for a folder: this, then its
/// name. Such a row is never a conversation and never leaves the list.
pub const FOLDER: &str = "folder:";
/// The id of the row, at the top of an open folder, that leads back out.
pub const BACK: &str = "back:";
/// The longest a folder's name may be.
const FOLDER_NAME: usize = 28;

/// Choosing a folder for a conversation.
pub struct Filing {
    /// The conversation being moved.
    pub chat: String,
    /// What has been typed: it narrows the folders offered, and is the
    /// name of a new one.
    pub typed: String,
    pub index: usize,
}

/// One thing the folder chooser offers.
#[derive(Debug, Clone, PartialEq)]
pub enum FilingChoice {
    /// A folder that exists, and how many conversations are in it.
    Folder(String, usize),
    /// A new folder by this name.
    New(String),
    /// Out of the folder it is in, back to the main list.
    Out(String),
}

/// One conversation's loaded history.
#[derive(Default)]
pub struct Convo {
    /// Oldest first; tapbacks are kept separately.
    pub messages: Vec<Message>,
    reaction_events: Vec<Message>,
    /// Target message guid -> (who, glyph).
    pub reactions: HashMap<String, Vec<(String, String)>>,
    pub exhausted: bool,
    pub loading: bool,
    /// True after jumping to an old message: what is loaded stops short of
    /// the present, so newer history loads on the way down and live
    /// messages are held back until the view returns to the latest.
    pub detached: bool,
    loading_newer: bool,
    /// Set when newer history was added below, so the next draw keeps the
    /// same messages in view instead of sliding down with the new rows.
    pub grew_below: bool,
    pub last_total: usize,
    pub loaded: bool,
    pub error: Option<String>,
    /// After the server failed to give a page: when to ask again. Until
    /// then the top of the conversation being in view asks for nothing.
    retry_after: Option<Instant>,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
}

impl Convo {
    /// The newest `count` messages, with the tapbacks that belong to them,
    /// in a form that can be saved. Messages still being sent are left out.
    fn newest(&self, count: usize) -> Vec<Message> {
        let start = self.messages.len().saturating_sub(count);
        let mut kept: Vec<Message> = self.messages[start..]
            .iter()
            .filter(|m| !m.pending && m.failed.is_none())
            .cloned()
            .collect();
        let since = kept.first().map_or(i64::MAX, |m| m.date_created);
        kept.extend(
            self.reaction_events
                .iter()
                .filter(|m| m.date_created >= since)
                .cloned(),
        );
        kept
    }

    fn upsert(list: &mut Vec<Message>, message: Message) -> bool {
        match list.iter_mut().find(|m| m.guid == message.guid) {
            Some(existing) => {
                *existing = message;
                false
            }
            None => {
                list.push(message);
                true
            }
        }
    }

    /// Add or update a message. Returns true if it is a new visible message.
    fn absorb(&mut self, message: Message) -> bool {
        if message.reaction().is_some() {
            Self::upsert(&mut self.reaction_events, message);
            return false;
        }
        if message.is_from_me
            && !message.pending
            && !self.messages.iter().any(|m| m.guid == message.guid)
        {
            // The server's copy of something we just sent, heard about
            // before the send itself answered: drop our placeholder. Only
            // when there is no doubt which one it is, though. With the
            // same text going out twice, each placeholder waits for its
            // own send to answer (see `confirm`), so that one which fails
            // is still there to say so.
            // And only to a copy from since it was sent: a page of history
            // holds older messages of ours that say the same thing (any
            // two pictures sent without words do), and those are not it.
            // (Two minutes' grace for the Mac's clock being behind ours.)
            let body = message.body();
            let mut waiting = self
                .messages
                .iter()
                .enumerate()
                .filter(|(_, m)| {
                    m.pending && m.body() == body && message.date_created >= m.date_created - 120_000
                })
                .map(|(i, _)| i);
            if let (Some(only), None) = (waiting.next(), waiting.next()) {
                self.messages.remove(only);
            }
        }
        Self::upsert(&mut self.messages, message)
    }

    /// A send has answered: put the server's copy of the message in place
    /// of the placeholder that stood for it.
    fn confirm(&mut self, temp: &str, real: Message) {
        self.messages.retain(|m| m.guid != temp);
        Self::upsert(&mut self.messages, real);
    }

    fn settle(&mut self) {
        self.messages.sort_by_key(|m| m.date_created);
        self.reaction_events.sort_by_key(|m| m.date_created);
        self.reactions.clear();
        for event in &self.reaction_events {
            let (Some(target), Some(reaction)) = (event.reaction_target(), event.reaction()) else {
                continue;
            };
            let who = event.sender_key();
            let entry = self.reactions.entry(target.to_string()).or_default();
            entry.retain(|(w, _)| *w != who);
            if let Reaction::Add(glyph) = reaction {
                entry.push((who, glyph));
            }
        }
        self.reactions.retain(|_, v| !v.is_empty());
    }
}

pub struct App {
    api: Option<Api>,
    tx: UnboundedSender<AppEvent>,
    pub theme: Theme,
    pub picker: Picker,
    image_limit: Arc<Semaphore>,

    pub chats: Vec<Chat>,
    pub chats_loading: bool,
    chats_exhausted: bool,
    /// How far back through the message history the list has been built.
    /// The date of the oldest message the list has been built back to.
    list_oldest: Option<i64>,
    detail_requested: HashSet<String>,
    pub chats_error: Option<String>,
    contacts: HashMap<String, String>,
    /// Every chat on the server, loaded quietly at startup so search can
    /// reach conversations older than the list has scrolled to.
    index: Vec<Chat>,
    /// The row highlighted in the search layer, counting conversations
    /// found by name first, then the conversations the words appear in,
    /// then messages.
    pub search_index: usize,
    /// Messages found by their text.
    pub found: Vec<crate::index::Hit>,
    /// The conversations those messages are in, the one with the most
    /// first, cut to the few that are listed.
    mentions: Vec<crate::index::Mention>,
    /// How many messages matched in all, and in how many conversations.
    pub found_total: u64,
    pub found_in: usize,
    search_generation: u64,
    /// The local copy of message text: how far it has got, and where
    /// newly arrived messages are handed to it.
    pub indexing: Option<crate::index::Progress>,
    index_tx: Option<UnboundedSender<Message>>,
    /// Search results beyond the loaded list: older conversations, and
    /// contacts you have no conversation with yet (as unsent drafts).
    search_extra: Vec<Chat>,
    /// Every contact address, as (name, address), for starting new chats.
    people: Vec<(String, String)>,
    /// The "To:" text while choosing who to start a conversation with.
    pub picking: Option<String>,
    pub pick_index: usize,
    /// Animation clock for the loading screen, advanced on each tick.
    pub frame: u64,
    /// Keep the loading screen up until this moment (used by the demo,
    /// which otherwise has nothing to wait for).
    pub splash_until: Option<Instant>,
    /// The help screen, and how far it is scrolled.
    pub help: bool,
    pub help_scroll: u16,
    /// Whether the terminal window has keyboard focus, when it tells us.
    window_focused: bool,
    notify: bool,
    /// Running without the interface, for `fizz bridge`: nothing is opened,
    /// announced or downloaded unless it is asked for.
    headless: bool,
    notified: HashSet<String>,
    last_sound: Option<Instant>,
    /// The most recent one-time code to arrive, and when.
    last_code: Option<(String, Instant)>,
    codes_seen: HashSet<String>,
    started_ms: i64,
    can_make_stills: bool,

    /// Chat under the list cursor, and the one shown on the right.
    pub cursor: Option<String>,
    pub open: Option<String>,
    pub list_top: usize,
    pub filter: String,
    pub filtering: bool,

    pub convos: HashMap<String, Convo>,
    pub images: HashMap<String, ImageState>,
    /// How many more GIFs may play in the open conversation.
    gif_budget: usize,
    pub focus: Focus,
    /// The stats screen, and what it needs to show.
    pub stats: bool,
    pub info: Option<ServerInfo>,
    pub started: Instant,
    pub usage: Usage,
    usage_at: Option<(Duration, Instant)>,
    pub cpu_percent: Option<f32>,
    /// Bytes in the picture cache, the downloaded-files cache, and the
    /// search index.
    pub cache_sizes: Option<(u64, u64, u64)>,
    /// A newer version's commit, when one has been pushed.
    pub update: Option<String>,
    /// The message being replied to: its id, which part, and a short
    /// description of it for the message box.
    pub replying: Option<(String, usize, String)>,
    /// The conversation waiting for "really delete?" to be answered.
    pub deleting: Option<String>,
    /// Conversations put in a folder of the chat list: id to folder name.
    filed: HashMap<String, String>,
    /// The rows that stand for folders in the main list, newest first, and
    /// the one that leads back out of an open folder.
    folder_rows: Vec<Chat>,
    back_row: Chat,
    /// The folder whose conversations the list is showing, if any.
    pub in_folder: Option<String>,
    /// The folder chooser, while it is up.
    pub filing: Option<Filing>,
    /// Chats where the other side is typing, and since when.
    typing_theirs: HashMap<String, Instant>,
    /// The chat we have told "typing…" to, if any.
    typing_mine: Option<String>,
    last_keystroke: Instant,
    /// People already added while choosing who a new group is for.
    pub pick_chosen: Vec<(String, String)>,
    /// Whether the picker was opened for a group (G) rather than one
    /// person (n): Enter then adds people instead of starting at once.
    pub pick_group: bool,
    /// Demo messages that have already had their pretend failure.
    demo_failed: HashSet<String>,
    /// The message waiting for "really unsend?" to be answered.
    pub unsending: Option<String>,
    /// The message being edited in the message box: its id, which part of
    /// it, and the draft that was in the box before.
    pub editing: Option<(String, usize, String)>,
    /// The tapback picker, and which of the six is highlighted.
    pub reacting: Option<usize>,
    /// The attachment highlighted while browsing.
    pub selected: Option<String>,
    /// Ask the next draw to scroll the highlighted attachment into view.
    pub reveal: bool,
    /// Where each attachment was drawn, for mouse clicks.
    pub hits: Vec<(Rect, String)>,
    downloading: HashSet<String>,
    pub input: Input,
    drafts: HashMap<String, String>,
    /// Which row of the emoji popup is highlighted, and whether Esc hid it.
    pub emoji_index: usize,
    emoji_hidden: bool,
    /// Files that will go out with the next send.
    pub pending_files: Vec<PathBuf>,
    draft_files: HashMap<String, Vec<PathBuf>>,

    unread: HashSet<String>,
    /// When each chat was last looked at (milliseconds), kept across runs
    /// so read chats stay read even though the server is not told.
    seen: HashMap<String, i64>,
    /// Chats opened lately, newest first: the ones worth remembering.
    recent_opens: Vec<String>,
    /// Conversations shown from memory that still need the server's
    /// current version.
    stale: HashSet<String>,
    /// Chats shown from memory that the server has not yet confirmed.
    unconfirmed: HashSet<String>,
    last_saved: Instant,
    last_trimmed: Instant,
    /// For each unread conversation, when the server was last asked
    /// whether it has been read elsewhere, and how many times so far.
    read_checks: HashMap<String, (Instant, u32)>,
    /// Fizz's database, for the small things written as you go.
    store: Option<rusqlite::Connection>,
    /// Whether the server can be told a chat was read.
    helper: bool,
    /// Chats already marked read on the server this run, with the newest
    /// message each had at the time, so the request is not repeated.
    marked: HashMap<String, i64>,

    pub online: Option<bool>,
    status: Option<(String, Instant)>,
    pub quit: bool,

    // Filled in while drawing, used for mouse hits and paging.
    pub list_rect: Rect,
    pub convo_rect: Rect,
    pub view_rows: usize,
    /// How wide the message box's text area is, for moving between lines.
    pub input_width: u16,
    pub wants_older: bool,
    pub wants_newer: bool,
    /// The message to highlight once a jump's history has arrived.
    jump_target: Option<String>,
    /// After a jump: keep the highlighted message in view as the layout
    /// settles, until the user scrolls or moves the selection.
    pinned: bool,
    temp_counter: u64,
}

/// Put a page from the server in the database, off the async threads.
/// `None` when that could not be done; the page is still shown.
async fn keep_page(
    chat: String,
    page: Vec<Message>,
    shape: crate::store::Shape,
    asked: i64,
) -> Option<crate::store::Stored> {
    tokio::task::spawn_blocking(move || {
        let mut conn = crate::store::open().ok()?;
        let stored = crate::store::keep_page(&mut conn, &chat, &page, shape, asked)
            .map_err(|e| crate::config::log(&format!("couldn't keep a page of messages: {e:#}")))
            .ok()?;
        if !stored.gone.is_empty() {
            crate::config::log(&format!(
                "a page of {} showed {} kept messages to have gone",
                page.len(),
                stored.gone.len()
            ));
        }
        Some(stored)
    })
    .await
    .ok()
    .flatten()
}

impl App {
    pub fn new(api: Option<Api>, tx: UnboundedSender<AppEvent>, picker: Picker) -> Self {
        App {
            api,
            tx,
            theme: Theme::load(),
            picker,
            image_limit: Arc::new(Semaphore::new(3)),
            chats: Vec::new(),
            chats_loading: false,
            chats_exhausted: false,
            list_oldest: None,
            detail_requested: HashSet::new(),
            chats_error: None,
            contacts: HashMap::new(),
            search_index: 0,
            found: Vec::new(),
            mentions: Vec::new(),
            found_total: 0,
            found_in: 0,
            search_generation: 0,
            indexing: None,
            index_tx: None,
            index: Vec::new(),
            search_extra: Vec::new(),
            people: Vec::new(),
            picking: None,
            pick_index: 0,
            frame: 0,
            splash_until: None,
            help: false,
            help_scroll: 0,
            window_focused: true,
            // FIZZ_NOTIFY=0 turns them off for one run without editing settings.
            notify: std::env::var("FIZZ_NOTIFY").map_or(
                crate::config::settings().notifications.enabled,
                |v| v != "0" && v != "off",
            ),
            headless: false,
            notified: HashSet::new(),
            last_sound: None,
            last_code: None,
            codes_seen: HashSet::new(),
            started_ms: chrono::Utc::now().timestamp_millis(),
            can_make_stills: false,
            cursor: None,
            open: None,
            list_top: 0,
            filed: HashMap::new(),
            folder_rows: Vec::new(),
            back_row: Chat {
                guid: BACK.to_string(),
                ..Chat::default()
            },
            in_folder: None,
            filing: None,
            filter: String::new(),
            filtering: false,
            convos: HashMap::new(),
            images: HashMap::new(),
            gif_budget: PLAYING_GIFS,
            focus: Focus::List,
            stats: false,
            info: None,
            started: Instant::now(),
            usage: Usage::default(),
            usage_at: None,
            cpu_percent: None,
            cache_sizes: None,
            update: None,
            replying: None,
            deleting: None,
            typing_theirs: HashMap::new(),
            typing_mine: None,
            last_keystroke: Instant::now(),
            pick_chosen: Vec::new(),
            pick_group: false,
            demo_failed: HashSet::new(),
            unsending: None,
            editing: None,
            reacting: None,
            selected: None,
            reveal: false,
            hits: Vec::new(),
            downloading: HashSet::new(),
            input: Input::default(),
            drafts: HashMap::new(),
            emoji_index: 0,
            emoji_hidden: false,
            pending_files: Vec::new(),
            draft_files: HashMap::new(),
            unread: HashSet::new(),
            seen: HashMap::new(),
            store: None,
            recent_opens: Vec::new(),
            stale: HashSet::new(),
            unconfirmed: HashSet::new(),
            last_saved: Instant::now(),
            last_trimmed: Instant::now(),
            read_checks: HashMap::new(),
            helper: false,
            marked: HashMap::new(),
            online: None,
            status: None,
            quit: false,
            list_rect: Rect::default(),
            convo_rect: Rect::default(),
            view_rows: 20,
            input_width: 60,
            wants_older: false,
            wants_newer: false,
            jump_target: None,
            pinned: false,
            temp_counter: 0,
        }
    }

    /// Load canned data instead of talking to a server.
    pub fn load_demo(&mut self) {
        let (chats, convos, contacts) = crate::demo::data();
        self.chats = chats;
        self.people = crate::demo::people();
        self.contacts = contacts;
        for (name, address) in &self.people {
            self.contacts
                .entry(normalize_address(address))
                .or_insert_with(|| name.clone());
        }
        for (guid, messages) in convos {
            let mut convo = Convo {
                loaded: true,
                exhausted: true,
                ..Convo::default()
            };
            for m in messages {
                convo.absorb(m);
            }
            convo.settle();
            self.convos.insert(guid, convo);
        }
        self.online = Some(true);
        self.unread.insert(self.chats[2].guid.clone());
        let first = self.chats[0].guid.clone();
        self.cursor = Some(first.clone());
        self.open_chat(&first);
        self.splash_until = Some(Instant::now() + Duration::from_millis(1600));
    }

    pub fn start(&mut self) {
        // The database first: opening a conversation reads from it.
        self.open_store();
        if !crate::index::keeps_whole()
            && let Some(conn) = &self.store
            && crate::store::whole_count(conn) > 0
        {
            // Turned off since last time: what was kept goes.
            if let Err(e) = crate::store::forget_whole(conn, crate::index::enabled()) {
                crate::config::log(&format!("couldn't remove the kept conversations: {e:#}"));
            }
        }
        self.restore();
        if crate::index::enabled() {
            self.indexing = Some(crate::index::Progress::load());
        }
        if crate::index::enabled() || crate::index::keeps_whole() {
            self.start_index();
        }
        if let Some(api) = self.api.clone() {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Ok(info) = api.server_info().await {
                    let _ = tx.send(AppEvent::Info(info));
                }
            });
        }
        if let Some(problem) = crate::config::settings_problem() {
            self.set_status(format!("Settings file ignored: {problem}"));
        }
        self.can_make_stills = images::can_make_stills();
        self.fetch_chats(false);
        if let Some(api) = self.api.clone() {
            let tx = self.tx.clone();
            tokio::spawn(async move {
                if let Ok(contacts) = api.contacts().await {
                    let _ = tx.send(AppEvent::Contacts(contacts));
                }
                // Every chat's name and members, a thousand at a time.
                const PAGE: usize = 1000;
                for page in 0..200 {
                    match api.all_chats(page * PAGE, PAGE).await {
                        Ok(chats) => {
                            let last = chats.len() < PAGE;
                            if tx.send(AppEvent::Index(chats)).is_err() || last {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }
    }

    /// As [`App::start`], for `fizz bridge`: the chat list, names and read
    /// markers, and nothing that belongs to the full interface (the saved
    /// session, the search index, every chat on the server).
    pub fn start_headless(&mut self) {
        self.headless = true;
        let Some(api) = self.api.clone() else {
            // The demo opens its first conversation; here nothing is open
            // until it is asked for.
            self.open = None;
            return;
        };
        self.open_store();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            if let Ok(info) = api.server_info().await {
                let _ = tx.send(AppEvent::Info(info));
            }
            if let Ok(contacts) = api.contacts().await {
                let _ = tx.send(AppEvent::Contacts(contacts));
            }
        });
        self.fetch_chats(false);
    }

    /// Open the database and load the read markers from it. Markers kept
    /// in a file by an older version are moved across, and the file
    /// removed.
    fn open_store(&mut self) {
        self.store = crate::store::open()
            .map_err(|e| crate::config::log(&format!("database unavailable: {e:#}")))
            .ok();
        if let Some(conn) = &self.store {
            if let Ok(bytes) = std::fs::read(Self::seen_path()) {
                let old: HashMap<String, i64> = serde_json::from_slice(&bytes).unwrap_or_default();
                if old.iter().all(|(chat, at)| crate::store::set_seen(conn, chat, *at).is_ok()) {
                    let _ = std::fs::remove_file(Self::seen_path());
                }
            }
            self.seen = crate::store::seen(conn);
            self.filed = crate::store::filed(conn);
        }
        self.rebuild_folders();
    }

    /// Take in read markers another running Fizz has written since ours
    /// were loaded (the app and the bar widget's bridge share them through
    /// the database). True when that changed anything.
    pub fn reload_seen(&mut self) -> bool {
        let Some(conn) = &self.store else {
            return false;
        };
        let theirs = crate::store::seen(conn);
        let mut changed = false;
        for (guid, at) in theirs {
            if self.seen.get(&guid).is_none_or(|ours| *ours < at) {
                self.seen.insert(guid.clone(), at);
                self.unread.remove(&guid);
                changed = true;
            }
        }
        changed
    }

    /// Stop showing any conversation, so what arrives next counts as unread.
    pub fn close_chat(&mut self) {
        self.open = None;
    }

    /// Send text to a conversation that already exists, as if it had been
    /// typed there. False when there is no such conversation.
    pub fn send_to(&mut self, guid: &str, text: &str) -> bool {
        if guid.starts_with("new:") || !self.chats.iter().any(|c| c.guid == guid) {
            return false;
        }
        self.open_chat(guid);
        self.input.set(text.to_string());
        self.send();
        true
    }

    /// Send one of our messages that failed again.
    pub fn retry(&mut self, chat: &str, message: &str) {
        if self.chats.iter().any(|c| c.guid == chat) {
            self.open_chat(chat);
            self.retry_failed(Some(message.to_string()));
        }
    }

    /// Ask the server for the chat list again.
    pub fn refresh_chats(&mut self) {
        self.fetch_chats(false);
    }

    // ----- names -------------------------------------------------------

    pub fn name_for(&self, address: &str) -> String {
        if let Some(name) = self.contacts.get(&normalize_address(address)) {
            return name.clone();
        }
        let digits: String = address.chars().filter(char::is_ascii_digit).collect();
        if address.starts_with("+1") && digits.len() == 11 {
            format!("({}) {}-{}", &digits[1..4], &digits[4..7], &digits[7..])
        } else {
            address.to_string()
        }
    }

    /// A phone number or email as people write it, never a contact name.
    pub fn pretty_address(&self, address: &str) -> String {
        let digits: String = address.chars().filter(char::is_ascii_digit).collect();
        if !address.contains('@') && digits.len() == 11 && digits.starts_with('1') {
            format!("({}) {}-{}", &digits[1..4], &digits[4..7], &digits[7..])
        } else if !address.contains('@') && digits.len() == 10 {
            format!("({}) {}-{}", &digits[..3], &digits[3..6], &digits[6..])
        } else {
            address.to_string()
        }
    }

    pub fn short_name_for(&self, address: &str) -> String {
        let full = self.name_for(address);
        if self.contacts.contains_key(&normalize_address(address)) {
            full.split_whitespace().next().unwrap_or(&full).to_string()
        } else {
            full
        }
    }

    pub fn chat_title(&self, chat: &Chat) -> String {
        if let Some(name) = chat.display_name.as_deref().filter(|n| !n.is_empty()) {
            return name.to_string();
        }
        match chat.participants.len() {
            0 => self.name_for(&chat.chat_identifier),
            1 => self.name_for(&chat.participants[0].address),
            _ => chat
                .participants
                .iter()
                .map(|p| self.short_name_for(&p.address))
                .collect::<Vec<_>>()
                .join(", "),
        }
    }

    pub fn is_unread(&self, chat: &Chat) -> bool {
        if chat.guid == BACK {
            return false;
        }
        if let Some(folder) = chat.guid.strip_prefix(FOLDER) {
            // A folder is unread while anything in it is.
            return self.folder_members(folder).any(|c| self.is_unread(c));
        }
        if self.unread.contains(&chat.guid) {
            return true;
        }
        let seen_at = self.seen.get(&chat.guid).copied().unwrap_or(0);
        chat.last_message.as_ref().is_some_and(|m| {
            !m.is_from_me && m.date_read.is_none() && m.item_type == 0 && m.date_created > seen_at
        })
    }

    /// A conversation was read on another device, as far as the message
    /// received at `through`: it is not unread here either. Nothing is
    /// sent to the server, which is where this was learnt.
    fn read_elsewhere(&mut self, guid: &str, through: i64) {
        self.unread.remove(guid);
        self.read_checks.remove(guid);
        if self.seen.get(guid).is_none_or(|at| *at < through) {
            self.seen.insert(guid.to_string(), through);
            if let Some(conn) = &self.store {
                let _ = crate::store::set_seen(conn, guid, through);
            }
        }
    }

    /// Ask the server about the conversations that are unread here: have
    /// they been read on the phone, or the Mac, since? Messages keeps the
    /// date each message was read, whichever device it was on. Each is
    /// asked about soon after it arrives and then less and less often, so
    /// a reply sent from the phone clears the mark here within seconds
    /// while a report nobody reads costs next to nothing.
    pub fn check_read_elsewhere(&mut self) {
        let Some(api) = self.api.clone() else { return };
        if self.online == Some(false) {
            return;
        }
        let now = Instant::now();
        let due: Vec<String> = self
            .chats
            .iter()
            .filter(|c| !c.is_draft() && self.open.as_deref() != Some(c.guid.as_str()) && self.is_unread(c))
            .filter(|c| {
                self.read_checks.get(&c.guid).is_none_or(|(asked, times)| {
                    // 10 s, 20 s, 40 s... up to about ten minutes apart.
                    now.duration_since(*asked) >= Duration::from_secs(10 << (*times).min(6))
                })
            })
            .take(6)
            .map(|c| c.guid.clone())
            .collect();
        for guid in due {
            let entry = self.read_checks.entry(guid.clone()).or_insert((now, 0));
            *entry = (now, entry.1 + 1);
            let (api, tx) = (api.clone(), self.tx.clone());
            tokio::spawn(async move {
                // The newest few: the very newest may be a tapback or ours.
                let Ok(page) = api.messages(&guid, None, None, 8).await else {
                    return;
                };
                let newest = page
                    .into_iter()
                    .filter(|m| !m.is_from_me && m.item_type == 0 && m.reaction().is_none())
                    .max_by_key(|m| m.date_created);
                if let Some(message) = newest {
                    let _ = tx.send(AppEvent::Newest { chat: guid, message });
                }
            });
        }
    }

    fn seen_path() -> PathBuf {
        crate::config::state_dir().join("seen.json")
    }

    /// Record that a chat has been read up to now: remembered here across
    /// restarts, and passed on to the server when it can accept it.
    fn mark_seen(&mut self, guid: &str) {
        self.unread.remove(guid);
        if guid.starts_with("new:") {
            return;
        }
        // The Mac's clock stamps the messages; allow for ours being behind.
        let latest = self
            .chats
            .iter()
            .find(|c| c.guid == guid)
            .map_or(0, Chat::last_activity);
        let now = chrono::Utc::now().timestamp_millis().max(latest);
        let was_unread = self.seen.get(guid).is_none_or(|at| *at < latest);
        self.seen.insert(guid.to_string(), now);
        let Some(api) = self.api.clone() else { return };
        if was_unread
            && let Some(conn) = &self.store
        {
            let _ = crate::store::set_seen(conn, guid, now);
        }
        // Tell the server too. This must not depend on what Fizz itself
        // remembers: a chat read here before the helper existed is still
        // unread on the Mac. Once per chat per newest message is enough.
        let already = self.marked.get(guid) == Some(&latest);
        if self.helper && crate::config::settings().messages.mark_read && !already {
            self.marked.insert(guid.to_string(), latest);
            let guid = guid.to_string();
            tokio::spawn(async move {
                match api.mark_read(&guid).await {
                    Ok(()) => crate::config::log("marked a chat read on the server"),
                    Err(e) => crate::config::log(&format!("mark read failed: {e:#}")),
                }
            });
        }
    }

    // ----- chat list ---------------------------------------------------

    /// Chats matching the search box, in display order.
    pub fn visible(&self) -> Vec<&Chat> {
        if let Some(folder) = &self.in_folder {
            // Inside a folder: the way back out, then what is in it.
            return std::iter::once(&self.back_row)
                .chain(self.folder_members(folder))
                .collect();
        }
        // The main list: conversations in no folder, and one row for each
        // folder, all in order of when anything last happened in them.
        let mut rows: Vec<&Chat> = self
            .chats
            .iter()
            .filter(|c| !self.filed.contains_key(&c.guid))
            .chain(&self.folder_rows)
            .collect();
        rows.sort_by_key(|c| std::cmp::Reverse(c.last_activity()));
        rows
    }

    // ----- folders -----------------------------------------------------

    /// The loaded conversations in a folder, newest first.
    pub fn folder_members<'a>(&'a self, folder: &'a str) -> impl Iterator<Item = &'a Chat> {
        self.chats
            .iter()
            .filter(move |c| self.filed.get(&c.guid).is_some_and(|f| f == folder))
    }

    /// The folder a conversation is in.
    pub fn folder_of(&self, guid: &str) -> Option<&str> {
        self.filed.get(guid).map(String::as_str)
    }

    /// Work out the rows that stand for folders again, after conversations
    /// or what is filed where have changed. A folder with nothing loaded
    /// in it has no row.
    fn rebuild_folders(&mut self) {
        let mut names: Vec<&String> = self.filed.values().collect();
        names.sort();
        names.dedup();
        let rows: Vec<Chat> = names
            .into_iter()
            .filter_map(|name| {
                // Chats are kept newest first, so the first is the newest.
                let newest = self.folder_members(name).next()?;
                Some(Chat {
                    guid: format!("{FOLDER}{name}"),
                    display_name: Some(name.clone()),
                    last_message: newest.last_message.clone(),
                    touched: newest.touched,
                    ..Chat::default()
                })
            })
            .collect();
        self.folder_rows = rows;
        if self
            .in_folder
            .as_ref()
            .is_some_and(|open| !self.folder_rows.iter().any(|row| row.guid[FOLDER.len()..] == **open))
        {
            // The folder being looked at has emptied.
            self.in_folder = None;
            self.cursor = self.visible().first().map(|c| c.guid.clone());
        }
    }

    /// Step into a folder, onto its newest conversation.
    fn enter_folder(&mut self, folder: &str) {
        self.in_folder = Some(folder.to_string());
        self.list_top = 0;
        let rows = self.visible();
        self.cursor = rows.get(1).or(rows.first()).map(|c| c.guid.clone());
    }

    /// Step back out to the main list, onto the folder's own row.
    fn leave_folder(&mut self) {
        if let Some(folder) = self.in_folder.take() {
            self.cursor = Some(format!("{FOLDER}{folder}"));
        }
    }

    /// `f` in the chat list: choose a folder for the conversation there.
    fn start_filing(&mut self) {
        let Some(guid) = self.cursor.clone() else { return };
        if guid.starts_with(FOLDER) || guid == BACK {
            return self.set_status("Open the folder, then press f on a conversation to move it".into());
        }
        if guid.starts_with("new:") {
            return self.set_status("A conversation can go in a folder once it has a message in it".into());
        }
        self.filing = Some(Filing {
            chat: guid,
            typed: String::new(),
            index: 0,
        });
    }

    /// What the folder chooser offers for what has been typed: the folders
    /// whose names contain it, a new one by that name, and the way out of
    /// the folder the conversation is in.
    pub fn filing_choices(&self) -> Vec<FilingChoice> {
        let Some(filing) = &self.filing else {
            return Vec::new();
        };
        let typed = filing.typed.trim();
        let current = self.folder_of(&filing.chat);
        let mut counts: Vec<(&String, usize)> = Vec::new();
        for folder in self.filed.values() {
            match counts.iter_mut().find(|(name, _)| *name == folder) {
                Some((_, count)) => *count += 1,
                None => counts.push((folder, 1)),
            }
        }
        counts.sort_by_key(|(name, _)| name.to_lowercase());
        let mut choices: Vec<FilingChoice> = counts
            .iter()
            .filter(|(name, _)| Some(name.as_str()) != current)
            .filter(|(name, _)| name.to_lowercase().contains(&typed.to_lowercase()))
            .map(|(name, count)| FilingChoice::Folder((*name).clone(), *count))
            .collect();
        let exists = counts.iter().any(|(name, _)| name.eq_ignore_ascii_case(typed));
        if !typed.is_empty() && !exists {
            choices.push(FilingChoice::New(typed.to_string()));
        }
        if let Some(current) = current {
            choices.push(FilingChoice::Out(current.to_string()));
        }
        choices
    }

    fn on_filing_key(&mut self, key: KeyEvent) {
        let choices = self.filing_choices();
        let Some(filing) = &mut self.filing else { return };
        match key.code {
            KeyCode::Esc => self.filing = None,
            KeyCode::Down | KeyCode::Tab if !choices.is_empty() => {
                filing.index = (filing.index + 1).min(choices.len() - 1)
            }
            KeyCode::Up => filing.index = filing.index.saturating_sub(1),
            KeyCode::Backspace => {
                filing.typed.pop();
                filing.index = 0;
            }
            KeyCode::Char(c)
                if !key.modifiers.contains(KeyModifiers::CONTROL)
                    && filing.typed.chars().count() < FOLDER_NAME =>
            {
                filing.typed.push(c);
                filing.index = 0;
            }
            KeyCode::Enter => {
                let chat = filing.chat.clone();
                if let Some(choice) = choices.get(filing.index.min(choices.len().saturating_sub(1))) {
                    self.filing = None;
                    self.file(&chat, choice.clone());
                }
            }
            _ => {}
        }
    }

    /// Carry out a choice from the folder chooser.
    fn file(&mut self, chat: &str, choice: FilingChoice) {
        // Where the cursor is, to leave it on whatever takes this row's place.
        let position = self.visible().iter().position(|c| c.guid == chat);
        let folder = match &choice {
            FilingChoice::Folder(name, _) | FilingChoice::New(name) => Some(name.clone()),
            FilingChoice::Out(_) => None,
        };
        match &folder {
            Some(name) => self.filed.insert(chat.to_string(), name.clone()),
            None => self.filed.remove(chat),
        };
        if let Some(conn) = &self.store
            && let Err(e) = crate::store::file(conn, chat, folder.as_deref())
        {
            crate::config::log(&format!("couldn't save a folder: {e:#}"));
        }
        self.rebuild_folders();
        let rows = self.visible();
        if !rows.iter().any(|c| c.guid == chat) {
            self.cursor = position
                .and_then(|p| rows.get(p).or(rows.last()))
                .map(|c| c.guid.clone());
        }
        self.set_status(match choice {
            FilingChoice::Folder(name, _) => format!("Moved to “{name}”"),
            FilingChoice::New(name) => format!("Moved to a new folder, “{name}”"),
            FilingChoice::Out(name) => format!("Taken out of “{name}”"),
        });
    }

    // ----- the search layer ------------------------------------------------

    /// Conversations and contacts matching the search text: loaded ones
    /// first, then older ones and contacts from beyond the list.
    pub fn conversation_hits(&self) -> Vec<&Chat> {
        if self.filter.trim().is_empty() {
            return Vec::new();
        }
        self.chats
            .iter()
            .filter(|c| self.matches_search(c))
            .chain(&self.search_extra)
            .take(SEARCH_CONVERSATIONS)
            .collect()
    }

    /// The conversations the searched words appear in, most matches
    /// first. Empty for a single match, where it would only repeat it.
    pub fn mention_hits(&self) -> &[crate::index::Mention] {
        match self.found_total {
            0 | 1 => &[],
            _ => &self.mentions,
        }
    }

    /// Where each section of the search layer starts, as row numbers:
    /// conversations by name, conversations by content, messages, the end.
    fn search_sections(&self) -> [usize; 4] {
        let names = self.conversation_hits().len();
        let mentions = names + self.mention_hits().len();
        [0, names, mentions, mentions + self.found.len()]
    }

    /// How many rows the search layer has to move through.
    pub fn search_rows(&self) -> usize {
        self.search_sections()[3]
    }

    /// Take in what a search found.
    fn set_found(&mut self, hits: Vec<crate::index::Hit>, mut mentions: Vec<crate::index::Mention>) {
        self.found_total = mentions.iter().map(|m| m.count).sum();
        self.found_in = mentions.len();
        mentions.truncate(SEARCH_MENTIONS);
        self.mentions = mentions;
        self.found = hits;
    }

    /// A conversation's name from its id, wherever we know it from.
    pub fn title_for(&self, guid: &str) -> String {
        self.chats
            .iter()
            .chain(&self.index)
            .find(|c| c.guid == guid)
            .map_or_else(|| "Conversation".to_string(), |c| self.chat_title(c))
    }

    /// Whether a conversation, by its id, has more than one other person
    /// in it. The id itself says so when we know nothing else about it.
    pub fn is_group(&self, guid: &str) -> bool {
        self.chats
            .iter()
            .chain(&self.index)
            .find(|c| c.guid == guid)
            .map_or_else(|| guid.contains(";+;"), Chat::is_group)
    }

    /// The search text changed: redo both kinds of result.
    fn search_changed(&mut self) {
        self.refresh_search();
        self.search_index = 0;
        self.search_generation += 1;
        let query = self.filter.trim().to_string();
        if query.chars().count() >= 2 && self.api.is_none() {
            // The demo searches its own made-up conversations.
            let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
            let mut found: Vec<crate::index::Hit> = self
                .convos
                .iter()
                .flat_map(|(chat, convo)| convo.messages.iter().map(move |m| (chat, m)))
                .filter(|(_, m)| {
                    let text = m.body().to_lowercase();
                    !text.is_empty() && words.iter().all(|w| text.contains(w.as_str()))
                })
                .map(|(chat, m)| crate::index::Hit {
                    chat: chat.clone(),
                    guid: m.guid.clone(),
                    date: m.date_created,
                    from_me: m.is_from_me,
                    sender: m.handle.as_ref().map_or(String::new(), |h| h.address.clone()),
                    text: m.body(),
                })
                .collect();
            found.sort_by_key(|h| std::cmp::Reverse(h.date));
            // Newest first, so the first of each conversation is where
            // opening that conversation should land.
            let mut mentions: Vec<crate::index::Mention> = Vec::new();
            for hit in &found {
                match mentions.iter_mut().find(|m| m.chat == hit.chat) {
                    Some(known) => known.count += 1,
                    None => mentions.push(crate::index::Mention {
                        chat: hit.chat.clone(),
                        count: 1,
                        date: hit.date,
                        guid: hit.guid.clone(),
                    }),
                }
            }
            mentions.sort_by_key(|m| (std::cmp::Reverse(m.count), std::cmp::Reverse(m.date)));
            self.set_found(found, mentions);
            return;
        }
        if query.chars().count() < 2 || !crate::index::enabled() {
            self.set_found(Vec::new(), Vec::new());
            return;
        }
        let (generation, tx) = (self.search_generation, self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let hits = crate::index::search(&query, SEARCH_MESSAGES).unwrap_or_default();
            let mentions = crate::index::mentions(&query).unwrap_or_default();
            let _ = tx.send(AppEvent::Found { generation, hits, mentions });
        });
    }

    /// Open whatever the search layer has highlighted.
    fn open_search_result(&mut self) {
        let conversations: Vec<Chat> = self.conversation_hits().into_iter().cloned().collect();
        self.filtering = false;
        if let Some(found) = conversations.get(self.search_index) {
            let guid = found.guid.clone();
            if let Some(address) = guid.strip_prefix("new:") {
                // A contact: opens their conversation if there is one.
                return self.start_chat(address);
            }
            self.adopt_chat(found.clone());
            self.cursor = Some(guid);
            return self.open_cursor();
        }
        let [_, names, mentions, _] = self.search_sections();
        // A conversation the words appear in opens at its newest match; a
        // message opens at that message.
        let (chat, guid, date, sender) = if self.search_index < mentions {
            let Some(found) = self.mention_hits().get(self.search_index.saturating_sub(names)) else {
                return;
            };
            (found.chat.clone(), found.guid.clone(), found.date, String::new())
        } else {
            let Some(hit) = self.found.get(self.search_index - mentions) else {
                return;
            };
            (hit.chat.clone(), hit.guid.clone(), hit.date, hit.sender.clone())
        };
        // Make sure the conversation is in the list, then open it at that
        // point in its history.
        let known = self
            .chats
            .iter()
            .chain(&self.index)
            .find(|c| c.guid == chat)
            .cloned()
            .unwrap_or_else(|| Chat {
                guid: chat.clone(),
                chat_identifier: sender,
                ..Chat::default()
            });
        self.adopt_chat(known);
        self.cursor = Some(chat.clone());
        self.open_at(&chat, &guid, date);
    }

    /// Move the highlight to the start of the next section of results,
    /// going round to the top after the last.
    fn next_search_section(&mut self) {
        let sections = self.search_sections();
        let rows = sections[3];
        self.search_index = sections[1..3]
            .iter()
            .copied()
            .find(|start| *start > self.search_index && *start < rows)
            .unwrap_or(0);
    }

    /// Open the search layer on some words, as typing them would.
    pub fn search_for(&mut self, words: &str) {
        self.filtering = true;
        self.filter = words.to_string();
        self.search_changed();
    }

    /// Put a conversation found by search into the list, if it is not there.
    fn adopt_chat(&mut self, mut chat: Chat) {
        if !self.chats.iter().any(|c| c.guid == chat.guid) {
            chat.touched = chrono::Utc::now().timestamp_millis();
            self.chats.push(chat);
            self.sort_chats();
        }
    }

    /// Start keeping message text on this computer, and keep it current.
    fn start_index(&mut self) {
        let Some(api) = self.api.clone() else { return };
        // Already going. (Once it has ended, because the index was turned
        // off from outside, its end of this channel is closed.)
        if self.index_tx.as_ref().is_some_and(|tx| !tx.is_closed()) {
            return;
        }
        let (live_tx, mut live_rx) = tokio::sync::mpsc::unbounded_channel();
        self.index_tx = Some(live_tx);
        let tx = self.tx.clone();
        tokio::spawn(async move {
            // One request the server does not answer must not end this for
            // the rest of the session: wait, then carry on from where it
            // stopped, less and less often while the trouble lasts.
            const FIRST_WAIT: Duration = Duration::from_secs(5);
            const LONGEST_WAIT: Duration = Duration::from_secs(300);
            let mut wait = FIRST_WAIT;
            loop {
                let began = Instant::now();
                let Err(e) = crate::index::run(api.clone(), tx.clone(), &mut live_rx, None).await
                else {
                    return;
                };
                if tx.is_closed() {
                    return;
                }
                if began.elapsed() > Duration::from_secs(60) {
                    // It had been working: this is new trouble.
                    wait = FIRST_WAIT;
                }
                crate::config::log(&format!(
                    "keeping messages in step paused: {e:#}; trying again in {} s",
                    wait.as_secs()
                ));
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(LONGEST_WAIT);
            }
        });
    }

    /// Ctrl+B in the search layer: the user agrees to the local copy.
    fn enable_index(&mut self) {
        if self.api.is_none() {
            return self.set_status("The demo has no messages to index".into());
        }
        match crate::index::enable() {
            Ok(()) => {
                // Whatever is running began without it: let that one end
                // (it does when this channel closes) and start afresh.
                self.index_tx = None;
                self.start_index();
                self.set_status("Downloading message text in the background…".into());
            }
            Err(e) => self.set_status(format!("Couldn't start: {e:#}")),
        }
    }

    /// Whether a chat's name, members or number match the search text.
    fn matches_search(&self, chat: &Chat) -> bool {
        let needle = self.filter.trim().to_lowercase();
        if self.chat_title(chat).to_lowercase().contains(&needle) {
            return true;
        }
        // Members of a named group, and numbers typed in any format.
        let digits: String = needle.chars().filter(char::is_ascii_digit).collect();
        let by_number = digits.len() >= 3 && digits.len() + 2 >= needle.len();
        chat.participants.iter().any(|p| {
            self.name_for(&p.address).to_lowercase().contains(&needle)
                || (by_number
                    && p.address
                        .chars()
                        .filter(char::is_ascii_digit)
                        .collect::<String>()
                        .contains(&digits))
        })
    }

    /// Rebuild the results that come from beyond the loaded list. Called
    /// whenever the search text changes.
    fn refresh_search(&mut self) {
        self.search_extra.clear();
        let needle = self.filter.trim().to_lowercase();
        if needle.chars().count() < 2 {
            return;
        }
        let loaded: HashSet<&str> = self.chats.iter().map(|c| c.guid.as_str()).collect();
        let mut older: Vec<Chat> = self
            .index
            .iter()
            .filter(|c| !loaded.contains(c.guid.as_str()) && self.matches_search(c))
            .cloned()
            .collect();
        // Named groups first, then by name; the server gives no dates here.
        older.sort_by_cached_key(|c| {
            let named = c.display_name.as_deref().is_some_and(|n| !n.is_empty());
            (!named, self.chat_title(c).to_lowercase())
        });
        older.truncate(40);

        // Contacts with no conversation among the matches so far.
        let mut taken: HashSet<String> = self
            .chats
            .iter()
            .filter(|c| self.matches_search(c))
            .chain(&older)
            .filter(|c| !c.is_group())
            .flat_map(|c| {
                c.participants
                    .iter()
                    .map(|p| normalize_address(&p.address))
                    .chain([normalize_address(&c.chat_identifier)])
            })
            .collect();
        let digits: String = needle.chars().filter(char::is_ascii_digit).collect();
        let people: Vec<Chat> = self
            .people
            .iter()
            .filter(|(name, address)| {
                name.to_lowercase().contains(&needle)
                    || (digits.len() >= 3
                        && address
                            .chars()
                            .filter(char::is_ascii_digit)
                            .collect::<String>()
                            .contains(&digits))
            })
            .filter(|(_, address)| taken.insert(normalize_address(address)))
            .take(15)
            .map(|(_, address)| {
                let mut draft = Chat::draft(&sendable_address(address));
                draft.touched = 0;
                draft
            })
            .collect();
        self.search_extra = older;
        self.search_extra.extend(people);
    }

    pub fn cursor_index(&self) -> usize {
        let visible = self.visible();
        self.cursor
            .as_ref()
            .and_then(|g| visible.iter().position(|c| &c.guid == g))
            .unwrap_or(0)
    }

    fn move_cursor(&mut self, delta: isize) {
        let (target, near_end) = {
            let visible = self.visible();
            if visible.is_empty() {
                return;
            }
            let last = visible.len() - 1;
            let index = (self.cursor_index() as isize + delta).clamp(0, last as isize) as usize;
            (visible[index].guid.clone(), index + 10 >= last)
        };
        self.cursor = Some(target);
        if near_end && self.filter.is_empty() {
            self.fetch_more_chats();
        }
    }

    fn cursor_to_edge(&mut self, end: bool) {
        let target = {
            let visible = self.visible();
            let pick = if end { visible.last() } else { visible.first() };
            pick.map(|c| c.guid.clone())
        };
        if target.is_some() {
            self.cursor = target;
        }
    }

    fn sort_chats(&mut self) {
        self.chats
            .sort_by_key(|c| std::cmp::Reverse(c.last_activity()));
        self.rebuild_folders();
    }

    /// Build (or extend) the chat list from the newest messages. The first
    /// page looks only at the last few months, which the server answers
    /// far faster than an open-ended request; `more` continues back from
    /// the oldest message seen so far.
    fn fetch_chats(&mut self, more: bool) {
        let Some(api) = self.api.clone() else { return };
        if self.chats_loading {
            return;
        }
        self.chats_loading = true;
        let now = chrono::Utc::now().timestamp_millis();
        let (before, after) = if more {
            (self.list_oldest.map(|oldest| oldest + 1), None)
        } else {
            (None, Some(now - RECENT_WINDOW_MS))
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .recent_messages(before, after, SCAN_PAGE)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Recent { more, result });
        });
    }

    fn fetch_more_chats(&mut self) {
        if !self.chats_exhausted {
            self.fetch_chats(true);
        }
    }

    // ----- conversations -----------------------------------------------

    /// Drop whatever was half-done in the open conversation: an edit (the
    /// message box goes back to the draft it held before), a reply, a
    /// tapback being chosen, an unsend waiting for its answer. Each names
    /// a message in that conversation, so none may outlive it being open.
    fn drop_unfinished(&mut self) {
        if let Some((_, _, draft)) = self.editing.take() {
            self.input.set(draft);
        }
        self.replying = None;
        self.reacting = None;
        self.unsending = None;
    }

    pub fn open_chat(&mut self, guid: &str) {
        // Wherever it was opened from (search, a notification's key), the
        // list shows where it lives: its folder, or the main list.
        let home = self.folder_of(guid).map(str::to_string);
        if self.in_folder != home && !self.headless {
            self.in_folder = home;
            self.list_top = 0;
        }
        if self.open.as_deref() != Some(guid) {
            self.drop_unfinished();
            if let Some(previous) = self.open.take() {
                let draft = self.input.take();
                let files = std::mem::take(&mut self.pending_files);
                if !files.is_empty() {
                    self.draft_files.insert(previous.clone(), files);
                }
                if draft.trim().is_empty() {
                    self.drafts.remove(&previous);
                } else {
                    self.drafts.insert(previous, draft);
                }
            }
            self.pending_files = self.draft_files.remove(guid).unwrap_or_default();
            self.input.set(self.drafts.remove(guid).unwrap_or_default());
            self.open = Some(guid.to_string());
            self.selected = None;
            // Pictures are kept only for the conversation on screen.
            self.images.clear();
            self.gif_budget = PLAYING_GIFS;
        }
        self.mark_seen(guid);
        self.recent_opens.retain(|g| g != guid);
        self.recent_opens.insert(0, guid.to_string());
        self.recent_opens.truncate(REMEMBERED_CONVERSATIONS);
        if self.stale.remove(guid) {
            // Shown from last time: fetch what it looks like now.
            self.refresh_latest(guid);
        }
        let convo = self.convos.entry(guid.to_string()).or_default();
        if convo.detached && !convo.loading {
            // Left over from an earlier jump into the past: start afresh.
            *convo = Convo::default();
        }
        convo.scroll = 0;
        if !convo.loaded && !convo.loading {
            // Opening it is asking again, whatever went wrong before.
            convo.retry_after = None;
            self.fetch_messages(guid);
        }
        self.request_images();
    }

    /// Whether whole messages are kept in the database, and it is there.
    fn keeps_whole(&self) -> bool {
        self.store.is_some() && crate::index::keeps_whole()
    }

    /// Ask the server for one page of a conversation and, when whole
    /// messages are kept, put it in the database before reporting it.
    fn fetch_page(
        &self,
        guid: &str,
        page: Page,
        before: Option<i64>,
        after: Option<i64>,
        shape: crate::store::Shape,
    ) {
        let Some(api) = self.api.clone() else { return };
        let (chat, tx, keep) = (guid.to_string(), self.tx.clone(), self.keeps_whole());
        tokio::spawn(async move {
            let asked = chrono::Utc::now().timestamp_millis();
            let result = api
                .messages(&chat, before, after, MESSAGE_PAGE)
                .await
                .map_err(|e| e.to_string());
            let stored = match (&result, keep) {
                (Ok(messages), true) => keep_page(chat.clone(), messages.clone(), shape, asked).await,
                _ => None,
            };
            let _ = tx.send(AppEvent::Messages {
                chat,
                page,
                result,
                stored,
            });
        });
    }

    /// Load the next page of a conversation going back (or its newest, if
    /// nothing is loaded): from the database at once where that part is
    /// held whole, and from the server, which has the last word.
    fn fetch_messages(&mut self, guid: &str) {
        if self.api.is_none() {
            return;
        }
        let keep = self.keeps_whole();
        let offline = self.online == Some(false);
        let convo = self.convos.entry(guid.to_string()).or_default();
        if convo.loading || convo.exhausted || convo.retry_after.is_some_and(|at| at > Instant::now()) {
            return;
        }
        convo.error = None;
        // One past the oldest we hold, so nothing sharing its timestamp is
        // skipped; anything fetched twice is simply recognised.
        let before = convo.messages.first().map(|m| m.date_created + 1);
        // What is held here comes first. A whole page of it can be shown,
        // and scrolled on from, without waiting for the server.
        let mut shown = false;
        let mut whole_page = false;
        if keep && let Some(conn) = &self.store {
            let local = crate::store::page(conn, guid, before, MESSAGE_PAGE);
            if !local.messages.is_empty() {
                shown = true;
                whole_page = local.complete;
                convo.loaded = true;
                convo.exhausted |= local.reaches_start;
                for message in local.messages {
                    convo.absorb(message);
                }
                convo.settle();
            }
        }
        if whole_page && offline {
            // Nothing to ask: this is all there is to know for now.
            return self.request_images();
        }
        convo.loading = !whole_page;
        let (page, shape) = match before {
            // The newest page, confirming what was just shown from here.
            None if shown => (Page::Latest, crate::store::Shape::Latest { limit: MESSAGE_PAGE }),
            None => (Page::Older, crate::store::Shape::Latest { limit: MESSAGE_PAGE }),
            Some(before) => (
                Page::Older,
                crate::store::Shape::Older {
                    before,
                    limit: MESSAGE_PAGE,
                },
            ),
        };
        self.fetch_page(guid, page, before, None, shape);
        if shown {
            self.request_images();
        }
    }

    /// Fetch a conversation's newest page to bring a remembered copy up to
    /// date, without disturbing what is on screen in the meantime.
    fn refresh_latest(&mut self, guid: &str) {
        let shape = crate::store::Shape::Latest { limit: MESSAGE_PAGE };
        self.fetch_page(guid, Page::Latest, None, None, shape);
    }

    fn snapshot_path() -> PathBuf {
        crate::config::state_dir().join("snapshot.json")
    }

    /// Show what was on screen last time, before the server has answered.
    fn restore(&mut self) {
        let path = Self::snapshot_path();
        if !crate::config::settings().startup.remember {
            let _ = std::fs::remove_file(path);
            return;
        }
        let Some(snapshot) = std::fs::read(path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Snapshot>(&bytes).ok())
            .filter(|snapshot| snapshot.version == SNAPSHOT_VERSION)
        else {
            return;
        };
        self.contacts = snapshot.contacts;
        self.people = snapshot.people;
        self.chats = snapshot.chats;
        self.unconfirmed = self.chats.iter().map(|c| c.guid.clone()).collect();
        for (guid, messages) in snapshot.conversations {
            let mut convo = Convo {
                loaded: true,
                ..Convo::default()
            };
            for message in messages {
                convo.absorb(message);
            }
            convo.settle();
            self.stale.insert(guid.clone());
            self.convos.insert(guid, convo);
        }
        self.sort_chats();
        let first = snapshot
            .open
            .filter(|guid| self.chats.iter().any(|c| &c.guid == guid))
            .or_else(|| self.chats.first().map(|c| c.guid.clone()));
        if let Some(guid) = first {
            self.cursor = Some(guid.clone());
            self.open_chat(&guid);
        }
    }

    /// Remember the chat list and the last few conversations for next time.
    pub fn remember(&mut self) {
        self.last_saved = Instant::now();
        if self.api.is_none() || !crate::config::settings().startup.remember {
            return;
        }
        let chats: Vec<Chat> = self
            .chats
            .iter()
            .filter(|c| !c.is_draft())
            .take(REMEMBERED_CHATS)
            .cloned()
            .collect();
        let conversations = self
            .recent_opens
            .iter()
            .filter_map(|guid| {
                let convo = self.convos.get(guid)?;
                // A view parked in the past is not what to reopen to.
                (convo.loaded && !convo.detached).then(|| (guid.clone(), convo.newest(REMEMBERED_MESSAGES)))
            })
            .collect();
        let snapshot = Snapshot {
            version: SNAPSHOT_VERSION,
            chats,
            contacts: self.contacts.clone(),
            people: self.people.clone(),
            open: self.open.clone().filter(|g| !g.starts_with("new:")),
            conversations,
        };
        let Ok(json) = serde_json::to_vec(&snapshot) else {
            return;
        };
        let path = Self::snapshot_path();
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let tmp = path.with_extension("tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let written = options
            .open(&tmp)
            .and_then(|mut file| std::io::Write::write_all(&mut file, &json));
        if written.is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }

    /// When the view is away from the present: fetch the next newer page.
    fn fetch_newer(&mut self, guid: &str) {
        if self.api.is_none() {
            return;
        }
        let Some(convo) = self.convos.get_mut(guid) else {
            return;
        };
        if !convo.detached || convo.loading_newer {
            return;
        }
        let Some(after) = convo.messages.last().map(|m| m.date_created - 1) else {
            return;
        };
        convo.loading_newer = true;
        self.fetch_page(guid, Page::Newer, None, Some(after), crate::store::Shape::Loose);
    }

    /// Open a conversation at one message from its past, loading the
    /// history on both sides of it.
    pub fn open_at(&mut self, chat: &str, message: &str, date: i64) {
        if self.api.is_none() {
            // The demo holds whole conversations already: just go there.
            self.open_chat(chat);
            self.selected = Some(format!("text:{message}"));
            self.focus = Focus::Browse;
            self.reveal = true;
            return;
        }
        // Mark it loading first, so opening does not also fetch the latest;
        // nor is a copy from last time waiting to be brought up to date.
        self.stale.remove(chat);
        self.convos.insert(
            chat.to_string(),
            Convo {
                loading: true,
                ..Convo::default()
            },
        );
        self.open_chat(chat);
        if let Some(convo) = self.convos.get_mut(chat) {
            convo.detached = true;
        }
        self.jump_target = Some(message.to_string());
        let Some(api) = self.api.clone() else { return };
        let (chat, tx, keep) = (chat.to_string(), self.tx.clone(), self.keeps_whole());
        tokio::spawn(async move {
            const SIDE: usize = 30;
            let asked = chrono::Utc::now().timestamp_millis();
            let older = api.messages(&chat, Some(date + 1), None, SIDE);
            let newer = api.messages(&chat, None, Some(date), SIDE);
            let result = match tokio::join!(older, newer) {
                (Ok(mut older), Ok(newer)) => {
                    let more_newer = newer.len() >= SIDE;
                    older.extend(newer);
                    Ok((older, more_newer))
                }
                (Err(e), _) | (_, Err(e)) => Err(e.to_string()),
            };
            let (page, result) = match result {
                Ok((messages, more_newer)) => (Page::Around { more_newer }, Ok(messages)),
                Err(e) => (Page::Around { more_newer: false }, Err(e)),
            };
            // Kept, though a stretch from the middle of a conversation
            // says nothing about what lies either side of it.
            let stored = match (&result, keep) {
                (Ok(messages), true) => {
                    keep_page(chat.clone(), messages.clone(), crate::store::Shape::Loose, asked).await
                }
                _ => None,
            };
            let _ = tx.send(AppEvent::Messages {
                chat,
                page,
                result,
                stored,
            });
        });
    }

    /// Back to the newest messages: scroll down, or reload the present if
    /// the view had jumped into the past.
    fn jump_to_latest(&mut self) {
        let Some(guid) = self.open.clone() else { return };
        let detached = self.convos.get(&guid).is_some_and(|c| c.detached);
        if detached {
            self.convos.insert(guid.clone(), Convo::default());
            self.selected = None;
            self.fetch_messages(&guid);
        } else if let Some(convo) = self.convos.get_mut(&guid) {
            convo.scroll = 0;
        }
    }

    /// Called after each draw: fetch older history once the top is in view.
    pub fn after_draw(&mut self) {
        if std::mem::take(&mut self.wants_older)
            && let Some(guid) = self.open.clone()
        {
            self.fetch_messages(&guid);
        }
        if std::mem::take(&mut self.wants_newer)
            && let Some(guid) = self.open.clone()
        {
            self.fetch_newer(&guid);
        }
    }

    fn request_images(&mut self) {
        if self.headless {
            return;
        }
        let Some(convo) = self.open.as_ref().and_then(|g| self.convos.get(g)) else {
            return;
        };
        let media = &crate::config::settings().media;
        if !media.pictures {
            return;
        }
        let stills = self.can_make_stills && media.video_stills;
        let still_limit = media.video_still_limit_mb.saturating_mul(1024 * 1024);
        let wanted: Vec<Attachment> = convo
            .messages
            .iter()
            .filter(|m| !m.pending)
            .flat_map(|m| &m.attachments)
            .filter(|a| !a.is_plugin_payload())
            .filter(|a| {
                a.is_image()
                    || (stills && a.is_video() && a.total_bytes.is_some_and(|b| b <= still_limit))
            })
            .filter(|a| !self.images.contains_key(&a.guid))
            .cloned()
            .collect();
        let font = self.picker.font_size();
        let box_px = (
            u32::from(media.picture_columns) * u32::from(font.width),
            u32::from(media.picture_rows) * u32::from(font.height),
        );
        // Every frame of a playing GIF is a picture held in memory, here
        // and in the terminal, so only so many play at once in a
        // conversation. The newest get the places; older ones show a still.
        let can_animate = self.can_animate();
        for attachment in wanted.into_iter().rev() {
            let animate = can_animate && attachment.is_gif() && self.gif_budget > 0;
            if animate {
                self.gif_budget -= 1;
            }
            let guid = attachment.guid.clone();
            match self.api.clone() {
                Some(api) => {
                    self.images.insert(guid, ImageState::Loading);
                    images::fetch(
                        api,
                        attachment,
                        box_px,
                        animate,
                        self.image_limit.clone(),
                        self.tx.clone(),
                    );
                }
                None if animate && attachment.is_gif() => {
                    let frames = crate::demo::animation(&guid, box_px);
                    self.set_image(guid, Ok(images::Loaded::Frames(frames)));
                }
                None => {
                    let picture = crate::demo::picture(&guid, box_px);
                    self.set_image(guid, Ok(images::Loaded::Still(picture)));
                }
            }
        }
    }

    fn set_image(&mut self, guid: String, result: Result<images::Loaded, String>) {
        let state = match result {
            Ok(images::Loaded::Still(picture)) => SlicedProtocol::new(&self.picker, picture, None)
                .map_or(ImageState::Failed, ImageState::Ready),
            Ok(images::Loaded::Frames(frames)) => {
                let (mut pictures, mut delays) = (Vec::new(), Vec::new());
                for (picture, delay) in frames {
                    if let Ok(protocol) = SlicedProtocol::new(&self.picker, picture, None) {
                        pictures.push(protocol);
                        delays.push(delay);
                    }
                }
                if pictures.is_empty() {
                    ImageState::Failed
                } else {
                    ImageState::Animated {
                        frames: pictures,
                        delays,
                        started: Instant::now(),
                    }
                }
            }
            Err(_) => ImageState::Failed,
        };
        self.images.insert(guid, state);
    }

    /// Whether GIFs can play here: the setting is on and the terminal
    /// holds pictures by reference, so changing frame costs almost nothing.
    fn can_animate(&self) -> bool {
        use ratatui_image::picker::ProtocolType;
        crate::config::settings().media.animate_gifs
            && self.picker.protocol_type() == ProtocolType::Kitty
    }

    // ----- selecting messages, pictures and files ------------------------

    /// Pictures and files in the open conversation, oldest first.
    fn attachments(&self) -> Vec<&Attachment> {
        self.open
            .as_ref()
            .and_then(|g| self.convos.get(g))
            .map(|convo| {
                convo
                    .messages
                    .iter()
                    .flat_map(|m| &m.attachments)
                    .filter(|a| !a.is_plugin_payload())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Everything that can be selected, oldest first: each attachment by
    /// its guid, and each message's text as `text:<message guid>`.
    fn targets(&self) -> Vec<String> {
        let Some(convo) = self.open.as_ref().and_then(|g| self.convos.get(g)) else {
            return Vec::new();
        };
        let mut keys = Vec::new();
        for message in convo.messages.iter().filter(|m| m.item_type == 0) {
            for attachment in message.attachments.iter().filter(|a| !a.is_plugin_payload()) {
                keys.push(attachment.guid.clone());
            }
            if message.date_retracted.is_none() && !message.body().is_empty() {
                keys.push(format!("text:{}", message.guid));
            }
        }
        keys
    }

    /// The message whose text is selected, if a text is.
    pub fn selected_text(&self) -> Option<&Message> {
        let guid = self.selected.as_deref()?.strip_prefix("text:")?;
        self.open
            .as_ref()
            .and_then(|g| self.convos.get(g))?
            .messages
            .iter()
            .find(|m| m.guid == guid)
    }

    /// The message behind the current selection, and which part of it is
    /// selected (attachments count from zero, the text comes after them).
    pub fn selected_message(&self) -> Option<(&Message, usize)> {
        let key = self.selected.as_deref()?;
        let convo = self.open.as_ref().and_then(|g| self.convos.get(g))?;
        let parts = |m: &Message| m.attachments.iter().filter(|a| !a.is_plugin_payload()).count();
        if let Some(guid) = key.strip_prefix("text:") {
            let message = convo.messages.iter().find(|m| m.guid == guid)?;
            return Some((message, parts(message)));
        }
        convo.messages.iter().find_map(|m| {
            m.attachments
                .iter()
                .filter(|a| !a.is_plugin_payload())
                .position(|a| a.guid == key)
                .map(|index| (m, index))
        })
    }

    /// The tapback of mine already on the selected message, if any.
    fn my_tapback(&self) -> Option<usize> {
        let (message, _) = self.selected_message()?;
        let convo = self.open.as_ref().and_then(|g| self.convos.get(g))?;
        let (_, glyph) = convo
            .reactions
            .get(&message.guid)?
            .iter()
            .find(|(who, _)| who == "me")?;
        TAPBACKS.iter().position(|(_, g)| g == glyph)
    }

    /// The selected message, if it is one of yours that has really gone out.
    fn selected_own(&mut self, verb: &str) -> Option<(String, usize, i64, String)> {
        let found = self.selected_message().map(|(m, part)| {
            (
                m.is_from_me && !m.pending && m.failed.is_none() && m.date_retracted.is_none(),
                m.guid.clone(),
                part,
                m.date_created,
                m.body(),
            )
        });
        match found {
            Some((true, guid, part, sent, body)) => Some((guid, part, sent, body)),
            Some(_) => {
                self.set_status(format!("You can only {verb} messages you have sent"));
                None
            }
            None => None,
        }
    }

    /// How long ago a message was sent, in minutes.
    fn minutes_since(sent_ms: i64) -> i64 {
        (chrono::Utc::now().timestamp_millis() - sent_ms) / 60_000
    }

    // ----- retrying a failed send ----------------------------------------

    /// Whether the selected message is one of ours that failed to send.
    pub fn selected_failed(&self) -> bool {
        self.selected_message()
            .is_some_and(|(m, _)| m.failed.is_some())
    }

    /// Send a failed message again: the one given, or the newest failure
    /// in the open conversation.
    fn retry_failed(&mut self, guid: Option<String>) {
        if self.editing.is_some() {
            return;
        }
        let Some(chat) = self.open.clone() else { return };
        let Some(convo) = self.convos.get_mut(&chat) else {
            return;
        };
        let found = convo.messages.iter().rposition(|m| {
            m.failed.is_some() && guid.as_ref().is_none_or(|g| &m.guid == g)
        });
        let Some(index) = found else {
            return self.set_status("Nothing here failed to send".into());
        };
        let failed = convo.messages.remove(index);
        // Put it back through the ordinary send path, without disturbing
        // whatever is being written at the moment.
        let draft = self.input.take();
        let files = std::mem::take(&mut self.pending_files);
        let replying = self.replying.take();
        match failed.local_file {
            Some(path) => self.pending_files = vec![path],
            None => self.input.set(failed.body()),
        }
        if let Some(original) = failed.thread_originator_guid {
            self.replying = Some((original, failed.reply_part, String::new()));
        }
        self.send();
        self.input.set(draft);
        self.pending_files = files;
        self.replying = replying;
        self.selected = None;
        self.set_status("Trying again…".into());
    }

    /// Give up on a failed message and remove it from the conversation.
    fn discard_failed(&mut self) {
        let Some(guid) = self
            .selected_message()
            .filter(|(m, _)| m.failed.is_some())
            .map(|(m, _)| m.guid.clone())
        else {
            return;
        };
        if let Some(convo) = self.open.as_ref().and_then(|g| self.convos.get_mut(g)) {
            convo.messages.retain(|m| m.guid != guid);
        }
        self.selected = None;
        self.focus = Focus::Compose;
    }

    fn start_replying(&mut self) {
        let Some((message, part)) = self.selected_message() else {
            return;
        };
        if message.pending || message.failed.is_some() {
            return self.set_status("That message has not been sent yet".into());
        }
        let who = if message.is_from_me {
            "yourself".to_string()
        } else {
            message
                .handle
                .as_ref()
                .map_or("them".into(), |h| self.short_name_for(&h.address))
        };
        let label = format!("{who}: {}", message.summary());
        self.replying = Some((message.guid.clone(), part, label));
        self.focus = Focus::Compose;
        if !self.helper {
            self.set_status("Replies need the server's Private API helper; this may be refused".into());
        }
    }

    // ----- typing indicators ---------------------------------------------

    /// Whether the other side is typing in this chat right now.
    pub fn is_typing(&self, chat: &str) -> bool {
        self.typing_theirs
            .get(chat)
            // Some senders never say they stopped; do not show it forever.
            .is_some_and(|since| since.elapsed() < Duration::from_secs(60))
    }

    /// Start or stop our own "typing…" so it matches what is on screen:
    /// shown while there is unsent text that was touched recently.
    fn sync_typing(&mut self) {
        let wanted = self
            .open
            .clone()
            .filter(|chat| !chat.starts_with("new:"))
            .filter(|_| {
                self.helper
                    && crate::config::settings().messages.send_typing
                    && self.focus == Focus::Compose
                    && self.editing.is_none()
                    && !self.input.is_empty()
                    && self.last_keystroke.elapsed() < Duration::from_secs(8)
            });
        if wanted == self.typing_mine {
            return;
        }
        let Some(api) = self.api.clone() else { return };
        let (stop, start) = (self.typing_mine.take(), wanted.clone());
        self.typing_mine = wanted;
        tokio::spawn(async move {
            if let Some(chat) = stop {
                let _ = api.typing(&chat, false).await;
            }
            if let Some(chat) = start {
                let _ = api.typing(&chat, true).await;
            }
        });
    }

    /// On the way out: make sure "typing…" is not left showing.
    pub fn stop_typing(&mut self) -> Option<impl std::future::Future<Output = ()> + use<>> {
        let chat = self.typing_mine.take()?;
        let api = self.api.clone()?;
        Some(async move {
            let _ = api.typing(&chat, false).await;
        })
    }

    // ----- deleting a conversation ---------------------------------------

    fn start_deleting(&mut self) {
        let Some(guid) = self.cursor.clone() else { return };
        if guid.starts_with(FOLDER) || guid == BACK {
            return self.set_status("That is a folder: open it to delete a conversation in it".into());
        }
        if guid.starts_with("new:") {
            // Never sent, so there is nothing on the server to delete.
            return self.forget_chat(&guid);
        }
        self.deleting = Some(guid);
    }

    fn delete_chat(&mut self) {
        let Some(chat) = self.deleting.take() else { return };
        match self.api.clone() {
            Some(api) => {
                self.set_status("Deleting…".into());
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.delete_chat(&chat).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppEvent::Deleted { chat, result });
                });
            }
            None => self.forget_chat(&chat),
        }
    }

    /// Remove a conversation from everything Fizz holds.
    fn forget_chat(&mut self, guid: &str) {
        let position = self.visible().iter().position(|c| c.guid == guid);
        self.chats.retain(|c| c.guid != guid);
        self.index.retain(|c| c.guid != guid);
        self.convos.remove(guid);
        self.drafts.remove(guid);
        self.unread.remove(guid);
        if self.filed.remove(guid).is_some() {
            if let Some(conn) = &self.store {
                let _ = crate::store::file(conn, guid, None);
            }
            self.rebuild_folders();
        }
        if self.open.as_deref() == Some(guid) {
            self.drop_unfinished();
            self.open = None;
            self.images.clear();
            self.gif_budget = PLAYING_GIFS;
            self.input.clear();
            self.focus = Focus::List;
        }
        // Leave the cursor on whatever took its place in the list.
        let next = {
            let visible = self.visible();
            position
                .and_then(|p| visible.get(p).or(visible.last()))
                .map(|c| c.guid.clone())
        };
        self.cursor = next;
    }

    fn start_unsending(&mut self) {
        if let Some((guid, _, sent, _)) = self.selected_own("unsend") {
            self.unsending = Some(guid);
            if Self::minutes_since(sent) >= 2 {
                self.set_status(
                    "Apple only allows unsending for about 2 minutes, so this may be refused".into(),
                );
            }
        }
    }

    fn unsend(&mut self) {
        let Some(target) = self.unsending.take() else { return };
        let Some(chat) = self.open.clone() else { return };
        let part = self.selected_message().map_or(0, |(_, part)| part);
        match self.api.clone() {
            Some(api) => {
                self.set_status("Unsending…".into());
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.unsend(&target, part).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppEvent::Changed {
                        chat,
                        what: "unsend",
                        text: None,
                        result,
                    });
                });
            }
            None => {
                // The demo applies it straight away.
                if let Some(m) = self
                    .convos
                    .get_mut(&chat)
                    .and_then(|c| c.messages.iter_mut().find(|m| m.guid == target))
                {
                    m.date_retracted = Some(chrono::Utc::now().timestamp_millis());
                }
            }
        }
    }

    fn start_editing(&mut self) {
        let Some((guid, part, sent, body)) = self.selected_own("edit") else {
            return;
        };
        if body.is_empty() {
            self.set_status("Only a message's text can be edited".into());
            return;
        }
        let draft = self.input.take();
        self.input.set(body);
        self.editing = Some((guid, part, draft));
        self.focus = Focus::Compose;
        if Self::minutes_since(sent) >= 15 {
            self.set_status(
                "Apple only allows editing for about 15 minutes, so this may be refused".into(),
            );
        }
    }

    fn cancel_editing(&mut self) {
        if let Some((_, _, draft)) = self.editing.take() {
            self.input.set(draft);
            self.focus = Focus::Browse;
        }
    }

    fn send_edit(&mut self) {
        let text = self.input.text().trim().to_string();
        if text.is_empty() {
            self.set_status("An edit needs some text (u unsends a message instead)".into());
            return;
        }
        let Some((target, part, draft)) = self.editing.take() else {
            return;
        };
        self.input.set(draft);
        self.focus = Focus::Browse;
        let Some(chat) = self.open.clone() else { return };
        match self.api.clone() {
            Some(api) => {
                self.set_status("Saving the edit…".into());
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api.edit(&target, &text, part).await.map_err(|e| e.to_string());
                    let _ = tx.send(AppEvent::Changed {
                        chat,
                        what: "edit",
                        text: Some(text),
                        result,
                    });
                });
            }
            None => {
                if let Some(m) = self
                    .convos
                    .get_mut(&chat)
                    .and_then(|c| c.messages.iter_mut().find(|m| m.guid == target))
                {
                    m.text = Some(text);
                    m.date_edited = Some(chrono::Utc::now().timestamp_millis());
                }
            }
        }
    }

    fn start_reacting(&mut self) {
        match self.selected_message() {
            Some((message, _)) if message.pending || message.failed.is_some() => {
                self.set_status("That message has not been sent yet".into())
            }
            Some(_) => self.reacting = Some(self.my_tapback().unwrap_or(0)),
            None => {}
        }
    }

    /// Send the chosen tapback; choosing the one already there removes it.
    fn send_reaction(&mut self, choice: usize) {
        self.reacting = None;
        let Some(chat) = self.open.clone() else { return };
        let Some((message, part)) = self.selected_message() else {
            return;
        };
        let (target, part) = (message.guid.clone(), part);
        let (kind, glyph) = TAPBACKS[choice.min(TAPBACKS.len() - 1)];
        let reaction = if self.my_tapback() == Some(choice) {
            format!("-{kind}")
        } else {
            kind.to_string()
        };
        match self.api.clone() {
            Some(api) => {
                self.set_status(format!("Sending {glyph}…"));
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    let result = api
                        .react(&chat, &target, &reaction, part)
                        .await
                        .map_err(|e| e.to_string());
                    let _ = tx.send(AppEvent::Reacted { chat, result });
                });
            }
            None => {
                // The demo applies it straight away.
                self.temp_counter += 1;
                let event = Message {
                    guid: format!("demo-reaction-{}", self.temp_counter),
                    is_from_me: true,
                    date_created: chrono::Utc::now().timestamp_millis(),
                    associated_message_guid: Some(format!("p:{part}/{target}")),
                    associated_message_type: Some(serde_json::Value::String(reaction)),
                    ..Message::default()
                };
                let convo = self.convos.entry(chat).or_default();
                convo.absorb(event);
                convo.settle();
            }
        }
    }

    fn browse(&mut self) {
        let all = self.targets();
        let Some(newest) = all.last() else {
            self.set_status("Nothing to select in the messages loaded so far".into());
            return;
        };
        if !self.selected.as_ref().is_some_and(|s| all.contains(s)) {
            self.selected = Some(newest.clone());
        }
        self.focus = Focus::Browse;
        self.reveal = true;
    }

    fn move_selection(&mut self, delta: isize) {
        let all = self.targets();
        let Some(current) = self
            .selected
            .as_ref()
            .and_then(|s| all.iter().position(|k| k == s))
        else {
            return;
        };
        let index = (current as isize + delta).clamp(0, all.len() as isize - 1) as usize;
        self.selected = Some(all[index].clone());
        self.reveal = true;
        self.pinned = false;
    }

    /// Open or save an attachment, downloading it first if needed.
    fn act_on(&mut self, guid: &str, then: Then) {
        let Some(attachment) = self
            .attachments()
            .into_iter()
            .find(|a| a.guid == guid)
            .cloned()
        else {
            return;
        };
        let Some(api) = self.api.clone() else {
            self.set_status("Opening and saving need a real server (this is the demo)".into());
            return;
        };
        if !self.downloading.insert(attachment.guid.clone()) {
            return;
        }
        let verb = if then == Then::Open { "Opening" } else { "Saving" };
        self.set_status(format!("{verb} {}…", attachment.name()));
        files::fetch(api, attachment, then, self.tx.clone());
    }

    /// Enter on the selection: open the file, or the link in the text.
    fn open_selected(&mut self) {
        let Some(key) = self.selected.clone() else { return };
        if !key.starts_with("text:") {
            return self.act_on(&key, Then::Open);
        }
        match self.selected_text().and_then(Message::first_link) {
            Some(link) => match files::open(&link) {
                Ok(()) => self.set_status(format!("Opened {link}")),
                Err(e) => self.set_status(format!("{e:#}")),
            },
            None => self.set_status("No link in this message (c copies its text)".into()),
        }
    }

    fn save_selected(&mut self) {
        match self.selected.clone() {
            Some(key) if !key.starts_with("text:") => self.act_on(&key, Then::Save),
            Some(_) => self.set_status("Only pictures and files can be saved (c copies text)".into()),
            None => {}
        }
    }

    fn copy_selected(&mut self) {
        let Some(text) = self.selected_text().map(Message::body) else {
            self.set_status("Select a message's text to copy it".into());
            return;
        };
        match files::copy_text(&text) {
            Ok(()) => self.set_status("Copied".into()),
            Err(e) => self.set_status(format!("Couldn't copy: {e:#}")),
        }
    }

    // ----- starting a new conversation -----------------------------------

    /// People matching the "To:" text, as (label, address).
    pub fn pick_candidates(&self) -> Vec<(String, String)> {
        let Some(query) = self.picking.as_deref() else {
            return Vec::new();
        };
        let needle = query.trim().to_lowercase();
        let digits: String = needle.chars().filter(char::is_ascii_digit).collect();
        let mut found: Vec<(String, String)> = self
            .people
            .iter()
            .filter(|(name, address)| {
                needle.is_empty()
                    || name.to_lowercase().contains(&needle)
                    || address.to_lowercase().contains(&needle)
                    || (digits.len() >= 3
                        && address
                            .chars()
                            .filter(char::is_ascii_digit)
                            .collect::<String>()
                            .contains(&digits))
            })
            .take(60)
            .cloned()
            .collect();
        // Something typed that is itself a number or an email address.
        let looks_like_email = needle.contains('@') && needle.contains('.') && !needle.contains(' ');
        let looks_like_phone = digits.len() >= 7
            && needle.chars().all(|c| c.is_ascii_digit() || " +-().".contains(c));
        if looks_like_email || looks_like_phone {
            let address = sendable_address(&needle);
            if !found.iter().any(|(_, a)| sendable_address(a) == address) {
                found.insert(0, (format!("Send to {address}"), address));
            }
        }
        // People already in the group are not offered again.
        found.retain(|(_, address)| !self.pick_chosen.iter().any(|(_, a)| a == address));
        // Once a group has two people and nothing is being typed, the first
        // row is the way to finish, so Enter's meaning is never a guess.
        if self.pick_group && needle.is_empty() && self.pick_chosen.len() >= 2 {
            found.insert(0, ("Start the group".to_string(), START_GROUP.to_string()));
        }
        found
    }

    fn start_chat(&mut self, address: &str) {
        let address = sendable_address(address);
        let wanted = normalize_address(&address);
        self.picking = None;
        self.pick_chosen.clear();
        self.filter.clear();
        let existing = self
            .chats
            .iter()
            .find(|c| {
                !c.is_group()
                    && (normalize_address(&c.chat_identifier) == wanted
                        || c.participants
                            .first()
                            .is_some_and(|p| normalize_address(&p.address) == wanted))
            })
            .map(|c| c.guid.clone());
        if let Some(guid) = existing {
            self.cursor = Some(guid);
            return self.open_cursor();
        }
        let Some(api) = self.api.clone() else {
            return self.open_draft(&address);
        };
        // It may exist further back than the list has loaded. One-to-one
        // chat ids are the address behind a fixed prefix; borrow the prefix
        // this server uses from a chat we already know.
        let prefix = self
            .chats
            .iter()
            .find(|c| !c.is_draft() && !c.is_group())
            .and_then(|c| c.guid.split(';').next())
            .unwrap_or("iMessage")
            .to_string();
        self.set_status("Looking for an existing conversation…".into());
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let chat = api.chat(&format!("{prefix};-;{address}")).await.ok();
            let _ = tx.send(AppEvent::Resolved { address, chat });
        });
    }

    /// Start a new group with everyone chosen in the picker.
    fn start_group(&mut self, addresses: Vec<String>) {
        self.picking = None;
        self.pick_chosen.clear();
        self.pick_group = false;
        self.filter.clear();
        let mut addresses: Vec<String> = addresses.iter().map(|a| sendable_address(a)).collect();
        addresses.dedup();
        match addresses.as_slice() {
            [] => {}
            [one] => self.start_chat(&one.clone()),
            _ => {
                self.open_draft_chat(Chat::draft_group(&addresses));
                if !self.helper && self.api.is_some() {
                    self.set_status(
                        "New groups need the server's Private API helper; sending may be refused".into(),
                    );
                }
            }
        }
    }

    fn open_draft(&mut self, address: &str) {
        self.open_draft_chat(Chat::draft(address));
    }

    fn open_draft_chat(&mut self, draft: Chat) {
        let guid = draft.guid.clone();
        if !self.chats.iter().any(|c| c.guid == guid) {
            self.chats.push(draft);
        }
        // Nothing to fetch for a conversation that does not exist yet.
        let convo = self.convos.entry(guid.clone()).or_default();
        convo.loaded = true;
        convo.exhausted = true;
        self.sort_chats();
        self.cursor = Some(guid);
        self.open_cursor();
        self.set_status("New conversation: it is created when you send the first message".into());
    }

    // ----- notifications -------------------------------------------------

    /// If a message that just arrived carries a one-time code, keep it at
    /// hand: say so, remember it for Ctrl+Y, and copy it if asked to.
    fn note_code(&mut self, message: &Message) {
        let settings = &crate::config::settings().codes;
        if self.headless
            || !settings.detect
            || message.is_from_me
            || message.date_created < self.started_ms - 10_000
            || !self.codes_seen.insert(message.guid.clone())
        {
            return;
        }
        let Some(code) = crate::codes::find(&message.body()) else {
            return;
        };
        let from = message
            .handle
            .as_ref()
            .map_or("someone".to_string(), |h| self.name_for(&h.address));
        let copied = settings.auto_copy && files::copy_text(&code).is_ok();
        self.set_status(if copied {
            format!("Code {code} from {from} copied")
        } else {
            format!("Code {code} from {from} · Ctrl+Y copies it")
        });
        // A code is wanted for longer than an ordinary status line lasts.
        if let Some((_, at)) = &mut self.status {
            *at = Instant::now() + Duration::from_secs(40);
        }
        self.last_code = Some((code, Instant::now()));
    }

    /// Ctrl+Y: copy the most recent code, while it could still be valid.
    fn copy_last_code(&mut self) {
        let recent = self
            .last_code
            .as_ref()
            .filter(|(_, at)| at.elapsed() < Duration::from_secs(15 * 60))
            .map(|(code, _)| code.clone());
        match recent {
            Some(code) => match files::copy_text(&code) {
                Ok(()) => self.set_status(format!("Copied {code}")),
                Err(e) => self.set_status(format!("Couldn't copy: {e:#}")),
            },
            None => self.set_status("No code has arrived in the last 15 minutes".into()),
        }
    }

    /// Capital C on a selected message: copy just the code in it.
    fn copy_selected_code(&mut self) {
        let code = self
            .selected_message()
            .and_then(|(m, _)| crate::codes::find(&m.body()));
        match code {
            Some(code) => match files::copy_text(&code) {
                Ok(()) => self.set_status(format!("Copied {code}")),
                Err(e) => self.set_status(format!("Couldn't copy: {e:#}")),
            },
            None => self.set_status("No code in this message (c copies its text)".into()),
        }
    }

    /// A desktop notification for a message that just arrived, unless you
    /// are looking at that conversation right now.
    fn maybe_notify(&mut self, message: &Message) {
        let sound = crate::config::settings().notifications.sound.as_str();
        let wants_sound = !matches!(sound.trim(), "" | "off");
        if self.headless
            || (!self.notify && !wants_sound)
            || self.api.is_none()
            || message.is_from_me
            || message.item_type != 0
            || message.date_created < self.started_ms - 10_000
            || !self.notified.insert(message.guid.clone())
        {
            return;
        }
        let chat_guid = message.chats.first().map(|c| c.guid.as_str());
        let watching = self.window_focused && chat_guid.is_some() && chat_guid == self.open.as_deref();
        if watching {
            return;
        }
        let sender = message
            .handle
            .as_ref()
            .map_or("Someone".to_string(), |h| self.name_for(&h.address));
        let chat = chat_guid.and_then(|g| self.chats.iter().find(|c| c.guid == g));
        let title = match chat {
            Some(chat) if chat.is_group() => format!("{sender} · {}", self.chat_title(chat)),
            _ => sender,
        };
        let code = crate::config::settings()
            .codes
            .detect
            .then(|| crate::codes::find(&message.body()))
            .flatten();
        let body = match (crate::config::settings().notifications.preview, code) {
            // Lead with the code, so it can be read off the notification.
            (true, Some(code)) => format!("{code}  ·  {}", message.summary()),
            (true, None) => message.summary(),
            (false, _) => "New message".to_string(),
        };
        if self.notify {
            files::notify(&title, &body);
        }
        // One sound for a burst of messages, not one each.
        let recently = self
            .last_sound
            .is_some_and(|at| at.elapsed() < Duration::from_millis(1500));
        if wants_sound && !recently {
            files::play_sound(sound);
            self.last_sound = Some(Instant::now());
        }
    }

    fn scroll_by(&mut self, rows: isize) {
        self.pinned = false;
        if let Some(convo) = self.open.as_ref().and_then(|g| self.convos.get_mut(g)) {
            convo.scroll = convo.scroll.saturating_add_signed(rows);
        }
    }

    // ----- sending -----------------------------------------------------

    /// Emoji to offer for the `:name` being typed; empty when there is none.
    pub fn emoji_matches(&self) -> Vec<emoji::Match> {
        if self.focus != Focus::Compose
            || self.emoji_hidden
            || !crate::config::settings().messages.emoji_popup
        {
            return Vec::new();
        }
        self.input
            .emoji_query()
            .map(|query| emoji::search(query, 7))
            .unwrap_or_default()
    }

    /// Queue files to go out with the next message in the open chat.
    fn attach(&mut self, paths: Vec<PathBuf>) {
        if self.open.is_none() {
            self.set_status("Open a conversation first, then drop the file".into());
            return;
        }
        let mut added = Vec::new();
        for path in paths {
            if !self.pending_files.contains(&path) {
                added.push(path.clone());
                self.pending_files.push(path);
            }
        }
        self.focus = Focus::Compose;
        match added.as_slice() {
            [] => {}
            [one] => {
                let name = one.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                self.set_status(format!("Attached {name}"));
            }
            many => self.set_status(format!("Attached {} files", many.len())),
        }
    }

    /// Ctrl+V: a picture on the clipboard becomes an attachment, text is typed.
    fn paste_clipboard(&mut self) {
        let tx = self.tx.clone();
        tokio::task::spawn_blocking(move || {
            let event = match files::read_clipboard() {
                Ok(files::Clipboard::Picture(path)) => AppEvent::Attach(vec![path]),
                Ok(files::Clipboard::Text(text)) => AppEvent::PasteText(text),
                Ok(files::Clipboard::Empty) => AppEvent::Status("Nothing to paste".into()),
                Err(e) => AppEvent::Status(format!("{e:#}")),
            };
            let _ = tx.send(event);
        });
    }

    /// Send the attached files, then the text, in that order.
    fn send(&mut self) {
        enum Outgoing {
            File(PathBuf),
            Text(String),
        }
        if self.editing.is_some() {
            return self.send_edit();
        }
        let Some(chat) = self.open.clone() else { return };
        let text = self.input.text().trim().to_string();
        if let Some(address) = chat.strip_prefix("new:") {
            return self.send_first(&chat, address, text);
        }
        if self.convos.get(&chat).is_some_and(|c| c.detached) {
            // What is sent belongs at the end of the conversation, so go
            // back there before adding it.
            self.jump_to_latest();
        }
        let mut queue: Vec<Outgoing> = std::mem::take(&mut self.pending_files)
            .into_iter()
            .map(Outgoing::File)
            .collect();
        if !text.is_empty() {
            queue.push(Outgoing::Text(text));
        }
        if queue.is_empty() {
            return;
        }
        self.input.clear();
        // A reply applies to the text that goes out now.
        let reply = self.replying.take().map(|(guid, part, _)| (guid, part));

        let now = chrono::Utc::now().timestamp_millis();
        let mut jobs = Vec::new();
        let mut last = None;
        for (i, item) in queue.into_iter().enumerate() {
            self.temp_counter += 1;
            let temp = format!("temp-{}-{}", std::process::id(), self.temp_counter);
            let mut placeholder = Message {
                guid: temp.clone(),
                is_from_me: true,
                // Keep the placeholders in the order they will be sent.
                date_created: now + i as i64,
                pending: self.api.is_some(),
                is_delivered: self.api.is_none(),
                ..Message::default()
            };
            match &item {
                Outgoing::Text(text) => {
                    placeholder.text = Some(text.clone());
                    placeholder.thread_originator_guid =
                        reply.as_ref().map(|(guid, _)| guid.clone());
                    placeholder.reply_part = reply.as_ref().map_or(0, |(_, part)| *part);
                    // In the demo, a message containing "!fail" fails the
                    // first time, so retrying can be tried out.
                    if self.api.is_none()
                        && text.contains("!fail")
                        && self.demo_failed.insert(text.clone())
                    {
                        placeholder.is_delivered = false;
                        placeholder.failed =
                            Some("the connection to the server failed (demo)".into());
                    }
                }
                Outgoing::File(path) => {
                    placeholder.local_file = Some(path.clone());
                    placeholder.attachments = vec![Attachment {
                        guid: format!("{temp}-file"),
                        transfer_name: path
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(str::to_string),
                        total_bytes: std::fs::metadata(path).ok().map(|m| m.len()),
                        ..Attachment::default()
                    }]
                }
            }
            let convo = self.convos.entry(chat.clone()).or_default();
            convo.absorb(placeholder.clone());
            last = Some(placeholder);
            jobs.push((temp, item));
        }
        if let Some(c) = self.chats.iter_mut().find(|c| c.guid == chat) {
            c.last_message = last;
        }
        self.sort_chats();
        let convo = self.convos.entry(chat.clone()).or_default();
        convo.settle();
        convo.scroll = 0;
        if self.api.is_none() {
            // In the demo the other side "types" for a few seconds, so the
            // indicator can be seen.
            self.typing_theirs
                .insert(chat.clone(), Instant::now() - Duration::from_secs(55));
        }

        if let Some(api) = self.api.clone() {
            let tx = self.tx.clone();
            // One at a time, so they arrive in the order shown.
            tokio::spawn(async move {
                for (temp, item) in jobs {
                    let mut tries = 0;
                    let result = loop {
                        tries += 1;
                        let attempt = match &item {
                            Outgoing::Text(text) => {
                                let reply_to =
                                    reply.as_ref().map(|(guid, part)| (guid.as_str(), *part));
                                api.send_text(&chat, &temp, text, reply_to).await
                            }
                            Outgoing::File(path) => api.send_attachment(&chat, &temp, path).await,
                        }
                        .map_err(|e| e.to_string());
                        // If the server was never reached, nothing was sent,
                        // so trying again cannot produce a duplicate.
                        match &attempt {
                            Err(e) if e == crate::api::UNREACHABLE && tries < 3 => {
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                            }
                            _ => break attempt,
                        }
                    };
                    let sent = AppEvent::Sent {
                        chat: chat.clone(),
                        temp,
                        result,
                    };
                    if tx.send(sent).is_err() {
                        return;
                    }
                }
            });
        }
    }

    /// The first message of a new conversation, which is what creates it.
    fn send_first(&mut self, draft: &str, address: &str, text: String) {
        if text.is_empty() {
            self.set_status("Type a message first: a new conversation starts with text".into());
            return;
        }
        if !self.pending_files.is_empty() {
            self.set_status("The files stay attached; they can go once the first text is sent".into());
        }
        let convo = self.convos.entry(draft.to_string()).or_default();
        if convo.messages.iter().any(|m| m.pending) {
            return;
        }
        self.input.clear();
        self.temp_counter += 1;
        let temp = format!("temp-{}-{}", std::process::id(), self.temp_counter);
        convo.messages.retain(|m| m.failed.is_none());
        convo.absorb(Message {
            guid: temp.clone(),
            text: Some(text.clone()),
            is_from_me: true,
            date_created: chrono::Utc::now().timestamp_millis(),
            pending: self.api.is_some(),
            is_delivered: self.api.is_none(),
            ..Message::default()
        });
        convo.settle();
        if let Some(api) = self.api.clone() {
            let (tx, draft) = (self.tx.clone(), draft.to_string());
            // A group's draft lists its addresses separated by commas.
            let addresses: Vec<String> = address.split(',').map(str::to_string).collect();
            tokio::spawn(async move {
                let result = api
                    .new_chat(&addresses, &temp, &text)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::Started { draft, temp, result });
            });
        }
    }

    // ----- events from background work ---------------------------------

    pub fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Index(chats) => {
                self.index.extend(chats);
                if !self.filter.is_empty() {
                    self.refresh_search();
                }
            }
            AppEvent::Update(found) => self.update = found,
            AppEvent::Info(info) => {
                let connected = info.private_api && info.helper_connected;
                self.info = Some(info);
                self.helper = connected;
                // Catch up the chat that was opened before we knew.
                if connected
                    && crate::config::settings().messages.mark_read
                    && let (Some(api), Some(guid)) = (self.api.clone(), self.open.clone())
                    && !guid.starts_with("new:")
                {
                    let latest = self
                        .chats
                        .iter()
                        .find(|c| c.guid == guid)
                        .map_or(0, Chat::last_activity);
                    self.marked.insert(guid.clone(), latest);
                    tokio::spawn(async move {
                        match api.mark_read(&guid).await {
                            Ok(()) => crate::config::log("marked a chat read on the server"),
                            Err(e) => crate::config::log(&format!("mark read failed: {e:#}")),
                        }
                    });
                }
            }
            AppEvent::Typing { chat, on } => {
                if on {
                    self.typing_theirs.insert(chat, Instant::now());
                } else {
                    self.typing_theirs.remove(&chat);
                }
            }
            AppEvent::Deleted { chat, result } => match result {
                Ok(()) => {
                    self.forget_chat(&chat);
                    self.seen.remove(&chat);
                    if let Some(conn) = &self.store {
                        crate::store::forget_seen(conn, &chat);
                        crate::store::forget_chat(conn, &chat);
                    }
                    self.set_status("Conversation deleted".into());
                }
                Err(e) if e.contains("Helper is not connected") => self.set_status(
                    "Deleting a conversation needs the server's Private API helper".into(),
                ),
                Err(e) => self.set_status(format!("Couldn't delete it: {e}")),
            },
            AppEvent::Changed {
                chat,
                what,
                text,
                result,
            } => match result {
                Ok(mut updated) => {
                    // Make sure the change shows even if the reply lags it.
                    let now = chrono::Utc::now().timestamp_millis();
                    match text {
                        Some(text) => {
                            updated.text = Some(text);
                            updated.date_edited.get_or_insert(now);
                        }
                        None => {
                            updated.date_retracted.get_or_insert(now);
                        }
                    }
                    let convo = self.convos.entry(chat).or_default();
                    // Replies to these requests come without attachments.
                    if let Some(existing) = convo.messages.iter().find(|m| m.guid == updated.guid)
                        && updated.attachments.is_empty()
                    {
                        updated.attachments = existing.attachments.clone();
                    }
                    convo.absorb(updated);
                    convo.settle();
                    self.status = None;
                }
                Err(e) if e.contains("Helper is not connected") => self.set_status(format!(
                    "To {what} a message the server needs its Private API helper connected"
                )),
                Err(e) => self.set_status(format!("Couldn't {what} it: {e}")),
            },
            AppEvent::Reacted { chat, result } => match result {
                Ok(event) => {
                    let convo = self.convos.entry(chat).or_default();
                    convo.absorb(event);
                    convo.settle();
                    self.status = None;
                }
                Err(e) if e.contains("Helper is not connected") => self.set_status(
                    "Tapbacks need the Private API helper connected on the server".into(),
                ),
                Err(e) => self.set_status(format!("Couldn't react: {e}")),
            },
            AppEvent::ChatDetail(detail) => {
                if let Some(chat) = self.chats.iter_mut().find(|c| c.guid == detail.guid) {
                    chat.participants = detail.participants;
                    if detail.display_name.as_deref().is_some_and(|n| !n.is_empty()) {
                        chat.display_name = detail.display_name;
                    }
                }
            }
            AppEvent::Recent { more, result } => {
                self.chats_loading = false;
                match result {
                    Ok(page) => {
                        self.chats_error = None;
                        self.online = Some(true);
                        let now = chrono::Utc::now().timestamp_millis();
                        let full = page.len() >= SCAN_PAGE;
                        // How far back this page reaches. A short first page
                        // covered its whole window; a short later page means
                        // there is no more history.
                        let reached = match (page.iter().map(|m| m.date_created).min(), more, full) {
                            (Some(oldest), _, true) | (Some(oldest), true, false) => oldest,
                            _ => now - RECENT_WINDOW_MS,
                        };
                        if more && !full {
                            self.chats_exhausted = true;
                        }
                        self.list_oldest = Some(self.list_oldest.map_or(reached, |o| o.min(reached)));
                        if !more {
                            // Chats remembered from last time that the server
                            // should have mentioned in this page but did not
                            // have been deleted since.
                            let seen: HashSet<&str> = page
                                .iter()
                                .flat_map(|m| m.chats.iter().map(|c| c.guid.as_str()))
                                .collect();
                            let open = self.open.clone();
                            let gone: Vec<String> = std::mem::take(&mut self.unconfirmed)
                                .into_iter()
                                .filter(|guid| {
                                    !seen.contains(guid.as_str())
                                        && open.as_deref() != Some(guid.as_str())
                                        && self
                                            .chats
                                            .iter()
                                            .find(|c| &c.guid == guid)
                                            .is_some_and(|c| c.last_activity() > reached)
                                })
                                .collect();
                            self.chats.retain(|c| !gone.contains(&c.guid));
                        }
                        for message in &page {
                            self.note_chat_activity(message);
                        }
                        self.sort_chats();
                        self.fetch_missing_details();
                        if self.cursor.is_none() {
                            self.cursor = self.chats.first().map(|c| c.guid.clone());
                        }
                        if self.open.is_none()
                            && !self.headless
                            && let Some(first) = self.cursor.clone()
                        {
                            self.open_chat(&first);
                        }
                    }
                    Err(e) => {
                        if self.chats.is_empty() {
                            self.chats_error = Some(e);
                        } else {
                            self.set_status(format!("Couldn't refresh chats: {e}"));
                        }
                        self.online = Some(false);
                    }
                }
            }
            AppEvent::Contacts(contacts) => {
                for contact in contacts {
                    let name = contact
                        .display_name
                        .filter(|n| !n.trim().is_empty())
                        .or_else(|| {
                            let joined = format!(
                                "{} {}",
                                contact.first_name.unwrap_or_default(),
                                contact.last_name.unwrap_or_default()
                            );
                            let joined = joined.trim().to_string();
                            (!joined.is_empty()).then_some(joined)
                        });
                    let Some(name) = name else { continue };
                    for entry in contact.phone_numbers.iter().chain(&contact.emails) {
                        let key = normalize_address(&entry.address);
                        if self.contacts.insert(key, name.clone()).is_none() {
                            self.people.push((name.clone(), entry.address.clone()));
                        }
                    }
                }
                self.people.sort_by_key(|(name, _)| name.to_lowercase());
            }
            AppEvent::Messages {
                chat,
                page,
                result,
                stored,
            } => {
                let convo = self.convos.entry(chat.clone()).or_default();
                if page == Page::Latest && convo.detached {
                    // Asked for before the view jumped into the past: the
                    // present does not belong in it.
                    return;
                }
                match page {
                    Page::Newer => convo.loading_newer = false,
                    _ => convo.loading = false,
                }
                match result {
                    Ok(messages) => {
                        convo.loaded = true;
                        convo.retry_after = None;
                        match page {
                            Page::Older => {
                                if messages.len() < MESSAGE_PAGE {
                                    convo.exhausted = true;
                                }
                            }
                            Page::Newer => {
                                // Caught up with the present.
                                if messages.len() < MESSAGE_PAGE {
                                    convo.detached = false;
                                }
                                convo.grew_below = true;
                            }
                            Page::Around { more_newer } => convo.detached = more_newer,
                            Page::Latest => {
                                // If what was on screen does not reach as
                                // far as this page, there is a gap of
                                // messages we never saw between them: start
                                // again from the page rather than show
                                // history with a hole in it. Two things must
                                // hold. What is on screen must share a
                                // message with the page: it may have come
                                // from last time's snapshot, which the
                                // database's word does not cover. And the
                                // database, where it can say, must hold
                                // the two as one run: a message that
                                // arrived live is shared with the page
                                // however much is missing before it.
                                let shares = convo.messages.is_empty()
                                    || messages
                                        .iter()
                                        .any(|m| convo.messages.iter().any(|c| c.guid == m.guid));
                                let joins = shares
                                    && stored.as_ref().is_none_or(|stored| !stored.vouched || stored.joined);
                                let whole = messages.len() < MESSAGE_PAGE;
                                if !joins || whole {
                                    // Only what the page reaches back to
                                    // is known to run on unbroken.
                                    let from = match whole {
                                        true => i64::MIN,
                                        false => messages.iter().map(|m| m.date_created).min().unwrap_or(i64::MIN),
                                    };
                                    let in_page: HashSet<&str> = messages.iter().map(|m| m.guid.as_str()).collect();
                                    let stays = |m: &Message| {
                                        m.pending
                                            || m.failed.is_some()
                                            || in_page.contains(m.guid.as_str())
                                            || (!whole && m.date_created >= from)
                                            // Too new for the page to have had it.
                                            || m.date_created > chrono::Utc::now().timestamp_millis() - 60_000
                                    };
                                    convo.messages.retain(stays);
                                    convo.reaction_events.retain(stays);
                                    convo.exhausted = whole;
                                }
                            }
                        }
                        // What the page showed to have been deleted since.
                        if let Some(stored) = &stored
                            && !stored.gone.is_empty()
                        {
                            let gone = |m: &Message| stored.gone.iter().any(|g| *g == m.guid);
                            convo.messages.retain(|m| !gone(m));
                            convo.reaction_events.retain(|m| !gone(m));
                        }
                        for message in messages {
                            convo.absorb(message);
                        }
                        convo.settle();
                        // A chat opened from search has no preview yet.
                        if let (Some(newest), Some(entry)) = (
                            convo.messages.last().cloned(),
                            self.chats.iter_mut().find(|c| c.guid == chat),
                        ) && entry.last_message.is_none()
                        {
                            entry.last_message = Some(newest);
                        }
                    }
                    Err(e) => {
                        // Shown only when there is nothing else to show: a
                        // conversation read from the database stays up.
                        convo.error = Some(e);
                        convo.retry_after = Some(Instant::now() + Duration::from_secs(10));
                    }
                }
                if self.open.as_deref() == Some(chat.as_str()) {
                    self.request_images();
                    // Arrived from a jump: highlight the message we came for.
                    if matches!(page, Page::Around { .. })
                        && let Some(target) = self.jump_target.take()
                    {
                        let key = self.convos.get(&chat).and_then(|c| {
                            let m = c.messages.iter().find(|m| m.guid == target)?;
                            if m.body().is_empty() {
                                m.attachments
                                    .iter()
                                    .find(|a| !a.is_plugin_payload())
                                    .map(|a| a.guid.clone())
                            } else {
                                Some(format!("text:{}", m.guid))
                            }
                        });
                        if key.is_some() {
                            self.selected = key;
                            self.focus = Focus::Browse;
                            self.reveal = true;
                            // Pictures around it are still arriving and
                            // will shift things; hold it in view meanwhile.
                            self.pinned = true;
                        }
                    }
                }
            }
            AppEvent::Newest { chat, message } => {
                if message.date_read.is_some() {
                    // The preview's copy of it learns the same.
                    if let Some(last) = self
                        .chats
                        .iter_mut()
                        .find(|c| c.guid == chat)
                        .and_then(|c| c.last_message.as_mut())
                        .filter(|last| last.guid == message.guid)
                    {
                        last.date_read = message.date_read;
                    }
                    self.read_elsewhere(&chat, message.date_created);
                }
            }
            AppEvent::ReadStatus { chat, read } => {
                let latest = self.chats.iter().find(|c| c.guid == chat).map(Chat::last_activity);
                match (read, latest) {
                    (true, Some(latest)) => self.read_elsewhere(&chat, latest),
                    (false, Some(_)) if self.open.as_deref() != Some(chat.as_str()) => {
                        self.seen.remove(&chat);
                        self.unread.insert(chat);
                    }
                    _ => {}
                }
            }
            AppEvent::Indexing(progress) => self.indexing = Some(progress),
            AppEvent::Found { generation, hits, mentions } => {
                // Results for text that has since been edited are stale.
                if generation == self.search_generation {
                    self.set_found(hits, mentions);
                    self.search_index = self.search_index.min(self.search_rows().saturating_sub(1));
                }
            }
            AppEvent::Incoming(messages) => {
                for message in messages {
                    if let Some(index) = &self.index_tx {
                        let _ = index.send(message.clone());
                    }
                    self.note_code(&message);
                    self.maybe_notify(&message);
                    self.on_incoming(message);
                }
                self.sort_chats();
                self.request_images();
            }
            AppEvent::Online(online) => {
                // Back in touch: the helper may have come or gone meanwhile.
                if online
                    && self.online == Some(false)
                    && let Some(api) = self.api.clone()
                {
                    let tx = self.tx.clone();
                    tokio::spawn(async move {
                        if let Ok(info) = api.server_info().await {
                            let _ = tx.send(AppEvent::Info(info));
                        }
                    });
                }
                self.online = Some(online);
            }
            AppEvent::Sent { chat, temp, result } => {
                let convo = self.convos.entry(chat.clone()).or_default();
                match result {
                    Ok(real) => {
                        if let Some(c) = self.chats.iter_mut().find(|c| c.guid == chat) {
                            c.last_message = Some(real.clone());
                        }
                        convo.confirm(&temp, real);
                    }
                    Err(e) => {
                        if let Some(m) = convo.messages.iter_mut().find(|m| m.guid == temp) {
                            m.pending = false;
                            m.failed = Some(e.clone());
                        }
                        self.set_status(format!("Message not sent: {e}. Ctrl+R retries"));
                    }
                }
                self.convos.entry(chat).or_default().settle();
            }
            AppEvent::Image { guid, result } => {
                // Ignore pictures for a conversation we have since left.
                if self.images.contains_key(&guid) {
                    self.set_image(guid, result);
                    if self.pinned && self.focus == Focus::Browse {
                        self.reveal = true;
                    }
                }
            }
            AppEvent::Status(text) => self.set_status(text),
            AppEvent::Resolved { address, chat } => match chat {
                Some(mut chat) => {
                    chat.touched = chrono::Utc::now().timestamp_millis();
                    let guid = chat.guid.clone();
                    if !self.chats.iter().any(|c| c.guid == guid) {
                        self.chats.push(chat);
                    }
                    self.sort_chats();
                    self.cursor = Some(guid);
                    self.open_cursor();
                    self.status = None;
                }
                None => self.open_draft(&address),
            },
            AppEvent::Started { draft, temp, result } => match result {
                Ok(mut chat) => {
                    // Swap the placeholder conversation for the real one.
                    self.chats.retain(|c| c.guid != draft);
                    self.convos.remove(&draft);
                    chat.touched = chrono::Utc::now().timestamp_millis();
                    let guid = chat.guid.clone();
                    match self.chats.iter_mut().find(|c| c.guid == guid) {
                        Some(existing) => existing.touched = chat.touched,
                        None => self.chats.push(chat),
                    }
                    self.sort_chats();
                    let was_open = self.open.as_deref() == Some(draft.as_str());
                    if self.cursor.as_deref() == Some(draft.as_str()) {
                        self.cursor = Some(guid.clone());
                    }
                    if was_open {
                        // Only the conversation's id has changed: what is
                        // being written in it, and the files waiting to
                        // go, stay where they are.
                        let typed = self.input.take();
                        let files = std::mem::take(&mut self.pending_files);
                        self.open = None;
                        self.open_chat(&guid);
                        self.input.set(typed);
                        self.pending_files = files;
                    } else {
                        // Left for another conversation meanwhile: its
                        // draft is filed under the old id.
                        if let Some(text) = self.drafts.remove(&draft) {
                            self.drafts.insert(guid.clone(), text);
                        }
                        if let Some(files) = self.draft_files.remove(&draft) {
                            self.draft_files.insert(guid.clone(), files);
                        }
                    }
                }
                Err(e) => {
                    if let Some(m) = self
                        .convos
                        .get_mut(&draft)
                        .and_then(|c| c.messages.iter_mut().find(|m| m.guid == temp))
                    {
                        m.pending = false;
                        m.failed = Some(e.clone());
                    }
                    self.set_status(format!("Couldn't start the conversation: {e}"));
                }
            },
            AppEvent::Attach(paths) => self.attach(paths),
            AppEvent::PasteText(text) => {
                if self.focus == Focus::Compose {
                    self.input.insert_str(&text);
                }
            }
            AppEvent::FileDone { guid, result } => {
                self.downloading.remove(&guid);
                match result {
                    Ok(text) => self.set_status(text),
                    Err(e) => self.set_status(format!("Couldn't get the file: {e}")),
                }
            }
            AppEvent::Theme(theme) => {
                if theme != self.theme {
                    self.set_status(format!("Theme: {}", theme.name));
                    self.theme = theme;
                }
            }
        }
    }

    /// Make sure the list has an entry for each chat this message belongs
    /// to, showing it as the latest message if it is the newest we know.
    fn note_chat_activity(&mut self, message: &Message) {
        for chat_ref in &message.chats {
            match self.chats.iter_mut().find(|c| c.guid == chat_ref.guid) {
                Some(chat) => {
                    if chat.participants.is_empty() && !chat_ref.participants.is_empty() {
                        chat.participants = chat_ref.participants.clone();
                    }
                    if chat.last_activity() < message.date_created {
                        chat.last_message = Some(message.clone());
                    }
                }
                None => self.chats.push(Chat::from_message(chat_ref, message)),
            }
        }
    }

    /// Messages do not always carry who is in their chat; ask once per chat.
    fn fetch_missing_details(&mut self) {
        let Some(api) = self.api.clone() else { return };
        let wanted: Vec<String> = self
            .chats
            .iter()
            .filter(|c| c.participants.is_empty())
            .filter(|c| self.detail_requested.insert(c.guid.clone()))
            .map(|c| c.guid.clone())
            .collect();
        if wanted.is_empty() {
            return;
        }
        let tx = self.tx.clone();
        tokio::spawn(async move {
            for guid in wanted {
                if let Ok(chat) = api.chat(&guid).await
                    && tx.send(AppEvent::ChatDetail(chat)).is_err()
                {
                    return;
                }
            }
        });
    }

    fn on_incoming(&mut self, message: Message) {
        if !message.is_from_me {
            for chat in &message.chats {
                self.typing_theirs.remove(&chat.guid);
            }
        }
        let mut known_chat = false;
        for chat_ref in message.chats.clone() {
            let guid = chat_ref.guid;
            let is_open = self.open.as_deref() == Some(guid.as_str());
            let mut is_new = false;
            let mut read_now = false;
            let mut read_elsewhere = false;
            // A view parked in the past must not have today's messages
            // appear straight after it; they load when it gets there.
            if let Some(convo) = self.convos.get_mut(&guid)
                && convo.loaded
                && !convo.detached
            {
                is_new = convo.absorb(message.clone());
                convo.settle();
            }
            if let Some(chat) = self.chats.iter_mut().find(|c| c.guid == guid) {
                known_chat = true;
                let newer = chat
                    .last_message
                    .as_ref()
                    .is_none_or(|last| last.pending || last.date_created <= message.date_created);
                if newer {
                    let fresh = chat
                        .last_message
                        .as_ref()
                        .is_none_or(|last| last.guid != message.guid);
                    chat.last_message = Some(message.clone());
                    if !message.is_from_me && message.item_type == 0 && message.date_read.is_some() {
                        // Read already, on another device: the phone had
                        // the conversation open, or this is the news that
                        // it has since been looked at there.
                        read_elsewhere = true;
                    } else if fresh && !message.is_from_me {
                        if is_open {
                            read_now = true;
                        } else {
                            self.unread.insert(guid.clone());
                            // Newly unread: ask about it again soon.
                            self.read_checks.remove(&guid);
                        }
                    } else if message.is_from_me && message.item_type == 0 {
                        // Answered, here or on another device: whatever
                        // came before has been seen.
                        self.unread.remove(&guid);
                    }
                }
            }
            if read_elsewhere {
                self.read_elsewhere(&guid, message.date_created);
            }
            if read_now {
                // It arrived in the conversation you have open.
                self.mark_seen(&guid);
            }
            let _ = is_new;
        }
        if !known_chat && !message.chats.is_empty() {
            // A conversation that was not in the list yet.
            self.note_chat_activity(&message);
            if !message.is_from_me {
                for chat_ref in &message.chats {
                    self.unread.insert(chat_ref.guid.clone());
                }
            }
            self.fetch_missing_details();
        }
    }

    pub fn set_status(&mut self, text: String) {
        self.status = Some((text, Instant::now()));
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_ref().map(|(text, _)| text.as_str())
    }

    /// Whether the full-window loading screen should be showing: before the
    /// first conversations arrive, or while the demo is showing it off.
    pub fn splash(&self) -> bool {
        let waiting = self.api.is_some()
            && self.chats.is_empty()
            && self.chats_error.is_none()
            && !self.chats_exhausted;
        waiting || self.splash_until.is_some_and(|until| Instant::now() < until)
    }

    /// Whether the open conversation is still fetching its first messages.
    pub fn opening(&self) -> bool {
        self.open
            .as_ref()
            .and_then(|g| self.convos.get(g))
            .is_some_and(|c| !c.loaded && c.error.is_none())
    }

    /// How often to redraw without input: quickly while something is
    /// animating, otherwise only enough to expire status messages.
    pub fn tick_rate(&self) -> Duration {
        let typing = self.open.as_deref().is_some_and(|chat| self.is_typing(chat));
        // A playing GIF sets the pace, but only while someone is looking.
        let gif = self
            .images
            .values()
            .filter_map(ImageState::frame_time)
            .min()
            .filter(|_| self.window_focused && !self.help && !self.stats && !self.filtering);
        if let Some(frame) = gif {
            return frame.clamp(Duration::from_millis(40), Duration::from_millis(250));
        }
        if self.splash() || self.opening() {
            Duration::from_millis(110)
        } else if typing {
            // Fast enough for the "is typing…" dots to move.
            Duration::from_millis(400)
        } else {
            Duration::from_secs(1)
        }
    }

    /// The first time Fizz starts somewhere that cannot draw real pictures,
    /// say so, and say where it would look better. Once only: after that
    /// the stats screen and `fizz check` still tell you.
    pub fn hint_about_terminal(&mut self) {
        use ratatui_image::picker::ProtocolType;
        if self.picker.protocol_type() != ProtocolType::Halfblocks {
            return;
        }
        let marker = crate::config::state_dir().join("terminal-hint-shown");
        if marker.exists() {
            return;
        }
        let inside_tmux = std::env::var_os("TMUX").is_some_and(|v| !v.is_empty());
        self.set_status(if inside_tmux {
            "Pictures show as blocks inside tmux. Fizz works best in kitty or Ghostty, outside tmux".into()
        } else {
            "Pictures show as blocks in this terminal. Fizz works best in kitty or Ghostty".into()
        });
        // Keep it up a little longer than an ordinary status line.
        if let Some((_, at)) = &mut self.status {
            *at = Instant::now() + Duration::from_secs(8);
        }
        if let Some(dir) = marker.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&marker, "");
    }

    // ----- stats ---------------------------------------------------------

    pub fn metrics(&self) -> Option<metrics::Shared> {
        self.api.as_ref().map(|api| api.metrics.clone())
    }

    pub fn server_address(&self) -> Option<&str> {
        self.api.as_ref().map(Api::address)
    }

    /// (conversations loaded, conversations searchable, contact addresses,
    /// messages in the open conversation, pictures held in memory)
    pub fn counts(&self) -> (usize, usize, usize, usize, usize) {
        let open = self
            .open
            .as_ref()
            .and_then(|g| self.convos.get(g))
            .map_or(0, |c| c.messages.len());
        let pictures = self
            .images
            .values()
            .filter(|state| state.current().is_some())
            .count();
        (
            self.chats.len(),
            self.index.len().max(self.chats.len()),
            self.people.len(),
            open,
            pictures,
        )
    }

    /// Take a fresh reading of what the app is using.
    fn sample_usage(&mut self) {
        let now = metrics::usage();
        if let (Some(cpu), Some((before, at))) = (now.cpu_time, self.usage_at) {
            let wall = at.elapsed().as_secs_f32();
            if wall > 0.2 {
                self.cpu_percent = Some(cpu.saturating_sub(before).as_secs_f32() / wall * 100.0);
            }
        }
        if let Some(cpu) = now.cpu_time {
            self.usage_at = Some((cpu, Instant::now()));
        }
        self.usage = now;
    }

    fn toggle_stats(&mut self) {
        self.stats = !self.stats;
        if self.stats {
            self.cpu_percent = None;
            self.usage_at = None;
            self.sample_usage();
            let cache = crate::config::cache_dir();
            self.cache_sizes = Some((
                metrics::folder_size(&cache.join("thumbs")),
                metrics::folder_size(&cache.join("files")),
                crate::store::size_on_disk(),
            ));
        }
    }

    pub fn on_tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        if self.last_saved.elapsed() > Duration::from_secs(90) {
            self.remember();
        }
        // Keep the caches within their limits while running, not only at
        // startup: a session can stay open for days.
        if self.last_trimmed.elapsed() > Duration::from_secs(15 * 60) {
            self.last_trimmed = Instant::now();
            std::thread::spawn(crate::storage::trim);
        }
        self.sync_typing();
        self.check_read_elsewhere();
        if self.stats {
            self.sample_usage();
        }
        if self
            .status
            .as_ref()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(6))
        {
            self.status = None;
        }
    }

    // ----- keyboard and mouse ------------------------------------------

    pub fn on_terminal(&mut self, event: Event) {
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                self.on_key(key);
                self.sync_typing();
            }
            // Files dragged onto the terminal arrive as pasted paths.
            Event::Paste(text) => match files::dropped_paths(&text) {
                Some(paths) => self.attach(paths),
                None if self.focus == Focus::Compose => self.input.insert_str(&text),
                None => {}
            },
            Event::Mouse(mouse) => self.on_mouse(mouse),
            Event::FocusGained => self.window_focused = true,
            Event::FocusLost => self.window_focused = false,
            _ => {}
        }
    }

    fn open_cursor(&mut self) {
        let Some(guid) = self.cursor.clone() else { return };
        // A folder's row opens the folder; the row at its top closes it.
        if guid == BACK {
            return self.leave_folder();
        }
        if let Some(folder) = guid.strip_prefix(FOLDER) {
            return self.enter_folder(folder);
        }
        self.open_chat(&guid);
        self.focus = Focus::Compose;
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.quit = true;
            return;
        }
        // Help: F1 anywhere, `?` wherever it would not be typed as text.
        let typing = self.focus == Focus::Compose
            || self.filtering
            || self.picking.is_some()
            || self.filing.is_some();
        if key.code == KeyCode::F(1) || (key.code == KeyCode::Char('?') && !typing && !self.help) {
            self.help = !self.help;
            self.help_scroll = 0;
            return;
        }
        if ctrl && key.code == KeyCode::Char('y') {
            return self.copy_last_code();
        }
        // Stats: F2 anywhere, `i` wherever it would not be typed as text.
        if key.code == KeyCode::F(2) || (key.code == KeyCode::Char('i') && !typing && !self.help) {
            return self.toggle_stats();
        }
        if self.stats {
            if matches!(key.code, KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter) {
                self.stats = false;
            }
            return;
        }
        if self.help {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('?') | KeyCode::Enter => {
                    self.help = false
                }
                KeyCode::Down | KeyCode::Char('j') => self.help_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => {
                    self.help_scroll = self.help_scroll.saturating_sub(1)
                }
                KeyCode::PageDown | KeyCode::Char(' ') => self.help_scroll += 10,
                KeyCode::PageUp => self.help_scroll = self.help_scroll.saturating_sub(10),
                _ => {}
            }
            return;
        }
        if self.deleting.is_some() {
            match key.code {
                KeyCode::Char('D') => self.delete_chat(),
                _ => self.deleting = None,
            }
            return;
        }
        if self.filing.is_some() {
            return self.on_filing_key(key);
        }
        if self.unsending.is_some() {
            match key.code {
                KeyCode::Enter | KeyCode::Char('y') | KeyCode::Char('u') => self.unsend(),
                _ => {
                    self.unsending = None;
                    self.status = None;
                }
            }
            return;
        }
        if let Some(chosen) = self.reacting {
            let last = TAPBACKS.len() - 1;
            match key.code {
                KeyCode::Left | KeyCode::Char('h') => {
                    self.reacting = Some(chosen.checked_sub(1).unwrap_or(last))
                }
                KeyCode::Right | KeyCode::Char('l') | KeyCode::Tab => {
                    self.reacting = Some(if chosen >= last { 0 } else { chosen + 1 })
                }
                KeyCode::Enter | KeyCode::Char(' ') => self.send_reaction(chosen),
                // 1 to 6 pick and send in one press.
                KeyCode::Char(c @ '1'..='6') => self.send_reaction(c as usize - '1' as usize),
                KeyCode::Esc | KeyCode::Char('q') | KeyCode::Char('r') => self.reacting = None,
                _ => {}
            }
            return;
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        let page = (self.view_rows / 2).max(1) as isize;
        match key.code {
            KeyCode::PageUp => return self.scroll_by(page),
            KeyCode::PageDown => return self.scroll_by(-page),
            // End in the conversation: back to the newest message.
            KeyCode::End if self.focus != Focus::List && self.input.is_empty() => {
                return self.jump_to_latest();
            }
            _ => {}
        }
        if self.picking.is_some() {
            return self.on_pick_key(key);
        }
        if self.filtering {
            return self.on_filter_key(key);
        }
        match self.focus {
            Focus::List => match key.code {
                KeyCode::Char('q') => self.quit = true,
                KeyCode::Char('j') | KeyCode::Down => self.move_cursor(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_cursor(-1),
                KeyCode::Home => self.cursor_to_edge(false),
                KeyCode::End => self.cursor_to_edge(true),
                KeyCode::Char('G') => {
                    self.picking = Some(String::new());
                    self.pick_chosen.clear();
                    self.pick_group = true;
                    self.pick_index = 0;
                }
                // Opens with the last search still there, ready to refine.
                KeyCode::Char('/') => self.filtering = true,
                KeyCode::Char('n') => {
                    self.picking = Some(String::new());
                    self.pick_chosen.clear();
                    self.pick_group = false;
                    self.pick_index = 0;
                }
                // Capital D, so it is never hit by accident.
                KeyCode::Char('D') => self.start_deleting(),
                KeyCode::Char('f') => self.start_filing(),
                KeyCode::Char('h') | KeyCode::Left | KeyCode::Backspace if self.in_folder.is_some() => {
                    self.leave_folder()
                }
                KeyCode::Char('r') => {
                    self.set_status("Refreshing…".into());
                    self.fetch_chats(false);
                }
                KeyCode::Esc => {
                    if self.filter.is_empty() {
                        if self.open.is_some() {
                            self.focus = Focus::Compose;
                        }
                    } else {
                        self.filter.clear();
                    }
                }
                KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right | KeyCode::Tab => {
                    self.open_cursor()
                }
                _ => {}
            },
            Focus::Compose => {
                let alt_or_shift = key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT);

                // While the emoji popup is up it takes the navigation keys.
                let matches = self.emoji_matches();
                if !matches.is_empty() {
                    let count = matches.len();
                    let chosen = self.emoji_index.min(count - 1);
                    match key.code {
                        KeyCode::Up => return self.emoji_index = (chosen + count - 1) % count,
                        KeyCode::Down => return self.emoji_index = (chosen + 1) % count,
                        KeyCode::Tab | KeyCode::Enter if !alt_or_shift => {
                            self.input.complete_emoji(matches[chosen].emoji);
                            self.emoji_index = 0;
                            return;
                        }
                        KeyCode::Esc => return self.emoji_hidden = true,
                        // Closing the name by hand: `:smile:`.
                        KeyCode::Char(':') => {
                            if let Some(found) = self.input.emoji_query().and_then(emoji::exact) {
                                self.input.complete_emoji(found);
                                self.emoji_index = 0;
                                return;
                            }
                        }
                        _ => {}
                    }
                }
                let before = self.input.text().len();

                match key.code {
                    KeyCode::Esc if self.editing.is_some() => self.cancel_editing(),
                    KeyCode::Esc if self.replying.is_some() => {
                        self.replying = None;
                        self.focus = Focus::Browse;
                    }
                    KeyCode::Esc => self.focus = Focus::List,
                    KeyCode::Tab => self.browse(),
                    KeyCode::Enter if alt_or_shift => self.input.insert('\n'),
                    KeyCode::Char('j') if ctrl => self.input.insert('\n'),
                    KeyCode::Enter => self.send(),
                    // With nothing typed the arrows scroll the conversation;
                    // otherwise they move through what is being written.
                    KeyCode::Up if self.input.is_empty() => self.scroll_by(2),
                    KeyCode::Down if self.input.is_empty() => self.scroll_by(-2),
                    KeyCode::Up => self.input.up(self.input_width),
                    KeyCode::Down => self.input.down(self.input_width),
                    KeyCode::Left if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                        self.input.word_left()
                    }
                    KeyCode::Right if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                        self.input.word_right()
                    }
                    KeyCode::Backspace if ctrl || key.modifiers.contains(KeyModifiers::ALT) => {
                        self.input.delete_word()
                    }
                    KeyCode::Backspace if self.input.is_empty() => {
                        if let Some(removed) = self.pending_files.pop() {
                            let name = removed.file_name().and_then(|n| n.to_str()).unwrap_or("file");
                            self.set_status(format!("Removed {name}"));
                        }
                    }
                    KeyCode::Backspace => self.input.backspace(),
                    KeyCode::Delete => self.input.delete(),
                    KeyCode::Left => self.input.left(),
                    KeyCode::Right => self.input.right(),
                    KeyCode::Home => self.input.home(),
                    KeyCode::End => self.input.end(),
                    KeyCode::Char('a') if ctrl => self.input.home(),
                    KeyCode::Char('e') if ctrl => self.input.end(),
                    KeyCode::Char('u') if ctrl => self.input.clear(),
                    KeyCode::Char('w') if ctrl => self.input.delete_word(),
                    KeyCode::Char('v') if ctrl => self.paste_clipboard(),
                    KeyCode::Char('r') if ctrl => self.retry_failed(None),
                    KeyCode::Char(c) if !ctrl => self.input.insert(c),
                    _ => {}
                }
                if self.input.text().len() != before {
                    self.last_keystroke = Instant::now();
                    // The text changed: start the popup afresh.
                    self.emoji_index = 0;
                    self.emoji_hidden = false;
                }
            }
            Focus::Browse => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter if self.selected_failed() => {
                    let guid = self.selected_message().map(|(m, _)| m.guid.clone());
                    self.retry_failed(guid);
                    self.focus = Focus::Compose;
                }
                KeyCode::Char('x') | KeyCode::Delete | KeyCode::Backspace
                    if self.selected_failed() =>
                {
                    self.discard_failed()
                }
                KeyCode::Enter | KeyCode::Char('o') => self.open_selected(),
                KeyCode::Char('s') => self.save_selected(),
                KeyCode::Char('y') | KeyCode::Char('c') => self.copy_selected(),
                KeyCode::Char('C') => self.copy_selected_code(),
                KeyCode::Char('r') => self.start_reacting(),
                KeyCode::Char('R') | KeyCode::Char('a') => self.start_replying(),
                KeyCode::Char('u') => self.start_unsending(),
                KeyCode::Char('e') => self.start_editing(),
                KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => self.focus = Focus::Compose,
                _ => {}
            },
        }
    }

    /// Add the highlighted person to the group being put together.
    fn pick_add(&mut self) {
        let candidates = self.pick_candidates();
        let highlighted = candidates
            .get(self.pick_index.min(candidates.len().saturating_sub(1)))
            .cloned();
        if let Some((name, address)) = highlighted {
            let name = name.strip_prefix("Send to ").unwrap_or(&name).to_string();
            if !self.pick_chosen.iter().any(|(_, a)| *a == address) {
                self.pick_chosen.push((name, address));
            }
            self.picking = Some(String::new());
            self.pick_index = 0;
        }
    }

    fn on_pick_key(&mut self, key: KeyEvent) {
        let count = self.pick_candidates().len();
        match key.code {
            KeyCode::Esc => {
                self.picking = None;
                self.pick_chosen.clear();
                self.pick_group = false;
            }
            KeyCode::Down if count > 0 => self.pick_index = (self.pick_index + 1).min(count - 1),
            KeyCode::Up => self.pick_index = self.pick_index.saturating_sub(1),
            // Tab adds the highlighted person and keeps going: a group.
            KeyCode::Tab => self.pick_add(),
            // Choosing a group: Enter adds whoever was searched for, and on
            // an empty line starts the group with everyone added so far.
            // Choosing a group: Enter acts on the highlighted row, which is
            // either a person to add or "Start the group".
            KeyCode::Enter if self.pick_group => {
                let highlighted = self
                    .pick_candidates()
                    .get(self.pick_index.min(count.saturating_sub(1)))
                    .map(|(_, address)| address.clone());
                match highlighted.as_deref() {
                    Some(START_GROUP) => {
                        let everyone = self.pick_chosen.iter().map(|(_, a)| a.clone()).collect();
                        self.start_group(everyone);
                    }
                    Some(_) => {
                        self.pick_add();
                        if self.pick_chosen.len() == 1 {
                            self.set_status("Added. Add at least one more person".into());
                        } else {
                            self.status = None;
                        }
                    }
                    None => self.set_status("No contact matches that".into()),
                }
            }
            KeyCode::Enter => {
                let typed = self.picking.as_deref().is_some_and(|q| !q.trim().is_empty());
                let highlighted = self
                    .pick_candidates()
                    .get(self.pick_index.min(count.saturating_sub(1)))
                    .map(|(_, address)| address.clone());
                let mut everyone: Vec<String> =
                    self.pick_chosen.iter().map(|(_, a)| a.clone()).collect();
                // With people already added, Enter only takes the highlighted
                // one too if something was typed to find them.
                if let Some(address) = highlighted.filter(|_| typed || everyone.is_empty()) {
                    everyone.push(address);
                }
                match everyone.len() {
                    0 => self.set_status("Type a name, a phone number or an email address".into()),
                    1 => self.start_chat(&everyone[0].clone()),
                    _ => self.start_group(everyone),
                }
            }
            KeyCode::Backspace => {
                let empty = self.picking.as_deref().is_none_or(str::is_empty);
                if empty {
                    self.pick_chosen.pop();
                } else if let Some(query) = &mut self.picking {
                    query.pop();
                }
                self.pick_index = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                if let Some(query) = &mut self.picking {
                    query.push(c);
                }
                self.pick_index = 0;
            }
            _ => {}
        }
    }

    fn on_filter_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let rows = self.search_rows();
        match key.code {
            KeyCode::Esc => self.filtering = false,
            KeyCode::Enter => self.open_search_result(),
            KeyCode::Down if rows > 0 => self.search_index = (self.search_index + 1).min(rows - 1),
            KeyCode::Tab => self.next_search_section(),
            KeyCode::Up => self.search_index = self.search_index.saturating_sub(1),
            KeyCode::PageDown if rows > 0 => {
                self.search_index = (self.search_index + 8).min(rows - 1)
            }
            KeyCode::PageUp => self.search_index = self.search_index.saturating_sub(8),
            KeyCode::Char('b') if ctrl => self.enable_index(),
            KeyCode::Char('u') if ctrl => {
                self.filter.clear();
                self.search_changed();
            }
            KeyCode::Backspace => {
                self.filter.pop();
                self.search_changed();
            }
            KeyCode::Char(c) if !ctrl => {
                self.filter.push(c);
                self.search_changed();
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.stats {
            if matches!(mouse.kind, MouseEventKind::Down(_)) {
                self.stats = false;
            }
            return;
        }
        if self.help {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.help_scroll += 2,
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(2),
                MouseEventKind::Down(_) => self.help = false,
                _ => {}
            }
            return;
        }
        // With something drawn over the panes, the mouse is not theirs:
        // the wheel moves through search results, and a click does nothing
        // rather than land on what is hidden underneath.
        if self.filtering {
            let rows = self.search_rows();
            match mouse.kind {
                MouseEventKind::ScrollDown if rows > 0 => {
                    self.search_index = (self.search_index + 1).min(rows - 1)
                }
                MouseEventKind::ScrollUp => self.search_index = self.search_index.saturating_sub(1),
                _ => {}
            }
            return;
        }
        if self.deleting.is_some()
            || self.unsending.is_some()
            || self.reacting.is_some()
            || self.filing.is_some()
        {
            return;
        }
        let at = Position::new(mouse.column, mouse.row);
        let in_list = self.list_rect.contains(at);
        match mouse.kind {
            MouseEventKind::ScrollUp if in_list => self.move_cursor(-1),
            MouseEventKind::ScrollDown if in_list => self.move_cursor(1),
            MouseEventKind::ScrollUp => self.scroll_by(3),
            MouseEventKind::ScrollDown => self.scroll_by(-3),
            MouseEventKind::Down(MouseButton::Left) if in_list && self.picking.is_some() => {}
            MouseEventKind::Down(MouseButton::Left) if in_list => {
                let row = usize::from(mouse.row.saturating_sub(self.list_rect.y)) / 2;
                let target = self
                    .visible()
                    .get(self.list_top + row)
                    .map(|c| c.guid.clone());
                if let Some(guid) = target {
                    self.cursor = Some(guid);
                    self.open_cursor();
                }
            }
            MouseEventKind::Down(MouseButton::Left) if self.convo_rect.contains(at) => {
                let hit = self
                    .hits
                    .iter()
                    .find(|(rect, _)| rect.contains(at))
                    .map(|(_, guid)| guid.clone());
                if let Some(guid) = hit {
                    let is_text = guid.starts_with("text:");
                    self.selected = Some(guid.clone());
                    self.focus = Focus::Browse;
                    if !is_text {
                        self.act_on(&guid, Then::Open);
                    }
                } else if self.open.is_some() {
                    self.focus = Focus::Compose;
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> App {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(None, tx, ratatui_image::picker::Picker::halfblocks());
        app.load_demo();
        app
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_filter_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    #[test]
    fn search_lists_names_then_conversations_then_messages() {
        let mut app = demo();
        app.search_for("the");
        let [_, names, mentions, rows] = app.search_sections();
        assert!(names > 0 && mentions > names && rows > mentions, "all three sections have rows");
        // Every match is counted once, in the conversation it belongs to.
        assert_eq!(app.found_total, app.found.len() as u64);
        assert_eq!(app.found_in, app.mention_hits().len());
        let most = app.mention_hits()[0].clone();
        assert!(app.mention_hits().iter().all(|m| m.count <= most.count), "most matches first");
        let newest = app.found.iter().find(|h| h.chat == most.chat).unwrap();
        assert_eq!(most.guid, newest.guid, "a conversation opens at its newest match");

        // Tab goes to the start of each section in turn, then back to the top.
        assert_eq!(app.search_index, 0);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.search_index, names);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.search_index, mentions);
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.search_index, 0);

        // Enter on a conversation that mentions the words opens it.
        app.search_index = names;
        press(&mut app, KeyCode::Enter);
        assert!(!app.filtering);
        assert_eq!(app.open.as_deref(), Some(most.chat.as_str()));
    }

    /// One of our own messages in the open conversation, selected.
    fn select_own_message(app: &mut App) -> String {
        let chat = app.open.clone().unwrap();
        let guid = app.convos[&chat]
            .messages
            .iter()
            .rev()
            .find(|m| m.is_from_me && !m.body().is_empty())
            .map(|m| m.guid.clone())
            .expect("the demo's first conversation has a message of ours");
        app.selected = Some(format!("text:{guid}"));
        app.focus = Focus::Browse;
        guid
    }

    #[test]
    fn nothing_half_done_follows_you_to_another_conversation() {
        let mut app = demo();
        let first = app.open.clone().unwrap();
        let second = app.chats.iter().find(|c| c.guid != first).unwrap().guid.clone();
        let count = |app: &App, chat: &str| app.convos[chat].messages.len();

        // An edit: leaving drops it and puts the draft back where it was.
        app.input.set("a draft for the first".into());
        let edited = select_own_message(&mut app);
        let original = app.selected_message().unwrap().0.body();
        app.start_editing();
        assert!(app.editing.is_some());
        app.input.set("changed my mind".into());
        app.open_chat(&second);
        assert!(app.editing.is_none());
        assert!(app.input.is_empty(), "the second conversation has no draft");
        // What is typed and sent here is a new message here, and the
        // message that was being edited is as it was.
        let before = count(&app, &second);
        app.input.set("meant for the second".into());
        app.send();
        assert_eq!(count(&app, &second), before + 1);
        assert_eq!(app.convos[&second].messages.last().unwrap().body(), "meant for the second");
        let untouched = app.convos[&first].messages.iter().find(|m| m.guid == edited).unwrap();
        assert_eq!(untouched.body(), original);
        app.open_chat(&first);
        assert_eq!(app.input.text(), "a draft for the first");

        // A reply, a tapback being chosen, an unsend waiting for its answer.
        select_own_message(&mut app);
        app.start_replying();
        assert!(app.replying.is_some());
        app.open_chat(&second);
        assert!(app.replying.is_none());
        app.open_chat(&first);
        select_own_message(&mut app);
        app.start_reacting();
        assert!(app.reacting.is_some());
        app.open_chat(&second);
        assert!(app.reacting.is_none());
        app.open_chat(&first);
        select_own_message(&mut app);
        app.start_unsending();
        assert!(app.unsending.is_some());
        app.open_chat(&second);
        assert!(app.unsending.is_none());
        // Opening the conversation already open changes none of this.
        app.open_chat(&first);
        select_own_message(&mut app);
        app.start_replying();
        app.open_chat(&first);
        assert!(app.replying.is_some());
    }

    fn sending(guid: &str, text: &str) -> Message {
        Message {
            guid: guid.into(),
            text: Some(text.into()),
            is_from_me: true,
            date_created: 1_790_000_000_000,
            pending: true,
            ..Message::default()
        }
    }

    fn sent(guid: &str, text: &str) -> Message {
        Message {
            pending: false,
            ..sending(guid, text)
        }
    }

    #[test]
    fn the_same_text_sent_twice_is_two_messages() {
        let mut app = demo();
        let chat = app.open.clone().unwrap();
        let waiting = |app: &App| -> Vec<String> {
            let mut guids: Vec<String> = app.convos[&chat]
                .messages
                .iter()
                .filter(|m| m.body() == "ok")
                .map(|m| m.guid.clone())
                .collect();
            guids.sort();
            guids
        };
        let convo = app.convos.get_mut(&chat).unwrap();
        convo.absorb(sending("temp-1", "ok"));
        convo.absorb(sending("temp-2", "ok"));
        assert_eq!(waiting(&app), ["temp-1", "temp-2"]);
        // The first is confirmed; the second is still on its way.
        app.on_event(AppEvent::Sent {
            chat: chat.clone(),
            temp: "temp-1".into(),
            result: Ok(sent("REAL-1", "ok")),
        });
        assert_eq!(waiting(&app), ["REAL-1", "temp-2"]);
        // Hearing of the first again from the live connection changes nothing.
        app.convos.get_mut(&chat).unwrap().absorb(sent("REAL-1", "ok"));
        assert_eq!(waiting(&app), ["REAL-1", "temp-2"]);
        // The second is refused: it is there to say so, and to be retried.
        app.on_event(AppEvent::Sent {
            chat: chat.clone(),
            temp: "temp-2".into(),
            result: Err("send failed".into()),
        });
        let failed = app.convos[&chat].messages.iter().find(|m| m.guid == "temp-2").unwrap();
        assert!(failed.failed.is_some() && !failed.pending);
    }

    #[test]
    fn a_lone_placeholder_gives_way_to_the_real_message() {
        // The live connection can deliver our own message before the send
        // answers. With one message waiting there is no doubt which it is.
        let mut convo = Convo::default();
        convo.absorb(sending("temp-1", "on my way"));
        convo.absorb(sent("REAL-1", "on my way"));
        assert_eq!(convo.messages.iter().map(|m| m.guid.as_str()).collect::<Vec<_>>(), ["REAL-1"]);
        // And the send's own answer, arriving after, adds nothing.
        convo.confirm("temp-1", sent("REAL-1", "on my way"));
        assert_eq!(convo.messages.len(), 1);
    }

    #[test]
    fn a_new_conversation_keeps_what_is_being_written() {
        let mut app = demo();
        app.open_draft("+15550109999");
        let draft = app.open.clone().unwrap();
        assert!(draft.starts_with("new:"));
        // The first message has gone; a second is being typed, with a file.
        app.input.set("and another thing".into());
        app.pending_files = vec![PathBuf::from("/tmp/picture.png")];
        app.on_event(AppEvent::Started {
            draft: draft.clone(),
            temp: "temp-1".into(),
            result: Ok(Chat {
                guid: "iMessage;-;+15550109999".into(),
                ..Chat::default()
            }),
        });
        assert_eq!(app.open.as_deref(), Some("iMessage;-;+15550109999"));
        assert_eq!(app.input.text(), "and another thing");
        assert_eq!(app.pending_files.len(), 1);
        assert!(!app.chats.iter().any(|c| c.guid == draft));
    }

    #[test]
    fn one_match_is_not_listed_twice() {
        let mut app = demo();
        app.search_for("see you");
        assert_eq!(app.found.len(), 1);
        assert!(app.mention_hits().is_empty());
        assert_eq!(app.search_rows(), app.conversation_hits().len() + 1);
    }
    // ----- pages from the database and the server ------------------------

    /// A message of ours from `days` ago, as the server reports it.
    fn old(guid: &str, days: i64) -> Message {
        Message {
            guid: guid.into(),
            text: Some(format!("message {guid}")),
            date_created: chrono::Utc::now().timestamp_millis() - days * 24 * 3600 * 1000,
            is_from_me: true,
            ..Message::default()
        }
    }

    /// A conversation showing these, as read from the database on opening.
    fn showing(app: &mut App, messages: Vec<Message>) {
        let mut convo = Convo {
            loaded: true,
            ..Convo::default()
        };
        for message in messages {
            convo.absorb(message);
        }
        convo.settle();
        app.convos.insert("chat".into(), convo);
    }

    fn latest(app: &mut App, page: Vec<Message>, stored: crate::store::Stored) {
        app.on_event(AppEvent::Messages {
            chat: "chat".into(),
            page: Page::Latest,
            result: Ok(page),
            stored: Some(stored),
        });
    }

    fn shown(app: &App) -> Vec<&str> {
        app.convos["chat"].messages.iter().map(|m| m.guid.as_str()).collect()
    }

    /// A page as full as pages come, ending (oldest) `oldest_days` ago.
    fn full_page(oldest_days: i64) -> Vec<Message> {
        (0..MESSAGE_PAGE as i64)
            .map(|i| {
                let mut m = old(&format!("p{i}"), oldest_days);
                m.date_created += i * 1000;
                m
            })
            .collect()
    }

    #[test]
    fn the_servers_page_joins_what_was_shown_from_the_database() {
        let mut app = demo();
        showing(&mut app, vec![old("a", 9), old("b", 8), old("p0", 3)]);
        let stored = crate::store::Stored {
            vouched: true,
            joined: true,
            ..Default::default()
        };
        latest(&mut app, full_page(3), stored);
        let shown = shown(&app);
        assert_eq!(&shown[..3], ["a", "b", "p0"], "older history stays, in order");
        assert_eq!(shown.len(), 2 + MESSAGE_PAGE);
        assert!(!app.convos["chat"].exhausted);
    }

    #[test]
    fn history_beyond_a_hole_is_not_left_on_screen() {
        let mut app = demo();
        let mut sending = old("temp", 0);
        sending.pending = true;
        showing(&mut app, vec![old("a", 40), old("b", 39), sending]);
        // The newest page reaches back three days; the database says that
        // does not meet what it held.
        let stored = crate::store::Stored {
            vouched: true,
            ..Default::default()
        };
        latest(&mut app, full_page(3), stored);
        let shown = shown(&app);
        assert!(!shown.contains(&"a") && !shown.contains(&"b"));
        assert!(shown.contains(&"temp"), "a message still being sent is not history");
        assert_eq!(shown.len(), MESSAGE_PAGE + 1);
    }

    #[test]
    fn without_the_databases_word_a_shared_message_is_what_joins() {
        let mut app = demo();
        showing(&mut app, vec![old("a", 9), old("p0", 3)]);
        // Kept, but not vouched for (a conversation under two names).
        latest(&mut app, full_page(3), crate::store::Stored::default());
        assert_eq!(shown(&app)[0], "a", "they share a message, so the older one stays");
        showing(&mut app, vec![old("a", 9), old("b", 8)]);
        latest(&mut app, full_page(3), crate::store::Stored::default());
        assert!(!shown(&app).contains(&"a"), "nothing shared: there is a hole");
    }

    #[test]
    fn a_message_deleted_elsewhere_leaves_the_screen() {
        let mut app = demo();
        showing(&mut app, vec![old("a", 5), old("b", 4), old("c", 3)]);
        let stored = crate::store::Stored {
            vouched: true,
            joined: true,
            gone: vec!["b".into()],
            ..Default::default()
        };
        // A short page: this is the whole conversation.
        latest(&mut app, vec![old("c", 3), old("a", 5)], stored);
        assert_eq!(shown(&app), ["a", "c"]);
        assert!(app.convos["chat"].exhausted);
    }

    #[test]
    fn a_server_out_of_reach_leaves_the_conversation_up() {
        let mut app = demo();
        showing(&mut app, vec![old("a", 5), old("b", 4)]);
        app.on_event(AppEvent::Messages {
            chat: "chat".into(),
            page: Page::Latest,
            result: Err(crate::api::UNREACHABLE.into()),
            stored: None,
        });
        let convo = &app.convos["chat"];
        assert!(convo.loaded && !convo.loading);
        assert_eq!(shown(&app), ["a", "b"]);
        assert!(convo.retry_after.is_some(), "and it is not asked for again at once");
    }

    // ----- from the review -------------------------------------------------

    #[test]
    fn a_message_being_sent_is_not_mistaken_for_an_old_one_like_it() {
        let mut convo = Convo::default();
        convo.absorb(sending("temp", "ok"));
        // A page of history arrives with the same word, said last week.
        let mut last_week = sent("old", "ok");
        last_week.date_created -= 7 * 24 * 3600 * 1000;
        convo.absorb(last_week);
        convo.settle();
        let guids: Vec<&str> = convo.messages.iter().map(|m| m.guid.as_str()).collect();
        assert_eq!(guids, ["old", "temp"], "the one being sent is still there to fail or succeed");
        // Its own copy from the server, a moment later, does take its place.
        let mut real = sent("real", "ok");
        real.date_created += 800;
        convo.absorb(real);
        let guids: Vec<&str> = convo.messages.iter().map(|m| m.guid.as_str()).collect();
        assert_eq!(guids, ["old", "real"]);
    }

    #[test]
    fn a_remembered_conversation_is_not_joined_on_the_databases_word_alone() {
        let mut app = demo();
        // On screen from last time's snapshot; while Fizz was closed, more
        // than a page arrived. The database took all of it in, so to the
        // database the newest page joins. To the screen it does not.
        showing(&mut app, vec![old("a", 9), old("b", 8)]);
        let stored = crate::store::Stored {
            vouched: true,
            joined: true,
            ..Default::default()
        };
        latest(&mut app, full_page(3), stored);
        let shown = shown(&app);
        assert!(!shown.contains(&"a") && !shown.contains(&"b"), "they lie beyond a hole");
        assert_eq!(shown.len(), MESSAGE_PAGE);
    }

    #[test]
    fn the_present_is_not_mixed_into_a_view_of_the_past() {
        let mut app = demo();
        showing(&mut app, vec![old("a", 400), old("b", 399)]);
        app.convos.get_mut("chat").unwrap().detached = true;
        // The newest page, asked for before the jump, arrives after it.
        latest(&mut app, full_page(3), crate::store::Stored::default());
        assert_eq!(shown(&app), ["a", "b"]);
        assert!(app.convos["chat"].detached);
    }

    #[test]
    fn answering_elsewhere_clears_the_unread_mark() {
        let mut app = demo();
        let chat = app.chats[1].guid.clone();
        let now = chrono::Utc::now().timestamp_millis();
        let arriving = |guid: &str, mine: bool, at: i64| Message {
            guid: guid.into(),
            text: Some("hello".into()),
            is_from_me: mine,
            date_created: at,
            chats: vec![crate::model::ChatRef {
                guid: chat.clone(),
                ..Default::default()
            }],
            ..Message::default()
        };
        app.on_incoming(arriving("theirs", false, now));
        assert!(app.is_unread(&app.chats.iter().find(|c| c.guid == chat).unwrap().clone()));
        // The reply was typed on the phone.
        app.on_incoming(arriving("mine", true, now + 1000));
        assert!(!app.is_unread(&app.chats.iter().find(|c| c.guid == chat).unwrap().clone()));
    }

    #[test]
    fn a_click_does_not_reach_what_search_is_drawn_over() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = demo();
        app.splash_until = None;
        let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(120, 36)).unwrap();
        terminal.draw(|frame| crate::ui::draw(frame, &mut app)).unwrap();
        assert!(app.list_rect.height > 6, "the list was drawn");
        let was_open = app.open.clone();
        let click = |app: &mut App| {
            app.on_mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: app.list_rect.x + 3,
                // The third conversation in the list (two rows each).
                row: app.list_rect.y + 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.search_for("the");
        click(&mut app);
        assert_eq!(app.open, was_open, "the list is underneath: nothing opens");
        assert!(app.filtering);
        // With search closed the same click does open that conversation.
        app.filtering = false;
        click(&mut app);
        assert_ne!(app.open, was_open);
    }

    // ----- folders ---------------------------------------------------------

    fn list(app: &App) -> Vec<String> {
        app.visible()
            .iter()
            .map(|c| match c.guid.strip_prefix(FOLDER) {
                Some(folder) => format!("[{folder}]"),
                None if c.guid == BACK => "<back>".to_string(),
                None => app.chat_title(c),
            })
            .collect()
    }

    fn keys(app: &mut App, typed: &str) {
        for name in typed.split_whitespace() {
            let code = match name {
                "Enter" => KeyCode::Enter,
                "Esc" => KeyCode::Esc,
                "Down" => KeyCode::Down,
                "Left" => KeyCode::Left,
                one => KeyCode::Char(one.chars().next().unwrap()),
            };
            app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
    }

    #[test]
    fn conversations_go_into_a_folder_and_come_out() {
        let mut app = demo();
        assert_eq!(list(&app), ["Maya Okafor", "Weekend Crew", "Sam Whitlock", "32665", "Dana Kessler"]);

        // The fourth, into a new folder; then the fifth, into the same one.
        keys(&mut app, "j j j f C o d e s Enter");
        assert_eq!(list(&app), ["Maya Okafor", "Weekend Crew", "Sam Whitlock", "[Codes]", "Dana Kessler"]);
        assert_eq!(app.cursor.as_deref(), Some("folder:Codes"), "the folder took its place");
        keys(&mut app, "j f Enter");
        assert_eq!(list(&app), ["Maya Okafor", "Weekend Crew", "Sam Whitlock", "[Codes]"]);

        // Enter on the folder shows what is in it; Left goes back out.
        keys(&mut app, "Enter");
        assert_eq!(app.in_folder.as_deref(), Some("Codes"));
        assert_eq!(list(&app), ["<back>", "32665", "Dana Kessler"]);
        assert!(app.open.as_deref() != Some("folder:Codes"), "a folder is not a conversation");
        keys(&mut app, "Left");
        assert_eq!((app.in_folder.as_deref(), app.cursor.as_deref()), (None, Some("folder:Codes")));

        // Inside again: the first row leads out, and D there deletes nothing.
        keys(&mut app, "Enter k");
        assert_eq!(app.cursor.as_deref(), Some(BACK));
        keys(&mut app, "D");
        assert!(app.deleting.is_none());
        keys(&mut app, "Enter");
        assert_eq!(app.in_folder, None);

        // Taking both out again leaves no folder behind.
        keys(&mut app, "Enter f Enter");
        assert_eq!(list(&app), ["<back>", "Dana Kessler"]);
        keys(&mut app, "f Enter");
        assert_eq!(app.in_folder, None, "the emptied folder is left");
        assert_eq!(list(&app), ["Maya Okafor", "Weekend Crew", "Sam Whitlock", "32665", "Dana Kessler"]);
    }

    #[test]
    fn a_folder_is_unread_while_anything_in_it_is() {
        let mut app = demo();
        // Sam, the third, is the demo's unread conversation.
        keys(&mut app, "j j f W o r k Enter");
        let folder = app.visible().into_iter().find(|c| c.guid == "folder:Work").unwrap().clone();
        assert!(app.is_unread(&folder));
        // Reading it inside the folder clears the folder's mark too.
        keys(&mut app, "Enter Enter");
        assert_eq!(app.open.as_deref(), Some(app.chats[2].guid.as_str()));
        assert!(!app.is_unread(&folder));
    }

    #[test]
    fn the_chooser_offers_what_fits_what_was_typed() {
        let mut app = demo();
        keys(&mut app, "f T r a i l Enter");
        keys(&mut app, "j j j f C o d e s Enter");
        // On a conversation in no folder: both folders, by name.
        keys(&mut app, "k f");
        let choices = app.filing_choices();
        assert_eq!(
            choices,
            [FilingChoice::Folder("Codes".into(), 1), FilingChoice::Folder("Trail".into(), 1)]
        );
        // Typing narrows them, and offers a new one unless the name is taken.
        keys(&mut app, "t r");
        assert_eq!(
            app.filing_choices(),
            [FilingChoice::Folder("Trail".into(), 1), FilingChoice::New("tr".into())]
        );
        keys(&mut app, "a i l");
        assert_eq!(app.filing_choices(), [FilingChoice::Folder("Trail".into(), 1)]);
        keys(&mut app, "Esc");
        assert!(app.filing.is_none());
        assert_eq!(app.filed.len(), 2, "nothing moved");
    }

    #[test]
    fn opening_a_conversation_shows_the_folder_it_lives_in() {
        let mut app = demo();
        let dana = app.chats[4].guid.clone();
        keys(&mut app, "j j j j f O l d Enter");
        assert_eq!(app.in_folder, None);
        // Opened some other way than through the list: from search, say.
        app.open_chat(&dana);
        assert_eq!(app.in_folder.as_deref(), Some("Old"));
        let maya = app.chats[0].guid.clone();
        app.open_chat(&maya);
        assert_eq!(app.in_folder, None);
    }

    // ----- read on another device ------------------------------------------

    fn received(chat: &str, guid: &str, at: i64, read: bool) -> Message {
        Message {
            guid: guid.into(),
            text: Some("hello".into()),
            date_created: at,
            date_read: read.then_some(at + 5_000),
            chats: vec![crate::model::ChatRef {
                guid: chat.into(),
                ..Default::default()
            }],
            ..Message::default()
        }
    }

    fn unread_of(app: &App, chat: &str) -> bool {
        app.is_unread(app.chats.iter().find(|c| c.guid == chat).unwrap())
    }

    #[test]
    fn reading_on_the_phone_clears_the_mark_here() {
        let mut app = demo();
        let chat = app.chats[1].guid.clone();
        let now = chrono::Utc::now().timestamp_millis();
        app.on_incoming(received(&chat, "m1", now, false));
        assert!(unread_of(&app, &chat));
        // The server's news that the same message has now been read.
        app.on_incoming(received(&chat, "m1", now, true));
        assert!(!unread_of(&app, &chat));
        // The next one to arrive is unread again.
        app.on_incoming(received(&chat, "m2", now + 60_000, false));
        assert!(unread_of(&app, &chat));
    }

    #[test]
    fn a_message_read_as_it_arrived_is_never_marked() {
        let mut app = demo();
        let chat = app.chats[1].guid.clone();
        let now = chrono::Utc::now().timestamp_millis();
        // The phone had the conversation open when it came in.
        app.on_incoming(received(&chat, "m1", now, true));
        assert!(!unread_of(&app, &chat));
    }

    #[test]
    fn asking_the_server_settles_an_unread_conversation() {
        let mut app = demo();
        let chat = app.chats[1].guid.clone();
        let now = chrono::Utc::now().timestamp_millis();
        app.on_incoming(received(&chat, "m1", now, false));
        // Still unread there too: nothing changes.
        app.on_event(AppEvent::Newest {
            chat: chat.clone(),
            message: received(&chat, "m1", now, false),
        });
        assert!(unread_of(&app, &chat));
        // Read there since.
        app.on_event(AppEvent::Newest {
            chat: chat.clone(),
            message: received(&chat, "m1", now, true),
        });
        assert!(!unread_of(&app, &chat));
        let last = app.chats.iter().find(|c| c.guid == chat).unwrap().last_message.as_ref().unwrap();
        assert!(last.date_read.is_some(), "the preview's copy knows too");
    }

    #[test]
    fn the_server_saying_read_or_unread_is_followed() {
        let mut app = demo();
        let chat = app.chats[1].guid.clone();
        let now = chrono::Utc::now().timestamp_millis();
        app.on_incoming(received(&chat, "m1", now, false));
        app.on_event(AppEvent::ReadStatus { chat: chat.clone(), read: true });
        assert!(!unread_of(&app, &chat));
        // Marked unread again from the phone.
        app.on_event(AppEvent::ReadStatus { chat: chat.clone(), read: false });
        assert!(unread_of(&app, &chat));
    }

}
