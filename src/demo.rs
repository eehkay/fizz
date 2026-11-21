//! Made-up conversations for `fizz --demo`, so the interface can be tried
//! (and screenshotted) without a server or anyone's real messages.

use image::{DynamicImage, Rgb, RgbImage};
use serde_json::json;
use std::collections::HashMap;

use crate::model::{Attachment, Chat, ChatRef, Handle, Message};

const MINUTE: i64 = 60_000;
const HOUR: i64 = 60 * MINUTE;

fn handle(address: &str) -> Handle {
    Handle {
        address: address.into(),
        service: Some("iMessage".into()),
    }
}

struct Builder {
    chat: String,
    now: i64,
    counter: usize,
    messages: Vec<Message>,
}

impl Builder {
    fn new(chat: &str) -> Self {
        Builder {
            chat: chat.into(),
            now: chrono::Utc::now().timestamp_millis(),
            counter: 0,
            messages: Vec::new(),
        }
    }

    fn base(&mut self, ago: i64, from: Option<&str>) -> Message {
        self.counter += 1;
        Message {
            guid: format!("{}-{}", self.chat, self.counter),
            handle: from.map(handle),
            is_from_me: from.is_none(),
            date_created: self.now - ago,
            date_read: Some(self.now - ago + MINUTE),
            is_delivered: true,
            chats: vec![ChatRef {
                guid: self.chat.clone(),
                ..ChatRef::default()
            }],
            ..Message::default()
        }
    }

    fn say(&mut self, ago: i64, from: Option<&str>, text: &str) -> &mut Self {
        let mut m = self.base(ago, from);
        m.text = Some(text.into());
        self.messages.push(m);
        self
    }

    fn photo(&mut self, ago: i64, from: Option<&str>, name: &str, caption: &str) -> &mut Self {
        let mut m = self.base(ago, from);
        m.text = (!caption.is_empty()).then(|| caption.to_string());
        m.attachments = vec![Attachment {
            guid: format!("demo-{name}"),
            mime_type: Some("image/jpeg".into()),
            transfer_name: Some(format!("{name}.jpg")),
            total_bytes: Some(1_800_000),
            ..Attachment::default()
        }];
        self.messages.push(m);
        self
    }

    fn gif(&mut self, ago: i64, from: Option<&str>, name: &str) -> &mut Self {
        let mut m = self.base(ago, from);
        m.attachments = vec![Attachment {
            guid: format!("demo-gif-{name}"),
            mime_type: Some("image/gif".into()),
            transfer_name: Some(format!("{name}.gif")),
            total_bytes: Some(420_000),
            ..Attachment::default()
        }];
        self.messages.push(m);
        self
    }

    fn file(&mut self, ago: i64, from: Option<&str>, name: &str, bytes: u64) -> &mut Self {
        let mut m = self.base(ago, from);
        m.attachments = vec![Attachment {
            guid: format!("demo-file-{name}"),
            mime_type: Some("application/pdf".into()),
            transfer_name: Some(name.into()),
            total_bytes: Some(bytes),
            ..Attachment::default()
        }];
        self.messages.push(m);
        self
    }

    /// An inline reply to the message `back` places before the latest.
    fn reply(&mut self, ago: i64, from: Option<&str>, back: usize, text: &str) -> &mut Self {
        let origin = self.messages[self.messages.len() - 1 - back].guid.clone();
        let mut m = self.base(ago, from);
        m.text = Some(text.into());
        m.thread_originator_guid = Some(origin);
        self.messages.push(m);
        self
    }

    /// A link, with the page title Messages would have fetched for it.
    fn link(&mut self, ago: i64, from: Option<&str>, url: &str, title: &str) -> &mut Self {
        let mut m = self.base(ago, from);
        m.text = Some(url.into());
        m.payload_data = Some(json!([{ "$objects": [
            "$null",
            { "title": { "UID": 2 }, "originalURL": { "UID": 3 } },
            title,
            url,
        ]}]));
        self.messages.push(m);
        self
    }

