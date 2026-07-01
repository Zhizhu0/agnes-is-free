mod ai_client;
mod config;
mod image_gen;
mod markdown;

use ai_client::{
    log_error, log_info, log_warn, ChatError, ChatMessage, MessageContent, Role, StreamResult,
};
use config::Config;
use egui::load::SizedTexture;
use egui::{Color32, RichText, Ui};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Compute display size for an image: scale to fit within max_width
/// while preserving aspect ratio (max 320px height to avoid giant images).
fn image_fit_size(handle: &egui::TextureHandle, max_width: f32) -> egui::Vec2 {
    let orig = handle.size_vec2();
    let max_height = 320.0;
    let scale = (max_width / orig.x).min(max_height / orig.y).min(1.0);
    egui::vec2(orig.x * scale, orig.y * scale)
}

/// The main app state shared between UI and async tasks.
struct AppState {
    config: Config,
    messages: Vec<ChatMessage>,
    input: String,
    /// The current streaming assistant message being built.
    assistant_buffer: String,
    /// Whether a stream is currently in progress.
    streaming: bool,
    /// Whether the settings modal is open.
    show_settings: bool,
    /// Temporary API key input in the settings modal.
    settings_api_key: String,
    /// Error message to display.
    error: Option<String>,
    /// Generated image textures, keyed by image ID (UUID-like string).
    textures: HashMap<String, egui::TextureHandle>,
    /// Raw PNG bytes for each generated image, keyed by image ID.
    image_bytes: HashMap<String, Vec<u8>>,
    /// Pending image data waiting to be registered as textures on the UI thread.
    pending_images: Vec<(String, Vec<u8>)>,
    /// Whether the API is currently generating an image.
    generating_image: bool,
    /// The system message describing tool availability (added once per session).
    system_message_added: bool,
}

impl AppState {
    fn new() -> Self {
        let config = Config::load();
        Self {
            config,
            messages: Vec::new(),
            input: String::new(),
            assistant_buffer: String::new(),
            streaming: false,
            show_settings: false,
            settings_api_key: String::new(),
            error: None,
            textures: HashMap::new(),
            image_bytes: HashMap::new(),
            pending_images: Vec::new(),
            generating_image: false,
            system_message_added: false,
        }
    }

    fn reset_settings(&mut self) {
        self.settings_api_key = self.config.api_key.clone();
    }
}

/// Generate a unique ID for an image.
fn gen_image_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("img_{n}")
}

/// Copy raw RGBA image data to the system clipboard.
fn copy_image_to_clipboard(img: &image::RgbaImage) {
    use arboard::ImageData;
    let (w, h) = (img.width() as usize, img.height() as usize);
    let raw = img.as_raw().to_vec(); // RGBA8
    match arboard::Clipboard::new() {
        Ok(mut cb) => {
            let image_data = ImageData {
                width: w,
                height: h,
                bytes: raw.into(),
            };
            if let Err(e) = cb.set_image(image_data) {
                log_error(&format!("Failed to copy image to clipboard: {e}"));
            } else {
                log_info("Image copied to clipboard");
            }
        }
        Err(e) => {
            log_error(&format!("Failed to open clipboard: {e}"));
        }
    }
}

// ─── App ───────────────────────────────────────────────────────────────

struct AgnesApp {
    state: Arc<Mutex<AppState>>,
}

