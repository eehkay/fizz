//! Drawing. Nothing here changes application state except the few layout
//! facts (pane rectangles, scroll limits) the event handlers need back.

use chrono::{DateTime, Datelike, Local, TimeZone};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};
use ratatui_image::sliced::{SignedPosition, SlicedImage};
use unicode_width::UnicodeWidthStr;

use crate::app::{App, Convo, Focus};
use crate::images::ImageState;
use crate::model::{Chat, Message};
use crate::theme::Theme;

const LIST_WIDTH: u16 = 34;
const NARROW: u16 = 76;
const MAX_INPUT_ROWS: u16 = 6;
const TIMELINE_MAX: u16 = 110;
/// Messages closer together than this share one header.
const RUN_GAP_MS: i64 = 10 * 60_000;

// ----- small text helpers ----------------------------------------------

fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Cut `s` to at most `max` cells, ending with an ellipsis if shortened.
fn truncate(s: &str, max: usize) -> String {
    if width(s) <= max {
        return s.to_string();
    }
    let mut out = String::new();
    let mut used = 0;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if used + w + 1 > max {
            break;
        }
        out.push(c);
        used += w;
    }
    out.push('…');
    out
}

fn pad(n: usize) -> String {
    " ".repeat(n)
}

fn local(ms: i64) -> DateTime<Local> {
    Local
        .timestamp_millis_opt(ms)
        .single()
        .unwrap_or_else(Local::now)
}

fn clock(ms: i64) -> String {
    local(ms).format("%-I:%M %p").to_string()
}

fn day_label(ms: i64) -> String {
    let (then, today) = (local(ms).date_naive(), Local::now().date_naive());
    match (today - then).num_days() {
        0 => "Today".into(),
        1 => "Yesterday".into(),
        2..=6 => then.format("%A").to_string(),
        _ if then.year() == today.year() => then.format("%a, %b %-d").to_string(),
        _ => then.format("%b %-d, %Y").to_string(),
    }
}

/// Compact timestamp for the chat list.
fn list_stamp(ms: i64) -> String {
    let (then, today) = (local(ms).date_naive(), Local::now().date_naive());
    match (today - then).num_days() {
        0 => clock(ms),
        1 => "Yesterday".into(),
        2..=6 => then.format("%a").to_string(),
        _ if then.year() == today.year() => then.format("%b %-d").to_string(),
        _ => then.format("%-m/%-d/%y").to_string(),
    }
}

fn file_size(bytes: u64) -> String {
    match bytes {
        0..1_000 => format!("{bytes} B"),
        1_000..1_000_000 => format!("{:.0} KB", bytes as f64 / 1e3),
        1_000_000..1_000_000_000 => format!("{:.1} MB", bytes as f64 / 1e6),
        _ => format!("{:.1} GB", bytes as f64 / 1e9),
    }
}

fn preview(message: &Message) -> String {
    let text = message.summary();
    if message.is_from_me && !text.is_empty() {
        format!("You: {text}")
    } else {
        text
    }
}

// ----- frame -----------------------------------------------------------

pub fn draw(frame: &mut Frame, app: &mut App) {
    let [main, status] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(frame.area());

    let narrow = main.width < NARROW;
    let (list_area, convo_area) = if narrow {
        match app.focus {
            Focus::List => (main, Rect::default()),
            Focus::Compose | Focus::Browse => (Rect::default(), main),
        }
    } else {
        let [l, c] =
            Layout::horizontal([Constraint::Length(LIST_WIDTH), Constraint::Min(30)]).areas(main);
        (l, c)
    };

    app.list_rect = Rect::default();
    app.convo_rect = convo_area;
    if list_area.width > 0 {
        draw_list(frame, app, list_area);
    }
    if convo_area.width > 0 {
        draw_convo(frame, app, convo_area);
    }
    draw_status(frame, app, status);
    if app.help {
        draw_help(frame, app, main);
    }
}

/// Everything the app can do, grouped by where you are when you do it.
const HELP: &[(&str, &[(&str, &str)])] = &[
    (
        "Chat list",
        &[
            ("↑ ↓  or  j k", "Move between conversations"),
            ("Enter", "Open the conversation and start typing"),
            ("n", "New conversation: type a name, number or email"),
            ("/", "Search the conversations loaded so far"),
            ("g  G", "Jump to the top or bottom"),
            ("r", "Refresh"),
            ("q", "Quit"),
        ],
    ),
    (
        "Writing a message",
        &[
            ("Enter", "Send"),
            ("Shift+Enter  Ctrl+J", "New line"),
            (":name", "Emoji: :thu then ↑ ↓ and Tab, or finish it as :fire:"),
            ("Drop a file", "Attach it (drag onto the window; several is fine)"),
            ("Ctrl+V", "Attach a picture from the clipboard"),
            ("Backspace", "On an empty box: remove the last attached file"),
            ("Ctrl+W  Ctrl+U", "Delete a word, or everything typed"),
            ("PgUp PgDn  ↑ ↓", "Scroll; older messages load at the top"),
            ("Tab", "Select messages, pictures and files"),
            ("Esc", "Back to the chat list"),
        ],
    ),
    (
        "Selecting (after Tab)",
        &[
            ("↑ ↓  or  j k", "Move between messages, pictures and files"),
            ("Enter", "Open the picture, video or file, or the link in a message"),
            ("s", "Save the picture or file to Downloads"),
            ("y", "Copy the message's text"),
            ("Esc", "Back to typing"),
        ],
    ),
    (
        "Mouse",
        &[
            ("Click a chat", "Open it"),
            ("Wheel", "Scroll the list or the conversation"),
            ("Click a picture/file", "Open it"),
            ("Click a message", "Select it, ready to copy"),
        ],
    ),
    (
        "Anywhere",
        &[
            ("?  or  F1", "This help (F1 while typing)"),
            ("Ctrl+C", "Quit"),
        ],
    ),
    (
        "Good to know",
        &[
            ("Pictures", "Shown inline; videos get a still frame (needs ffmpeg)"),
            ("Colours", "Follow the current Omarchy theme, live"),
            ("Notifications", "For messages in chats you are not looking at"),
            ("Settings", "~/.config/fizz/env; see fizz --help"),
        ],
    ),
];

