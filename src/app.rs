//! Application state and everything that changes it.

use image::DynamicImage;
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
use crate::model::{
    Attachment, Chat, Contact, Message, Reaction, normalize_address, sendable_address,
};
use crate::theme::Theme;

/// The largest box, in terminal cells, a picture may take in the timeline.


/// How many of the newest messages to scan per step when building the list.
const SCAN_PAGE: usize = 400;
const MESSAGE_PAGE: usize = 50;

pub enum AppEvent {
    /// A slice of the newest messages, from which the chat list is built.
    Recent {
        offset: usize,
        result: Result<Vec<Message>, String>,
    },
    ChatDetail(Chat),
    /// A slice of the server's full chat list, for searching by name.
    Index(Vec<Chat>),
    Contacts(Vec<Contact>),
    Messages {
        chat: String,
        result: Result<Vec<Message>, String>,
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
        result: Result<DynamicImage, String>,
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

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    List,
    Compose,
    /// Stepping through the open conversation's messages, pictures and files.
    Browse,
}

/// One conversation's loaded history.
#[derive(Default)]
pub struct Convo {
    /// Oldest first; tapbacks are kept separately.
    pub messages: Vec<Message>,
    reaction_events: Vec<Message>,
    /// Target message guid -> (who, glyph).
    pub reactions: HashMap<String, Vec<(String, String)>>,
    /// How many server rows have been fetched, for paging.
    fetched: usize,
    pub exhausted: bool,
    pub loading: bool,
    pub loaded: bool,
    pub error: Option<String>,
    /// Rows scrolled up from the newest message.
    pub scroll: usize,
}

impl Convo {
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
        if message.is_from_me && !self.messages.iter().any(|m| m.guid == message.guid) {
            // The real copy of something we just sent: drop our placeholder.
            let body = message.body();
            if let Some(i) = self
                .messages
                .iter()
                .position(|m| m.pending && m.body() == body)
            {
                self.messages.remove(i);
            }
        }
        Self::upsert(&mut self.messages, message)
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
    scanned: usize,
    detail_requested: HashSet<String>,
    pub chats_error: Option<String>,
    contacts: HashMap<String, String>,
    /// Every chat on the server, loaded quietly at startup so search can
    /// reach conversations older than the list has scrolled to.
    index: Vec<Chat>,
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
    notified: HashSet<String>,
    last_sound: Option<Instant>,
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
    pub focus: Focus,
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
    seen: HashSet<String>,

    pub online: Option<bool>,
    status: Option<(String, Instant)>,
    pub quit: bool,

    // Filled in while drawing, used for mouse hits and paging.
    pub list_rect: Rect,
    pub convo_rect: Rect,
    pub view_rows: usize,
    pub wants_older: bool,
    temp_counter: u64,
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
            scanned: 0,
            detail_requested: HashSet::new(),
            chats_error: None,
            contacts: HashMap::new(),
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
            notified: HashSet::new(),
            last_sound: None,
            started_ms: chrono::Utc::now().timestamp_millis(),
            can_make_stills: false,
            cursor: None,
            open: None,
            list_top: 0,
            filter: String::new(),
            filtering: false,
            convos: HashMap::new(),
            images: HashMap::new(),
            focus: Focus::List,
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
            seen: HashSet::new(),
            online: None,
            status: None,
            quit: false,
            list_rect: Rect::default(),
            convo_rect: Rect::default(),
            view_rows: 20,
            wants_older: false,
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
        if let Some(problem) = crate::config::settings_problem() {
            self.set_status(format!("Settings file ignored: {problem}"));
        }
        self.can_make_stills = images::can_make_stills();
        self.fetch_chats(0);
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
        if self.unread.contains(&chat.guid) {
            return true;
        }
        if self.seen.contains(&chat.guid) {
            return false;
        }
        chat.last_message
            .as_ref()
            .is_some_and(|m| !m.is_from_me && m.date_read.is_none() && m.item_type == 0)
    }

    // ----- chat list ---------------------------------------------------

    /// Chats matching the search box, in display order.
    pub fn visible(&self) -> Vec<&Chat> {
        if self.filter.trim().is_empty() {
            return self.chats.iter().collect();
        }
        self.chats
            .iter()
            .filter(|c| self.matches_search(c))
            .chain(&self.search_extra)
            .collect()
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
    }

