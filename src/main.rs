use axum::extract::ws::{Message, WebSocketUpgrade, WebSocket};
use axum::response::Response;
use axum::Router;
use axum::routing;
use axum_server::tls_openssl::OpenSSLConfig;
use base64::prelude::*;
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use futures::stream::SplitSink;
use serde::{Serialize, Deserialize};
use tokio_tungstenite::tungstenite;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tower_http::services::ServeDir;
use webrtc::api::APIBuilder;
use webrtc::api::media_engine::{MIME_TYPE_OPUS, MediaEngine};
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::peer_connection::configuration::RTCConfiguration;
use webrtc::peer_connection::sdp::sdp_type::RTCSdpType;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::ice_transport::ice_candidate::{RTCIceCandidate,RTCIceCandidateInit};
use webrtc::track::track_local::TrackLocal;
use webrtc::track::track_remote::TrackRemote;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc_media::Sample;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
//use webrtc::ice_transport::ice_server::RTCIceServer;

const SAMPLE_RATE: u32 = 24000;
const BITRATE: usize = 64000; // [bit/s]
const BYTERATE: usize = BITRATE / 8;
const FRAME_TIME: usize = 20; // [ms]
const SEND_FRAME_SIZE: usize = (FRAME_TIME as f64 / 1000.0 * SAMPLE_RATE as f64) as usize;

#[derive(Serialize, Deserialize)]
struct MyMsg {
    op: String,
    data: String,
}

#[derive(Deserialize)]
struct OpenApiResp {
    #[serde(rename = "type")]
    typ: String,
    delta: Option<String>,
}

struct ClientContext {
    tx: SplitSink<WebSocket, Message>,
    rtc_peer: Option<RTCPeerConnection>,
    local_track: Option<Arc<TrackLocalStaticSample>>,
    have_remote_track: bool,
    openai_connected: bool,
    openai_tx: Option<SplitSink<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, tokio_tungstenite::tungstenite::Message>>,
}

type ClientContextPtr = Arc<Mutex<ClientContext>>;

async fn ws_upgrade(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(ws_handle)
}

async fn ws_handle(ws: WebSocket) {
    let (tx, mut rx) = ws.split();

    let client_context_p = Arc::new(Mutex::new(ClientContext {
        tx,
        rtc_peer: None,
        local_track: None,
        have_remote_track: false,
        openai_connected: false,
        openai_tx: None,
    }));

    while let Some(ws_msg) = rx.next().await {
        let s = match ws_msg {
            Ok(ws_msg) => match ws_msg {
                Message::Text(s) => s,
                _ => continue,
            },
            Err(e) => {
                eprintln!("ws: {}", e);
                break;
            }
        };

        let msg: MyMsg = match serde_json::from_str(&s) {
            Ok(msg) => msg,
            Err(_) => {
                ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("ERR"), data: String::from("Unable to deserialize received data") }).await;
                continue;
            }
        };

        let r = match msg.op.as_ref() {
            "INIT" => ws_handle_init(Arc::clone(&client_context_p), &msg).await,
            "ICE_CLIENT" => ws_handle_ice(Arc::clone(&client_context_p), &msg).await,
            _ => ws_handle_unknown(Arc::clone(&client_context_p), &msg).await,
        };

        if let Err(e) = r {
            ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("ERR"), data: e }).await;
        }
    }
}