impl AgnesApp {
    fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(AppState::new())),
        }
    }

    fn send_message(&self, ctx: egui::Context) {
        let mut state = self.state.lock().unwrap();

        let input = state.input.trim().to_string();
        if input.is_empty() || state.streaming {
            return;
        }

        // Validate API key
        if state.config.api_key.is_empty() {
            state.error = Some("Please set your API key in Settings first.".into());
            return;
        }

        // Add user message
        log_info(&format!("Sending user message: {:?}", &input[..input.len().min(80)]));
        state.messages.push(ChatMessage::text(Role::User, &input));
        state.input.clear();
        state.streaming = true;
        state.assistant_buffer.clear();
        state.error = None;

        // Add system message with tool description if not already present.
        if !state.system_message_added {
            state.system_message_added = true;
            state.messages.insert(
                0,
                ChatMessage::text(
                    Role::System,
                    "You have access to an image generation tool called generate_image. \
                     When the user asks to create, generate, draw, or produce an image \
                     (including in Chinese: 生成, 画, 制作图片, 画一张), use the \
                     generate_image tool. Write detailed English prompts for best results. \
                     After generating an image, briefly describe what you created.",
                ),
            );
        }

        let config = state.config.clone();
        let messages = state.messages.clone();
        let state = self.state.clone();
        let ctx = ctx.clone();

        // Spawn async task on a new thread with tokio runtime
        log_info("Spawning worker thread...");
        std::thread::spawn(move || {
            log_info("Worker thread started");
            let rt = tokio::runtime::Runtime::new().unwrap();
            log_info("Created tokio runtime");

            rt.block_on(async {
                let mut conversation = messages;
                let base_url = config.base_url.clone();
                let api_key = config.api_key.clone();

                // Tool-use loop: continue until the model returns plain text
                // or an error occurs.
                loop {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
                    let base_url2 = base_url.clone();
                    let api_key2 = api_key.clone();
                    let msgs = conversation.clone();

                    let dispatch_handle =
                        tokio::task::spawn(async move {
                            ai_client::chat_stream_tools(
                                &base_url2,
                                &api_key2,
                                msgs,
                                Some(tx),
                            )
                            .await
                        });

                    // Drain incoming text chunks while waiting for the result.
                    while let Some(chunk) = rx.recv().await {
                        let mut app_state = state.lock().unwrap();
                        app_state.assistant_buffer.push_str(&chunk);
                        drop(app_state);
                        ctx.request_repaint();
                        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    }

                    // Get the streaming result.
                    let result = match dispatch_handle.await {
                        Ok(r) => r,
                        Err(e) => StreamResult::Error(ChatError::Http(format!(
                            "Dispatch task panicked: {e}"
                        ))),
                    };

                    match result {
                        StreamResult::Text(text) => {
                            // Model returned text — finalize the conversation turn.
                            log_info(&format!(
                                "Model returned text ({} chars)",
                                text.len()
                            ));
                            let mut app_state = state.lock().unwrap();
                            app_state.streaming = false;
                            if !text.is_empty() {
                                app_state.messages.push(ChatMessage::text(
                                    Role::Assistant,
                                    &text,
                                ));
                            }
                            app_state.assistant_buffer.clear();
                            app_state.generating_image = false;
                            break;
                        }
                        StreamResult::ToolCalls(tool_calls) => {
                            // Model wants to call one or more functions.
                            log_info(&format!(
                                "Model requested {} tool call(s)",
                                tool_calls.len()
                            ));

                            // Register the assistant message with tool_calls for
                            // protocol correctness.
                            tool_calls.iter().for_each(|tc| {
                                conversation.push(ChatMessage {
                                    role: Role::Assistant,
                                    content: MessageContent::Text(String::new()),
                                    tool_calls: Some(vec![tc.clone()]),
                                    tool_call_id: None,
                                });
                            });

                            // Execute each tool call.
                            let mut any_error = false;
                            for tc in &tool_calls {
                                if tc.name != "generate_image" {
                                    log_warn(&format!(
                                        "Unknown tool call: {}",
                                        tc.name
                                    ));
                                    conversation.push(ChatMessage {
                                        role: Role::Tool,
                                        content: MessageContent::Text(format!(
                                            "Unknown tool: {}",
                                            tc.name
                                        )),
                                        tool_calls: None,
                                        tool_call_id: Some(tc.id.clone()),
                                    });
                                    continue;
                                }

                                // Parse the prompt from tool call arguments.
                                let prompt: String =
                                    match serde_json::from_str::<serde_json::Value>(
                                        &tc.arguments,
                                    ) {
                                        Ok(v) => v
                                            .get("prompt")
                                            .and_then(|p| p.as_str())
                                            .unwrap_or("")
                                            .to_string(),
                                        Err(e) => {
                                            log_error(&format!(
                                                "Failed to parse tool args: {e}"
                                            ));
                                            conversation.push(ChatMessage {
                                                role: Role::Tool,
                                                content: MessageContent::Text(format!(
                                                    "Error parsing arguments: {e}"
                                                )),
                                                tool_calls: None,
                                                tool_call_id: Some(tc.id.clone()),
                                            });
                                            continue;
                                        }
                                    };

                                if prompt.is_empty() {
                                    conversation.push(ChatMessage {
                                        role: Role::Tool,
                                        content: MessageContent::Text(
                                            "Error: empty prompt".into(),
                                        ),
                                        tool_calls: None,
                                        tool_call_id: Some(tc.id.clone()),
                                    });
                                    continue;
                                }

                                log_info(&format!(
                                    "Generating image with prompt: {:?}",
                                    &prompt[..prompt.len().min(80)]
                                ));

                                // Show generating indicator.
                                {
                                    let mut app_state = state.lock().unwrap();
                                    app_state.generating_image = true;
                                    app_state.assistant_buffer = "🎨 正在生成图片...".into();
                                }
                                ctx.request_repaint();

                                // Call the image generation API.
                                let image_result = image_gen::ImageGenClient::new()
                                    .generate(&base_url, &api_key, &prompt)
                                    .await;

                                match image_result {
                                    Ok(png_bytes) => {
                                        let image_id = gen_image_id();
                                        log_info(&format!(
                                            "Image generated: {} ({} bytes)",
                                            image_id,
                                            png_bytes.len()
                                        ));

                                        // Queue the image for texture creation on UI thread.
                                        {
                                            let mut app_state = state.lock().unwrap();
                                            app_state.pending_images.push((
                                                image_id.clone(),
                                                png_bytes.clone(),
                                            ));
                                            app_state
                                                .image_bytes
                                                .insert(image_id.clone(), png_bytes);
                                            app_state.messages.push(
                                                ChatMessage::image(&image_id),
                                            );
                                        }

                                        // Add tool response message.
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(format!(
                                                "Image generated successfully (id: {}).",
                                                image_id
                                            )),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                        });

                                        ctx.request_repaint();
                                    }
                                    Err(e) => {
                                        log_error(&format!(
                                            "Image generation failed: {e}"
                                        ));
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(format!(
                                                "Image generation failed: {e}"
                                            )),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                        });
                                        any_error = true;
                                    }
                                }
                            }

                            // Clear generating indicator.
                            {
                                let mut app_state = state.lock().unwrap();
                                app_state.generating_image = false;
                                app_state.assistant_buffer.clear();
                            }
                            ctx.request_repaint();

                            if any_error {
                                // If all tool calls failed, break to avoid infinite loop.
                                let mut app_state = state.lock().unwrap();
                                app_state.streaming = false;
                                app_state.generating_image = false;
                                break;
                            }
                            // Otherwise, continue the loop to let the model respond
                            // to tool results.
                            continue;
                        }
                        StreamResult::Error(e) => {
                            log_error(&format!("Stream error: {e}"));
                            let mut app_state = state.lock().unwrap();
                            app_state.streaming = false;
                            app_state.generating_image = false;
                            app_state.assistant_buffer.clear();
                            app_state.error = Some(format!("Request failed: {e}"));
                            break;
                        }
                    }
                }
            });

            log_info("Worker thread exiting");
        });
    }

    fn persist_settings(&self, api_key: String) {
        let mut state = self.state.lock().unwrap();
        state.config.api_key = api_key;
        state.config.save();
    }

    fn stop_streaming(&self) {
        let buffer;
        {
            let mut state = self.state.lock().unwrap();
            buffer = state.assistant_buffer.clone();
            if state.streaming && !buffer.is_empty() {
                state.messages.push(ChatMessage::text(Role::Assistant, &buffer));
            }
            state.assistant_buffer.clear();
            state.streaming = false;
            state.generating_image = false;
        }
        drop(buffer);
    }
}

