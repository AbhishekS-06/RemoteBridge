//! Thin JSON-over-WebSocket client for the Go signaling server. It knows
//! about exactly two message shapes -- offer and answer -- because v1 skips
//! trickle ICE: gathering finishes before either side sends its SDP, so
//! candidates are already embedded and never travel as separate messages.

use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum SignalMessage {
    Offer { sdp: String },
    Answer { sdp: String },
}

pub struct SignalingClient {
    socket: WebSocketStream<MaybeTlsStream<TcpStream>>,
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
