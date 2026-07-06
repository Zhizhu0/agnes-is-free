use base64::Engine;
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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Copy)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// Content of a chat message. Most messages are plain text;
/// image/video messages carry a resource ID for rendering.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Image { id: String },
    Video { id: String },
    /// Tool call result displayed in the UI.
    ToolResult {
        /// Tool name (e.g. "generate_image").
        tool_name: String,
        /// Formatted arguments shown to the user.
        args_display: String,
        /// Result text (success message or error).
        result: String,
    },
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
    /// Image URLs (base64 data URIs) to attach to this message for vision input.
    /// When present, content is serialized as an array of content parts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_urls: Option<Vec<String>>,
    /// Client-side only: texture ID of an uploaded image to show inline in the UI.
    /// Never serialized to the API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uploaded_image: Option<String>,
    /// Client-side only: resource ID this message references.
    /// When present AND in ai_visible_resources → embed real image in API request.
    /// When present BUT NOT in ai_visible_resources → content is annotation text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ref_resource_id: Option<String>,
}

impl Default for ChatMessage {
    fn default() -> Self {
        Self {
            role: Role::User,
            content: MessageContent::Text(String::new()),
            tool_calls: None,
            tool_call_id: None,
            image_urls: None,
            uploaded_image: None,
            ref_resource_id: None,
        }
    }
}

impl ChatMessage {
    /// Create a simple text message.
    pub fn text(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: MessageContent::Text(content.into()),
            tool_calls: None,
            tool_call_id: None,
            image_urls: None,
            uploaded_image: None,
            ref_resource_id: None,
        }
    }

    /// Create an image message.
    pub fn image(id: impl Into<String>) -> Self {
        let id = id.into();
        Self {
            role: Role::Assistant,
            content: MessageContent::Image { id: id.clone() },
            tool_calls: None,
            tool_call_id: None,
            image_urls: None,
            uploaded_image: None,
            ref_resource_id: Some(id),
        }
    }

    /// Create a tool result message for UI display.
    pub fn tool_result(tool_name: &str, args_display: &str, result: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: MessageContent::ToolResult {
                tool_name: tool_name.to_string(),
                args_display: args_display.to_string(),
                result: result.to_string(),
            },
            tool_calls: None,
            tool_call_id: None,
            image_urls: None,
            uploaded_image: None,
            ref_resource_id: None,
        }
    }

}

/// Build the wireside "content" JSON value for a chat message.
/// When `image_urls` is present and non-empty, produces an OpenAI vision
/// content array; otherwise produces a plain string (backward compatible).
pub fn content_json_for_api(_role: Role, msg: &ChatMessage) -> serde_json::Value {
    let text = match &msg.content {
        MessageContent::Text(s) => s.clone(),
        MessageContent::Image { id } => format!("[generated image: {id}]"),
        MessageContent::Video { id } => format!("[generated video: {id}]"),
        MessageContent::ToolResult { result, .. } => result.clone(),
    };
    match &msg.image_urls {
        Some(urls) if !urls.is_empty() => {
            let mut parts = Vec::new();
            // Only include text part if non-empty (avoids confusing the model
            // with an empty text label alongside the image).
            if !text.is_empty() {
                parts.push(serde_json::json!({"type": "text", "text": text}));
            }
            for url in urls {
                parts.push(serde_json::json!({
                    "type": "image_url",
                    "image_url": {"url": url}
                }));
            }
            serde_json::json!(parts)
        }
        _ => serde_json::json!(text),
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
        "description": "Generate or edit an image using AI image generation. Use in two scenarios: (1) User asks to create/draw a brand-new image → text-to-image, no reference_resource_ids. (2) User asks to modify/edit an existing image (e.g. 'remove text', 'change background', 'add X') → pass the image's reference_resource_ids AND write an editing instruction as the prompt. NEVER use this tool when the user only wants to discuss or analyze an image — just respond with plain text.",
        "parameters": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "For text-to-image: a detailed English description of the image to create. For image-editing (reference_resource_ids given): a concise instruction describing what to change (e.g. 'Remove all text and logos', 'Replace the background with a beach at sunset', 'Add a bird on the left shoulder'). Do NOT rewrite the whole scene — only describe the desired change."
                },
                "reference_resource_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Resource numbers (integers, no 'R' prefix) of input images for editing (e.g. [5] for R5). For resources already visible in the current message (text mentions '资源 id 为 R5'), pass their ids directly WITHOUT calling view_resource first. For resources only mentioned as '如需查看，请调用 view_resource(N)' in older messages, call view_resource first. When reference images are provided, the prompt should be an editing instruction, not a full scene description."
                },
                "width": {
                    "type": "integer",
                    "description": "Output image width in pixels. Optional. For image-editing, defaults to the input image's width if omitted. For text-to-image, defaults to 1024 if omitted. Choose a size that matches the use case — e.g. match the reference image's aspect ratio when editing."
                },
                "height": {
                    "type": "integer",
                    "description": "Output image height in pixels. Optional. For image-editing, defaults to the input image's height if omitted. For text-to-image, defaults to 768 if omitted. Choose a size that matches the use case — e.g. match the reference image's aspect ratio when editing."
                }
            },
            "required": ["prompt"]
        }
    }
}"#;

