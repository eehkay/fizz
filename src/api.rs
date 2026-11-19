//! Thin client for the BlueBubbles Server REST API.

use anyhow::{Result, anyhow};
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use std::time::Duration;

use crate::config::Config;
use crate::model::{Chat, Contact, Message, ServerInfo};

#[derive(Clone)]
pub struct Api {
    http: reqwest::Client,
    base: String,
    password: String,
    /// Timings of every request, for the stats screen.
    pub metrics: crate::metrics::Shared,
}

/// Percent-encode a path segment (chat guids contain `;` and `+`).
pub fn encode(segment: &str) -> String {
    let mut out = String::with_capacity(segment.len());
    for b in segment.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

impl Api {
    pub fn new(config: &Config) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(90))
            // The server closes connections that sit idle for a few seconds.
            // Reusing one of those is what makes a send fail out of the
            // blue, so let them go before the server does.
            .pool_idle_timeout(Duration::from_secs(3))
            .build()?;
        Ok(Api {
            http,
            base: format!("{}/api/v1", config.url),
            password: config.password.clone(),
            metrics: crate::metrics::Shared::default(),
        })
    }

    /// The server's address, without the password.
    pub fn address(&self) -> &str {
        self.base.trim_end_matches("/api/v1")
    }

    fn auth(&self) -> [(&'static str, &str); 1] {
        [("password", self.password.as_str())]
    }

    async fn unwrap<T: DeserializeOwned>(response: reqwest::Response) -> Result<T> {
        let status = response.status();
        let body: Value = response
            .json()
            .await
            .map_err(|_| anyhow!("the server sent an unreadable reply (HTTP {status})"))?;
        if !status.is_success() {
            let detail = body
                .pointer("/error/message")
                .or_else(|| body.get("message"))
                .and_then(|m| m.as_str())
                .unwrap_or("request failed");
            return Err(anyhow!("{detail} (HTTP {})", status.as_u16()));
        }
        let data = body.get("data").cloned().unwrap_or(Value::Null);
        Ok(serde_json::from_value(data)?)
    }

    /// Note how long a request took and whether it worked.
    fn timed<T>(&self, path: &str, started: std::time::Instant, result: Result<T>) -> Result<T> {
        crate::metrics::record(
            &self.metrics,
            crate::metrics::kind(path),
            started,
            result.is_ok(),
        );
        result
    }

    async fn get<T: DeserializeOwned>(&self, path: &str, query: &[(&str, &str)]) -> Result<T> {
        let started = std::time::Instant::now();
        let result = async {
            let response = self
                .http
                .get(format!("{}{path}", self.base))
                .query(&self.auth())
                .query(query)
                .send()
                .await
                .map_err(friendly)?;
            Self::unwrap(response).await
        }
        .await;
        self.timed(path, started, result)
    }

    async fn post<T: DeserializeOwned>(&self, path: &str, body: Value) -> Result<T> {
        let started = std::time::Instant::now();
        let result = async {
            let response = self
                .http
                .post(format!("{}{path}", self.base))
                .query(&self.auth())
                .json(&body)
                .send()
                .await
                .map_err(friendly)?;
            Self::unwrap(response).await
        }
        .await;
        self.timed(path, started, result)
    }

    pub async fn server_info(&self) -> Result<ServerInfo> {
        self.get("/server/info", &[]).await
    }

    /// The newest messages across every chat, newest first. The chat list is
    /// built from these: the server's own "chats by last message" ordering
    /// has proved unreliable, and this is always in true order.
    pub async fn recent_messages(&self, offset: usize, limit: usize) -> Result<Vec<Message>> {
        self.post(
            "/message/query",
            json!({
                "limit": limit,
                "offset": offset,
                "with": ["chat", "chat.participants", "attachment", "handle"],
                "sort": "DESC",
            }),
        )
        .await
    }

    /// A slice of every chat the server knows, in no useful order. Used to
    /// make all conversations searchable by name, not only recent ones.
    pub async fn all_chats(&self, offset: usize, limit: usize) -> Result<Vec<Chat>> {
        self.post(
            "/chat/query",
            json!({ "limit": limit, "offset": offset, "with": ["participants"] }),
        )
        .await
    }

    /// Add a tapback to a message, or remove one by prefixing its name
    /// with `-`. The server only allows this with the Private API helper.
    pub async fn react(
        &self,
        chat: &str,
        message: &str,
        reaction: &str,
        part: usize,
    ) -> Result<Message> {
        self.post(
            "/message/react",
            json!({
                "chatGuid": chat,
                "selectedMessageGuid": message,
                "reaction": reaction,
                "partIndex": part,
            }),
        )
        .await
    }

    /// Take back a message you sent. Apple allows this for about two
    /// minutes after sending; needs the Private API helper.
    pub async fn unsend(&self, message: &str, part: usize) -> Result<Message> {
        self.post(
            &format!("/message/{}/unsend", encode(message)),
            json!({ "partIndex": part }),
        )
        .await
    }

    /// Change the text of a message you sent. Apple allows this for about
    /// fifteen minutes after sending; needs the Private API helper.
    pub async fn edit(&self, message: &str, text: &str, part: usize) -> Result<Message> {
        self.post(
            &format!("/message/{}/edit", encode(message)),
            json!({
                "editedMessage": text,
                // What people on devices too old to show edits will see.
                "backwardsCompatibilityMessage": format!("Edited to: {text}"),
                "partIndex": part,
            }),
        )
        .await
    }

    /// Mark a chat read on the Mac, which clears it on the user's other
    /// devices. The server only allows this with the Private API helper.
    pub async fn mark_read(&self, chat: &str) -> Result<()> {
        let _: Value = self
            .post(&format!("/chat/{}/read", encode(chat)), json!({}))
            .await?;
        Ok(())
    }

    /// One chat with its participants.
    pub async fn chat(&self, guid: &str) -> Result<Chat> {
        self.get(&format!("/chat/{}", encode(guid)), &[("with", "participants")])
            .await
    }

    pub async fn contacts(&self) -> Result<Vec<Contact>> {
        self.get("/contact", &[]).await
    }

    /// One page of a chat's messages, newest first. Goes through the query
    /// endpoint because only that one can include link-preview data.
    ///
    /// History is walked by time rather than by position: `before` gives
    /// the page ending just before that moment (newest first), `after` the
    /// page starting just after it (oldest first). With neither, the most
    /// recent page. Times are milliseconds.
    pub async fn messages(
        &self,
        chat: &str,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut body = json!({
            "chatGuid": chat,
            "limit": limit,
            "with": ["attachment", "handle", "payloadData"],
            "sort": if after.is_some() { "ASC" } else { "DESC" },
        });
        if let Some(before) = before {
            body["before"] = json!(before);
        }
        if let Some(after) = after {
            body["after"] = json!(after);
        }
        self.post("/message/query", body).await
    }

    /// A page of every chat's messages, for the local search index: text
    /// and which chat each belongs to, and nothing heavier. `before` pages
    /// back through history (newest first); `after` forward (oldest first).
    pub async fn history(
        &self,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        Ok(self.history_page(before, after, limit).await?.0)
    }

    /// As [`Api::history`] going back, with the server's count of all
    /// messages, which gives the download something to measure against.
    pub async fn history_with_total(
        &self,
        before: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Message>, Option<u64>)> {
        self.history_page(before, None, limit).await
    }

    async fn history_page(
        &self,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<(Vec<Message>, Option<u64>)> {
        let mut body = json!({
            "limit": limit,
            "with": ["chat", "handle"],
            "sort": if after.is_some() { "ASC" } else { "DESC" },
        });
        if let Some(before) = before {
            body["before"] = json!(before);
        }
        if let Some(after) = after {
            body["after"] = json!(after);
        }
        let started = std::time::Instant::now();
        let result = async {
            let response = self
                .http
                .post(format!("{}/message/query", self.base))
                .query(&self.auth())
                .json(&body)
                .send()
                .await
                .map_err(friendly)?;
            let status = response.status();
            let reply: Value = response
                .json()
                .await
                .map_err(|_| anyhow!("the server sent an unreadable reply (HTTP {status})"))?;
            if !status.is_success() {
                return Err(anyhow!("the server refused the request (HTTP {})", status.as_u16()));
            }
            let total = reply.pointer("/metadata/total").and_then(|t| t.as_u64());
            let data = reply.get("data").cloned().unwrap_or(Value::Null);
            Ok((serde_json::from_value(data)?, total))
        }
        .await;
        crate::metrics::record(&self.metrics, "search index", started, result.is_ok());
        result
    }

    /// Start a one-to-one conversation by sending its first message.
    pub async fn new_chat(
        &self,
        addresses: &[String],
        temp_guid: &str,
        text: &str,
    ) -> Result<Chat> {
        // Only the Private API can create a group.
        let method = if addresses.len() > 1 {
            "private-api"
        } else {
            send_method()
        };
        self.post(
            "/chat/new",
            json!({
                "addresses": addresses,
                "message": text,
                "tempGuid": temp_guid,
                "method": method,
            }),
        )
        .await
    }

    /// Everything created after `since` (milliseconds), oldest first.
    pub async fn messages_after(&self, since: i64) -> Result<Vec<Message>> {
        self.post(
            "/message/query",
            json!({
                "limit": 200,
                "after": since,
                "with": ["chat", "attachment", "handle"],
                "sort": "ASC",
            }),
        )
        .await
    }

    /// Send text. With `reply_to` (a message id and which part of it), it
    /// goes as an inline reply, which only the Private API can do.
    pub async fn send_text(
        &self,
        chat: &str,
        temp_guid: &str,
        text: &str,
        reply_to: Option<(&str, usize)>,
    ) -> Result<Message> {
        let mut body = json!({
            "chatGuid": chat,
            "tempGuid": temp_guid,
            "message": text,
            "method": send_method(),
        });
        if let Some((message, part)) = reply_to {
            body["method"] = json!("private-api");
            body["selectedMessageGuid"] = json!(message);
            body["partIndex"] = json!(part);
        }
        self.post("/message/text", body).await
    }

    /// Delete a whole conversation from Messages on the Mac. Needs the
    /// Private API helper. There is no undo.
    pub async fn delete_chat(&self, chat: &str) -> Result<()> {
        let path = format!("/chat/{}", encode(chat));
        let started = std::time::Instant::now();
        let result = async {
            let response = self
                .http
                .delete(format!("{}{path}", self.base))
                .query(&self.auth())
                .send()
                .await
                .map_err(friendly)?;
            Self::unwrap::<Value>(response).await.map(|_| ())
        }
        .await;
        crate::metrics::record(&self.metrics, "delete chat", started, result.is_ok());
        result
    }

    /// Show or hide "is typing" for the other side. Private API only.
    pub async fn typing(&self, chat: &str, on: bool) -> Result<()> {
        let path = format!("/chat/{}/typing", encode(chat));
        if on {
            let _: Value = self.post(&path, json!({})).await?;
            return Ok(());
        }
        let started = std::time::Instant::now();
        let result = async {
            let response = self
                .http
                .delete(format!("{}{path}", self.base))
                .query(&self.auth())
                .send()
                .await
                .map_err(friendly)?;
            Self::unwrap::<Value>(response).await.map(|_| ())
        }
        .await;
        self.timed(&path, started, result)
    }

    /// Send a file from disk into a chat.
    pub async fn send_attachment(
        &self,
        chat: &str,
        temp_guid: &str,
        path: &std::path::Path,
    ) -> Result<Message> {
        use reqwest::multipart::{Form, Part};
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("attachment")
            .to_string();
        let part = Part::file(path)
            .await
            .map_err(|e| anyhow!("couldn't read {name}: {e}"))?
            .file_name(name.clone());
        let form = Form::new()
            .text("chatGuid", chat.to_string())
            .text("tempGuid", temp_guid.to_string())
            .text("name", name)
            .text("method", send_method())
            .part("attachment", part);
        let response = self
            .http
            .post(format!("{}/message/attachment", self.base))
            .query(&self.auth())
            .multipart(form)
            // Uploads of large videos can take a long while.
            .timeout(Duration::from_secs(3600))
            .send()
            .await
            .map_err(friendly)?;
        Self::unwrap(response).await
    }

    /// Stream an attachment to a file, calling `progress(done, total)` as it
    /// arrives. Used for anything that may be large.
    pub async fn download_to(
        &self,
        guid: &str,
        path: &std::path::Path,
        mut progress: impl FnMut(u64, Option<u64>),
    ) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut response = self
            .http
            .get(format!("{}/attachment/{}/download", self.base, encode(guid)))
            .query(&self.auth())
            // Videos can take far longer than an ordinary request.
            .timeout(Duration::from_secs(3600))
            .send()
            .await
            .map_err(friendly)?;
        if !response.status().is_success() {
            return Err(download_error(response).await);
        }
        let total = response.content_length();
        let mut file = tokio::fs::File::create(path).await?;
        let mut done = 0u64;
        while let Some(chunk) = response.chunk().await.map_err(friendly)? {
            file.write_all(&chunk).await?;
            done += chunk.len() as u64;
            progress(done, total);
        }
        file.flush().await?;
        Ok(())
    }

    /// The attachment's bytes. The server converts HEIC to JPEG on the way.
    pub async fn attachment(&self, guid: &str) -> Result<Vec<u8>> {
        let response = self
            .http
            .get(format!("{}/attachment/{}/download", self.base, encode(guid)))
            .query(&self.auth())
            .send()
            .await
            .map_err(friendly)?;
        if !response.status().is_success() {
            return Err(download_error(response).await);
        }
        Ok(response.bytes().await?.to_vec())
    }
}