fn help_lines(theme: &Theme, sections: &[(&str, &[(&str, &str)])], w: usize) -> Vec<Line<'static>> {
    const KEY_W: usize = 22;
    let mut lines = Vec::new();
    for (title, rows) in sections {
        if !lines.is_empty() {
            lines.push(Line::default());
        }
        lines.push(Line::styled(
            title.to_string(),
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
        for (key, what) in rows.iter() {
            let key = truncate(key, KEY_W - 1);
            let mut wrapped = textwrap::wrap(what, w.saturating_sub(KEY_W).max(16)).into_iter();
            let first = wrapped.next().unwrap_or_default().into_owned();
            lines.push(Line::from(vec![
                Span::styled(
                    format!("{key}{}", pad(KEY_W - width(&key))),
                    Style::new().fg(theme.bright),
                ),
                Span::styled(first, Style::new().fg(theme.text)),
            ]));
            for more in wrapped {
                lines.push(Line::from(vec![
                    Span::raw(pad(KEY_W)),
                    Span::styled(more.into_owned(), Style::new().fg(theme.text)),
                ]));
            }
        }
    }
    lines
}

fn draw_help(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    // Two columns when there is room, so it all fits without scrolling.
    let two = area.width >= 112;
    let w = area.width.saturating_sub(4).min(if two { 128 } else { 70 });
    let col_w = if two { (w - 7) / 2 } else { w - 4 };
    let columns: Vec<Vec<Line>> = if two {
        vec![
            help_lines(&theme, &HELP[..2], usize::from(col_w)),
            help_lines(&theme, &HELP[2..], usize::from(col_w)),
        ]
    } else {
        vec![help_lines(&theme, HELP, usize::from(col_w))]
    };
    let content = columns.iter().map(Vec::len).max().unwrap_or(0) as u16;
    let h = (content + 4).min(area.height.saturating_sub(2)).max(5);
    let popup = Rect::new(
        area.x + (area.width.saturating_sub(w)) / 2,
        area.y + (area.height.saturating_sub(h)) / 2,
        w.min(area.width),
        h.min(area.height),
    );
    frame.render_widget(Clear, popup);
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(theme.accent))
        .title(Span::styled(
            " Fizz help ",
            Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(popup).inner(ratatui::layout::Margin::new(2, 1));
    frame.render_widget(block, popup);

    let max_scroll = content.saturating_sub(inner.height);
    app.help_scroll = app.help_scroll.min(max_scroll);
    for (i, lines) in columns.into_iter().enumerate() {
        let column = Rect::new(inner.x + i as u16 * (col_w + 3), inner.y, col_w.min(inner.width), inner.height);
        frame.render_widget(Paragraph::new(lines).scroll((app.help_scroll, 0)), column);
    }
    let foot = if max_scroll > 0 {
        " ↑↓ scroll · esc close "
    } else {
        " esc close "
    };
    let foot = Span::styled(foot, Style::new().fg(theme.muted));
    let fw = foot.width() as u16;
    if popup.width > fw + 4 {
        let spot = Rect::new(popup.right() - fw - 2, popup.bottom() - 1, fw, 1);
        frame.render_widget(foot, spot);
    }
}

fn pane<'a>(theme: &Theme, title: String, focused: bool) -> Block<'a> {
    let border = if focused { theme.accent } else { theme.muted };
    let title_style = if focused {
        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text)
    };
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(border))
        .title(Span::styled(format!(" {title} "), title_style))
}

// ----- chat list -------------------------------------------------------

