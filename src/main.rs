use clap::Parser;
use duration_str::parse;
use futures_util::stream::SplitSink;
use futures_util::{future::join_all, SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use std::{ops::ControlFlow, sync::Arc};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::{protocol::Message, Error};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, registry, EnvFilter};
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

#[derive(Default, Debug, Clone, Serialize, Deserialize)]
struct AgentDataMemory {
    used_mem: u64,
    free_mem: u64,
    av_mem: u64,
    total_mem: u64,
}
// TODO: Use struct from unpatched server as a dependency
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
struct Script {
    id: String,
    name: String,
    version: String,
    output_regex: String,
    labels: String,
    timeout: String,
    script_content: String,
}

const RETRY: Duration = Duration::new(5, 0);

type SenderSinkArc = Arc<Mutex<SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>>>;

#[tokio::main]
async fn main() {
    registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(fmt::layer())
        .init();

    info!("Starting unpatched agent...");

    let agent_id = std::fs::read_to_string("agent_id").unwrap_or_else(|_| {
        let new_uuid = Uuid::new_v4().to_string();
        if let Err(e) = std::fs::write("agent_id", &new_uuid) {
            warn!("Agent ID could not be saved on filesystem, agent will get a new ID each restart. Enable debug log for more info");
            warn!("{:?}", e);
        };
        new_uuid
    });

    // Dont die on connection loss
    let args = Args::parse();
    let who = 0;

    loop {
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

        // ##################
        // ALL THE SEND STUFF
        // ##################
        let sender_clone_arc_sink = Arc::clone(&arc_sink);
        let _sender_handle = tokio::spawn(async move {
            let messages = vec![
                sink_message(&sender_clone_arc_sink, Message::Text(format!("id:{id}"))),
                sink_message(
                    &sender_clone_arc_sink,
                    Message::Text(format!("alias:{alias}")),
                ),
                sink_message(
                    &sender_clone_arc_sink,
                    Message::Text("attributes:hello,world".into()),
                ),
            ];
            let _m_res = join_all(messages).await;
            debug!("sending init values");
            if let Err(e) = sender_clone_arc_sink.lock().await.flush().await {
                error!("Unable to send init values to unpatched server\n{e}");
            };

            loop {
                let _ping =
                    sink_message(&sender_clone_arc_sink, Message::Ping(alias.clone().into())).await;
                debug!("sending values");
                if let Err(e) = sender_clone_arc_sink.lock().await.flush().await {
                    error!("Unable to send values to unpatched server\n{e}");
                };
                // dont go crazy, sleep for a while after checking for data/sending data
                tokio::time::sleep(RETRY).await;
            }
        });

        // #####################
        // ALL THE RECEIVE STUFF
        // #####################
        let recv_clone_arc_sink = Arc::clone(&arc_sink);
        let recv_handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                // print message and break if instructed to do so
                if process_message(msg, who, &recv_clone_arc_sink)
                    .await
                    .is_break()
                {
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
    debug!("feeding sink: {:?}", m);
    x.feed(m).await
}

/// Function to handle messages we get (with a slight twist that Frame variant is visible
/// since we are working with the underlying tungstenite library directly without axum here).
async fn process_message(
    msg: Message,
    who: usize,
    arc_sink: &SenderSinkArc,
) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(raw_msg) => {
            debug!(">>> got str: {:?}", raw_msg);
            let (message_type, msg) = raw_msg.split_once(':').unwrap();
            match message_type {
                "script" => {
                    let script: Script = serde_json::from_str(msg).unwrap();
                    debug!("{:?}", script);

                    let exec_result =
                        exec_command(script.script_content, duration(&script.timeout)).await;
                    let _exec_result_str = match exec_result {
                        Ok(s) => {
                            debug!("Script response:\n{s}");
                            s
                        }
                        Err(e) => {
                            error!("{e}");
                            format!("{e}")
                        }
                    };
                }
                _ => warn!("Server sent unsupported message type {message_type}:\n{msg}"),
            }
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

        // Heartbeat from Server
        Message::Pong(v) => {
            debug!(">>> {} got pong with {:?}", who, v);
            info!("Connection established to host: {who}");
        }

        // Heartbeat test from Server to check if agent is alive
        Message::Ping(v) => {
            debug!(">>> {} got ping with {:?}", who, v);
            let _pong = sink_message(arc_sink, Message::Pong("pong".into())).await;
        }

        Message::Frame(_) => {
            unreachable!("This is never supposed to happen")
        }
    }
    ControlFlow::Continue(())
}

async fn exec_command(cmd: String, timeout: Duration) -> std::io::Result<String> {
    let spawn = tokio::process::Command::new("timeout")
        .arg(format!("{}", timeout.as_secs()))
        .args(["sh", "-c", cmd.as_str()])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = spawn.wait_with_output().await?;
    let code = match output.status.code() {
        Some(code) => code,
        None => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Interrupted,
                "Process terminated by signal",
            ))
        }
    };
    // FIXME: clean this up
    match code {
        0 => match std::str::from_utf8(&output.stdout) {
            Ok(s) => Ok(s.strip_suffix('\n').unwrap_or(s).to_string()),
            Err(e) => Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("stderr was not valid utf-8: {e}"),
            )),
        },
        124 => {
            error!("Executing Process was killed by timeout ({:?})!", timeout);
            let res = match std::str::from_utf8(&output.stdout) {
                Ok(s) => s.strip_suffix('\n').unwrap_or(s).to_string(),
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("stderr was not valid utf-8: {e}"),
                    ))
                }
            };
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!("Script timed out - Partial exec log:\n{res}"),
            ))
        }
        _ => {
            error!("Executing Process resulted in error code {}!", code);
            match std::str::from_utf8(&output.stderr) {
                Ok(s) => Ok(s.strip_suffix('\n').unwrap_or(s).to_string()),
                Err(e) => Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("stderr was not valid utf-8: {e}"),
                )),
            }
        }
    }
}

fn duration(timeout: &str) -> Duration {
    match parse(timeout) {
        Ok(d) => {
            debug!("Executing Script with duration: {d:?}");
            d
        }
        Err(e) => {
            // TODO: Return error to server
            warn!("Could not parse timeout: {e}, defaulting to 30s");
            Duration::new(30, 0)
        }
    }
}