    /// A tapback on the most recent message.
    fn react(&mut self, from: Option<&str>, kind: &str) -> &mut Self {
        let target = self.messages.last().unwrap().clone();
        let mut m = self.base(self.now - target.date_created - MINUTE / 2, from);
        m.associated_message_guid = Some(format!("p:0/{}", target.guid));
        m.associated_message_type = Some(json!(kind));
        self.messages.push(m);
        self
    }
}

pub fn data() -> (
    Vec<Chat>,
    HashMap<String, Vec<Message>>,
    HashMap<String, String>,
) {
    let people = [
        ("+15550100001", "Maya Okafor"),
        ("+15550100002", "Theo Lindqvist"),
        ("+15550100003", "Priya Raman"),
        ("+15550100004", "Sam Whitlock"),
        ("+15550100005", "Dana Kessler"),
    ];
    let contacts = people
        .iter()
        .map(|(number, name)| (crate::model::normalize_address(number), name.to_string()))
        .collect();
    let [maya, theo, priya, sam, dana] = people.map(|(number, _)| number);

    let mut convos = Vec::new();

    let mut b = Builder::new("demo;-;maya");
    b.say(26 * HOUR, Some(maya), "Are you still up for the trail on Saturday?")
        .say(26 * HOUR - 2 * MINUTE, None, "Definitely. What time were you thinking?")
        .say(26 * HOUR - 5 * MINUTE, Some(maya), "Early. Lot fills up by 8.")
        .react(None, "like")
        .photo(3 * HOUR, Some(maya), "ridge", "Scouted it this morning. Look at this.")
        .react(None, "love")
        .say(3 * HOUR - 4 * MINUTE, None, "That's unreal. I'll bring the good camera.")
        .say(
            3 * HOUR - 6 * MINUTE,
            None,
            "Also, do we need permits for the upper loop or only if we go past the lake? \
             I looked at the site but couldn't tell.",
        )
        .say(40 * MINUTE, Some(maya), "Only past the lake. We're fine.")
        .say(38 * MINUTE, Some(maya), "Bring water though, there's nothing up there")
        .link(
            30 * MINUTE,
            Some(maya),
            "https://example.org/trails/upper-loop",
            "Upper Loop Trail: map, parking and conditions",
        )
        .reply(12 * MINUTE, None, 2, "Will do. See you at 7:15");
    convos.push((vec![maya], None, b));

    let mut b = Builder::new("demo;+;weekend");
    b.say(8 * HOUR, Some(theo), "Who's bringing the grill?")
        .say(8 * HOUR - MINUTE, Some(priya), "Not me, mine is still in storage")
        .say(8 * HOUR - 3 * MINUTE, None, "I've got it. Charcoal too.")
        .react(Some(theo), "emphasize")
        .react(Some(priya), "love")
        .photo(2 * HOUR, Some(priya), "lake", "")
        .gif(118 * MINUTE, Some(theo), "sunrise")
        .say(2 * HOUR - MINUTE, Some(priya), "Found the spot from last year")
        .say(95 * MINUTE, Some(theo), "Perfect. Same picnic table?")
        .say(90 * MINUTE, Some(priya), "Same one");
    convos.push((vec![theo, priya], Some("Weekend Crew"), b));

    let mut b = Builder::new("demo;-;sam");
    b.say(30 * HOUR, None, "Did the contract come through?")
        .file(5 * HOUR, Some(sam), "Lease-Renewal-2026.pdf", 482_113)
        .say(5 * HOUR - MINUTE, Some(sam), "Just sent it. Page 4 is the only change.");
    b.messages.last_mut().unwrap().date_read = None;
    convos.push((vec![sam], None, b));

    // Dana is on a phone that texts, so this one shows the green bubbles.
    let mut b = Builder::new("demo;-;dana");
    b.say(50 * HOUR, Some(dana), "Happy birthday!! 🎉")
        .say(49 * HOUR, None, "Thank you! Dinner soon?")
        .say(49 * HOUR - 10 * MINUTE, Some(dana), "Yes please. Thursday?")
        .say(48 * HOUR, None, "Thursday works");
    for m in &mut b.messages {
        if let Some(h) = &mut m.handle {
            h.service = Some("SMS".into());
        }
    }
    convos.push((vec![dana], None, b));

    let mut b = Builder::new("demo;-;code");
    b.say(
        6 * HOUR,
        Some("32665"),
        "482913 is your Trailhead verification code. It expires in 10 minutes.",
    );
    convos.push((vec!["32665"], None, b));

    let mut chats = Vec::new();
    let mut by_chat = HashMap::new();
    for (participants, name, builder) in convos {
        let last = builder
            .messages
            .iter()
            .filter(|m| m.reaction().is_none())
            .max_by_key(|m| m.date_created)
            .cloned();
        chats.push(Chat {
            guid: builder.chat.clone(),
            chat_identifier: builder.chat.clone(),
            display_name: name.map(str::to_string),
            style: if participants.len() > 1 { 43 } else { 45 },
            participants: participants.into_iter().map(handle).collect(),
            last_message: last,
            touched: 0,
        });
        by_chat.insert(builder.chat, builder.messages);
    }
    chats.sort_by_key(|c| std::cmp::Reverse(c.last_activity()));
    (chats, by_chat, contacts)
}

