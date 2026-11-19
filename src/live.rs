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

const POLL_EVERY: Duration = Duration::from_secs(12);

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn start(api: Api, config: Config, tx: UnboundedSender<AppEvent>) {
    let nudge = Arc::new(Notify::new());
    tokio::spawn(poll(api, tx.clone(), nudge.clone()));
    tokio::spawn(socket(config, tx, nudge));
}

async fn poll(api: Api, tx: UnboundedSender<AppEvent>, nudge: Arc<Notify>) {
    // Start a little in the past: the Mac's clock and ours may disagree.
    let mut since = now_ms() - 120_000;
    loop {
        tokio::select! {
            _ = nudge.notified() => {}
            _ = tokio::time::sleep(POLL_EVERY) => {}
        }
        match api.messages_after(since).await {
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

async fn socket(config: Config, tx: UnboundedSender<AppEvent>, nudge: Arc<Notify>) {
    let url = format!("{}/?password={}", config.url, encode(&config.password));
    loop {
        let on_message = {
            let (tx, nudge) = (tx.clone(), nudge.clone());
            move |payload: Payload, _: Client| {
                let (tx, nudge) = (tx.clone(), nudge.clone());
                async move {
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