    fn fetch_chats(&mut self, offset: usize) {
        let Some(api) = self.api.clone() else { return };
        if self.chats_loading {
            return;
        }
        self.chats_loading = true;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let result = api
                .recent_messages(offset, SCAN_PAGE)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Recent { offset, result });
        });
    }

    fn fetch_more_chats(&mut self) {
        if !self.chats_exhausted {
            self.fetch_chats(self.scanned);
        }
    }

    // ----- conversations -----------------------------------------------

    pub fn open_chat(&mut self, guid: &str) {
        if self.open.as_deref() != Some(guid) {
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
        }
        self.unread.remove(guid);
        self.seen.insert(guid.to_string());
        let convo = self.convos.entry(guid.to_string()).or_default();
        convo.scroll = 0;
        if !convo.loaded && !convo.loading {
            self.fetch_messages(guid);
        }
        self.request_images();
    }

    fn fetch_messages(&mut self, guid: &str) {
        let Some(api) = self.api.clone() else { return };
        let convo = self.convos.entry(guid.to_string()).or_default();
        if convo.loading || convo.exhausted {
            return;
        }
        convo.loading = true;
        convo.error = None;
        let (offset, chat, tx) = (convo.fetched, guid.to_string(), self.tx.clone());
        tokio::spawn(async move {
            let result = api
                .messages(&chat, offset, MESSAGE_PAGE)
                .await
                .map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Messages { chat, result });
        });
    }

    /// Called after each draw: fetch older history once the top is in view.
    pub fn after_draw(&mut self) {
        if std::mem::take(&mut self.wants_older)
            && let Some(guid) = self.open.clone()
        {
            self.fetch_messages(&guid);
        }
    }

    fn request_images(&mut self) {
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
        for attachment in wanted {
            let guid = attachment.guid.clone();
            match self.api.clone() {
                Some(api) => {
                    self.images.insert(guid, ImageState::Loading);
                    images::fetch(
                        api,
                        attachment,
                        box_px,
                        self.image_limit.clone(),
                        self.tx.clone(),
                    );
                }
                None => {
                    let picture = crate::demo::picture(&guid, box_px);
                    self.set_image(guid, Ok(picture));
                }
            }
        }
    }

    fn set_image(&mut self, guid: String, result: Result<DynamicImage, String>) {
        let state = result
            .ok()
            .and_then(|img| SlicedProtocol::new(&self.picker, img, None).ok())
            .map_or(ImageState::Failed, ImageState::Ready);
        self.images.insert(guid, state);
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
            None => self.set_status("No link in this message (y copies its text)".into()),
        }
    }

    fn save_selected(&mut self) {
        match self.selected.clone() {
            Some(key) if !key.starts_with("text:") => self.act_on(&key, Then::Save),
            Some(_) => self.set_status("Only pictures and files can be saved (y copies text)".into()),
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
        found
    }

    fn start_chat(&mut self, address: &str) {
        let address = sendable_address(address);
        let wanted = normalize_address(&address);
        self.picking = None;
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

    fn open_draft(&mut self, address: &str) {
        let draft = Chat::draft(address);
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

    /// A desktop notification for a message that just arrived, unless you
    /// are looking at that conversation right now.
    fn maybe_notify(&mut self, message: &Message) {
        let sound = crate::config::settings().notifications.sound.as_str();
        let wants_sound = !matches!(sound.trim(), "" | "off");
        if (!self.notify && !wants_sound)
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
        let body = if crate::config::settings().notifications.preview {
            message.summary()
        } else {
            "New message".to_string()
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
        let Some(chat) = self.open.clone() else { return };
        let text = self.input.text().trim().to_string();
        if let Some(address) = chat.strip_prefix("new:") {
            return self.send_first(&chat, address, text);
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
                Outgoing::Text(text) => placeholder.text = Some(text.clone()),
                Outgoing::File(path) => {
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

        if let Some(api) = self.api.clone() {
            let tx = self.tx.clone();
            // One at a time, so they arrive in the order shown.
            tokio::spawn(async move {
                for (temp, item) in jobs {
                    let result = match &item {
                        Outgoing::Text(text) => api.send_text(&chat, &temp, text).await,
                        Outgoing::File(path) => api.send_attachment(&chat, &temp, path).await,
                    }
                    .map_err(|e| e.to_string());
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
            let (tx, draft, address) = (self.tx.clone(), draft.to_string(), address.to_string());
            tokio::spawn(async move {
                let result = api
                    .new_chat(&address, &temp, &text)
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
            AppEvent::ChatDetail(detail) => {
                if let Some(chat) = self.chats.iter_mut().find(|c| c.guid == detail.guid) {
                    chat.participants = detail.participants;
                    if detail.display_name.as_deref().is_some_and(|n| !n.is_empty()) {
                        chat.display_name = detail.display_name;
                    }
                }
            }
            AppEvent::Recent { offset, result } => {
                self.chats_loading = false;
                match result {
                    Ok(page) => {
                        self.chats_error = None;
                        self.online = Some(true);
                        if page.len() < SCAN_PAGE {
                            self.chats_exhausted = true;
                        }
                        self.scanned = self.scanned.max(offset + page.len());
                        for message in &page {
                            self.note_chat_activity(message);
                        }
                        self.sort_chats();
                        self.fetch_missing_details();
                        if self.cursor.is_none() {
                            self.cursor = self.chats.first().map(|c| c.guid.clone());
                        }
                        if self.open.is_none()
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
            AppEvent::Messages { chat, result } => {
                let convo = self.convos.entry(chat.clone()).or_default();
                convo.loading = false;
                match result {
                    Ok(page) => {
                        convo.loaded = true;
                        convo.fetched += page.len();
                        if page.len() < MESSAGE_PAGE {
                            convo.exhausted = true;
                        }
                        for message in page {
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
                    Err(e) => convo.error = Some(e),
                }
                if self.open.as_deref() == Some(chat.as_str()) {
                    self.request_images();
                }
            }
            AppEvent::Incoming(messages) => {
                for message in messages {
                    self.maybe_notify(&message);
                    self.on_incoming(message);
                }
                self.sort_chats();
                self.request_images();
            }
            AppEvent::Online(online) => self.online = Some(online),
            AppEvent::Sent { chat, temp, result } => {
                let convo = self.convos.entry(chat.clone()).or_default();
                match result {
                    Ok(real) => {
                        convo.messages.retain(|m| m.guid != temp);
                        if let Some(c) = self.chats.iter_mut().find(|c| c.guid == chat) {
                            c.last_message = Some(real.clone());
                        }
                        // One more server row now exists ahead of our paging offset.
                        if convo.absorb(real) {
                            convo.fetched += 1;
                        }
                    }
                    Err(e) => {
                        if let Some(m) = convo.messages.iter_mut().find(|m| m.guid == temp) {
                            m.pending = false;
                            m.failed = Some(e.clone());
                        }
                        self.set_status(format!("Message not sent: {e}"));
                    }
                }
                self.convos.entry(chat).or_default().settle();
            }
            AppEvent::Image { guid, result } => {
                // Ignore pictures for a conversation we have since left.
                if self.images.contains_key(&guid) {
                    self.set_image(guid, result);
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
                        self.open = None;
                        self.open_chat(&guid);
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
        let mut known_chat = false;
        for chat_ref in message.chats.clone() {
            let guid = chat_ref.guid;
            let is_open = self.open.as_deref() == Some(guid.as_str());
            let mut is_new = false;
            if let Some(convo) = self.convos.get_mut(&guid)
                && convo.loaded
            {
                is_new = convo.absorb(message.clone());
                if is_new {
                    convo.fetched += 1;
                }
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
                    if fresh && !message.is_from_me && !is_open {
                        self.unread.insert(guid.clone());
                        self.seen.remove(&guid);
                    }
                }
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
        if self.splash() || self.opening() {
            Duration::from_millis(110)
        } else {
            Duration::from_secs(1)
        }
    }

    pub fn on_tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
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
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key),
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
        // A search result from beyond the loaded list.
        if let Some(found) = self.search_extra.iter().find(|c| c.guid == guid).cloned() {
            self.search_extra.clear();
            if let Some(address) = found.guid.strip_prefix("new:") {
                // A contact: opens their conversation if one exists.
                return self.start_chat(address);
            }
            if !self.chats.iter().any(|c| c.guid == guid) {
                let mut chat = found;
                chat.touched = chrono::Utc::now().timestamp_millis();
                self.chats.push(chat);
                self.sort_chats();
            }
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
        let typing = self.focus == Focus::Compose || self.filtering || self.picking.is_some();
        if key.code == KeyCode::F(1) || (key.code == KeyCode::Char('?') && !typing && !self.help) {
            self.help = !self.help;
            self.help_scroll = 0;
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
        let page = (self.view_rows / 2).max(1) as isize;
        match key.code {
            KeyCode::PageUp => return self.scroll_by(page),
            KeyCode::PageDown => return self.scroll_by(-page),
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
                KeyCode::Char('g') | KeyCode::Home => self.cursor_to_edge(false),
                KeyCode::Char('G') | KeyCode::End => self.cursor_to_edge(true),
                KeyCode::Char('/') => {
                    self.filtering = true;
                    self.filter.clear();
                }
                KeyCode::Char('n') => {
                    self.picking = Some(String::new());
                    self.pick_index = 0;
                }
                KeyCode::Char('r') => {
                    self.set_status("Refreshing…".into());
                    self.fetch_chats(0);
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
                    KeyCode::Esc => self.focus = Focus::List,
                    KeyCode::Tab => self.browse(),
                    KeyCode::Enter if alt_or_shift => self.input.insert('\n'),
                    KeyCode::Char('j') if ctrl => self.input.insert('\n'),
                    KeyCode::Enter => self.send(),
                    KeyCode::Up => self.scroll_by(2),
                    KeyCode::Down => self.scroll_by(-2),
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
                    KeyCode::Char(c) if !ctrl => self.input.insert(c),
                    _ => {}
                }
                if self.input.text().len() != before {
                    // The text changed: start the popup afresh.
                    self.emoji_index = 0;
                    self.emoji_hidden = false;
                }
            }
            Focus::Browse => match key.code {
                KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
                KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
                KeyCode::Enter | KeyCode::Char('o') => self.open_selected(),
                KeyCode::Char('s') => self.save_selected(),
                KeyCode::Char('y') | KeyCode::Char('c') => self.copy_selected(),
                KeyCode::Esc | KeyCode::Tab | KeyCode::Char('q') => self.focus = Focus::Compose,
                _ => {}
            },
        }
    }

    fn on_pick_key(&mut self, key: KeyEvent) {
        let count = self.pick_candidates().len();
        match key.code {
            KeyCode::Esc => self.picking = None,
            KeyCode::Down if count > 0 => self.pick_index = (self.pick_index + 1).min(count - 1),
            KeyCode::Up => self.pick_index = self.pick_index.saturating_sub(1),
            KeyCode::Enter => {
                let chosen = self
                    .pick_candidates()
                    .get(self.pick_index.min(count.saturating_sub(1)))
                    .map(|(_, address)| address.clone());
                match chosen {
                    Some(address) => self.start_chat(&address),
                    None => self.set_status("Type a name, a phone number or an email address".into()),
                }
            }
            KeyCode::Backspace => {
                if let Some(query) = &mut self.picking {
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
        match key.code {
            KeyCode::Esc => {
                self.filtering = false;
                self.filter.clear();
                self.refresh_search();
            }
            KeyCode::Enter => {
                self.filtering = false;
                // Opening may move a result into the list; clear after.
                self.open_cursor();
                self.filter.clear();
                self.refresh_search();
            }
            KeyCode::Down => self.move_cursor(1),
            KeyCode::Up => self.move_cursor(-1),
            KeyCode::Backspace => {
                self.filter.pop();
                self.refresh_search();
                self.cursor_to_edge(false);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.filter.push(c);
                self.refresh_search();
                self.cursor_to_edge(false);
            }
            _ => {}
        }
    }

    fn on_mouse(&mut self, mouse: MouseEvent) {
        if self.help {
            match mouse.kind {
                MouseEventKind::ScrollDown => self.help_scroll += 2,
                MouseEventKind::ScrollUp => self.help_scroll = self.help_scroll.saturating_sub(2),
                MouseEventKind::Down(_) => self.help = false,
                _ => {}
            }
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