impl eframe::App for AgnesApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Register any pending images as egui textures on the UI thread.
        let pending = {
            let mut state = self.state.lock().unwrap();
            std::mem::take(&mut state.pending_images)
        };
        if !pending.is_empty() {
            for (id, png_bytes) in pending {
                if let Ok(img) = image::load_from_memory(&png_bytes) {
                    let rgba = img.into_rgba8();
                    let size = [rgba.width() as usize, rgba.height() as usize];
                    let color_image =
                        egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
                    let handle = ctx.load_texture(&id, color_image, egui::TextureOptions::default());
                    self.state.lock().unwrap().textures.insert(id, handle);
                }
            }
            ctx.request_repaint();
        }

        // Top bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Agnes AI Chat");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙").clicked() {
                        let mut st = self.state.lock().unwrap();
                        st.reset_settings();
                        st.show_settings = true;
                        ctx.request_repaint();
                    }
                });
            });
        });

        // Bottom input bar — fixed at bottom, centered, never pushed by messages
        egui::TopBottomPanel::bottom("input_bar").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.vertical_centered(|ui| {
                ui.horizontal(|ui| {
                    let mut state = self.state.lock().unwrap();

                    // Error message
                    if let Some(ref err) = state.error {
                        ui.colored_label(Color32::RED, err);
                    }

                    // Input field
                    let response = ui.add(
                        egui::TextEdit::singleline(&mut state.input)
                            .desired_width(400.0)
                            .hint_text("Type a message..."),
                    );
                    let enter_pressed =
                        response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));

                    // Send / Stop button
                    if state.streaming {
                        if ui.button("Stop").clicked() {
                            drop(state);
                            self.stop_streaming();
                            return;
                        }
                    } else if ui.button("Send").clicked() || enter_pressed {
                        drop(state);
                        self.send_message(ctx.clone());
                        return;
                    }
                });
            });
            ui.add_space(6.0);
        });

        // Main area: messages only (takes all remaining space)
        egui::CentralPanel::default().show(ctx, |ui| {
            // Read state for rendering
            let state = self.state.lock().unwrap();

            // Message list (scrollable)
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    self.render_messages(ui, &state);
                });
        });

        // Settings modal
        {
            let show_settings;
            {
                let state = self.state.lock().unwrap();
                show_settings = state.show_settings;
            }

            if show_settings {
                let screen_rect = ctx.input(|i| i.viewport().inner_rect).unwrap();

                egui::Window::new("Settings")
                    .collapsible(false)
                    .resizable(false)
                    .fixed_pos(egui::pos2(
                        (screen_rect.left() + screen_rect.right() - 400.0) / 2.0,
                        (screen_rect.top() + screen_rect.bottom() - 200.0) / 2.0,
                    ))
                    .show(ctx, |ui| {
                        ui.set_width(360.0);
                        ui.heading("API Settings");
                        ui.separator();

                        ui.label("API Key:");
                        ui.add(
                            egui::TextEdit::singleline(&mut self.state.lock().unwrap().settings_api_key)
                                .password(true)
                                .desired_width(f32::INFINITY),
                        );

                        ui.separator();
                        ui.horizontal(|ui| {
                            if ui.button("Save").clicked() {
                                let api_key = self.state.lock().unwrap().settings_api_key.clone();
                                self.persist_settings(api_key);
                            }
                            if ui.button("Cancel").clicked() {
                                self.state.lock().unwrap().show_settings = false;
                            }
                        });
                    });
            }
        }

        // Request repaint during streaming or image generation for smooth updates
        {
            let state = self.state.lock().unwrap();
            if state.streaming || state.generating_image {
                ctx.request_repaint();
            }
        }
    }
}