fn draw_list(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let focused = app.focus == Focus::List;
    let title = if let Some(query) = &app.picking {
        format!("To: {query}▏")
    } else if app.filtering || !app.filter.is_empty() {
        format!("/{}{}", app.filter, if app.filtering { "▏" } else { "" })
    } else {
        "Messages".to_string()
    };
    let block = pane(&theme, title, focused);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    app.list_rect = inner;
    if inner.width < 8 || inner.height == 0 {
        return;
    }

    if app.picking.is_some() {
        return draw_picker(frame, app, inner);
    }

    let cursor = app.cursor_index();
    let rows = usize::from(inner.height / 2).max(1);
    if cursor < app.list_top {
        app.list_top = cursor;
    } else if cursor >= app.list_top + rows {
        app.list_top = cursor + 1 - rows;
    }
    app.list_top = app.list_top.min(app.visible().len().saturating_sub(1));

    let visible = app.visible();
    if visible.is_empty() {
        let note = if let Some(error) = &app.chats_error {
            format!("{error}\n\nPress r to try again.")
        } else if app.chats_loading {
            "Loading conversations…".into()
        } else if !app.filter.is_empty() {
            "No matches".into()
        } else {
            "No conversations".into()
        };
        let text = Paragraph::new(note)
            .style(Style::new().fg(theme.muted))
            .wrap(ratatui::widgets::Wrap { trim: true });
        frame.render_widget(text, inner.inner(ratatui::layout::Margin::new(1, 1)));
        return;
    }
    let w = usize::from(inner.width);
    for (slot, chat) in visible.iter().skip(app.list_top).take(rows).enumerate() {
        let index = app.list_top + slot;
        let y = inner.y + (slot as u16) * 2;
        let is_cursor = index == cursor;
        let is_open = app.open.as_deref() == Some(chat.guid.as_str());
        let lines = chat_row(app, chat, w, is_cursor && focused, is_open);
        for (i, line) in lines.into_iter().enumerate() {
            let row = Rect::new(inner.x, y + i as u16, inner.width, 1);
            if row.y < inner.bottom() {
                frame.render_widget(line, row);
            }
        }
    }
}

/// The list of people to start a conversation with.
fn draw_picker(frame: &mut Frame, app: &mut App, inner: Rect) {
    let theme = app.theme.clone();
    let people = app.pick_candidates();
    if people.is_empty() {
        let note = if app.picking.as_deref().is_some_and(|q| q.trim().is_empty()) {
            "Type a name, a phone number or an email address."
        } else {
            "No contact matches. Type a full phone number or email address to message it directly."
        };
        let text = Paragraph::new(note)
            .style(Style::new().fg(theme.muted))
            .wrap(ratatui::widgets::Wrap { trim: true });
        frame.render_widget(text, inner.inner(ratatui::layout::Margin::new(1, 1)));
        return;
    }
    let chosen = app.pick_index.min(people.len() - 1);
    app.pick_index = chosen;
    let rows = usize::from(inner.height / 2).max(1);
    let top = (chosen + 1).saturating_sub(rows);
    let w = usize::from(inner.width);
    for (slot, (name, address)) in people.iter().skip(top).take(rows).enumerate() {
        let is_chosen = top + slot == chosen;
        let bg = if is_chosen { theme.surface } else { Color::Reset };
        let base = Style::new().bg(bg);
        let bar = Span::styled(if is_chosen { "▌" } else { " " }, base.fg(theme.accent));
        let name = truncate(name, w.saturating_sub(4));
        let detail = truncate(&app.pretty_address(address), w.saturating_sub(4));
        let lines = [
            Line::from(vec![
                bar.clone(),
                Span::styled(
                    format!("  {name}{}", pad(w.saturating_sub(3 + width(&name)))),
                    base.fg(theme.bright).add_modifier(Modifier::BOLD),
                ),
            ]),
            Line::from(vec![
                bar,
                Span::styled(
                    format!("  {detail}{}", pad(w.saturating_sub(3 + width(&detail)))),
                    base.fg(theme.muted),
                ),
            ]),
        ];
        for (i, line) in lines.into_iter().enumerate() {
            let row = Rect::new(inner.x, inner.y + (slot * 2 + i) as u16, inner.width, 1);
            if row.y < inner.bottom() {
                frame.render_widget(line, row);
            }
        }
    }
}

fn chat_row(app: &App, chat: &Chat, w: usize, cursor: bool, open: bool) -> [Line<'static>; 2] {
    let theme = &app.theme;
    let unread = app.is_unread(chat);
    let bg = if open { theme.surface } else { Color::Reset };
    let base = Style::new().bg(bg);

    let bar = Span::styled(
        if cursor { "▌" } else { " " },
        base.fg(theme.accent),
    );
    let dot = Span::styled(if unread { "● " } else { "  " }, base.fg(theme.accent));

    let stamp = chat
        .last_message
        .as_ref()
        .map(|m| list_stamp(m.date_created))
        .unwrap_or_default();
    let room = w.saturating_sub(3 + width(&stamp) + 2);
    let title = truncate(&app.chat_title(chat), room);
    let gap = w.saturating_sub(3 + width(&title) + width(&stamp) + 1);
    let title_style = if unread || open {
        base.fg(theme.bright).add_modifier(Modifier::BOLD)
    } else {
        base.fg(theme.text)
    };

    let line1 = Line::from(vec![
        bar.clone(),
        dot,
        Span::styled(title, title_style),
        Span::styled(pad(gap), base),
        Span::styled(format!("{stamp} "), base.fg(theme.muted)),
    ]);

    let text = match &chat.last_message {
        Some(message) => preview(message),
        None if chat.is_draft() => "New conversation".to_string(),
        None => String::new(),
    };
    let text = truncate(&text, w.saturating_sub(4));
    let fill = w.saturating_sub(3 + width(&text));
    let line2 = Line::from(vec![
        bar,
        Span::styled("  ", base),
        Span::styled(text, base.fg(theme.muted)),
        Span::styled(pad(fill), base),
    ]);
    [line1, line2]
}

