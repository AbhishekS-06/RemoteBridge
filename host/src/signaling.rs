//! Thin JSON-over-WebSocket client for the Go signaling server. It knows
//! about exactly two message shapes -- offer and answer -- because v1 skips
//! trickle ICE: gathering finishes before either side sends its SDP, so
//! candidates are already embedded and never travel as separate messages.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

/// One STUN/TURN entry from the server. `username` and `credential` are
/// absent for plain STUN and present for TURN, where they are the short-lived
/// pair the server minted (see configMessage in signaling/main.go).
#[derive(Debug, Serialize, Deserialize)]
pub struct IceServerConfig {
    pub urls: Vec<String>,
    #[serde(default)]
    pub username: Option<String>,
    #[serde(default)]
    pub credential: Option<String>,
}

/// `Config` is sent by the server, first, right after connecting: the relay
/// servers to use. `Offer` and `Answer` are the host/viewer handshake and are
/// relayed between the two sides.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMessage {
    Config { ice_servers: Vec<IceServerConfig> },
    Offer { sdp: String },
    Answer { sdp: String },
}

pub struct SignalingClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
}

/// Build the host's signaling URL from a server base like
/// `wss://signal.example.com` (or `ws://localhost:8080`) and an optional
/// token. The token goes in the query string, so it is restricted to
/// URL-safe characters instead of pulling in a URL-encoding crate; a token
/// from `openssl rand -hex 16` always qualifies.
pub fn host_url(server: &str, token: Option<&str>) -> Result<String, String> {
    if !(server.starts_with("ws://") || server.starts_with("wss://")) {
        return Err(format!("server must start with ws:// or wss://, got {server:?}"));
    }
    let mut url = format!("{}/ws?role=host", server.trim_end_matches('/'));
    if let Some(token) = token.filter(|t| !t.is_empty()) {
        if !token.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
            return Err("token may only contain letters, digits, '-' and '_'".into());
        }
        url.push_str("&token=");
        url.push_str(token);
    }
    Ok(url)
}

impl SignalingClient {
    pub async fn connect(url: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let (socket, _) = connect_async(url).await?;
        Ok(Self { socket })
    }

    pub async fn send(&mut self, msg: &SignalMessage) -> Result<(), Box<dyn std::error::Error>> {
        let text = serde_json::to_string(msg)?;
        self.socket.send(Message::Text(text)).await?;
        Ok(())
    }

    /// Block until the next parseable signaling message arrives. Anything
    /// that isn't a text frame (ping/pong, close) is skipped rather than
    /// treated as an error -- the websocket protocol sends those on its own.
    pub async fn recv(&mut self) -> Result<SignalMessage, Box<dyn std::error::Error>> {
        while let Some(msg) = self.socket.next().await {
            if let Message::Text(text) = msg? {
                if let Ok(signal) = serde_json::from_str(&text) {
                    return Ok(signal);
                }
            }
        }
        Err("signaling connection closed before a message arrived".into())
    }
}
