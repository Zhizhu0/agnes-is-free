mod ai_client;
mod config;
mod markdown;

use ai_client::ChatMessage;
use config::Config;
use egui::{Color32, Ui};
use std::sync::{Arc, Mutex};

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
        }
    }

    fn reset_settings(&mut self) {
        self.settings_api_key = self.config.api_key.clone();
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

    fn send_message(&self) {
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
        state.messages.push(ChatMessage {
            role: ai_client::Role::User,
            content: input,
        });
        state.input.clear();
        state.streaming = true;
        state.assistant_buffer.clear();
        state.error = None;

        let config = state.config.clone();
        let messages = state.messages.clone();
        let state = self.state.clone();

        // Spawn async task on a new thread with tokio runtime
        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let full_content = ai_client::AiClient::new()
                    .chat_stream_collect(&config.base_url, &config.api_key, messages)
                    .await
                    .unwrap_or_default();

                let mut app_state = state.lock().unwrap();
                app_state.assistant_buffer = full_content.clone();
                app_state.streaming = false;

                // Save assistant message to history
                if !full_content.is_empty() {
                    app_state.messages.push(ChatMessage {
                        role: ai_client::Role::Assistant,
                        content: full_content,
                    });
                }
            });
        });
    }

    fn save_settings(&self, api_key: String) {
        let mut state = self.state.lock().unwrap();
        state.config.api_key = api_key;
        state.config.save();
        state.show_settings = false;
    }

    fn cancel_settings(&self) {
        let mut state = self.state.lock().unwrap();
        state.show_settings = false;
    }

    fn stop_streaming(&self) {
        let buffer;
        {
            let mut state = self.state.lock().unwrap();
            buffer = state.assistant_buffer.clone();
            if state.streaming && !buffer.is_empty() {
                state.messages.push(ChatMessage {
                    role: ai_client::Role::Assistant,
                    content: buffer.clone(),
                });
            }
            state.assistant_buffer.clear();
            state.streaming = false;
        }
        drop(buffer);
    }
}

impl eframe::App for AgnesApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Top bar
        egui::TopBottomPanel::top("top_bar").show(ctx, |ui| {
            ui.horizontal(|ui| {
                ui.heading("Agnes AI Chat");
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("⚙️").clicked() {
                        let mut st = self.state.lock().unwrap();
                        st.reset_settings();
                        st.show_settings = true;
                        ctx.request_repaint();
                    }
                });
            });
        });

        // Main area: messages + input
        egui::CentralPanel::default().show(ctx, |ui| {
            // Read state for rendering
            let mut state = self.state.lock().unwrap();

            // Message list (scrollable)
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .max_height(ui.available_height() - 80.0)
                .show(ui, |ui| {
                    self.render_messages(ui, &state);
                });

            // Bottom bar: error + input
            ui.horizontal(|ui| {
                // Error message
                if let Some(ref err) = state.error {
                    ui.colored_label(Color32::RED, err);
                }

                // Input row
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    ui.text_edit_singleline(&mut state.input);
                    if state.streaming {
                        if ui.button("Stop").clicked() {
                            drop(state);
                            self.stop_streaming();
                            return;
                        }
                    } else if ui.button("Send").clicked() {
                        drop(state);
                        self.send_message();
                        return;
                    }
                });
            });
        });

        // Settings modal
        {
            let state = self.state.lock().unwrap();
            if state.show_settings {
                let api_key = state.settings_api_key.clone();
                let screen_rect = ctx.input(|i| i.viewport().inner_rect).unwrap();
                let show_settings = state.show_settings;
                drop(state);

                if show_settings {
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
                                egui::TextEdit::singleline(&mut api_key.clone())
                                    .password(true)
                                    .desired_width(f32::INFINITY),
                            );

                            ui.separator();
                            ui.horizontal(|ui| {
                                if ui.button("Save").clicked() {
                                    self.save_settings(api_key);
                                }
                                if ui.button("Cancel").clicked() {
                                    self.cancel_settings();
                                }
                            });
                        });
                }
            }
        }

        // Request repaint during streaming for smooth updates
        {
            let state = self.state.lock().unwrap();
            if state.streaming {
                ctx.request_repaint();
            }
        }
    }
}

impl AgnesApp {
    fn render_messages(&self, ui: &mut Ui, state: &AppState) {
        // Render completed messages
        for msg in &state.messages {
            self.render_message(ui, msg);
            ui.separator();
        }

        // Render streaming assistant message
        if state.streaming && !state.assistant_buffer.is_empty() {
            let msg = ChatMessage {
                role: ai_client::Role::Assistant,
                content: state.assistant_buffer.clone(),
            };
            self.render_message(ui, &msg);
        }

        // Scroll to bottom during streaming
        if state.streaming {
            ui.scroll_to_cursor(None);
        }
    }

    fn render_message(&self, ui: &mut Ui, msg: &ChatMessage) {
        let is_user = matches!(msg.role, ai_client::Role::User);

        ui.horizontal(|ui| {
            // Avatar / indicator
            let avatar = if is_user { "👤" } else { "🤖" };
            ui.label(avatar);

            // Message bubble
            ui.scope(|ui| {
                ui.set_max_width(ui.available_width());

                // Render markdown
                let rich_texts = markdown::render_markdown(&msg.content);

                // Use a horizontal layout for each segment
                for rt in rich_texts {
                    ui.add(egui::Label::new(rt).wrap());
                }
            });
        });

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
            // Register Chinese font before creating the app
            if let Some(font_data) = chinese_font_data() {
                let mut fonts = egui::FontDefinitions::default();
                fonts.font_data.insert(
                    "chinese".into(),
                    Arc::new(egui::FontData::from_owned(font_data)),
                );
                fonts.families.insert(
                    egui::FontFamily::Proportional,
                    fonts
                        .families
                        .get(&egui::FontFamily::Proportional)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .chain(std::iter::once("chinese".to_string()))
                        .collect(),
                );
                cc.egui_ctx.set_fonts(fonts);
            }

            Ok(Box::new(AgnesApp::new()))
        }),
    )
}
