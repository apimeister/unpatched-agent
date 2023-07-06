use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::ops::{ControlFlow};
use std::time::Duration;
use sysinfo::{System, SystemExt};
use tokio::sync::mpsc::{self, Sender};
use tokio_tungstenite::tungstenite::protocol::Message;
use uuid::Uuid;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    server: String,
    #[arg(short, long)]
    alias: String,
}

const RETRY: Duration = Duration::new(5, 0);

#[tokio::main]
async fn main() {
    let agent_id = Uuid::new_v4().to_string();
    // Dont die on connection loss
    let args = Args::parse();
    let who = 0;

    loop {
        // create new channel to trigger data send everytime we restart the client
        // with mpsc we only every allow one receiver
        let (tx, mut rx) = mpsc::channel::<bool>(8);

        let id = agent_id.clone();
        let alias = args.alias.clone();

        // get websocket stream
        let (ws_stream, _) = tokio_tungstenite::connect_async(&args.server)
            .await
            .unwrap();
        // split websocket stream so we can have both directions working independently
        let (sender, mut receiver) = ws_stream.split();

        // trigger first data send via mpsc channel
        let _tx_send = tx.send(true).await;

        // all things outgoing
        let _sender_handle = tokio::spawn(async move {
            let mut sink = sender;
            let _ping = sink.feed(Message::Ping("Hello, Server!".into())).await;

            loop {
                if let Some(_data_trigger) = rx.recv().await {
                    let mut sys = System::new_all();
                    sys.refresh_all();
                    let os_version = sys.long_os_version().unwrap_or("".into());
                    let uptime = sys.uptime().to_string();

                    let _send = sink.feed(Message::Text(format!("uuid:{id}"))).await;
                    let _send = sink.feed(Message::Text(format!("alias:{alias}"))).await;
                    let _send = sink.feed(Message::Text(format!("os:{os_version}"))).await;
                    let _send = sink.feed(Message::Text(format!("uptime:{uptime}"))).await;
                    let _flush = sink.flush().await;
                }
                // dont go crazy, sleep for a while after checking for data/sending data
                tokio::time::sleep(RETRY).await;
            }
        });

        // all things incoming
        let recv_handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                // print message and break if instructed to do so
                if process_message(msg, who, tx.clone()).is_break() {
                    println!("we are breaking!");
                    break;
                }
            }
        });

        let _ = recv_handle.await;

        println!(
            "Lost Connection to server, retrying in {} seconds ...",
            RETRY.as_secs()
        );
        tokio::time::sleep(RETRY).await;
    }
}

/// Function to handle messages we get (with a slight twist that Frame variant is visible
/// since we are working with the underlying tungstenite library directly without axum here).
fn process_message(msg: Message, who: usize, tx: Sender<bool>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            println!(">>> {} got str: {:?}", who, t);
        }
        Message::Binary(d) => {
            println!(">>> {} got {} bytes: {:?}", who, d.len(), d);
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                println!(
                    ">>> {} got close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                println!(">>> {} somehow got close message without CloseFrame", who);
            }
            return ControlFlow::Break(());
        }

        Message::Pong(v) => {
            println!(">>> {} got pong with {:?}", who, v);
        }
        // Just as with axum server, the underlying tungstenite websocket library
        // will handle Ping for you automagically by replying with Pong and copying the
        // v according to spec. But if you need the contents of the pings you can see them here.
        Message::Ping(v) => {
            println!(">>> {} got ping with {:?}", who, v);
            tokio::spawn(async move {
                tx.send(true).await.unwrap();
            });
        }

        Message::Frame(_) => {
            unreachable!("This is never supposed to happen")
        }
    }
    ControlFlow::Continue(())
}