/// The demo's address book, for trying "new message".
pub fn people() -> Vec<(String, String)> {
    [
        ("Dana Kessler", "+15550100005"),
        ("Maya Okafor", "+15550100001"),
        ("Noor Haddad", "+15550100006"),
        ("Priya Raman", "+15550100003"),
        ("Sam Whitlock", "+15550100004"),
        ("Theo Lindqvist", "+15550100002"),
    ]
    .map(|(name, number)| (name.to_string(), number.to_string()))
    .to_vec()
}

/// A short generated animation, standing in for a GIF: the sun crossing
/// the sky.
pub fn animation(guid: &str, box_px: (u32, u32)) -> Vec<(DynamicImage, std::time::Duration)> {
    const FRAMES: usize = 16;
    (0..FRAMES)
        .map(|i| {
            let t = i as f32 / FRAMES as f32;
            // An arc from left to right, highest in the middle.
            let sun = (0.12 + 0.76 * t, 0.42 - 0.26 * (t * std::f32::consts::PI).sin());
            (scene(guid, box_px, sun), std::time::Duration::from_millis(110))
        })
        .collect()
}

/// A generated landscape so the demo has pictures without shipping any.
pub fn picture(guid: &str, box_px: (u32, u32)) -> DynamicImage {
    scene(guid, box_px, (0.72, 0.28))
}

/// The landscape, with the sun at the given place (as fractions of the
/// picture's width and height).
fn scene(guid: &str, box_px: (u32, u32), sun_at: (f32, f32)) -> DynamicImage {
    let seed = guid.bytes().fold(7u32, |a, b| a.wrapping_mul(31) + u32::from(b));
    let (w, h) = (640u32, 420u32);
    let hue = (seed % 3) as usize;
    let img = RgbImage::from_fn(w, h, |x, y| {
        let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
        // Sky gradient, a sun, and two ridgelines.
        let ridge1 = 0.62 + 0.10 * ((fx * 6.0 + seed as f32).sin()) + 0.04 * (fx * 23.0).sin();
        let ridge2 = 0.76 + 0.07 * ((fx * 4.0 + 1.3 + seed as f32).cos());
        let sun = ((fx - sun_at.0).powi(2) + (fy - sun_at.1).powi(2) * 2.2).sqrt();
        let mut c = [
            0.95 - 0.55 * fy,
            0.55 + 0.15 * fy,
            0.45 + 0.45 * (1.0 - fy),
        ];
        c.rotate_left(hue);
        if sun < 0.09 {
            c = [1.0, 0.93, 0.75];
        }
        if fy > ridge1 {
            c = [0.20, 0.25, 0.38];
        }
        if fy > ridge2 {
            c = [0.09, 0.12, 0.20];
        }
        Rgb(c.map(|v| (v.clamp(0.0, 1.0) * 255.0) as u8))
    });
    crate::images::shrink(DynamicImage::ImageRgb8(img), box_px.0, box_px.1)
}
