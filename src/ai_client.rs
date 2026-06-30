use futures_util::StreamExt;
use serde::{Deserialize, Serialize};

/// Represents a chat message role.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
}

/// A single chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
}

/// Chunk from the streaming response.
#[derive(Debug, Deserialize)]
struct StreamChunk {
    choices: Vec<StreamChoice>,
}

#[derive(Debug, Deserialize)]
struct StreamChoice {
    delta: Option<StreamDelta>,
}

#[derive(Debug, Deserialize)]
struct StreamDelta {
    content: Option<String>,
}

/// Streaming AI client.
pub struct AiClient {
    client: reqwest::Client,
}

impl AiClient {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
        }
    }

    /// Send a streaming chat request.
    /// Returns the accumulated response string.
    pub async fn chat_stream_collect(
        &self,
        base_url: &str,
        api_key: &str,
        messages: Vec<ChatMessage>,
    ) -> Result<String, reqwest::Error> {
        let url = format!("{base_url}/chat/completions");

        let body = serde_json::json!({
            "model": "default",
            "messages": messages.into_iter().map(|m| {
                serde_json::json!({
                    "role": match m.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                    },
                    "content": m.content,
                })
            }).collect::<Vec<_>>(),
            "stream": true,
        });

        let resp = self
            .client
            .post(&url)
            .header("Authorization", format!("Bearer {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await?;

        let mut full_content = String::new();
        let mut stream = resp.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk?;
            let text = String::from_utf8_lossy(bytes.as_ref());
            for line in text.lines() {
                if let Some(data) = line.strip_prefix("data: ") {
                    if let Ok(chunk) = serde_json::from_str::<StreamChunk>(data) {
                        if let Some(content) = chunk
                            .choices
                            .first()
                            .and_then(|c| c.delta.as_ref())
                            .and_then(|d| d.content.clone())
                        {
                            full_content.push_str(&content);
                        }
                    }
                }
            }
        }

        Ok(full_content)
    }
}
