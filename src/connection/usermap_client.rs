use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};
use url::Url;

pub struct UsermapClient {
    url: Url,
}

impl UsermapClient {
    pub fn new(url: &str) -> Result<Self, url::ParseError> {
        Ok(Self {
            url: Url::parse(url)?,
        })
    }

    pub async fn connect(&self) -> Result<(), Box<dyn std::error::Error>> {
        let (ws_stream, _) = connect_async(&self.url).await?;
        let (mut write, mut read) = ws_stream.split();

        // Spawn a task to handle incoming messages
        tokio::spawn(async move {
            while let Some(message) = read.next().await {
                match message {
                    Ok(Message::Text(text)) => {
                        match serde_json::from_str(&text) {
                            Ok(json) => println!("Received update: {:?}", json),
                            Err(e) => eprintln!("Failed to parse message: {}", e),
                        }
                    }
                    Ok(Message::Close(_)) => {
                        println!("Server closed connection");
                        break;
                    }
                    Err(e) => {
                        eprintln!("Error receiving message: {}", e);
                        break;
                    }
                    _ => {}
                }
            }
        });

        Ok(())
    }
}

// Example usage:
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = UsermapClient::new("ws://localhost:5001")?;
    client.connect().await?;

    // Keep the connection alive
    loop {
        tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
    }
} 