// ----- conversation ----------------------------------------------------

/// One vertical slice of the timeline.
enum Item {
    Line(Line<'static>),
    Image {
        guid: String,
        x: u16,
        cols: u16,
        rows: u16,
    },
    /// A selectable, clickable row: a file, or one line of a message's
        /// text (tagged `text:<message guid>`).
    File {
        guid: String,
        line: Line<'static>,
        x: u16,
        cols: u16,
    },
}

impl Item {
    fn rows(&self) -> usize {
        match self {
            Item::Line(_) | Item::File { .. } => 1,
            Item::Image { rows, .. } => usize::from(*rows),
        }
    }
}

fn draw_convo(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme.clone();
    let focused = app.focus == Focus::Compose;

    let Some(chat) = app
        .open
        .as_ref()
        .and_then(|g| app.chats.iter().find(|c| &c.guid == g))
        .cloned()
    else {
        let block = pane(&theme, "Fizz".into(), false);
        let inner = block.inner(area);
        frame.render_widget(block, area);
        let note = Paragraph::new("Select a conversation")
            .style(Style::new().fg(theme.muted))
            .centered();
        let mid = Rect::new(inner.x, inner.y + inner.height / 2, inner.width, 1);
        frame.render_widget(note, mid.intersection(inner));
        return;
    };

    // Compose box first: its height decides how much timeline there is.
    let box_width = area.width.saturating_sub(4);
    let (input_lines, (cx, cy)) = app.input.layout(box_width);
    let input_rows = (input_lines.len() as u16).clamp(1, MAX_INPUT_ROWS);
    let chips = attachment_chips(app, usize::from(box_width));
    let chip_rows = chips.len() as u16;
    let [timeline_area, input_area] =
        Layout::vertical([Constraint::Min(3), Constraint::Length(chip_rows + input_rows + 2)])
            .areas(area);

    let title = app.chat_title(&chat);
    let title = if chat.is_group() {
        format!("{title} · {} people", chat.participants.len() + 1)
    } else {
        title
    };
    let browsing = app.focus == Focus::Browse;
    let block = pane(
        &theme,
        truncate(&title, usize::from(area.width).saturating_sub(6)),
        browsing,
    );
    let inner = block.inner(timeline_area);
    frame.render_widget(block, timeline_area);
    // On a wide window, keep the two sides of the conversation within
    // reading distance of each other instead of pinned to the far edges.
    let mut view = inner.inner(ratatui::layout::Margin::new(1, 0));
    if view.width > TIMELINE_MAX {
        view.x += (view.width - TIMELINE_MAX) / 2;
        view.width = TIMELINE_MAX;
    }
    draw_timeline(frame, app, &chat, view);

    // Compose box.
    let name = if chat.is_group() {
        app.chat_title(&chat)
    } else {
        app.short_name_for(&chat.participants.first().map_or("", |p| &p.address))
    };
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { theme.accent } else { theme.muted }));
    let mut inner = block.inner(input_area).inner(ratatui::layout::Margin::new(1, 0));
    frame.render_widget(block, input_area);
    for (i, chip) in chips.into_iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + i as u16, inner.width, 1);
        frame.render_widget(chip, row.intersection(inner));
    }
    inner.y += chip_rows.min(inner.height);
    inner.height = inner.height.saturating_sub(chip_rows);
    if app.input.is_empty() {
        let hint = if chip_rows > 0 {
            "Add a message, or press Enter to send".to_string()
        } else {
            format!("Message {name} · drop a file here to attach it")
        };
        let hint = truncate(&hint, usize::from(inner.width));
        frame.render_widget(
            Paragraph::new(hint).style(Style::new().fg(theme.muted)),
            inner,
        );
    } else {
        let first = usize::from(cy.saturating_sub(input_rows - 1));
        let shown: Vec<Line> = input_lines
            .iter()
            .skip(first)
            .take(usize::from(input_rows))
            .map(|l| Line::styled(l.clone(), Style::new().fg(theme.bright)))
            .collect();
        frame.render_widget(Paragraph::new(shown), inner);
    }
    // Emoji suggestions float just above the message box.
    let matches = app.emoji_matches();
    if !matches.is_empty() {
        let chosen = app.emoji_index.min(matches.len() - 1);
        let widest = matches.iter().map(|m| width(m.label)).max().unwrap_or(0);
        let w = ((widest + 9) as u16).clamp(18, area.width.saturating_sub(2).max(18));
        let h = matches.len() as u16 + 2;
        let popup = Rect::new(
            input_area.x + 1,
            input_area.y.saturating_sub(h),
            w.min(area.width),
            h.min(input_area.y.saturating_sub(area.y)),
        );
        if popup.height > 2 {
            frame.render_widget(Clear, popup);
            let block = Block::bordered()
                .border_type(BorderType::Rounded)
                .border_style(Style::new().fg(theme.accent));
            let rows = block.inner(popup);
            frame.render_widget(block, popup);
            for (i, m) in matches.iter().take(usize::from(rows.height)).enumerate() {
                let style = if i == chosen {
                    Style::new().fg(theme.on_accent).bg(theme.accent)
                } else {
                    Style::new().fg(theme.text)
                };
                // Pad to the full width so the highlight is a solid bar.
                let label = format!(" {}  {}", m.emoji, m.label);
                let fill = usize::from(rows.width).saturating_sub(width(&label));
                let line = Line::styled(format!("{label}{}", pad(fill)), style);
                frame.render_widget(line, Rect::new(rows.x, rows.y + i as u16, rows.width, 1));
            }
        }
    }

    if focused {
        let first = cy.saturating_sub(input_rows - 1);
        frame.set_cursor_position(Position::new(
            inner.x + cx.min(inner.width.saturating_sub(1)),
            inner.y + (cy - first),
        ));
    }
}