const GENERATE_VIDEO_TOOL: &str = r#"{
    "type": "function",
    "function": {
        "name": "generate_video",
        "description": "Generate a video from text, images, or keyframes. Supports four modes: (1) Text-to-video: prompt only, no reference_resource_ids. (2) Image-to-video: one reference_resource_ids entry, animate that image. (3) Multi-image video: multiple reference_resource_ids, AI transitions between them. (4) Keyframe animation: multiple reference_resource_ids + is_keyframe=true for smooth cinematic transitions. The video generation is asynchronous: the tool creates a task and returns immediately. A background worker will poll for completion and update the UI. Current pricing: $0/second (free).",
        "parameters": {
            "type": "object",
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text description of the video to generate. For text-to-video: describe full scene (subject+action+scene+camera+lighting+style). For image-to-video: describe what should move. For multi-image/keyframe: describe the transition between images. Example: 'A cat walking on the beach at sunset, soft waves, warm golden light, cinematic style'."
                },
                "reference_resource_ids": {
                    "type": "array",
                    "items": { "type": "integer" },
                    "description": "Resource numbers (integers, no 'R' prefix) of input images. One entry = image-to-video (animate that image). Multiple entries = multi-image video (transition between them). None = text-to-video. Examples: [5] for single image R5, [5, 8] for multi-image from R5 and R8."
                },
                "is_keyframe": {
                    "type": "boolean",
                    "description": "Set to true when the user requests keyframe animation / smooth transition between multiple images. Only meaningful when 2+ reference_resource_ids are provided. When true, generates smooth cinematic transitions between the images as keyframes. Default: false."
                },
                "width": {
                    "type": "integer",
                    "description": "Video width in pixels. Optional, default 1152. Model supports 480p, 720p, 1080p auto-mapping."
                },
                "height": {
                    "type": "integer",
                    "description": "Video height in pixels. Optional, default 768. Model supports 480p, 720p, 1080p auto-mapping."
                },
                "num_frames": {
                    "type": "integer",
                    "description": "Number of frames. Must follow 8n+1 rule and ≤441. Optional, default 121 (~5 seconds at 24fps). Common values: 81 (≈3s), 121 (≈5s), 241 (≈10s), 441 (≈18s)."
                },
                "frame_rate": {
                    "type": "number",
                    "description": "Video frame rate. Optional, default 24. Range: 1-60. Higher = smoother."
                },
                "negative_prompt": {
                    "type": "string",
                    "description": "Optional. Describe what to avoid in the video (e.g. 'blurry, distorted faces')."
                },
                "seed": {
                    "type": "integer",
                    "description": "Optional. Random seed for reproducible results."
                }
            },
            "required": ["prompt"]
        }
    }
}"#;

