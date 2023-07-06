use clap::Parser;
use futures_util::{SinkExt, StreamExt};
use std::fs;
use std::ops::ControlFlow;
use std::time::Duration;
use tokio_tungstenite::connect_async;
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

const RETRY: Duration = Duration::new(5, 0); //set to desired number

#[tokio::main]
async fn main() {
    let agent_id = Uuid::new_v4().to_string();
    // Dont die on connection loss
    loop {
        let _ = tokio::spawn(spawn_client(0, agent_id.clone())).await;
        println!(
            "Lost Connection to server, retrying in {} seconds ...",
            RETRY.as_secs()
        );
        tokio::time::sleep(RETRY).await;
    }
}

//creates a client. quietly exits on failure.
async fn spawn_client(who: usize, agent_id: String) {
    let args = Args::parse();
    // let req = Request::builder()
    //     .method("GET")
    //     .header("Host", args.hostname)
    //     .header("Connection", "Upgrade")
    //     .header("Upgrade", "websocket")
    //     .header("Sec-WebSocket-Version", "13")
    //     .header("Sec-WebSocket-Key", generate_key())
    //     .header("User-Agent", "internal-monitoring-agent/1.0")
    //     .uri(args.server)
    //     .body(())
    //     .unwrap();

    let ws_stream = match connect_async(args.server).await {
        Ok((stream, response)) => {
            println!("Handshake for client {} has been completed", who);
            // This will be the HTTP response, same as with server this is the last moment we
            // can still access HTTP stuff.
            println!("Server response was {:?}", response);
            stream
        }
        Err(e) => {
            println!("WebSocket handshake for client {who} failed with {e}!");
            return;
        }
    };

    let (mut sender, mut receiver) = ws_stream.split();

    //we can ping the server for start
    sender
        .send(Message::Ping("Hello, Server!".into()))
        .await
        .expect("Can not send!");

    //spawn an async sender to push some more messages into the server
    let send_task = tokio::spawn(async move {
        let _send_alias = sender
            .send(Message::Text("uuid:".to_string() + &agent_id))
            .await;

        let _send_alias = sender
            .send(Message::Text("alias:".to_string() + &args.alias))
            .await;
        let _send_os_version = sender
            .send(Message::Text("os:".to_string() + &read_os_version()))
            .await;
        let _send_uptime = sender
            .send(Message::Text("uptime:".to_string() + &read_uptime()))
            .await;
    });

    //receiver just prints whatever it gets
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            // print message and break if instructed to do so
            if process_message(msg, who).is_break() {
                println!("we are breaking!");
                break;
            }
        }
    });

    let a = send_task.await;
    let b = recv_task.await;

    println!("a: {:?}", a);
    println!("b: {:?}", b);

    // When we are done we may want our client to close connection cleanly.
    // println!("Sending close to {}...", who);
    // if let Err(e) = sender
    //     .send(Message::Close(Some(CloseFrame {
    //         code: CloseCode::Normal,
    //         reason: Cow::from("Goodbye"),
    //     })))
    //     .await
    // {
    //     println!("Could not send Close due to {:?}, probably it is ok?", e);
    // };
}

/// Function to handle messages we get (with a slight twist that Frame variant is visible
/// since we are working with the underlying tungstenite library directly without axum here).
fn process_message(msg: Message, who: usize) -> ControlFlow<(), ()> {
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
        }

        Message::Frame(_) => {
            unreachable!("This is never supposed to happen")
        }
    }
    ControlFlow::Continue(())
}

fn read_os_version() -> String {
    println!("os");
    match fs::read_to_string("/etc/os-release") {
        Ok(os) => os,
        Err(_) => {
            "OS Release (/etc/os-release) could not be opened on client, insufficient rights?"
                .into()
        }
    }
}

fn read_uptime() -> String {
    match fs::read_to_string("/proc/uptime") {
        Ok(uptime) => {
            let (up_sec, _) = uptime.split_once(' ').unwrap_or(("0", "0"));
            up_sec.into()
        }
        Err(_) => "Uptime (/proc/uptime) could not be read on client, insufficient rights?".into(),
    }
}
