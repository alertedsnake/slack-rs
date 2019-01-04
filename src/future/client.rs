use {api, url, reqwest, Error, Event, Sender, TxType, WsMessage};
use futures::sync::{mpsc, oneshot};
use futures::{Future, Stream, Sink};
use futures::future::{err, ok, IntoFuture};
use tokio::net::TcpStream;
use native_tls::TlsConnector;
use tokio_tls::TlsConnectorExt;
use std::net::ToSocketAddrs;
use tungstenite::Message;
use tokio_tungstenite::client_async;
use std::boxed::Box;
use std::thread;

/// The slack messaging client.
pub struct Client {
    start_response: api::rtm::StartResponse,
    rx: Option<mpsc::UnboundedReceiver<WsMessage>>,
    sender: Sender,
}

/// Implement this trait in your code to handle message events
pub trait EventHandler {
    type EventFut: IntoFuture<Item = (), Error = ()>;
    type OnCloseFut: IntoFuture<Item = (), Error = ()>;
    type OnConnectFut: IntoFuture<Item = (), Error = ()>;

    /// When a message is received this will be called with self, the slack client,
    /// and the `Event` received.
    fn on_event(&mut self, cli: &Client, event: Event) -> Self::EventFut;

    /// Called when the connection is closed for any reason.
    fn on_close(&mut self, cli: &Client) -> Self::OnCloseFut;

    /// Called when the connection is opened.
    fn on_connect(&mut self, cli: &Client) -> Self::OnConnectFut;
}

/// Like `try!` but for a future
macro_rules! try_fut {
    ($expr:expr) => {
        match $expr {
            Ok(v) => v,
            Err(e) => return Box::new(err(Error::Internal(format!("{:?}", e)))),
        }
    }
}

impl Client {
    fn login_blocking(token: String) -> Result<Self, Error> {
        let reqwest_client = reqwest::Client::new()?;
        let start_response = api::rtm::start(&reqwest_client, &token, &Default::default())?;
        let (tx, rx) = mpsc::unbounded();
        let sender = Sender::new(TxType::Future(tx));

        Ok(Client {
               start_response: start_response,
               rx: Some(rx),
               sender: sender,
           })
    }

    /// Login to slack. Call this before calling `run`.
    ///
    /// Alternatively use `login_and_run`.
    pub fn login(token: String) -> Box<Future<Item = Self, Error = Error>> {
        let (tx, rx) = oneshot::channel();
        thread::spawn(move || tx.send(Client::login_blocking(token).into_future()));
        Box::new(rx.map_err(|e| Error::Internal(format!("{:?}", e)))
                     .and_then(|client| client))
    }

    /// Run a non-blocking slack client
    // XXX: once `impl Trait` is stabilized we can get rid of all of these `Box`es
    pub fn run<'a, T: EventHandler + 'a>(mut self,
                                         mut handler: T)
                                         -> Box<Future<Item = (), Error = Error> + 'a> {
        // needed to make sure the borrow of self ends within this block so it can be borrowed
        // later below
        let wss_url = {
            let start_url = match self.start_response.url {
                Some(ref url) => url,
                None => {
                    return Box::new(err(Error::Internal("Missing websocket url from slack".into())))
                }
            };
            try_fut!(reqwest::Url::parse(start_url))
        };

        let addr = match try_fut!(wss_url.to_socket_addrs()).next() {
            None => return Box::new(err(Error::Internal("Websocket socket addr not found".into()))),
            Some(a) => a,
        };

        // unwrap is okay, rx will always be `Some` here
        let rx = self.rx.take().unwrap();

        let domain = match wss_url.origin() {
            url::Origin::Tuple(_, domain, _) => {
                match domain {
                    url::Host::Domain(d) => d,
                    s => {
                        return Box::new(err(Error::Internal(format!("Expected domain in url found: \
                                                                    {:?}",
                                                                    s))));
                    }
                }
            }
            s => {
                return Box::new(err(Error::Internal(format!("Expected domain origin, found: {:?}",
                                                            s))))
            }
        };
        let socket = TcpStream::connect(&addr);
        let cx = try_fut!(try_fut!(TlsConnector::builder()).build());
        let tls_handshake = socket
            .map_err(Error::from)
            .and_then(move |socket| {
                          cx.connect_async(&domain, socket)
                              .map_err(|e| Error::Internal(format!("{:?}", e)))
                      });

        let stream =
            tls_handshake
                .map_err(Error::from)
                .and_then(move |stream| client_async(wss_url, stream).map_err(Error::from));

        let client = stream
            .and_then(move |ws_stream| {
                handler
                    .on_connect(&mut self)
                    .into_future()
                    .map_err(|_| Error::Internal("Unit Error".into()))
                    .and_then(move |_| {
                        let (mut sink, stream) = ws_stream.split();
                        let ws_reader = stream
                            .map_err(Error::from)
                            .and_then(move |message| match message {
                                          Message::Text(text) => {
                                              match Event::from_json(&text[..]) {
                                                  Ok(event) => {
                                                      Box::new(handler
                                                                   .on_event(&self, event)
                                                                   .into_future()
                                                                   .map_err(|_| {
                                                                                Error::Internal(
                                                                           "Unit Error".into())
                                                                            })) as
                                                      Box<Future<Item = (), Error = Error>>
                                                  }
                                                  Err(err) => {
                                info!("Unable to deserialize slack message, error: {}: json: {}",
                                      err,
                                      text);
                                Box::new(ok::<(), Error>(()))
                            }
                                              }
                                          }
                                          Message::Binary(_) => Box::new(ok::<(), Error>(())),
                                      })
                            .for_each(|_| Ok(()));

                        // ws_writer stream ends when a `WsMessage::Close` message is received
                        let ws_writer = rx.take_while(|msg| match *msg {
                                                          WsMessage::Close => Ok(false),
                                                          _ => Ok(true),
                                                      })
                            .for_each(move |msg| {
                                match msg {
                                    WsMessage::Text(text) => {
                                        if sink.start_send(Message::Text(text)).is_err() {
                                            return Err(());
                                        }
                                    }
                                    WsMessage::Close => unreachable!(),
                                }
                                Ok(())
                            })
                            .map_err(|_| Error::Internal("Unit Error".into()))
                            .map(|_| ());

                        // return future when either the reader or writer terminate
                        ws_reader
                            .select(ws_writer)
                            .then(|res| match res {
                                      Ok(_) => Ok(()),
                                      Err((a, _)) => Err(a.into()),
                                  })
                    })
            })
            .map_err(Error::from);
        Box::new(client)
    }

    /// Connect to slack using the provided slack `token`, `EventHandler`, and `reactor::Handle`
    pub fn login_and_run<'a, T, S>(token: S,
                                   handler: T)
                                   -> Box<Future<Item = (), Error = Error> + 'a>
        where T: EventHandler + 'a,
              S: Into<String>
    {
        Box::new(Client::login(token.into()).and_then(move |client| client.run(handler)))
    }

    /// Get a reference thread-safe cloneable message `Sender`
    pub fn sender(&self) -> &Sender {
        &self.sender
    }

    /// Returns a reference to the `StartResponse`
    pub fn start_response(&self) -> &api::rtm::StartResponse {
        &self.start_response
    }
}