/// One row per file waiting to be sent, capped so the box stays small.
fn attachment_chips(app: &App, w: usize) -> Vec<Line<'static>> {
    const SHOWN: usize = 4;
    let theme = &app.theme;
    let files = &app.pending_files;
    let style = Style::new().fg(theme.bright).bg(theme.surface);
    let mut lines: Vec<Line> = files
        .iter()
        .take(if files.len() > SHOWN { SHOWN - 1 } else { SHOWN })
        .map(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
            let size = std::fs::metadata(path).map(|m| file_size(m.len())).unwrap_or_default();
            let name = truncate(name, w.saturating_sub(width(&size) + 8));
            Line::from(Span::styled(format!(" ▤ {name}  {size} "), style))
        })
        .collect();
    if files.len() > SHOWN {
        let more = files.len() - (SHOWN - 1);
        lines.push(Line::styled(format!(" + {more} more"), Style::new().fg(theme.muted)));
    }
    lines
}

fn draw_timeline(frame: &mut Frame, app: &mut App, chat: &Chat, view: Rect) {
    let theme = app.theme.clone();
    app.view_rows = usize::from(view.height);
    app.hits.clear();
    if view.width < 12 || view.height == 0 {
        return;
    }
    let Some(convo) = app.convos.get(&chat.guid) else {
        return;
    };

    let centered = |text: String, color: Color| {
        Paragraph::new(text).style(Style::new().fg(color)).centered()
    };
    let middle = Rect::new(view.x, view.y + view.height / 2, view.width, 1);
    if !convo.loaded {
        let note = match &convo.error {
            Some(e) => centered(format!("Couldn't load messages: {e}"), theme.red),
            None => centered("Loading…".into(), theme.muted),
        };
        frame.render_widget(note, middle);
        return;
    }
    if convo.messages.is_empty() {
        frame.render_widget(centered("No messages yet".into(), theme.muted), middle);
        return;
    }

    let mut items = build_items(app, chat, convo, view.width);
    if convo.loading {
        items.insert(
            0,
            Item::Line(
                Line::styled("Loading earlier messages…", Style::new().fg(theme.muted)).centered(),
            ),
        );
    }

    let total: usize = items.iter().map(Item::rows).sum();
    let rows = usize::from(view.height);
    let max_scroll = total.saturating_sub(rows);
    let mut scroll = convo.scroll.min(max_scroll);
    let selected = app.selected.as_deref().filter(|_| app.focus == Focus::Browse);
    if app.reveal
        && let Some(selected) = selected
    {
        // Bring the highlighted attachment fully into view.
        let mut top = 0;
        for item in &items {
            let h = item.rows();
            let is_it = match item {
                Item::Image { guid, .. } | Item::File { guid, .. } => guid == selected,
                Item::Line(_) => false,
            };
            if is_it {
                let below = total - (top + h);
                if below < scroll {
                    scroll = below;
                } else if below + h > scroll + rows {
                    scroll = (below + h).saturating_sub(rows);
                }
                break;
            }
            top += h;
        }
        scroll = scroll.min(max_scroll);
    }
    let near_top = max_scroll - scroll < 4;
    let wants_older = near_top && !convo.exhausted && !convo.loading;

    // Row of the first item relative to the top of the view; negative when
    // it starts above. Short conversations sit at the bottom.
    let mut y: i64 = if total <= rows {
        (rows - total) as i64
    } else {
        -((max_scroll - scroll) as i64)
    };
    let mut hits = Vec::new();
    for item in &items {
        let h = item.rows() as i64;
        if y + h > 0 && y < rows as i64 {
            match item {
                Item::Line(line) => {
                    let row = Rect::new(view.x, view.y + y as u16, view.width, 1);
                    frame.render_widget(line, row);
                }
                Item::File { guid, line, x, cols } => {
                    let row = Rect::new(view.x, view.y + y as u16, view.width, 1);
                    frame.render_widget(line, row);
                    hits.push((Rect::new(view.x + x, row.y, *cols, 1), guid.clone()));
                    if selected == Some(guid.as_str()) {
                        // A bar in the gutter on the bubble's own side.
                        let (bar_x, glyph) = if *x == 0 {
                            (view.x.saturating_sub(1), "▐")
                        } else {
                            (view.right(), "▌")
                        };
                        frame.render_widget(
                            Span::styled(glyph, Style::new().fg(theme.accent)),
                            Rect::new(bar_x, row.y, 1, 1),
                        );
                    }
                }
                Item::Image { guid, x, cols, .. } => {
                    if let Some(ImageState::Ready(protocol)) = app.images.get(guid) {
                        let at = SignedPosition::from((*x as i16, y as i16));
                        frame.render_widget(SlicedImage::new(protocol, at), view);
                    }
                    // The part of the picture that is on screen.
                    let first = y.max(0) as u16;
                    let last = (y + h).min(rows as i64) as u16;
                    let shown = Rect::new(view.x + x, view.y + first, *cols, last - first);
                    hits.push((shown, guid.clone()));
                    if selected == Some(guid.as_str()) {
                        // An accent bar down the free side of the picture.
                        let bar_x = if *x == 0 {
                            Some(shown.right()).filter(|bx| *bx < view.right())
                        } else {
                            Some(shown.x - 1)
                        };
                        if let Some(bar_x) = bar_x {
                            let glyph = if *x == 0 { "▌" } else { "▐" };
                            for row in shown.y..shown.bottom() {
                                frame.render_widget(
                                    Span::styled(glyph, Style::new().fg(theme.accent)),
                                    Rect::new(bar_x, row, 1, 1),
                                );
                            }
                        }
                    }
                }
            }
        }
        y += h;
    }
    app.hits = hits;
    app.reveal = false;

    if scroll > 0 {
        let label = Span::styled(
            format!(" ↓ {scroll} more "),
            Style::new().fg(theme.on_accent).bg(theme.accent),
        );
        let w = label.width() as u16;
        let spot = Rect::new(
            view.right().saturating_sub(w),
            view.bottom().saturating_sub(1),
            w.min(view.width),
            1,
        );
        frame.render_widget(label, spot);
    }

    if let Some(convo) = app.convos.get_mut(&chat.guid) {
        convo.scroll = scroll;
    }
    app.wants_older = wants_older;
}

