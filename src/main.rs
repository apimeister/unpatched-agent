//! Based on tokio-tungstenite example websocket client, but with multiple
//! concurrent websocket clients in one package
//!
//! This will connect to a server specified in the SERVER with N_CLIENTS
//! concurrent connections, and then flood some test messages over websocket.
//! This will also print whatever it gets into stdout.
//!
//! Note that this is not currently optimized for performance, especially around
//! stdout mutex management. Rather it's intended to show an example of working with axum's
//! websocket server and how the client-side and server-side code can be quite similar.
//!

use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use std::borrow::Cow;
use std::fs;
use std::ops::ControlFlow;
use tokio_tungstenite::tungstenite::handshake::client::{generate_key, Request};

// we will use tungstenite for websocket client impl (same library as what axum is using)
use tokio_tungstenite::{
    connect_async,
    tungstenite::protocol::{frame::coding::CloseCode, CloseFrame, Message},
};

use clap::Parser;

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    server: String,
    #[arg(long)]
    hostname: String,
}

const N_CLIENTS: usize = 1; //set to desired number

#[tokio::main]
async fn main() {
    //spawn several clients that will concurrently talk to the server
    let mut clients = (0..N_CLIENTS)
        .map(|cli| tokio::spawn(spawn_client(cli)))
        .collect::<FuturesUnordered<_>>();

    //wait for all our clients to exit
    while clients.next().await.is_some() {}
}

//creates a client. quietly exits on failure.
async fn spawn_client(who: usize) {
    let args = Args::parse();
    let req = Request::builder()
        .method("GET")
        .header("Host", args.hostname)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header("Sec-WebSocket-Key", generate_key())
        .header("User-Agent", "internal-monitoring-agent/1.0")
        .uri(args.server)
        .body(())
        .unwrap();

    let ws_stream = match connect_async(req).await {
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
    let mut send_task = tokio::spawn(async move {
        let _send_os_version = sender
            .send(Message::Text("os:".to_string() + &read_os_version()))
            .await;
        let _send_uptime = sender
            .send(Message::Text("uptime:".to_string() + &read_uptime()))
            .await;
        tokio::time::sleep(std::time::Duration::from_millis(3000)).await;
        // if send_os_version.is_err() || send_uptime.is_err() {
        //     println!("errrrrr");
        // }

        //     if sender
        //     .send(Message::Text(read_os_version()))
        //     .await
        //     .is_err()
        // {
        //     //just as with server, if send fails there is nothing we can do but exit.
        //     return;
        // }

        // for i in 1..30 {
        //     // In any websocket error, break loop.
        //     if sender
        //         .send(Message::Text(read_os_version()))
        //         .await
        //         .is_err()
        //     {
        //         //just as with server, if send fails there is nothing we can do but exit.
        //         return;
        //     }

        //     tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        // }

        // When we are done we may want our client to close connection cleanly.
        println!("Sending close to {}...", who);
        if let Err(e) = sender
            .send(Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: Cow::from("Goodbye"),
            })))
            .await
        {
            println!("Could not send Close due to {:?}, probably it is ok?", e);
        };
    });

    //receiver just prints whatever it gets
    let mut recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            // print message and break if instructed to do so
            if process_message(msg, who).is_break() {
                break;
            }
        }
    });

    //wait for either task to finish and kill the other task
    tokio::select! {
        _ = (&mut send_task) => {
            recv_task.abort();
        },
        _ = (&mut recv_task) => {
            send_task.abort();
        }
    }
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