impl AgnesApp {
    fn render_messages(&self, ui: &mut Ui, state: &AppState) {
        // Render completed messages (skip System role messages).
        for msg in &state.messages {
            if msg.role == Role::System {
                continue;
            }
            self.render_message(ui, msg, state);
            ui.separator();
        }

        // Render streaming assistant message
        if state.streaming && !state.assistant_buffer.is_empty() {
            let msg = ChatMessage::text(Role::Assistant, &state.assistant_buffer);
            self.render_message(ui, &msg, state);
        }

        // Scroll to bottom during streaming
        if state.streaming {
            ui.scroll_to_cursor(None);
        }
    }

    /// Render a single line of RichText segments, each potentially with different styling.
    fn render_line(ui: &mut Ui, segments: &[RichText]) {
        ui.horizontal_wrapped(|ui| {
            for seg in segments {
                ui.add(egui::Label::new(egui::WidgetText::from(Arc::new(seg.clone()))).selectable(true));
            }
        });
    }

    fn render_message(&self, ui: &mut Ui, msg: &ChatMessage, state: &AppState) {
        let is_user = msg.role == Role::User;

        ui.horizontal(|ui| {
            // Agent / indicator
            let avatar = if is_user { "👤" } else { "🤖" };
            ui.label(avatar);
        });

        match &msg.content {
            MessageContent::Text(text) => {
                // Render text with markdown styling.
                let rich_texts = markdown::render_markdown(text);
                let mut line_segments: Vec<RichText> = Vec::new();
                for rt in rich_texts {
                    if rt.text() == "\n" {
                        // Flush the current line.
                        if !line_segments.is_empty() {
                            Self::render_line(ui, &line_segments);
                            line_segments.clear();
                        }
                        // Force a line break.
                        ui.allocate_space(egui::vec2(0.0, 0.0));
                    } else {
                        line_segments.push(rt);
                    }
                }
                // Flush remaining line.
                if !line_segments.is_empty() {
                    Self::render_line(ui, &line_segments);
                }
            }
            MessageContent::Image { id } => {
                // Render the generated image.
                if let Some(handle) = state.textures.get(id) {
                    // Scale image to fit within the available width.
                    let avail_width = ui.available_width();
                    let display_size = image_fit_size(handle, avail_width);

                    let tex = SizedTexture::new(handle.id(), display_size);
                    // Allocate a clickable area so right-clicks are detected
                    // (ui.image() alone only has hover Sense by default).
                    let (rect, resp) =
                        ui.allocate_exact_size(display_size, egui::Sense::click());
                    // Render the image inside the allocated rect.
                    egui::Image::new(tex)
                        .paint_at(ui, rect);
                    // Right-click context menu.
                    resp.context_menu(|ui| {
                        if ui.button("📋 复制图片").clicked() {
                            if let Some(bytes) = state.image_bytes.get(id) {
                                match image::load_from_memory(bytes) {
                                    Ok(img) => {
                                        let rgba = img.into_rgba8();
                                        copy_image_to_clipboard(&rgba);
                                    }
                                    Err(e) => {
                                        log_error(&format!(
                                            "Failed to decode image for clipboard: {e}"
                                        ));
                                    }
                                }
                            }
                            ui.close();
                        }
                    });
                    resp.on_hover_text("右键点击复制图片");
                } else {
                    ui.label("🖼️ 图片加载中...");
                }
            }
        }

        ui.allocate_space(egui::vec2(0.0, 8.0));
    }
}

