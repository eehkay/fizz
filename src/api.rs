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
    /// Whether the server's Private API helper was connected when last
    /// asked. Shared by every copy of this, and set by whoever asks.
    helper: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Which way to send, from the `send_method` setting and whether the
/// helper is there. `auto` sends text through the helper when it is
/// connected; files stay with AppleScript unless the helper is asked for
/// by name, as that is the route known to work everywhere.
fn method_for(setting: &str, helper: bool, file: bool) -> &'static str {
    match setting {
        "private-api" => "private-api",
        "auto" if helper && !file => "private-api",
        _ => "apple-script",
    }
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
            helper: Default::default(),
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
        let info: ServerInfo = self.get("/server/info", &[]).await?;
        self.set_helper(info.private_api && info.helper_connected);
        Ok(info)
    }

    /// Note whether the Private API helper is connected.
    pub fn set_helper(&self, connected: bool) {
        self.helper.store(connected, std::sync::atomic::Ordering::Relaxed);
    }

    /// Whether the setting leaves the way of sending to Fizz.
    fn chose_for_itself(&self) -> bool {
        crate::config::settings().messages.send_method == "auto"
    }

    /// The way to send a message (or, with `file`, an attachment) now.
    fn method(&self, file: bool) -> &'static str {
        method_for(
            &crate::config::settings().messages.send_method,
            self.helper.load(std::sync::atomic::Ordering::Relaxed),
            file,
        )
    }

    /// The newest messages across every chat, newest first. The chat list is
    /// built from these: the server's own "chats by last message" ordering
    /// has proved unreliable, and this is always in true order.
    ///
    /// Asking for everything at once makes the server work for over a
    /// second however little is wanted; giving it a date to start from
    /// (`after`) or to work back from (`before`) is several times quicker.
    pub async fn recent_messages(
        &self,
        before: Option<i64>,
        after: Option<i64>,
        limit: usize,
    ) -> Result<Vec<Message>> {
        let mut body = json!({
            "limit": limit,
            "with": ["chat", "chat.participants", "attachment", "handle"],
            "sort": "DESC",
        });
        if let Some(before) = before {
            body["before"] = json!(before);
        }
        if let Some(after) = after {
            body["after"] = json!(after);
        }
        self.post("/message/query", body).await
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
            self.method(false)
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
        let method = self.method(false);
        let mut body = json!({
            "chatGuid": chat,
            "tempGuid": temp_guid,
            "message": text,
            "method": method,
        });
        if let Some((message, part)) = reply_to {
            body["method"] = json!("private-api");
            body["selectedMessageGuid"] = json!(message);
            body["partIndex"] = json!(part);
            return self.post("/message/text", body).await;
        }
        match self.post("/message/text", body.clone()).await {
            // Chosen by `auto` on the strength of a helper that has since
            // gone: the server refused outright, so nothing was sent and
            // the ordinary route can be used instead.
            Err(e) if method == "private-api" && self.chose_for_itself() && helper_refused(&e.to_string()) => {
                self.set_helper(false);
                body["method"] = json!("apple-script");
                self.post("/message/text", body).await
            }
            other => other,
        }
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
            .text("method", self.method(true))
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
        // Through `friendly` like every other failure: an error from
        // reading the reply carries the request's address, password and
        // all, and these errors are written to the log.
        Ok(response.bytes().await.map_err(friendly)?.to_vec())
    }
}

/// How the server should send: plain AppleScript, or the Private API.
/// Whether a send failed because the server has no Private API helper to
/// hand it to, as opposed to anything that might have sent the message.
fn helper_refused(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("private api") && !error.contains("timed out") && !error.contains("timeout")
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

    #[tokio::test]
    async fn a_failed_download_does_not_repeat_the_password() {
        use std::io::{Read, Write};
        // A server that promises a thousand bytes and hangs up after five.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.read(&mut [0u8; 4096]);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nshort");
            }
        });
        let api = super::Api::new(&crate::config::Config {
            url: format!("http://127.0.0.1:{port}"),
            password: "hunter2secret".into(),
        })
        .unwrap();
        let error = api.attachment("some-picture").await.unwrap_err();
        // However it is written out: the log uses the first of these.
        for text in [format!("{error:#}"), format!("{error}"), format!("{error:?}")] {
            assert!(!text.contains("hunter2secret") && !text.contains("password"), "{text}");
        }
    }

    #[test]
    fn picks_the_way_to_send() {
        use super::method_for;
        // Left to itself: text through the helper when there is one.
        assert_eq!(method_for("auto", true, false), "private-api");
        assert_eq!(method_for("auto", false, false), "apple-script");
        assert_eq!(method_for("auto", true, true), "apple-script", "files stay with AppleScript");
        // Said outright, it is obeyed either way.
        assert_eq!(method_for("private-api", false, true), "private-api");
        assert_eq!(method_for("apple-script", true, false), "apple-script");
    }

    #[test]
    fn tells_a_missing_helper_from_other_failures() {
        use super::helper_refused;
        assert!(helper_refused("iMessage Private API Helper is not connected! (HTTP 500)"));
        assert!(helper_refused("iMessage Private API is not enabled (HTTP 500)"));
        assert!(!helper_refused("can't reach the server"));
        assert!(!helper_refused("Private API send timed out (HTTP 500)"), "it may have gone");
    }

    #[test]
    fn encodes_chat_guids() {
        assert_eq!(encode("iMessage;-;+15551234567"), "iMessage%3B-%3B%2B15551234567");
    }
}
