use clap::Parser;
use duration_str::parse;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
// use rustls::{ClientConfig, RootCertStore};
use serde::{Deserialize, Serialize};
use std::process::Stdio;
use std::time::Duration;
use std::{ops::ControlFlow, sync::Arc};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio_tungstenite::tungstenite::handshake::client::generate_key;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::{protocol::Message, Error};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{debug, error, info, warn};
use tracing_subscriber::{fmt, registry, EnvFilter};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
use uuid::Uuid;

/// A bash first monitoring solution
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// host:port with unpatched server running
    #[arg(short, long, default_value = "127.0.0.1:3000")]
    server: String,
    /// this agents name
    #[arg(short, long)]
    alias: String,
    /// attributes describing the server
    #[arg(long)]
    attributes: Option<String>,
    /// deactivate tls for frontend
    #[arg(long)]
    no_tls: bool,
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
    id: Uuid,
    name: String,
    version: String,
    output_regex: String,
    labels: Vec<String>,
    timeout: String,
    script_content: String,
}

#[derive(Serialize, Deserialize, PartialEq, Debug, Clone, Default)]
pub struct Host {
    pub id: Uuid,
    pub alias: String,
    pub attributes: Vec<String>,
    pub seed_key: Uuid,
    pub ip: String,
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

    // uuid for this instance of running software
    let x_seed_key = Uuid::new_v4();

    let args = Args::parse();

    let attributes: Vec<String> = args
        .attributes
        .unwrap_or("".into())
        .split(',')
        .map(|a| a.to_string())
        .collect();
    let host = Host {
        id: Uuid::parse_str(&agent_id).unwrap(),
        alias: args.alias,
        attributes,
        seed_key: x_seed_key,
        ..Default::default()
    };
    let serde_host = serde_json::to_string(&host).unwrap();

    

    // let mut roots = RootCertStore::empty();
    // roots.add_trust_anchors(webpki_roots::TLS_SERVER_ROOTS.iter().map(|ta| {
    //     rustls::OwnedTrustAnchor::from_subject_spki_name_constraints(
    //         ta.subject,
    //         ta.spki,
    //         ta.name_constraints,
    //     )
    // }));

    // let mut cc = ClientConfig::builder()
    //     .with_safe_defaults()
    //     .with_root_certificates(roots)
    //     .with_no_client_auth();
    // error!("{:?}", cc);
    // // cc.dangerous()
    // //     .set_certificate_verifier(Arc::new(InsecureServerCertVerifier));
    // error!("{:?}", cc);

    // Dont die on connection loss
    loop {
        // load x_api_key
        let x_api_key = std::fs::read_to_string("x_api_key").unwrap_or_else(|_| {
            let nil_uuid = Uuid::nil().to_string();
            if let Err(e) = std::fs::write("x_api_key", &nil_uuid) {
                warn!("API KEY could not be saved on filesystem, agent will get a new API KEY each restart");
                warn!("If the agent disconnects you will need to approve data-send on server side! Enable debug log for more info ");
                debug!("{:?}", e);
            };
            nil_uuid
        });

        // build request by hand
        let mut req = Request::builder()
            .method("GET")
            .header("Host", &args.server)
            .header("X_API_KEY", x_api_key)
            .header("X_SEED_KEY", &x_seed_key.to_string())
            .header("X_AGENT_ID", &agent_id)
            .header("X_AGENT_ALIAS", &host.alias)
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", generate_key())
            .body(())
            .unwrap();

        let (ws_stream, _) = if args.no_tls {
            // get websocket stream via HTTP
            let request = format!("ws://{}/ws", &args.server);
            *req.uri_mut() = request.parse().unwrap();
            match tokio_tungstenite::connect_async(&request).await {
                Ok((a, b)) => (a, b),
                Err(e) => {
                    error!("http error: \n{e}");
                    error!("Websocket connection to {request} could not be established, retrying in {} seconds", RETRY.as_secs());
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
            }
        } else {
            // get websocket stream via HTTPS
            let request = format!("wss://{}/ws", &args.server);
            *req.uri_mut() = request.parse().unwrap();
            match tokio_tungstenite::connect_async_tls_with_config(req, None, false, None).await {
                Ok((a, b)) => (a, b),
                Err(e) => {
                    error!("https error: \n{e}");
                    error!("Websocket connection to {request} could not be established, retrying in {} seconds", RETRY.as_secs());
                    tokio::time::sleep(RETRY).await;
                    continue;
                }
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
        let _sender_handle = tokio::spawn({
            let host = serde_host.clone();
            async move {
                let host_msg = format!("host:{host}");
                let _host_init = send_message(&sender_arc_sink, Message::Text(host_msg)).await;
                debug!("sent {:?}", host);

                loop {
                    let ping =
                        send_message(&sender_arc_sink, Message::Ping("Server you there?".into()))
                            .await;
                    if let Err(Error::AlreadyClosed) = ping {
                        break;
                    }
                    // dont go crazy, sleep for a while after checking for data/sending data
                    tokio::time::sleep(RETRY).await;
                }
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
                "api_key" => {
                    match msg.parse::<Uuid>() {
                        Ok(api_key) => {
                            if let Err(e) = std::fs::write("x_api_key", api_key.to_string()) {
                                warn!("API KEY could not be saved on filesystem, agent will get a new API KEY each restart");
                                warn!("If the agent disconnects you will need to approve data-send on server side! Enable debug log for more info ");
                                debug!("{:?}", e);
                            };
                        },
                        Err(e) => {
                            warn!("API KEY could not be parsed, agent will not be able to connect. Enable debug log for more info");
                            debug!("{:?}", e);
                        }
                    } 
                },

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

// struct InsecureServerCertVerifier;

// impl rustls::client::ServerCertVerifier for InsecureServerCertVerifier {
//     fn verify_server_cert(
//         &self,
//         _end_entity: &rustls::Certificate,
//         _intermediates: &[rustls::Certificate],
//         _server_name: &rustls::ServerName,
//         _scts: &mut dyn Iterator<Item = &[u8]>,
//         _ocsp_response: &[u8],
//         _now: std::time::SystemTime,
//     ) -> Result<rustls::client::ServerCertVerified, rustls::Error> {
//         Ok(rustls::client::ServerCertVerified::assertion())
//     }
// }

// #[cfg(test)]
// mod tests {
//     use std::time::SystemTime;

//     use rustls::{server::DnsName, ServerName};

//     #[test]
//     fn test_tls_import() {
//         let mut roots = rustls::RootCertStore::empty();
//         let c = rustls_native_certs::load_native_certs().expect("could not load platform certs");
//         let (acc, rej) = roots.add_parsable_certificates(&c);
//         println!("{acc}/{rej}");
//     }
// }
