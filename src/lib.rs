//
// Copyright 2014-2016 the slack-rs authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Slack realtime messaging client: https://api.slack.com/bot-users
//!
//! See [CHANGELOG.md](https://github.com/slack-rs/slack-rs/blob/master/CHANGELOG.md) for latest
//! release notes.

extern crate reqwest;
pub extern crate slack_api as api;
extern crate url;
#[macro_use]
extern crate serde_derive;
extern crate serde_json;
extern crate tungstenite;
#[macro_use]
extern crate log;
#[macro_use]
extern crate cfg_if;

cfg_if! {
    if #[cfg(feature = "future")] {
        extern crate tokio_tungstenite;
        extern crate futures;
        extern crate tokio_core;
        extern crate tokio_tls;
        extern crate native_tls;
        pub mod future;
    } else {}
}

pub mod error;
pub use error::Error;

pub use api::{Channel, Group, Im, Message, Team, User};

mod events;
pub use events::Event;

use events::{MessageError, MessageSent};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, channel};
use std::sync::Arc;

/// Implement this trait in your code to handle message events
pub trait EventHandler {
    /// When a message is received this will be called with self, the slack client,
    /// and the `Event` received.
    fn on_event(&mut self, cli: &RtmClient, event: Event);

    /// Called when the connection is closed for any reason.
    fn on_close(&mut self, cli: &RtmClient);

    /// Called when the connection is opened.
    fn on_connect(&mut self, cli: &RtmClient);
}

/// Used for passing websocket messages in channels
#[derive(Debug)]
enum WsMessage {
    Close,
    Text(String),
}

#[derive(Clone)]
enum TxType {
    Sync(mpsc::Sender<WsMessage>),
    #[cfg(feature = "future")]
    Future(futures::sync::mpsc::UnboundedSender<WsMessage>),
}

impl TxType {
    fn send(&self, msg: WsMessage) -> Result<(), Error> {
        match *self {
            TxType::Sync(ref tx) => tx
                .send(msg)
                .map_err(|err| Error::Internal(format!("{}", err))),
            #[cfg(feature = "future")]
            TxType::Future(ref tx) => tx
                .send(msg)
                .map_err(|err| Error::Internal(format!("{}", err))),
        }
    }
}

/// The actual messaging client.
pub struct RtmClient {
    start_response: api::rtm::StartResponse,
    sender: Sender,
    rx: mpsc::Receiver<WsMessage>,
}

/// Thread-safe API for sending messages asynchronously
#[derive(Clone)]
pub struct Sender {
    tx: TxType,
    msg_num: Arc<AtomicUsize>,
}

