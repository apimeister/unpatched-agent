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
    #[arg(long, default_value = "")]
    attributes: String,
}

// TODO: Use struct from unpatched server as a dependency
#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
struct ScriptExec {
    pub id: String,
    pub script: Script,
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

    let args = Args::parse();

    // Dont die on connection loss
    loop {
        let id = agent_id.clone();
        let alias = args.alias.clone();
        let attr = args.attributes.clone();

        // get websocket stream
        let (ws_stream, _) = match tokio_tungstenite::connect_async(&args.server).await {
            Ok((a, b)) => (a, b),
            Err(_) => {
                error!(
                    "Websocket connection to {} could not be established, retrying in {} seconds",
                    &args.server,
                    RETRY.as_secs()
                );
                tokio::time::sleep(RETRY).await;
                continue;
            }
        };
        // split websocket stream so we can have both directions working independently
        let (sender, mut receiver) = ws_stream.split();
        let arc_sink = Arc::new(Mutex::new(sender));
        info!("Connection established to host: {}", &args.server);

        // ##################
        // ALL THE SEND STUFF
        // ##################
        let sender_arc_sink = Arc::clone(&arc_sink);
        let _sender_handle = tokio::spawn(async move {
            let messages = vec![
                sink_message(&sender_arc_sink, Message::Text(format!("id:{id}"))),
                sink_message(&sender_arc_sink, Message::Text(format!("alias:{alias}"))),
                sink_message(
                    &sender_arc_sink,
                    Message::Text(format!("attributes:{attr}")),
                ),
            ];
            let _m_res = join_all(messages).await;
            debug!("sending init values");
            if let Err(e) = sender_arc_sink.lock().await.flush().await {
                error!("Unable to send init values to unpatched server\n{e}");
            };

            loop {
                let _ping =
                    send_message(&sender_arc_sink, Message::Ping("Server you there?".into())).await;
                // dont go crazy, sleep for a while after checking for data/sending data
                tokio::time::sleep(RETRY).await;
            }
        });

        // #####################
        // ALL THE RECEIVE STUFF
        // #####################
        let recv_arc_sink = Arc::clone(&arc_sink);
        let recv_handle = tokio::spawn(async move {
            while let Some(Ok(msg)) = receiver.next().await {
                // print message and break if instructed to do so
                if process_message(msg, &recv_arc_sink).await.is_break() {
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

/// Get ARC to Splitsink and push message onto it and flush them
async fn send_message(arc: &SenderSinkArc, m: Message) -> Result<(), Error> {
    let mut x = arc.lock().await;
    if m.is_ping() {
        debug!("sending ping: {:?}", m.clone().into_text().unwrap());
    } else if m.is_pong() {
        debug!("sending pong: {:?}", m.clone().into_text().unwrap());
    } else {
        debug!("sending: {:?}", m);
    }
    x.send(m).await
}

/// Function to handle messages we get (with a slight twist that Frame variant is visible
/// since we are working with the underlying tungstenite library directly without axum here).
async fn process_message(msg: Message, arc_sink: &SenderSinkArc) -> ControlFlow<(), ()> {
    match msg {
        Message::Text(content) => {
            debug!("got str: {:?}", content);
            let (message_type, msg) = content.split_once(':').unwrap();
            match message_type {
                "script" => {
                    let mut script_exec: ScriptExec = serde_json::from_str(msg).unwrap();
                    debug!("{:?}", script_exec);
                    let script = script_exec.script.clone();
                    let exec_result =
                        exec_command(&script.script_content, duration(&script.timeout)).await;
                    script_exec.script.script_content = match exec_result {
                        Ok(s) => {
                            debug!("Script {} response:\n{s}", script.name);
                            s
                        }
                        Err(e) => {
                            warn!("Script {} response:\n{e}", script.name);
                            format!("{e}")
                        }
                    };

                    let json_script = match serde_json::to_string(&script_exec) {
                        Ok(j) => j,
                        Err(e) => {
                            let res = format!(
                                "Could not transform script {} answer to json\n{e}",
                                script.id
                            );
                            error!(res);
                            res
                        }
                    };
                    let _script_res =
                        send_message(arc_sink, Message::Text(format!("script:{json_script}")))
                            .await;
                }
                _ => warn!("Server sent unsupported message type {message_type}:\n{msg}"),
            }
        }
        Message::Binary(d) => {
            debug!("Got {} bytes: {:?}", d.len(), d);
        }
        Message::Close(c) => {
            if let Some(cf) = c {
                debug!("Got close with code {} and reason `{}`", cf.code, cf.reason);
            } else {
                debug!("Somehow got close message without CloseFrame");
            }
            return ControlFlow::Break(());
        }

        // Heartbeat from Server
        Message::Pong(v) => {
            debug!(
                "Got pong with {}",
                std::str::from_utf8(&v).unwrap_or("utf-8 error, not parsable")
            );
        }

        // Heartbeat test from Server to check if agent is alive
        Message::Ping(v) => {
            debug!(
                "Got ping with {}",
                std::str::from_utf8(&v).unwrap_or("utf-8 error, not parsable")
            );
            let _pong = send_message(arc_sink, Message::Pong("still here".into())).await;
        }

        Message::Frame(_) => {
            unreachable!("This is never supposed to happen")
        }
    }
    ControlFlow::Continue(())
}

async fn exec_command(cmd: &str, timeout: Duration) -> std::io::Result<String> {
    let spawn = tokio::process::Command::new("timeout")
        .arg(format!("{}", timeout.as_secs()))
        .args(["sh", "-c", cmd])
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
