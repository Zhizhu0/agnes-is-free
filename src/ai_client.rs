use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use std::fmt;
use tokio::sync::mpsc;

/// Log prefix for all ai_client output.
const LOG_PREFIX: &str = "[agnes]";

pub fn log_info(msg: &str) {
    eprintln!("{LOG_PREFIX} INFO: {msg}");
}

pub fn log_debug(msg: &str) {
    eprintln!("{LOG_PREFIX} DEBUG: {msg}");
}

pub fn log_warn(msg: &str) {
    eprintln!("{LOG_PREFIX} WARN: {msg}");
}

pub fn log_error(msg: &str) {
    eprintln!("{LOG_PREFIX} ERROR: {msg}");
}

/// Represents a chat message role.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a chat message. Most messages are plain text;
/// image messages carry a texture ID for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Image { id: String },
}

impl Default for MessageContent {
    fn default() -> Self {
        MessageContent::Text(String::new())
    }
}

/// A function call the model wants to make.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub arguments: String,
}

/// A single chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    #[serde(default)]
    pub content: MessageContent,
    /// For assistant messages with tool calls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    /// For tool response messages — the ID of the tool call this responds to.
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "tool_call_id")]
    pub tool_call_id: Option<String>,
}

impl ChatMessage {
    /// Create a simple text message.
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
        }
    }

    /// Create an image message.
    pub fn image(id: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::Image { id: id.into() },
            tool_calls: None,
            tool_call_id: None,
        }
    }

}

/// Errors that can occur during streaming chat.
#[derive(Debug, Clone)]
pub enum ChatError {
    /// The HTTP request failed (network, timeout, HTTP error).
    Http(String),
    /// The response contained no usable content.
    EmptyResponse,
}

impl fmt::Display for ChatError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChatError::Http(msg) => write!(f, "HTTP error: {msg}"),
            ChatError::EmptyResponse => write!(f, "Empty response from API"),
        }
    }
}

impl std::error::Error for ChatError {}

// ─── Tool-use streaming ────────────────────────────────────────────────

/// Result of a streaming chat request that may include tool calls.
#[derive(Debug)]
pub enum StreamResult {
    /// Normal text response, fully accumulated.
    Text(String),
    /// Model wants to call one or more functions.
    ToolCalls(Vec<ToolCall>),
    /// An error occurred.
    Error(ChatError),
}

/// Tool-aware stream chunk (includes tool_calls in delta).
#[derive(Debug, Deserialize)]
struct ToolStreamChunk {
    choices: Vec<ToolStreamChoice>,
}

#[derive(Debug, Deserialize)]
struct ToolStreamChoice {
    delta: Option<ToolStreamDelta>,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ToolStreamDelta {
    content: Option<String>,
    #[serde(default)]
    tool_calls: Option<Vec<ToolStreamDeltaToolCall>>,
}

#[derive(Debug, Deserialize)]
struct ToolStreamDeltaToolCall {
    index: usize,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    function: Option<ToolStreamFunction>,
}

#[derive(Debug, Deserialize)]
struct ToolStreamFunction {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
}

/// Tool definitions to include in the chat request.
const GENERATE_IMAGE_TOOL: &str = r#"{
    "type": "function",
    "function": {
        "name": "generate_image",
        "description": "Generate an image from a text prompt using AI image generation. Use when the user asks to create, generate, draw, or produce an image (including in Chinese: 生成, 画, 制作图片).",
        "parameters": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Detailed English description of the image to generate. Be specific about subject, style, lighting, and composition."
                }
            },
            "required": ["prompt"]
        }
    }
}"#;