async fn ws_handle_init(client_context_p: ClientContextPtr, msg: &MyMsg) -> Result<(), String> {
    let answer = {
        let mut client_context = client_context_p.lock().await;

        if client_context.rtc_peer.is_some() {
            return Err(String::from("RTC peer already initialized"));
        }

        //println!("Received SDP: {}", msg.data);

        // Init webrtc.

        let mut rtc_media_engine = MediaEngine::default();
        rtc_media_engine.register_default_codecs().unwrap();

        let rtc_api = APIBuilder::new().with_media_engine(rtc_media_engine).build();

        let rtc_config = RTCConfiguration {
            /*
            ice_servers: vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".to_owned()],
                ..Default::default()
            }],
            */
            ..Default::default()
        };

        let rtc_peer = rtc_api.new_peer_connection(rtc_config).await.map_err(|e| format!("RTC new_peer_connection: {}", e))?;
        let p = Arc::clone(&client_context_p);
        rtc_peer.on_ice_candidate(Box::new(move |candidate| Box::pin(rtc_ice(Arc::clone(&p), candidate))));
        let p = Arc::clone(&client_context_p);
        rtc_peer.on_track(Box::new(move |remote_track, _, _| Box::pin(rtc_track(Arc::clone(&p), remote_track))));

        let local_track = Arc::new(TrackLocalStaticSample::new(RTCRtpCodecCapability {
            mime_type: String::from(MIME_TYPE_OPUS),
            clock_rate: 48000, 
            channels: 2,
            sdp_fmtp_line: String::new(),
            rtcp_feedback: Vec::new(),
        }, String::from("webrtc"), String::from("webrtc")));
        rtc_peer.add_track(Arc::clone(&local_track) as Arc<dyn TrackLocal + Send + Sync>).await.map_err(|e| format!("RTC add_track: {}", e))?;

        let mut sdp = RTCSessionDescription::default();
        sdp.sdp_type = RTCSdpType::Offer;
        sdp.sdp = msg.data.clone();        
        rtc_peer.set_remote_description(sdp).await.map_err(|e| format!("RTC set_remote_description: {}", e))?;

        let answer = rtc_peer.create_answer(None).await.map_err(|e| format!("RTC create_answer: {}", e))?;
        rtc_peer.set_local_description(answer.clone()).await.map_err(|e| format!("RTC set_local_description: {}", e))?;

        client_context.rtc_peer = Some(rtc_peer);
        client_context.local_track = Some(local_track);

        //println!("Sending SDP: {}", answer.sdp);

        answer.sdp
    };

    ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("INIT_RESP"), data: answer }).await;

    Ok(())
}

async fn ws_handle_ice(client_context_p: ClientContextPtr, msg: &MyMsg) -> Result<(), String> {
    let client_context = client_context_p.lock().await;

    //println!("Received ICE: {}", msg.data);

    match &client_context.rtc_peer {
        Some(rtc_peer) => {
            let candidate: RTCIceCandidateInit = serde_json::from_str(&msg.data).map_err(|e| format!("RTC unable to parse ICE: {}", e))?;
            rtc_peer.add_ice_candidate(candidate).await.map_err(|e| format!("RTC add_ice_candidate: {}", e))?;
        },
        None => {
            return Err(String::from("RTC peer not initialized yet"));
        }
    };

    Ok(())
}

async fn ws_handle_unknown(_client_context_p: ClientContextPtr, _msg: &MyMsg) -> Result<(), String> {
    Err(String::from("Unknown op in received data"))
}

async fn ws_send(client_context_p: ClientContextPtr, msg: &MyMsg) {
    let s = serde_json::to_string(msg).unwrap();
    let ws_msg = Message::Text(s);

    if let Err(e) = client_context_p.lock().await.tx.send(ws_msg).await {
        eprintln!("ws: {}", e);
    }
}

async fn rtc_ice(client_context_p: ClientContextPtr, candidate: Option<RTCIceCandidate>) {
    if let Some(candidate) = candidate {
        let candidate = candidate.to_json().unwrap();
        let s = serde_json::to_string(&candidate).unwrap();
        ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("ICE_SERVER"), data: s }).await;
    }
}

async fn rtc_track(client_context_p: ClientContextPtr, remote_track: Arc<TrackRemote>) {
    let err = 'block: {
        let mut client_context = client_context_p.lock().await;
        if client_context.have_remote_track {
            break 'block Some(String::from("RTC: already have remote track"));
        }

        if remote_track.codec().capability.mime_type != MIME_TYPE_OPUS {
            break 'block Some(String::from("RTC: unsupported codec"));
        }

        client_context.have_remote_track = true;
        None
    };

    if let Some(s) = err {
        ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("ERR"), data: s }).await;
        return;
    }

    let (remote_to_openai_tx, remote_to_openai_rx) = mpsc::unbounded_channel();// ??? should be bounded

    tokio::spawn(openai_handle(Arc::clone(&client_context_p), remote_to_openai_rx));
    tokio::spawn(remote_track_handle(Arc::clone(&client_context_p), Arc::clone(&remote_track), remote_to_openai_tx));
}