/// Turn a conversation into rows for a view `w` cells wide.
fn build_items(app: &App, chat: &Chat, convo: &Convo, w: u16) -> Vec<Item> {
    let theme = &app.theme;
    let w = usize::from(w);
    let bubble_max = (w * 3 / 4).clamp(12, 72);
    let group = chat.is_group();
    let muted = Style::new().fg(theme.muted);
    let mut items: Vec<Item> = Vec::new();
    let blank = || Item::Line(Line::default());

    let mut previous: Option<&Message> = None;
    let last_guid = convo.messages.last().map(|m| m.guid.as_str());

    for message in &convo.messages {
        let mine = message.is_from_me;
        let new_day = previous
            .is_none_or(|p| local(p.date_created).date_naive() != local(message.date_created).date_naive());
        if new_day {
            if previous.is_some() {
                items.push(blank());
            }
            let label = format!("─── {} ───", day_label(message.date_created));
            items.push(Item::Line(Line::styled(label, muted).centered()));
        }

        // Group changes and the like: one quiet centred line.
        if message.item_type != 0 {
            let who = if mine {
                "You".to_string()
            } else {
                message
                    .handle
                    .as_ref()
                    .map_or("Someone".into(), |h| app.short_name_for(&h.address))
            };
            let what = match (&message.group_title, message.item_type) {
                (Some(title), 2) => format!("{who} named the conversation “{title}”"),
                (_, 1) => format!("{who} changed who is in the conversation"),
                (_, 3) => format!("{who} left the conversation"),
                _ => format!("{who} updated the conversation"),
            };
            items.push(blank());
            items.push(Item::Line(
                Line::styled(truncate(&what, w), muted.add_modifier(Modifier::ITALIC)).centered(),
            ));
            previous = Some(message);
            continue;
        }

        let new_run = new_day
            || previous.is_none_or(|p| {
                p.item_type != 0
                    || p.sender_key() != message.sender_key()
                    || message.date_created - p.date_created > RUN_GAP_MS
            });
        if new_run {
            items.push(blank());
            let time = clock(message.date_created);
            if mine {
                let left = w.saturating_sub(width(&time));
                items.push(Item::Line(Line::from(vec![
                    Span::raw(pad(left)),
                    Span::styled(time, muted),
                ])));
            } else {
                let mut spans = Vec::new();
                if group {
                    let name = message
                        .handle
                        .as_ref()
                        .map_or("Unknown".into(), |h| app.name_for(&h.address));
                    spans.push(Span::styled(
                        truncate(&name, w.saturating_sub(width(&time) + 2)),
                        Style::new().fg(theme.accent).add_modifier(Modifier::BOLD),
                    ));
                    spans.push(Span::raw("  "));
                }
                spans.push(Span::styled(time, muted));
                items.push(Item::Line(Line::from(spans)));
            }
        }

        let (bubble_bg, bubble_fg) = if mine {
            (theme.accent, theme.on_accent)
        } else {
            (theme.surface, theme.bright)
        };
        let mut bubble_style = Style::new().fg(bubble_fg).bg(bubble_bg);
        if message.pending {
            bubble_style = bubble_style.add_modifier(Modifier::DIM);
        }
        // A padded, coloured row aligned to the sender's side.
        let bubble_line = |text: &str, inner: usize, style: Style| {
            let fill = inner.saturating_sub(width(text));
            let left = if mine { w.saturating_sub(inner + 2) } else { 0 };
            Line::from(vec![
                Span::raw(pad(left)),
                Span::styled(format!(" {text}{} ", pad(fill)), style),
            ])
        };
        // The same row, made selectable and clickable under `key`.
        let tagged = |key: String, text: &str, inner: usize, style: Style| Item::File {
            guid: key,
            line: bubble_line(text, inner, style),
            x: if mine { w.saturating_sub(inner + 2) as u16 } else { 0 },
            cols: (inner + 2) as u16,
        };

        // Which message this one answers, when it is an inline reply.
        if let Some(origin) = &message.thread_originator_guid {
            let quoted = convo.messages.iter().find(|m| &m.guid == origin).map(|m| {
                let who = if m.is_from_me {
                    "You".to_string()
                } else {
                    m.handle
                        .as_ref()
                        .map_or("They".into(), |h| app.short_name_for(&h.address))
                };
                format!("{who}: {}", m.summary())
            });
            let text = match quoted {
                Some(quoted) => format!("↳ {quoted}"),
                None => "↳ Reply to an earlier message".to_string(),
            };
            let text = truncate(&text, bubble_max);
            let left = if mine { w.saturating_sub(width(&text) + 1) } else { 1 };
            items.push(Item::Line(Line::from(vec![
                Span::raw(pad(left)),
                Span::styled(text, muted.add_modifier(Modifier::ITALIC)),
            ])));
        }

        for attachment in &message.attachments {
            if attachment.is_plugin_payload() {
                continue;
            }
            let state = app.images.get(&attachment.guid);
            let visual = attachment.is_image() || attachment.is_video();
            if let (true, Some(ImageState::Ready(protocol))) = (visual, state) {
                let size = protocol.size();
                let x = if mine {
                    (w as u16).saturating_sub(size.width)
                } else {
                    0
                };
                items.push(Item::Image {
                    guid: attachment.guid.clone(),
                    x,
                    cols: size.width,
                    rows: size.height,
                });
                if attachment.is_image() {
                    continue;
                }
                // A video's still is followed by its name row, below.
            }
            let label = match (attachment.is_image(), state) {
                (true, Some(ImageState::Loading)) | (true, None) => "◌ Loading photo…".to_string(),
                _ => {
                    let icon = match attachment.mime_type.as_deref().unwrap_or("") {
                        m if m.starts_with("image/") => "▨",
                        m if m.starts_with("video/") => "▶",
                        m if m.starts_with("audio/") => "♪",
                        _ => "▤",
                    };
                    let size = attachment.total_bytes.map(file_size).unwrap_or_default();
                    let name = truncate(attachment.name(), bubble_max.saturating_sub(14));
                    format!("{icon} {name}  {size}").trim_end().to_string()
                }
            };
            let label = truncate(&label, bubble_max - 2);
            let chosen = app.focus == Focus::Browse
                && app.selected.as_deref() == Some(attachment.guid.as_str());
            let style = if chosen {
                Style::new().fg(theme.on_accent).bg(theme.accent)
            } else {
                Style::new().fg(theme.text).bg(theme.surface)
            };
            items.push(tagged(attachment.guid.clone(), &label, width(&label), style));
        }

        if message.date_retracted.is_some() {
            let note = if mine { "You unsent a message" } else { "Unsent a message" };
            let line = bubble_line(note, width(note), muted.add_modifier(Modifier::ITALIC));
            items.push(Item::Line(line));
        } else {
            let body: String = message
                .body()
                .chars()
                .map(|c| if c == '\t' { ' ' } else { c })
                .filter(|c| *c == '\n' || !c.is_control())
                .collect();
            if !body.is_empty() {
                let wrap = |text: &str| -> Vec<String> {
                    text.split('\n')
                        .flat_map(|paragraph| {
                            textwrap::wrap(paragraph, bubble_max - 2)
                                .into_iter()
                                .map(|l| l.into_owned())
                                .collect::<Vec<_>>()
                        })
                        .collect()
                };
                // A link with a known page title leads with the title and
                // keeps the address to one line.
                let mut rows: Vec<(String, Style)> = Vec::new();
                match message.link_title() {
                    Some(title) => {
                        let bold = bubble_style.add_modifier(Modifier::BOLD);
                        rows.extend(wrap(&title).into_iter().take(2).map(|l| (l, bold)));
                        rows.push((truncate(&body.replace('\n', " "), bubble_max - 2), bubble_style));
                    }
                    None => rows.extend(wrap(&body).into_iter().map(|l| (l, bubble_style))),
                }
                let inner = rows.iter().map(|(l, _)| width(l)).max().unwrap_or(0);
                let key = format!("text:{}", message.guid);
                for (line, style) in &rows {
                    items.push(tagged(key.clone(), line, inner, *style));
                }
            }
        }

        if let Some(reactions) = convo.reactions.get(&message.guid) {
            let mut tally: Vec<(String, usize)> = Vec::new();
            for (_, glyph) in reactions {
                match tally.iter_mut().find(|(g, _)| g == glyph) {
                    Some((_, n)) => *n += 1,
                    None => tally.push((glyph.clone(), 1)),
                }
            }
            let text = tally
                .iter()
                .map(|(g, n)| if *n > 1 { format!("{g} {n}") } else { g.clone() })
                .collect::<Vec<_>>()
                .join("  ");
            let left = if mine { w.saturating_sub(width(&text) + 1) } else { 1 };
            items.push(Item::Line(Line::from(vec![
                Span::raw(pad(left)),
                Span::styled(text, Style::new().fg(theme.accent)),
            ])));
        }

        // Delivery state, under the newest message only and only for ours.
        if mine && last_guid == Some(message.guid.as_str()) {
            let (text, color) = if let Some(reason) = &message.failed {
                (format!("Not sent: {reason}"), theme.red)
            } else if message.error != 0 {
                ("Not delivered".to_string(), theme.red)
            } else if message.pending {
                ("Sending…".to_string(), theme.muted)
            } else if let Some(read) = message.date_read.filter(|_| !group) {
                (format!("Read {}", clock(read)), theme.muted)
            } else if message.is_delivered || message.date_delivered.is_some() {
                ("Delivered".to_string(), theme.muted)
            } else {
                ("Sent".to_string(), theme.muted)
            };
            let text = truncate(&text, w);
            items.push(Item::Line(Line::from(vec![
                Span::raw(pad(w.saturating_sub(width(&text)))),
                Span::styled(text, Style::new().fg(color)),
            ])));
        } else if let Some(reason) = &message.failed {
            let text = truncate(&format!("Not sent: {reason}"), w);
            items.push(Item::Line(Line::from(vec![
                Span::raw(pad(w.saturating_sub(width(&text)))),
                Span::styled(text, Style::new().fg(theme.red)),
            ])));
        }

        previous = Some(message);
    }
    items
}

