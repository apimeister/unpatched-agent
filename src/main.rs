use clap::Parser;
use futures_util::stream::SplitSink;
use futures_util::{future::join_all, SinkExt, StreamExt};
use std::time::Duration;
use std::{ops::ControlFlow, sync::Arc};
use sysinfo::{System, SystemExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc::{self, Sender};
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::protocol::Message;
use tokio_tungstenite::tungstenite::Error;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
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

type SenderSinkArc = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    info!("Starting unpatched agent...");
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
        let (ws_stream, _) = match tokio_tungstenite::connect_async(&args.server).await {
            Ok((a, b)) => (a, b),
            Err(_) => {
                error!(
                    "Websocket connection could not be established, retrying in {} seconds",
                    RETRY.as_secs()
                );
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        // split websocket stream so we can have both directions working independently
        let (sender, mut receiver) = ws_stream.split();
        let arc_sink = Arc::new(Mutex::new(sender));
        // trigger first data send via mpsc channel
        let _tx_send = tx.send(true).await;

        // all things outgoing
        let _sender_handle = tokio::spawn(async move {
            // start off easy
            let _ping = sink_message(&arc_sink, Message::Ping(alias.clone().into())).await;
            info!("Connection established and validated via ping message");
            loop {
                if let Some(_data_trigger) = rx.recv().await {
                    let mut sys = System::new_all();
                    sys.refresh_all();
                    let os_version = sys.long_os_version().unwrap_or("".into());
                    let uptime = sys.uptime();

                    let units = systemctl::list_units_full(None, None, None).unwrap();
                    let units_str: String = units.iter().fold("".into(), |acc, x| {
                        let vp = match x.vendor_preset {
                            Some(true) => "enabled",
                            Some(false) => "disabled",
                            _ => "Not available",
                        };
                        format!("{}unit:{}/{}/{}\n", acc, x.unit_file, x.state, vp)
                    });
                    // u32, sqlx on server side cant parse u64
                    let free_mem = sys.free_memory();
                    let av_mem = sys.available_memory();
                    let total_mem = sys.total_memory();
                    let used_mem = sys.used_memory();

                    let messages = vec![
                        sink_message(&arc_sink, Message::Text(format!("uuid:{id}"))),
                        sink_message(&arc_sink, Message::Text(format!("alias:{alias}"))),
                        sink_message(&arc_sink, Message::Text(format!("os:{os_version}"))),
                        sink_message(&arc_sink, Message::Text(format!("uptime:{uptime}"))),
                        sink_message(
                            &arc_sink,
                            Message::Text(format!(
                                "memory:{used_mem}/{free_mem}/{av_mem}/{total_mem}"
                            )),
                        ),
                        sink_message(&arc_sink, Message::Text(units_str)),
                    ];
                    let _m_res = join_all(messages).await;
                    let clone_arc_sink = Arc::clone(&arc_sink);
                    debug!("flushing...");
                    let _flush = clone_arc_sink.lock().await.flush().await;
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
                    debug!("we are breaking!");
                    break;
                }
            }
        });

        let _ = recv_handle.await;

        error!(
            "Lost Connection to server, retrying in {} seconds ...",
            RETRY.as_secs()
        );
        tokio::time::sleep(RETRY).await;
    }
}

/// Get ARC to Splitsink and push message onto it
/// Will not actually flush any data, needs another send event
/// either via .close() or .flush()
async fn sink_message(arc: &SenderSinkArc, m: Message) -> Result<(), Error> {
    let mut x = arc.lock().await;
    debug!("feeding sink_message {:?}", m);
    x.feed(m).await
}

/// Function to handle messages we get (with a slight twist that Frame variant is visible
/// since we are working with the underlying tungstenite library directly without axum here).
fn process_message(msg: Message, who: usize, tx: Sender<bool>) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(t) => {
            debug!(">>> {} got str: {:?}", who, t);
        }
        Message::Binary(d) => {
            debug!(">>> {} got {} bytes: {:?}", who, d.len(), d);
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                debug!(
                    ">>> {} got close with code {} and reason `{}`",
                    who, cf.code, cf.reason
                );
            } else {
                debug!(">>> {} somehow got close message without CloseFrame", who);
            }
            return ControlFlow::Break(());
        }

        Message::Pong(v) => {
            debug!(">>> {} got pong with {:?}", who, v);
        }
        // Just as with axum server, the underlying tungstenite websocket library
        // will handle Ping for you automagically by replying with Pong and copying the
        // v according to spec. But if you need the contents of the pings you can see them here.
        Message::Ping(v) => {
            debug!(">>> {} got ping with {:?}", who, v);
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