async fn openai_handle(client_context_p: ClientContextPtr, remote_to_openai_rx: UnboundedReceiver<String>) {
    // Connect to openai.

    let api_key = std::env::var("OPENAI_API_KEY").expect("OPENAI_API_KEY is missing");

    let mut req = "wss://api.openai.com/v1/realtime?model=gpt-4o-realtime-preview-2024-10-01".into_client_request().unwrap();
    req.headers_mut().insert("Authorization", format!("Bearer {}", api_key).parse().unwrap());
    req.headers_mut().insert("OpenAI-Beta", "realtime=v1".parse().unwrap());
    
    let openapi = match tokio_tungstenite::connect_async(req).await {
        Ok((openapi, _)) => openapi,
        Err(e) => {
            ws_send(Arc::clone(&client_context_p), &MyMsg { op: String::from("ERR"), data: format!("openapi connect: {}", e) }).await;
            return;
        }
    };

    let (openapi_tx, mut openapi_rx) = openapi.split();
    {
        let mut client_context = client_context_p.lock().await;
        client_context.openai_tx = Some(openapi_tx);
    }

    // Send instructions.

    let instructions = std::env::var("OPENAI_INSTRUCTIONS").expect("OPENAI_INSTRUCTIONS is missing");
    let json = serde_json::json!({
        "type": "session.update",
        "session": {
            "modalities": ["text", "audio"],
            "instructions": instructions,
            "voice": "alloy",
            "input_audio_format": "pcm16",
            "output_audio_format": "pcm16",
            "input_audio_transcription": null,
            "turn_detection": {
                "type": "server_vad",
                "threshold": 0.5,
                "prefix_padding_ms": 300,
                "silence_duration_ms": 500
            },
            "tools": [],
            "tool_choice": "auto",
            "temperature": 0.8,
            "max_response_output_tokens": "inf"
        }        
    });
    openai_send(Arc::clone(&client_context_p), serde_json::to_string(&json).unwrap()).await;

    // Enter read loop.

    let mut encoder = opus::Encoder::new(SAMPLE_RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
    let (openai_to_remote_tx, openai_to_remote_rx) = mpsc::unbounded_channel();// ??? should be bounded
    let mut got_session = false;

    let mut remote_to_openai_rx_opt = Some(remote_to_openai_rx);
    let mut openai_to_remote_rx_opt = Some(openai_to_remote_rx);
    let mut sample_buf = Vec::new();

    while let Some(msg) = openapi_rx.next().await {
        let msg = match msg {
            Ok(msg) => msg,
            Err(e) => {
                eprintln!("openapi_ws: {}", e);
                break;
            }
        };

        let msg = match msg {
            tokio_tungstenite::tungstenite::Message::Text(msg) => msg,
            _ => continue,
        };

        let resp: OpenApiResp = match serde_json::from_str(&msg) {
            Ok(resp) => resp,
            Err(_) => {
                eprintln!("openapi_ws: failed to decode resp: {}", msg);
                continue;
            },
        };

        match resp.typ.as_ref() {
            "session.updated" => {
                if !got_session {
                    got_session = true;

                    tokio::spawn(openai_bufsender(Arc::clone(&client_context_p), remote_to_openai_rx_opt.take().unwrap()));

                    {
                        let mut client_context = client_context_p.lock().await;
                        client_context.openai_connected = true;
                    }
                
                    tokio::spawn(remote_bufsender(Arc::clone(&client_context_p), openai_to_remote_rx_opt.take().unwrap()));
                }
            },
            "response.audio.delta" => {
                if let Some(delta) = resp.delta {
                    openai_handle_response(&mut sample_buf, &mut encoder, &delta, &openai_to_remote_tx).await;
                }
            },//???at response end: flush encoder!
            _ => {
                //println!("{}", msg);
            }
        };
    }
}

async fn openai_handle_response(sample_buf: &mut Vec<i16>, encoder: &mut opus::Encoder, b64_buf: &str, openai_to_remote_tx: &UnboundedSender<Sample>) {
    let buf = match BASE64_STANDARD.decode(b64_buf) {
        Ok(buf) => buf,
        Err(_) => {
            println!("failed to decode b64");
            return;
        },
    };

    if buf.is_empty() || (buf.len() % 2 != 0) {
        println!("invalid b64 buffer");
        return;
    }

    let (_, pcm_buf, _) = unsafe { buf.align_to::<i16>() }; // Works only on little-endian machines.//???unsafe
    assert!(buf.len() == pcm_buf.len() * 2);

    sample_buf.extend_from_slice(pcm_buf);

    while sample_buf.len() >= SEND_FRAME_SIZE {
        let pcm_buf = &sample_buf[..SEND_FRAME_SIZE];

        let duration = pcm_buf.len() as f64 / SAMPLE_RATE as f64;
        let mut size = (duration * BYTERATE as f64) as usize;
        if size < 100 { // ???: What should be the minimal size?
            size = 100;
        }

        let mut encbuf = vec![0_u8; size];

        match encoder.encode(&pcm_buf, &mut encbuf) {
            Ok(s) => {
                encbuf.truncate(s);

                let sample = Sample {
                    data: Bytes::from(encbuf),
                    duration: std::time::Duration::from_secs_f64(duration),
                    ..Default::default()
                };

                if let Err(_) = openai_to_remote_tx.send(sample) {
                    println!("failed to send");
                }
            },
            Err(e) => {
                println!("encode failed: {}", e);
            },
        }

        let remaining = sample_buf.split_off(SEND_FRAME_SIZE);
        sample_buf.clear();
        sample_buf.extend_from_slice(&remaining);
    }
}

async fn remote_track_handle(client_context_p: ClientContextPtr, remote_track: Arc<TrackRemote>, remote_to_openai_tx: UnboundedSender<String>) {
    let mut decoder = opus::Decoder::new(SAMPLE_RATE, opus::Channels::Mono).unwrap();
    let mut connected = false;

    loop {
        match remote_track.read_rtp().await {
            Ok((pkt, _)) => {
                // While openai is not connected, discard received packets.

                if !connected {
                    let c = {
                        let client_context = client_context_p.lock().await;
                        client_context.openai_connected
                    };
                    if !c {
                        continue;
                    } else {
                        connected = true;
                    }
                }

                // Process packet.

                if let Ok(size) = decoder.get_nb_samples(&pkt.payload) {
                    if size > 0 {
                        let mut pcm_buf = vec![0_i16; size];

                        if let Ok(s) = decoder.decode(&pkt.payload, &mut pcm_buf, false) {
                            assert!(s == size);

                            let (_, buf, _) = unsafe { pcm_buf.align_to::<u8>() }; // Works only on little-endian machines.//???unsafe
                            let b64_buf = BASE64_STANDARD.encode(buf);

                            if let Err(_) = remote_to_openai_tx.send(b64_buf) {
                                break;
                            }
                        } else {
                            println!("decode failed");
                        }
                    }
                } else {
                    println!("get_nb_samples failed");
                }
            },
            Err(e) => {
                println!("rtp recv err: {}", e);
                break;
            },
        };
    }
}

async fn openai_bufsender(client_context_p: ClientContextPtr, mut remote_to_openai_rx: UnboundedReceiver<String>) {
    loop {
        let b64_buf = match remote_to_openai_rx.recv().await {
            Some(b64_buf) => b64_buf,
            None => break,
        };

        let json = serde_json::json!({
            "type": "input_audio_buffer.append",
            "audio": b64_buf,
        });

        openai_send(Arc::clone(&client_context_p), serde_json::to_string(&json).unwrap()).await;
    }
}

async fn openai_send(client_context_p: ClientContextPtr, s: String) {
    if let Err(e) = client_context_p.lock().await.openai_tx.as_mut().unwrap().send(tungstenite::protocol::Message::Text(s)).await {
        eprintln!("openai_send: {}", e);
    }
}

async fn remote_bufsender(client_context_p: ClientContextPtr, mut openai_to_remote_rx: UnboundedReceiver<Sample>) {
    let local_track = {
        let client_context = client_context_p.lock().await;
        Arc::clone(client_context.local_track.as_ref().unwrap())
    };

    loop {
        let sample = match openai_to_remote_rx.recv().await {
            Some(sample) => sample,
            None => break,
        };

        if let Err(e) = local_track.write_sample(&sample).await {
            println!("rtp send err: {}", e);
            break;
        }

        tokio::time::sleep(std::time::Duration::from_millis(FRAME_TIME as u64 - 1)).await;
    }
}

#[tokio::main]
async fn main() {
    let config = OpenSSLConfig::from_pem_file(std::env::var("CRT").expect("CRT is missing"), std::env::var("KEY").expect("KEY is missing")).unwrap();

    let app = Router::new()
        .route("/ws", routing::get(ws_upgrade))
        .nest_service("/", ServeDir::new("www"));

    let addr = SocketAddr::from(([0, 0, 0, 0], std::env::var("PORT").expect("PORT is missing").parse().unwrap()));
    axum_server::bind_openssl(addr, config).serve(app.into_make_service()).await.unwrap();
}
