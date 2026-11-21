//! Data shapes returned by the BlueBubbles server, trimmed to what the UI uses.

use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Handle {
    pub address: String,
    pub service: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Attachment {
    pub guid: String,
    pub mime_type: Option<String>,
    pub transfer_name: Option<String>,
    pub total_bytes: Option<u64>,
    pub uti: Option<String>,
    pub hide_attachment: bool,
}

/// How a message travels: Apple's own service, or the phone network.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    IMessage,
    Sms,
    Rcs,
}

impl Service {
    pub fn label(self) -> &'static str {
        match self {
            Service::IMessage => "iMessage",
            Service::Sms => "SMS",
            Service::Rcs => "RCS",
        }
    }
}

impl Handle {
    /// The service the server recorded for this address, if it is one we
    /// know.
    pub fn service_kind(&self) -> Option<Service> {
        match self.service.as_deref()?.to_lowercase().as_str() {
            "imessage" => Some(Service::IMessage),
            "sms" => Some(Service::Sms),
            "rcs" => Some(Service::Rcs),
            _ => None,
        }
    }
}

impl Attachment {
    pub fn is_video(&self) -> bool {
        self.mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("video/"))
    }

    pub fn is_gif(&self) -> bool {
        self.mime_type.as_deref() == Some("image/gif")
            || self
                .transfer_name
                .as_deref()
                .is_some_and(|n| n.to_lowercase().ends_with(".gif"))
    }

    pub fn is_image(&self) -> bool {
        self.mime_type
            .as_deref()
            .is_some_and(|m| m.starts_with("image/"))
    }

    /// Link previews and similar plugin payloads are not files a person sent.
    pub fn is_plugin_payload(&self) -> bool {
        self.hide_attachment
            || self
                .transfer_name
                .as_deref()
                .is_some_and(|n| n.ends_with(".pluginPayloadAttachment"))
    }

    pub fn name(&self) -> &str {
        self.transfer_name.as_deref().unwrap_or("attachment")
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChatRef {
    pub guid: String,
    pub chat_identifier: String,
    pub display_name: Option<String>,
    pub style: i64,
    pub participants: Vec<Handle>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Message {
    pub guid: String,
    pub text: Option<String>,
    pub attributed_body: Option<Value>,
    pub handle: Option<Handle>,
    pub is_from_me: bool,
    pub date_created: i64,
    pub date_read: Option<i64>,
    pub date_delivered: Option<i64>,
    pub date_retracted: Option<i64>,
    pub date_edited: Option<i64>,
    pub is_delivered: bool,
    pub error: i64,
    pub attachments: Vec<Attachment>,
    pub associated_message_guid: Option<String>,
    pub associated_message_type: Option<Value>,
    pub associated_message_emoji: Option<String>,
    pub item_type: i64,
    pub group_title: Option<String>,
    pub chats: Vec<ChatRef>,
    /// The message this one is an inline reply to.
    pub thread_originator_guid: Option<String>,
    /// Apple's archived link-preview data, when the message is a link.
    pub payload_data: Option<Value>,

    /// Set locally for a message that is still being sent.
    #[serde(skip)]
    pub pending: bool,
    /// Set locally when sending failed.
    #[serde(skip)]
    pub failed: Option<String>,
    /// The file a message of ours is sending, kept so it can be retried.
    #[serde(skip)]
    pub local_file: Option<std::path::PathBuf>,
    /// For a reply of ours: which part of the original it answers.
    #[serde(skip)]
    pub reply_part: usize,
}

/// The six classic tapbacks: the server's name for each and how it is shown.
pub const TAPBACKS: [(&str, &str); 6] = [
    ("love", "♥"),
    ("like", "👍"),
    ("dislike", "👎"),
    ("laugh", "😂"),
    ("emphasize", "‼"),
    ("question", "?"),
];

/// A tapback, resolved from the server's string or numeric type.
#[derive(Debug, Clone, PartialEq)]
pub enum Reaction {
    Add(String),
    Remove,
}

impl Message {
    /// The message text with attachment placeholders removed.
    pub fn body(&self) -> String {
        let raw = match &self.text {
            Some(t) => t.clone(),
            None => self
                .attributed_body
                .as_ref()
                .and_then(|b| b.get(0))
                .and_then(|b| b.get("string"))
                .and_then(|s| s.as_str())
                .unwrap_or_default()
                .to_string(),
        };
        raw.chars()
            // Attachment placeholders and other non-characters iMessage embeds.
            .filter(|c| !matches!(c, '\u{FFF0}'..='\u{FFFF}' | '\r'))
            .collect::<String>()
            .trim()
            .to_string()
    }

    /// The page title Messages fetched for a link, if this message has one.
    pub fn link_title(&self) -> Option<String> {
        // The payload is an NSKeyedArchiver dump: a flat list of objects
        // that point at each other by index.
        let objects = self
            .payload_data
            .as_ref()?
            .get(0)?
            .get("$objects")?
            .as_array()?;
        let metadata = objects
            .iter()
            .find(|o| o.get("title").is_some() && o.get("originalURL").is_some())?;
        let index = metadata.get("title")?.get("UID")?.as_u64()? as usize;
        let title = objects.get(index)?.as_str()?.trim();
        (!title.is_empty() && title != "$null").then(|| title.to_string())
    }

    /// The first web address in the text, ready to open.
    pub fn first_link(&self) -> Option<String> {
        self.body().split_whitespace().find_map(|word| {
            let word = word.trim_start_matches(['(', '<', '"', '\'']);
            let word = word.trim_end_matches(['.', ',', ')', '>', '!', '?', ';', ':', '"', '\'']);
            if word.starts_with("http://") || word.starts_with("https://") {
                Some(word.to_string())
            } else if word.starts_with("www.") && word.len() > 6 {
                Some(format!("https://{word}"))
            } else {
                None
            }
        })
    }

    /// What to show for this message in one line, without the sender.
    pub fn summary(&self) -> String {
        let body = self.body().replace('\n', " ");
        if !body.is_empty() {
            body
        } else if self.attachments.iter().any(Attachment::is_image) {
            "Photo".into()
        } else if self.item_type != 0 {
            "Conversation updated".into()
        } else {
            // The chat list's copy of a message comes without its attachments,
            // so a message with no text is almost always one.
            "Attachment".into()
        }
    }

    /// The guid of the message this one reacts to, without the part prefix.
    pub fn reaction_target(&self) -> Option<&str> {
        let g = self.associated_message_guid.as_deref()?;
        let g = g.rsplit('/').next().unwrap_or(g);
        Some(g.strip_prefix("bp:").unwrap_or(g))
    }

    /// `Some` when this message is a tapback rather than something to show on
    /// its own line. Unknown associated types are treated as "hide".
    pub fn reaction(&self) -> Option<Reaction> {
        let kind = self.associated_message_type.as_ref()?;
        if kind.is_null() {
            return None;
        }
        let name = match kind {
            Value::String(s) => s.clone(),
            Value::Number(n) => match n.as_i64().unwrap_or(0) {
                2000 => "love".into(),
                2001 => "like".into(),
                2002 => "dislike".into(),
                2003 => "laugh".into(),
                2004 => "emphasize".into(),
                2005 => "question".into(),
                2006 => "emoji".into(),
                3000..=3999 => "-".into(),
                other => other.to_string(),
            },
            _ => return None,
        };
        if name.starts_with('-') {
            return Some(Reaction::Remove);
        }
        let known = TAPBACKS.iter().find(|(kind, _)| *kind == name);
        let glyph = match known {
            Some((_, glyph)) => *glyph,
            None => match &self.associated_message_emoji {
                Some(e) if !e.is_empty() => e.as_str(),
                // Stickers, edits and anything newer: keep out of the timeline.
                _ => return Some(Reaction::Remove),
            },
        };
        Some(Reaction::Add(glyph.to_string()))
    }

    pub fn sender_key(&self) -> String {
        if self.is_from_me {
            "me".into()
        } else {
            self.handle
                .as_ref()
                .map(|h| h.address.clone())
                .unwrap_or_default()
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Chat {
    pub guid: String,
    pub chat_identifier: String,
    pub display_name: Option<String>,
    pub style: i64,
    pub participants: Vec<Handle>,
    pub last_message: Option<Message>,
    /// Set locally when a chat is opened from "new message", so it sorts to
    /// the top even before it has any messages.
    #[serde(skip)]
    pub touched: i64,
}

impl Chat {
    /// A list entry made from a message and the chat it says it belongs to.
    pub fn from_message(chat: &ChatRef, message: &Message) -> Self {
        Chat {
            guid: chat.guid.clone(),
            chat_identifier: chat.chat_identifier.clone(),
            display_name: chat.display_name.clone(),
            style: chat.style,
            participants: chat.participants.clone(),
            last_message: Some(message.clone()),
            touched: 0,
        }
    }

    /// A conversation that does not exist on the server until its first
    /// message is sent.
    pub fn draft(address: &str) -> Self {
        Chat {
            guid: format!("new:{address}"),
            chat_identifier: address.to_string(),
            style: 45,
            participants: vec![Handle {
                address: address.to_string(),
                service: None,
            }],
            touched: chrono::Utc::now().timestamp_millis(),
            ..Chat::default()
        }
    }

    /// As [`Chat::draft`], for a new group. Its id lists the addresses.
    pub fn draft_group(addresses: &[String]) -> Self {
        Chat {
            guid: format!("new:{}", addresses.join(",")),
            chat_identifier: addresses.join(", "),
            style: 43,
            participants: addresses
                .iter()
                .map(|address| Handle {
                    address: address.clone(),
                    service: None,
                })
                .collect(),
            touched: chrono::Utc::now().timestamp_millis(),
            ..Chat::default()
        }
    }

    pub fn is_draft(&self) -> bool {
        self.guid.starts_with("new:")
    }

    pub fn is_group(&self) -> bool {
        self.style == 43 || self.participants.len() > 1
    }

    pub fn last_activity(&self) -> i64 {
        self.last_message
            .as_ref()
            .map_or(0, |m| m.date_created)
            .max(self.touched)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ContactAddress {
    pub address: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Contact {
    pub phone_numbers: Vec<ContactAddress>,
    pub emails: Vec<ContactAddress>,
    pub display_name: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
}

// Unlike the rest of the API, this one answers in snake_case.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ServerInfo {
    pub server_version: Option<String>,
    pub os_version: Option<String>,
    pub private_api: bool,
    pub helper_connected: bool,
}

/// A typed or stored address in the form the server expects to send to,
/// using the country code from the settings for numbers typed without one.
pub fn sendable_address(addr: &str) -> String {
    sendable_address_in(addr, &crate::config::settings().messages.default_country_code)
}

/// As [`sendable_address`], with the default country code given.
pub fn sendable_address_in(addr: &str, country: &str) -> String {
    let addr = addr.trim();
    let country: String = country.chars().filter(char::is_ascii_digit).collect();
    if addr.contains('@') {
        return addr.to_lowercase();
    }
    let digits: String = addr.chars().filter(char::is_ascii_digit).collect();
    if addr.starts_with('+') {
        format!("+{digits}")
    } else if digits.len() < 7 || country.is_empty() {
        // Short codes, or no default to add.
        digits
    } else if country == "1" {
        match digits.len() {
            10 => format!("+1{digits}"),
            11 if digits.starts_with('1') => format!("+{digits}"),
            _ => digits,
        }
    } else if digits.starts_with(&country) && digits.len() > 10 {
        format!("+{digits}")
    } else {
        // National numbers are usually written with a leading 0 to drop.
        format!("+{country}{}", digits.trim_start_matches('0'))
    }
}

/// Reduce a phone number or email to a form that matches across formats.
pub fn normalize_address(addr: &str) -> String {
    if addr.contains('@') {
        return addr.trim().to_lowercase();
    }
    let digits: String = addr.chars().filter(char::is_ascii_digit).collect();
    if digits.len() >= 10 {
        digits[digits.len() - 10..].to_string()
    } else if digits.is_empty() {
        addr.trim().to_lowercase()
    } else {
        digits
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn reads_links_and_titles() {
        let mut m = Message {
            text: Some("look (https://example.org/a?b=1). and www.example.com!".into()),
            ..Message::default()
        };
        assert_eq!(m.first_link().as_deref(), Some("https://example.org/a?b=1"));
        assert_eq!(m.link_title(), None);
        m.payload_data = Some(json!([{ "$objects": [
            "$null",
            { "title": { "UID": 2 }, "originalURL": { "UID": 3 } },
            " A page ",
            "https://example.org",
        ]}]));
        assert_eq!(m.link_title().as_deref(), Some("A page"));
        m.text = Some("no links here".into());
        assert_eq!(m.first_link(), None);
    }

    #[test]
    fn makes_addresses_sendable() {
        let us = |a| sendable_address_in(a, "+1");
        assert_eq!(us("(555) 010-0001"), "+15550100001");
        assert_eq!(us("1 555 010 0001"), "+15550100001");
        assert_eq!(us("+44 20 7946 0000"), "+442079460000");
        assert_eq!(us(" Me@Example.com "), "me@example.com");
        assert_eq!(us("72345"), "72345");
        let uk = |a| sendable_address_in(a, "+44");
        assert_eq!(uk("020 7946 0000"), "+442079460000");
        assert_eq!(uk("+1 555 010 0001"), "+15550100001");
    }
}
