//! Keeping up with new messages.
//!
//! Two sources feed the app: the server's socket.io stream, for messages the
//! instant they arrive, and a slow poll that catches whatever the socket
//! missed (laptop asleep, network change, server restart).

use futures_util::FutureExt;
use rust_socketio::Payload;
use rust_socketio::asynchronous::{Client, ClientBuilder};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::sync::mpsc::UnboundedSender;

use crate::api::{Api, encode};
use crate::app::AppEvent;
use crate::config::Config;
use crate::model::Message;


fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn start(api: Api, config: Config, tx: UnboundedSender<AppEvent>) {
    let nudge = Arc::new(Notify::new());
    let metrics = api.metrics.clone();
    tokio::spawn(poll(api, tx.clone(), nudge.clone()));
    tokio::spawn(socket(config, tx, nudge, metrics));
}

async fn poll(api: Api, tx: UnboundedSender<AppEvent>, nudge: Arc<Notify>) {
    // Start a little in the past: the Mac's clock and ours may disagree.
    let mut since = now_ms() - 120_000;
    let poll_every = Duration::from_secs(crate::config::settings().messages.poll_seconds);
    loop {
        tokio::select! {
            _ = nudge.notified() => {}
            _ = tokio::time::sleep(poll_every) => {}
        }
        let result = api.messages_after(since).await;
        if let Ok(mut m) = api.metrics.lock() {
            m.polls += 1;
            m.last_poll = Some(std::time::Instant::now());
            if result.is_err() {
                m.poll_failures += 1;
            }
        }
        match result {
            Ok(messages) => {
                if let Some(latest) = messages.iter().map(|m| m.date_created).max() {
                    since = since.max(latest);
                }
                if tx.send(AppEvent::Online(true)).is_err() {
                    return;
                }
                if !messages.is_empty() && tx.send(AppEvent::Incoming(messages)).is_err() {
                    return;
                }
            }
            Err(_) => {
                if tx.send(AppEvent::Online(false)).is_err() {
                    return;
                }
            }
        }
    }
}

async fn socket(
    config: Config,
    tx: UnboundedSender<AppEvent>,
    nudge: Arc<Notify>,
    metrics: crate::metrics::Shared,
) {
    let url = format!("{}/?password={}", config.url, encode(&config.password));
    loop {
        // Keep the stats screen's idea of the connection current.
        let mark = |up: bool| {
            let metrics = metrics.clone();
            move |_: Payload, _: Client| {
                let metrics = metrics.clone();
                async move {
                    if let Ok(mut m) = metrics.lock() {
                        m.live = Some(up);
                    }
                }
                .boxed()
            }
        };
        let on_message = {
            let (tx, nudge, metrics) = (tx.clone(), nudge.clone(), metrics.clone());
            move |payload: Payload, _: Client| {
                let (tx, nudge, metrics) = (tx.clone(), nudge.clone(), metrics.clone());
                async move {
                    if let Ok(mut m) = metrics.lock() {
                        m.live = Some(true);
                        m.live_events += 1;
                        m.last_live_event = Some(std::time::Instant::now());
                    }
                    if let Payload::Text(values) = payload {
                        let parsed: Vec<Message> = values
                            .into_iter()
                            .filter_map(|v| serde_json::from_value(v).ok())
                            .filter(|m: &Message| !m.guid.is_empty())
                            .collect();
                        if !parsed.is_empty() {
                            let _ = tx.send(AppEvent::Incoming(parsed));
                        }
                    }
                    // The poll fills in anything the event did not carry.
                    nudge.notify_one();
                }
                .boxed()
            }
        };
        let client = ClientBuilder::new(url.clone())
            .on("new-message", on_message.clone())
            .on("updated-message", on_message)
            .on("typing-indicator", {
                let tx = tx.clone();
                move |payload: Payload, _: Client| {
                    let tx = tx.clone();
                    async move {
                        if let Payload::Text(values) = payload
                            && let Some(event) = values.first()
                            && let Some(chat) = event.get("guid").and_then(|g| g.as_str())
                        {
                            let on = event.get("display").and_then(|d| d.as_bool()).unwrap_or(false);
                            let _ = tx.send(AppEvent::Typing {
                                chat: chat.to_string(),
                                on,
                            });
                        }
                    }
                    .boxed()
                }
            })
            .on("chat-read-status-changed", {
                let tx = tx.clone();
                move |payload: Payload, _: Client| {
                    let tx = tx.clone();
                    async move {
                        if let Payload::Text(values) = payload
                            && let Some(event) = values.first()
                            && let Some(chat) = event.get("chatGuid").and_then(|g| g.as_str())
                            && let Some(read) = event.get("read").and_then(|r| r.as_bool())
                        {
                            let _ = tx.send(AppEvent::ReadStatus {
                                chat: chat.to_string(),
                                read,
                            });
                        }
                    }
                    .boxed()
                }
            })
            .on(rust_socketio::Event::Connect, mark(true))
            .on(rust_socketio::Event::Close, mark(false))
            .on(rust_socketio::Event::Error, mark(false))
            .reconnect(true)
            .reconnect_on_disconnect(true)
            .reconnect_delay(1_000, 20_000)
            .connect()
            .await;
        match client {
            // The client reconnects by itself from here on; just keep it alive.
            Ok(_client) => return std::future::pending().await,
            Err(_) => tokio::time::sleep(Duration::from_secs(20)).await,
        }
        if tx.is_closed() {
            return;
        }
    }
}

/// Connect once to see whether live updates work with this server.
pub async fn probe(config: &Config) -> anyhow::Result<()> {
    let url = format!("{}/?password={}", config.url, encode(&config.password));
    let client = ClientBuilder::new(url)
        .reconnect(false)
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let _ = client.disconnect().await;
    Ok(())
}
