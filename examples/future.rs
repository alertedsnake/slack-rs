#[macro_use]
extern crate cfg_if;
cfg_if! {
    if #[cfg(feature = "future")] {
        extern crate slack;
        extern crate futures;
        extern crate tokio;
    } else {}
}

#[cfg(feature = "future")]
fn main() {
    use slack::{Event, Message};
    use slack::api::MessageStandard;
    use slack::future::client::{Client, EventHandler};
    use futures::future::{ok, FutureResult};

    struct MyHandler;

    impl EventHandler for MyHandler {
        type EventFut = FutureResult<(), ()>;
        type OnCloseFut = FutureResult<(), ()>;
        type OnConnectFut = FutureResult<(), ()>;

        fn on_event(&mut self, _cli: &Client, event: Event) -> Self::EventFut {
            // print out the event
            println!("event = {:#?}", event);
            // do something if it's a `Message::Standard`
            if let Event::Message(ref message) = event {
                if let Message::Standard(MessageStandard {
                                             ref channel,
                                             ref user,
                                             ref text,
                                             ref ts,
                                             ..
                                         }) = **message {
                    println!("{:?}, {:?}, {:?}, {:?}", channel, user, text, ts);
                }
            }
            ok(())
        }

        fn on_close(&mut self, _cli: &Client) -> Self::OnCloseFut {
            println!("on_close");
            ok(())
        }

        fn on_connect(&mut self, _cli: &Client) -> Self::OnConnectFut {
            println!("on_connect");
            ok(())
        }
    }

    let args: Vec<String> = std::env::args().collect();
    let api_key = match args.len() {
        0 | 1 => panic!("No api-key in args! Usage: cargo run --features future --example future -- <api-key>"),
        x => args[x - 1].clone(),
    };
    let mut runtime = tokio::runtime::current_thread::Runtime::new().unwrap();
    runtime.block_on(Client::login_and_run(api_key, MyHandler)).unwrap();
}

#[cfg(not(feature = "future"))]
fn main() {}