impl Sender {
    fn new(tx: TxType) -> Self {
        Sender {
            tx: tx,
            msg_num: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Get the next message id
    ///
    /// A value returned from this method *must* be included in the JSON payload
    /// (the `id` field) when constructing your own message.
    pub fn get_msg_uid(&self) -> usize {
        self.msg_num.fetch_add(1, Ordering::SeqCst)
    }

    /// Send a raw message
    ///
    /// Must set `message.id` using result of `get_msg_id()`.
    ///
    /// Success from this API does not guarantee the message is delivered
    /// successfully since that runs on a separate task.
    pub fn send(&self, raw: &str) -> Result<(), Error> {
        self.tx
            .send(WsMessage::Text(raw.to_string()))
            .map_err(|err| Error::Internal(format!("{}", err)))?;
        Ok(())
    }

    /// Send a message to the specified channel id
    ///
    /// Success from this API does not guarantee the message is delivered
    /// successfully since that runs on a separate task.
    ///
    /// `channel_id` is the slack channel id, e.g. `UXYZ1234`, *not* `#general`.
    ///
    /// Only valid after `RtmClient::run`.
    pub fn send_message(&self, channel_id: &str, msg: &str) -> Result<usize, Error> {
        let n = self.get_msg_uid();
        let msg_json = serde_json::to_string(&msg)?;
        let mstr = format!(
            r#"{{"id": {},"type": "message", "channel": "{}","text": "{}"}}"#,
            n,
            channel_id,
            &msg_json[1..msg_json.len() - 1]
        );

        self.send(&mstr[..])
            .map_err(|err| Error::Internal(format!("{}", err)))?;

        Ok(n)
    }

    /// Marks connected client as being typing to a channel
    /// This is mostly used to signal to other peers that a message
    /// is being typed. Will have the server send a "user_typing" message to all the
    /// peers.
    /// Slack doc can be found at https://api.slack.com/rtm under "Typing Indicators"
    ///
    /// `channel_id` is the slack channel id, e.g. `UXYZ1234`, not `#general`.
    pub fn send_typing(&self, channel_id: &str) -> Result<usize, Error> {
        let n = self.get_msg_uid();
        let mstr = format!(
            r#"{{"id": {}, "type": "typing", "channel": "{}"}}"#,
            n, channel_id
        );

        self.send(&mstr)
            .map_err(|err| Error::Internal(format!("{:?}", err)))?;
        Ok(n)
    }

    /// Shutdown `RtmClient`
    pub fn shutdown(&self) -> Result<(), Error> {
        self.tx
            .send(WsMessage::Close)
            .map_err(|_| Error::Internal("Error sending shutdown message".into()))
    }
}

impl RtmClient {
    /// Logs in to slack. Call this before calling `run`.
    /// Alternatively use `login_and_run`.
    pub fn login(token: &str) -> Result<RtmClient, Error> {
        let client = api::default_client()?;
        let start_response = api::rtm::start(&client, token, &Default::default())?;

        // setup channels for passing messages
        let (tx, rx) = channel::<WsMessage>();
        let sender = Sender::new(TxType::Sync(tx));

        Ok(RtmClient {
            start_response: start_response,
            sender: sender,
            rx: rx,
        })
    }

    /// Runs the message receive loop
    pub fn run<T: EventHandler>(&self, handler: &mut T) -> Result<(), Error> {
        let start_url = self
            .start_response
            .url
            .as_ref()
            .ok_or(Error::Api("Slack did not provide a URL".into()))?;

        let wss_url = reqwest::Url::parse(&start_url)?;
        let (mut websocket, _) = tungstenite::connect(wss_url)?;

        handler.on_connect(self);
        // receive loop
        loop {
            // try to write out pending messages (if any)
            loop {
                match self.rx.try_recv() {
                    Ok(msg) => match msg {
                        WsMessage::Text(text) => {
                            websocket.write_message(tungstenite::Message::Text(text))?
                        }
                        WsMessage::Close => {
                            handler.on_close(self);
                            return websocket.close(None).map_err(|e| e.into());
                        }
                    },
                    Err(mpsc::TryRecvError::Disconnected) => {
                        handler.on_close(self);
                        return Err(Error::Internal("rx disconnected".into()));
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                }
            }

            // blocks until a message is received or websocket errors
            let message = websocket.read_message()?;

            // handle the message
            match message {
                tungstenite::Message::Text(text) => match Event::from_json(&text[..]) {
                    Ok(event) => handler.on_event(self, event),
                    Err(err) => {
                        info!(
                            "Unable to deserialize slack message, error: {}: json: {}",
                            err, text
                        );
                    }
                },
                tungstenite::Message::Binary(_) => {}
                tungstenite::Message::Ping(_) => {}
                tungstenite::Message::Pong(_) => {}
                tungstenite::Message::Close(_) => {}
            }
        }
    }

    /// Runs the main loop for the client after logging in to slack.
    ///
    /// Returns an error if the process fails at any point, or an Ok(()) on successful
    /// close.
    ///
    /// Takes an `EventHandler` implemented by the user which will be called when `Event`s are
    /// received.
    pub fn login_and_run<T: EventHandler>(token: &str, handler: &mut T) -> Result<(), Error> {
        let client = RtmClient::login(token)?;
        client.run(handler)
    }

    /// Get a reference thread-safe cloneable message `Sender`
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// Returns a reference to the `StartResponse`.
    pub fn start_response(&self) -> &api::rtm::StartResponse {
        &self.start_response
    }
}

impl Event {
    /// Try to deserialize an `Event` from a json-encoded `&str`
    fn from_json(s: &str) -> Result<Event, Error> {
        match serde_json::from_str::<Event>(s) {
            Ok(ev) => Ok(ev),
            Err(e) => {
                // try for the MessageSent / MessageError variants that don't expose type
                if let Ok(ev) = serde_json::from_str::<MessageSent>(s) {
                    Ok(Event::MessageSent(ev))
                } else if let Ok(ev) = serde_json::from_str::<MessageError>(s) {
                    Ok(Event::MessageError(ev))
                } else {
                    Err(e.into())
                }
            }
        }
    }
}