const VIEW_RESOURCE_TOOL: &str = r#"{
    "type": "function",
    "function": {
        "name": "view_resource",
        "description": "View a resource (image) referenced in the conversation by its number. Resource references look like '[此处用户上传了 id 为 R5 的资源]'. Call this tool with the number (e.g. 5 for R5) to see the actual image. Once viewed, the image content is included in your subsequent messages until your response completes.",
        "parameters": {
            "type": "object",
            "properties": {
                "id": {
                    "type": "integer",
                    "description": "The resource number to view (e.g. 1 for R1, 5 for R5). ONLY the number, no 'R' prefix."
                }
            },
            "required": ["id"]
        }
    }
}"#;

/// Process one complete SSE event given its `data:` line contents.
///
/// Returns:
/// - `Some(StreamResult)` if this event completes the stream (finish_reason set).
/// - `None` if the stream should continue.
fn handle_sse_event(
    data: &str,
    content_text: &mut String,
    tool_buffers: &mut Vec<ToolCall>,
    had_parse_warning: &mut bool,
    chunk_count: u64,
    tx: &Option<mpsc::Sender<String>>,
) -> Option<StreamResult> {
    if data == "[DONE]" {
        return None;
    }

    // Try deserializing as a generic JSON value first so we can detect
    // metadata chunks with no real content.
    let raw_json: serde_json::Value = match serde_json::from_str(data) {
        Ok(v) => v,
        Err(e) => {
            log_warn(&format!(
                "Eventchunk #{chunk_count} not valid JSON: {e}, data: {:?}",
                data.chars().take(200).collect::<String>()
            ));
            return None;
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
        return None;
    }

    if let Ok(chunk) = serde_json::from_str::<ToolStreamChunk>(data) {
        if let Some(choice) = chunk.choices.first() {
            // Handle text content delta.
            if let Some(ref content) = choice.delta.as_ref().and_then(|d| d.content.as_ref()) {
                content_text.push_str(content);
                log_debug(&format!("Tool-chunk #{chunk_count}: text=\"{content}\""));
                if let Some(ref sender) = *tx {
                    // Best-effort non-blocking send — drop the delta if
                    // the receiver is closed; `content_text` still
                    // accumulates the full response.
                    let _ = sender.try_send(content.to_string());
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
                        return Some(StreamResult::ToolCalls(tool_buffers.clone()));
                    }
                    "stop" => {
                        log_info("Stream ended with stop");
                        return Some(StreamResult::Text(content_text.clone()));
                    }
                    _ => {}
                }
            }
        }
    } else if !*had_parse_warning {
        *had_parse_warning = true;
        log_warn(&format!(
            "Chunk #{chunk_count} had choices but failed structured parse: {:?}",
            data.chars().take(120).collect::<String>()
        ));
    }

    None
}

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

    let tools = serde_json::json!([
        serde_json::from_str::<serde_json::Value>(GENERATE_IMAGE_TOOL).unwrap(),
        serde_json::from_str::<serde_json::Value>(VIEW_RESOURCE_TOOL).unwrap(),
        serde_json::from_str::<serde_json::Value>(GENERATE_VIDEO_TOOL).unwrap(),
    ]);
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
                "content": content_json_for_api(m.role, &m),
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
        "tools": tools,
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
    // Clone tx once so handle_sse_event can push to the channel.
    let tx_inner = tx.clone();

    // SSE line-reassembly buffer.
    //
    // hyper's `bytes_stream()` splits the HTTP body at arbitrary byte
    // boundaries — an SSE `data: {...}` line may be split across two
    // consecutive `Bytes` chunks.  SSE events are terminated by a blank
    // line, so we buffer incoming bytes, drain complete lines, and only
    // hand the `data:` payload to `handle_sse_event` once its terminating
    // blank line has arrived.
    let mut line_buf: Vec<u8> = Vec::new();
    let mut pending_data_line: Option<String> = None;

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

        line_buf.extend_from_slice(&bytes);

        // Drain complete lines from the buffer.
        loop {
            // Find any line ending (\r\n or \n).
            let newline_pos = line_buf.windows(2).position(|w| w == b"\r\n")
                .or_else(|| line_buf.iter().position(|&b| b == b'\n').map(|p| p));

            let pos = match newline_pos {
                Some(p) => p,
                None => break,  // need more bytes
            };

            // Extract line without newline.
            let line_bytes: Vec<u8> = line_buf.drain(..pos).collect();
            // Consume the line terminator (\r\n or \n).
            if line_buf.first() == Some(&b'\r') {
                line_buf.drain(..1);
            }
            if line_buf.first() == Some(&b'\n') {
                line_buf.drain(..1);
            }

            let line_str = String::from_utf8_lossy(&line_bytes);
            let line_trimmed = line_str.trim_end();

            if line_trimmed.is_empty() {
                // Blank line → SSE event boundary.  Flush the pending
                // `data:` payload (if any) through the event handler.
                if let Some(data) = pending_data_line.take() {
                    chunk_count += 1;
                    if let Some(result) = handle_sse_event(
                        &data,
                        &mut content_text,
                        &mut tool_buffers,
                        &mut had_parse_warning,
                        chunk_count,
                        &tx_inner,
                    ) {
                        return result;
                    }
                }
            } else if let Some(data) = line_trimmed.strip_prefix("data: ") {
                // SSE spec allows multiple `data:` lines per event — join
                // them with `\n` per the spec.
                match &mut pending_data_line {
                    Some(prev) => {
                        prev.push('\n');
                        prev.push_str(data);
                    }
                    None => pending_data_line = Some(data.to_string()),
                }
            }
            // Ignore other SSE line types (event:, id:, retry:, comment).
        }
    }

    // Stream exhausted — flush any remaining pending data line (some APIs
    // omit the trailing blank line on the final event).
    if let Some(data) = pending_data_line.take() {
        chunk_count += 1;
        if let Some(result) = handle_sse_event(
            &data,
            &mut content_text,
            &mut tool_buffers,
            &mut had_parse_warning,
            chunk_count,
            &tx_inner,
        ) {
            return result;
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

/// Describe a single image with a short Chinese text.
///
/// Used when the user uploads an image but the model hasn't called
/// `view_resource` for it yet — the description gives the model a rough
/// idea of what the image contains without the full base64 payload.
///
/// Spawns a streaming chat request with `tx: None` (text accumulated
/// internally), retrying up to 3 times on failure with exponential
/// backoff (1s → 2s → 4s).
pub async fn describe_image(
    base_url: &str,
    api_key: &str,
    image_bytes: &[u8],
) -> Result<String, ChatError> {
    let data_uri = format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(image_bytes)
    );

    let system_msg = ChatMessage {
        role: Role::System,
        content: MessageContent::Text(
            "You are an image description assistant. Describe the image concisely in Chinese."
                .to_string(),
        ),
        ..Default::default()
    };

    let user_msg = ChatMessage {
        role: Role::User,
        content: MessageContent::Text(
            "请用一句话简单描述这张图片的主要内容（关键文字、人物特征、场景）。控制在 50 字以内。"
                .to_string(),
        ),
        image_urls: Some(vec![data_uri]),
        ..Default::default()
    };

    let messages = vec![system_msg, user_msg];

    // Retry up to 3 times with exponential backoff: 1s, 2s, 4s.
    let mut last_error = None;
    for attempt in 0..3 {
        if attempt > 0 {
            let delay = 1u64 << (attempt - 1); // 1s, 2s
            log_warn(&format!(
                "describe_image retry {attempt}/3 after {delay}s..."
            ));
            tokio::time::sleep(std::time::Duration::from_secs(delay)).await;
        }

        match chat_stream_tools(base_url, api_key, messages.clone(), None).await {
            StreamResult::Text(text) if !text.is_empty() => {
                log_info(&format!("describe_image succeeded (attempt {attempt})"));
                return Ok(text);
            }
            StreamResult::Text(_) => {
                last_error = Some(ChatError::EmptyResponse);
            }
            StreamResult::ToolCalls(_) => {
                // Model unexpectedly tried to call a tool — treat as empty
                // (unlikely for an image description request but possible).
                last_error = Some(ChatError::EmptyResponse);
            }
            StreamResult::Error(e) => {
                log_warn(&format!("describe_image attempt {attempt} failed: {e}"));
                last_error = Some(e);
            }
        }
    }

    Err(last_error.unwrap_or(ChatError::EmptyResponse))
}