// ─── Entry point ───────────────────────────────────────────────────────

/// Load a Chinese-supporting font so CJK characters render correctly.
fn chinese_font_data() -> Option<Vec<u8>> {
    let paths: &[&str] = &[
        // Windows
        "C:/Windows/Fonts/msyh.ttc",   // Microsoft YaHei
        "C:/Windows/Fonts/simhei.ttf", // Black
        "C:/Windows/Fonts/simsun.ttc", // SimSun
        // macOS
        "/System/Library/Fonts/Supplemental/PingFang.ttc",
        // Linux
        "/usr/share/fonts/truetype/wqy/wqy-zenhei.ttc",
        "/usr/share/fonts/wqy-zenhei/wqy-zenhei.ttc",
    ];
    for path in paths {
        if let Ok(data) = std::fs::read(path) {
            return Some(data);
        }
    }
    // Allow overriding via env var
    if let Ok(env_path) = std::env::var("AGNES_CHINESE_FONT") {
        if let Ok(data) = std::fs::read(&env_path) {
            return Some(data);
        }
    }
    None
}

fn main() -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder {
            title: Some("Agnes AI Chat".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    eframe::run_native(
        "Agnes AI Chat",
        options,
        Box::new(|cc| {
            // Register emoji + Chinese fonts before creating the app.
            // Order matters: emoji first (for avatars), then Chinese (for CJK),
            // then the original families (Latin fallback).
            let mut fonts = egui::FontDefinitions::default();

            // Load emoji font: Windows Segoe UI Emoji is "seguiemj.ttf".
            let emoji_paths: &[&str] = &[
                "C:/Windows/Fonts/seguiemj.ttf", // Segoe UI Emoji (Windows)
                "/System/Library/Fonts/Apple Color Emoji.ttc", // macOS
                "/usr/share/fonts/truetype/noto/NotoColorEmoji.ttf", // Linux
            ];
            let mut emoji_loaded = false;
            for path in emoji_paths {
                if let Ok(data) = std::fs::read(path) {
                    fonts.font_data.insert(
                        "emoji".into(),
                        Arc::new(egui::FontData::from_owned(data)),
                    );
                    emoji_loaded = true;
                    break;
                }
            }
            if !emoji_loaded {
                // If no emoji font found, just ensure "emoji" key exists as empty
                fonts.font_data.insert(
                    "emoji".into(),
                    Arc::new(egui::FontData::from_owned(Vec::new())),
                );
            }

            // Load Chinese font for CJK characters.
            if let Some(font_data) = chinese_font_data() {
                fonts.font_data.insert(
                    "chinese".into(),
                    Arc::new(egui::FontData::from_owned(font_data)),
                );
            }

            // Build Proportional family: emoji → chinese → original
            let mut proportional = fonts
                .families
                .get(&egui::FontFamily::Proportional)
                .cloned()
                .unwrap_or_default();
            proportional.retain(|n| n != "emoji" && n != "chinese");
            proportional.insert(0, "emoji".to_string());
            proportional.insert(1, "chinese".to_string());
            fonts.families.insert(egui::FontFamily::Proportional, proportional);

            // Also add chinese font to Monospace family so code blocks render CJK.
            if fonts.families.contains_key(&egui::FontFamily::Monospace) {
                let mut monospace = fonts
                    .families
                    .get(&egui::FontFamily::Monospace)
                    .cloned()
                    .unwrap_or_default();
                monospace.retain(|n| n != "chinese");
                monospace.insert(0, "chinese".to_string());
                fonts.families.insert(egui::FontFamily::Monospace, monospace);
            }

            cc.egui_ctx.set_fonts(fonts);

            Ok(Box::new(AgnesApp::new()))
        }),
    )
}