// ----- status line -----------------------------------------------------

fn draw_status(frame: &mut Frame, app: &App, area: Rect) {
    let theme = &app.theme;
    let (dot, label) = match app.online {
        Some(true) => (theme.green, "connected"),
        Some(false) => (theme.red, "offline, retrying"),
        None => (theme.yellow, "connecting"),
    };
    let mut left = vec![
        Span::styled(" ● ", Style::new().fg(dot)),
        Span::styled(label, Style::new().fg(theme.muted)),
    ];
    if let Some(status) = app.status() {
        left.push(Span::styled("  ·  ", Style::new().fg(theme.muted)));
        left.push(Span::styled(status.to_string(), Style::new().fg(theme.text)));
    }

    let hints: &[(&str, &str)] = if app.help {
        &[("esc", "close help")]
    } else if app.filtering {
        &[("type", "search"), ("↑↓", "move"), ("⏎", "open"), ("esc", "cancel")]
    } else if app.focus == Focus::List {
        &[("↑↓", "move"), ("⏎", "open"), ("n", "new"), ("/", "search"), ("?", "help"), ("q", "quit")]
    } else if app.picking.is_some() {
        &[("type", "name or number"), ("↑↓", "choose"), ("⏎", "start"), ("esc", "cancel")]
    } else if app.focus == Focus::Browse && app.selected_text().is_some() {
        &[("↑↓", "select"), ("y", "copy"), ("⏎", "open link"), ("?", "help"), ("esc", "back")]
    } else if app.focus == Focus::Browse {
        &[("↑↓", "select"), ("⏎", "open"), ("s", "save"), ("?", "help"), ("esc", "back")]
    } else if !app.emoji_matches().is_empty() {
        &[("↑↓", "choose"), ("tab", "insert"), ("esc", "dismiss")]
    } else if !app.pending_files.is_empty() {
        &[("⏎", "send"), ("⌫", "remove file"), ("^V", "paste picture"), ("esc", "chats")]
    } else {
        &[
            ("⏎", "send"),
            ("⇧⏎", "new line"),
            (":name", "emoji"),
            ("tab", "select"),
            ("F1", "help"),
            ("esc", "chats"),
        ]
    };
    let mut right = Vec::new();
    for (key, what) in hints {
        right.push(Span::styled(*key, Style::new().fg(theme.accent)));
        right.push(Span::styled(format!(" {what}   "), Style::new().fg(theme.muted)));
    }

    let left = Line::from(left);
    let right = Line::from(right);
    let right_width = right.width() as u16;
    // Hints give way when a status message needs the room.
    let show_hints = area.width > right_width + 24
        && (app.status().is_none() || left.width() as u16 + right_width + 2 <= area.width);
    frame.render_widget(left, area);
    if show_hints {
        let spot = Rect::new(area.right() - right_width, area.y, right_width, 1);
        frame.render_widget(right, spot);
    }
}