/// Send a streaming chat request with tool-use support.
/// Returns either accumulated text, or a list of tool calls the model wants to make.
pub async fn chat_stream_tools(
    base_url: &str,
    api_key: &str,
    messages: Vec<ChatMessage>,
    tx: Option<mpsc::Sender<String>>,
) -> StreamResult {
    let url = format!("{base_url}/chat/completions");
    log_info(&format!("POST {url} (with tools) with {} messages", messages.len()));

    let tools: serde_json::Value = serde_json::from_str(GENERATE_IMAGE_TOOL).unwrap();
    let body = serde_json::json!({
        "model": "agnes-2.0-flash",
        "messages": messages.into_iter().map(|m| {
            let role_str = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            let mut msg_obj = serde_json::json!({
                "role": role_str,
                "content": match &m.content {
                    MessageContent::Text(s) => serde_json::json!(s),
                    MessageContent::Image { .. } => serde_json::json!("[image]"),
                },
            });
            if let Some(ref tc) = m.tool_calls {
                msg_obj["tool_calls"] = serde_json::json!(tc);
            }
            if let Some(ref id) = m.tool_call_id {
                msg_obj["tool_call_id"] = serde_json::json!(id);
            }
            msg_obj
        }).collect::<Vec<_>>(),
        "stream": true,
        "tools": [tools],
        "tool_choice": "auto",
    });

    let client = reqwest::Client::new();

    log_info("Sending HTTP request (with tools)...");
    let resp = match client
        .post(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            let err = ChatError::Http(format!("Request failed: {e}"));
            log_error(&format!("{err}"));
            return StreamResult::Error(err);
        }
    };

    let status = resp.status();
    log_info(&format!("Got HTTP {status}"));
    if !status.is_success() {
        let body = resp
            .text()
            .await
            .unwrap_or_else(|_| "<unable to read error body>".into());
        let err = ChatError::Http(format!("API error ({status}): {body}"));
        log_error(&format!("{err}"));
        return StreamResult::Error(err);
    }

    let mut stream = resp.bytes_stream();
    let mut content_text = String::new();
    let mut tool_buffers: Vec<ToolCall> = Vec::new();
    let mut had_parse_warning = false;
    let mut chunk_count = 0u64;

    log_info("Starting to read stream chunks (with tools)...");
    while let Some(raw_chunk) = stream.next().await {
        let bytes = match raw_chunk {
            Ok(b) => b,
            Err(e) => {
                let err = ChatError::Http(format!("Stream read error: {e}"));
                log_error(&format!("{err}"));
                return StreamResult::Error(err);
            }
        };

        let text = String::from_utf8_lossy(bytes.as_ref());
        for line in text.lines() {
            let data = match line.strip_prefix("data: ") {
                Some(d) => d,
                None => continue,
            };
            chunk_count += 1;
            if data == "[DONE]" {
                continue;
            }

            // Try deserializing as a generic JSON value first so we can
            // log more detail on partial-match chunks.
            let raw_json: serde_json::Value = match serde_json::from_str(data) {
                Ok(v) => v,
                Err(e) => {
                    log_warn(&format!(
                        "Chunk #{chunk_count} not valid JSON: {e}, data: {:?}",
                        data.chars().take(120).collect::<String>()
                    ));
                    continue;
                }
            };

            // Alpine/Agnes sometimes returns metadata chunks with
            // "choices": [] or with no delta at all.  Skip these silently.
            if raw_json
                .get("choices")
                .and_then(|c| c.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true)
            {
                // Normal metadata chunk (id/model/usage) — skip silently.
                continue;
            }

            if let Ok(chunk) = serde_json::from_str::<ToolStreamChunk>(data) {
                if let Some(choice) = chunk.choices.first() {
                    // Handle text content delta.
                    if let Some(ref content) = choice.delta.as_ref().and_then(|d| d.content.as_ref()) {
                        content_text.push_str(content);
                        log_debug(&format!("Tool-chunk #{chunk_count}: text=\"{content}\""));
                        if let Some(ref sender) = tx {
                            let _ = sender.send(content.to_string()).await;
                        }
                    }
                    // Handle tool_calls delta.
                    if let Some(ref tool_calls) = choice.delta.as_ref().and_then(|d| d.tool_calls.as_ref()) {
                        for tc_delta in tool_calls.iter() {
                            // Ensure buffer has enough entries.
                            while tool_buffers.len() <= tc_delta.index {
                                tool_buffers.push(ToolCall {
                                    id: String::new(),
                                    name: String::new(),
                                    arguments: String::new(),
                                });
                            }
                            let buf = &mut tool_buffers[tc_delta.index];
                            if let Some(ref id) = tc_delta.id {
                                buf.id = id.clone();
                            }
                            if let Some(ref func) = tc_delta.function {
                                if let Some(ref name) = func.name {
                                    buf.name = name.clone();
                                }
                                if let Some(ref args) = func.arguments {
                                    buf.arguments.push_str(args);
                                }
                            }
                        }
                    }
                    // Check finish_reason.
                    if let Some(ref reason) = choice.finish_reason {
                        match reason.as_str() {
                            "tool_calls" => {
                                log_info(&format!(
                                    "Stream ended with tool_calls: {} tool call(s)",
                                    tool_buffers.len()
                                ));
                                return StreamResult::ToolCalls(tool_buffers);
                            }
                            "stop" => {
                                log_info("Stream ended with stop");
                                return StreamResult::Text(content_text);
                            }
                            _ => {}
                        }
                    }
                }
            } else if !had_parse_warning {
                had_parse_warning = true;
                log_warn(&format!(
                    "Chunk #{chunk_count} had choices but failed structured parse: {:?}",
                    data.chars().take(120).collect::<String>()
                ));
            }
        }
    }

    // Stream ended without explicit finish_reason.
    if !tool_buffers.is_empty() {
        log_info("Stream ended (EOF) with pending tool calls");
        StreamResult::ToolCalls(tool_buffers)
    } else if !content_text.is_empty() {
        log_info("Stream ended (EOF) with text");
        StreamResult::Text(content_text)
    } else {
        log_warn("Stream ended with no content");
        StreamResult::Error(ChatError::EmptyResponse)
    }
}
