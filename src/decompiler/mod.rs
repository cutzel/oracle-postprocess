use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
};

use futures::{SinkExt, StreamExt};
use serde_derive::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Error as TungsteniteError;
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
    MaybeTlsStream, WebSocketStream,
};

use crate::decompiler::options::DecompileOptions;

mod options;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type")]
enum WebsocketServerboundMessage {
    #[serde(rename = "decompile")]
    Decompile { data: Vec<String> },
    // i dont care about this thing existing!
    // users, however, might!
    #[allow(dead_code)]
    #[serde(rename = "options")]
    Options { options: DecompileOptions },
}

#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
enum WebsocketClientboundMessage {
    #[serde(rename = "decompilation_result")]
    DecompilationResult {
        success: bool,
        data: String,
        input_hash: String,
    },
}

pub struct DecompilationRequest {
    pub bytecode: Arc<str>,
    pub bytecode_hash: String,
    pub bytecode_len: u32,
    pub tx: oneshot::Sender<Result<String, String>>,
}

pub struct Decompiler {
    decompile_tx: mpsc::UnboundedSender<DecompilationRequest>,
    _websocket_handle: tokio::task::JoinHandle<()>,
}

const MAX_BYTES_IN_FLIGHT: u32 = 8 * 1024 * 1024; // 8 mib

impl Decompiler {
    pub async fn new(endpoint: &str, auth_token: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let mut request = endpoint.into_client_request()?;
        request
            .headers_mut()
            .insert("Authorization", format!("Bearer {}", auth_token).parse()?);

        let ws_connect = connect_async(request).await;

        let ws_stream = match ws_connect {
            Ok((ws_stream, _)) => ws_stream,
            Err(TungsteniteError::Http(e)) => {
                if let Some(body) = e.body() {
                    if let Ok(body_string) = String::from_utf8(body.clone()) {
                        return Err(body_string.into());
                    }
                }
                return Err(format!("http error: {:?}", e).into());
            }
            Err(e) => {
                eprintln!("error: {:?}", e);
                return Err(e.into());
            }
        };

        let (decompile_tx, decompile_rx) = mpsc::unbounded_channel::<DecompilationRequest>();
        let websocket_handle = tokio::spawn(Self::websocket_handler(ws_stream, decompile_rx));

        Ok(Self {
            decompile_tx,
            _websocket_handle: websocket_handle,
        })
    }

    async fn websocket_handler(
        ws_stream: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
        mut decompile_rx: mpsc::UnboundedReceiver<DecompilationRequest>,
    ) {
        let bytes_in_flight = Arc::new(AtomicU32::new(0));
        let (mut write, mut read) = ws_stream.split();

        let mut pending_requests: HashMap<String, (Vec<DecompilationRequest>, u32)> =
            HashMap::new();
        let mut queued_requests: Vec<DecompilationRequest> = Vec::new();

        loop {
            tokio::select! {
                message = read.next() => {
                    let text = match message {
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Close(_))) => {
                            eprintln!("error: websocket connection closed by server");
                            std::process::exit(1);
                        },
                        Some(Err(e)) => {
                            eprintln!("error: websocket connection error: {}", e);
                            std::process::exit(1);
                        },
                        None => {
                            eprintln!("error: websocket connection terminated unexpectedly");
                            std::process::exit(1);
                        },
                        _ => continue
                    };

                    let Ok(response) = serde_json::from_str::<WebsocketClientboundMessage>(&text) else {
                        println!("server sent something unknown: {:?}", &text);
                        continue;
                    };

                    let WebsocketClientboundMessage::DecompilationResult { success, data, input_hash } = response;

                    let Some((requests, byte_size)) = pending_requests.remove(&input_hash) else { continue; };

                    bytes_in_flight.fetch_sub(byte_size, Ordering::Relaxed);

                    let result = if success {
                        Ok(data)
                    } else {
                        Err(data)
                    };

                    for request in requests {
                        request.tx.send(result.clone()).unwrap();
                    }

                    // try to send queued requests now that we have space
                    let mut remaining_queue = Vec::with_capacity(queued_requests.len());
                    while let Some(queued_request) = queued_requests.pop() {
                        let current_bytes = bytes_in_flight.load(Ordering::Relaxed);

                        if current_bytes + queued_request.bytecode_len > MAX_BYTES_IN_FLIGHT {
                            remaining_queue.push(queued_request);
                            continue;
                        }

                        let message = serde_json::to_string(&WebsocketServerboundMessage::Decompile {
                            data: vec![queued_request.bytecode.to_string()]
                        }).unwrap();

                        if let Err(e) = write.send(Message::Text(message.into())).await {
                            eprintln!("error: failed to send websocket message (connection lost): {}", e);
                            std::process::exit(1);
                        }

                        bytes_in_flight.fetch_add(queued_request.bytecode_len, Ordering::Relaxed);
                        let bytecode_len = queued_request.bytecode_len;
                        pending_requests.insert(
                            queued_request.bytecode_hash.clone(),
                            (vec![queued_request], bytecode_len)
                        );
                    }
                    queued_requests = remaining_queue;
                }
                decompile_request = decompile_rx.recv() => {
                    let Some(request) = decompile_request else {
                        // channel closed, check if we can exit
                        if pending_requests.is_empty() && queued_requests.is_empty() {
                            break;
                        }
                        continue;
                    };

                    // check if there's already a pending request for this script hash
                    if let Some((existing_requests, _)) = pending_requests.get_mut(&request.bytecode_hash) {
                        existing_requests.push(request);
                        continue;
                    }

                    // check if single request exceeds limit
                    if request.bytecode_len > MAX_BYTES_IN_FLIGHT {
                        request.tx.send(Err(format!("bytecode too large ({:.2} mb) exceeds 8mb limit",
                            request.bytecode_len as f64 / 1024.0 / 1024.0))).unwrap();
                        continue;
                    }

                    let current_bytes = bytes_in_flight.load(Ordering::Relaxed);

                    if current_bytes + request.bytecode_len > MAX_BYTES_IN_FLIGHT {
                        queued_requests.push(request);
                        continue;
                    }

                    let message = serde_json::to_string(&WebsocketServerboundMessage::Decompile {
                        data: vec![request.bytecode.to_string()]
                    }).unwrap();

                    if let Err(e) = write.send(Message::Text(message.into())).await {
                        eprintln!("error: failed to send websocket message (connection lost): {}", e);
                        std::process::exit(1);
                    }

                    bytes_in_flight.fetch_add(request.bytecode_len, Ordering::Relaxed);
                    let bytecode_len = request.bytecode_len;
                    pending_requests.insert(request.bytecode_hash.clone(), (vec![request], bytecode_len));
                }
            }
        }
    }

    pub async fn decompile_batch(
        &self,
        requests: Vec<DecompilationRequest>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        for request in requests {
            self.decompile_tx.send(request)?;
        }
        Ok(())
    }

    pub async fn decompile_single(
        &self,
        bytecode: &str,
    ) -> Result<Result<String, String>, Box<dyn std::error::Error>> {
        let (tx, rx) = oneshot::channel();
        let bytecode_hash = format!("{:x}", Sha256::digest(bytecode.as_bytes()));
        let bytecode_len = bytecode.len() as u32;

        let request = DecompilationRequest {
            bytecode: Arc::from(bytecode),
            bytecode_hash,
            bytecode_len,
            tx,
        };

        self.decompile_tx.send(request)?;
        let result = rx.await?;
        Ok(result)
    }
}