/// How the server should send: plain AppleScript, or the Private API.
fn send_method() -> &'static str {
    crate::config::settings().messages.send_method.as_str()
}

/// Explain a failed attachment download.
async fn download_error(response: reqwest::Response) -> anyhow::Error {
    let status = response.status().as_u16();
    let detail = response
        .json::<Value>()
        .await
        .ok()
        .and_then(|body| {
            body.pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default();
    if detail.contains("does not exist in disk") {
        anyhow!("the Mac doesn't have this file (Messages never downloaded it)")
    } else if detail.is_empty() {
        anyhow!("HTTP {status}")
    } else {
        anyhow!("{detail} (HTTP {status})")
    }
}

/// Turn transport errors into something a person can act on, and make sure
/// the password in the request URL never ends up in an error message.
fn friendly(err: reqwest::Error) -> anyhow::Error {
    let err = err.without_url();
    if err.is_connect() {
        anyhow!(UNREACHABLE)
    } else if err.is_timeout() {
        anyhow!("the server took too long to answer")
    } else {
        // reqwest's own wording ("error sending request") says little; the
        // cause underneath is what tells you what happened.
        let mut cause: &dyn std::error::Error = &err;
        while let Some(deeper) = cause.source() {
            cause = deeper;
        }
        let detail = cause.to_string();
        if detail.is_empty() || detail == err.to_string() {
            anyhow!("the connection to the server failed")
        } else {
            anyhow!("the connection to the server failed ({detail})")
        }
    }
}

/// The wording [`friendly`] uses when nothing reached the server at all,
/// which is the one failure that is always safe to retry.
pub const UNREACHABLE: &str = "can't reach the server";

#[cfg(test)]
mod tests {
    use super::encode;

    #[test]
    fn encodes_chat_guids() {
        assert_eq!(encode("iMessage;-;+15551234567"), "iMessage%3B-%3B%2B15551234567");
    }
}
