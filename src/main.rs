#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(unsafe_op_in_unsafe_fn)]

mod ai_client;
mod config;
mod image_gen;
mod input_box;
mod markdown;
mod video_gen;
mod video_player;
mod video_task;

use ai_client::{
    log_error, log_info, log_warn,
    ChatError, ChatMessage, MessageContent, Role, StreamResult,
};
use config::Config;
use egui::load::SizedTexture;
use egui::{Color32, RichText, Ui};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use video_player::VideoPlayerState;
use video_task::VideoStatus;

/// Compute display size for an image: scale to fit within max_width
/// while preserving aspect ratio (max 320px height to avoid giant images).
fn image_fit_size(handle: &egui::TextureHandle, max_width: f32) -> egui::Vec2 {
    let orig = handle.size_vec2();
    let max_height = 320.0;
    let scale = (max_width / orig.x).min(max_height / orig.y).min(1.0);
    egui::vec2(orig.x * scale, orig.y * scale)
}

/// Who created a given resource ID.
#[derive(Debug, Clone)]
enum ResourceSource {
    UserUpload,
    Generated,
}

/// Metadata about how an AI-generated image was created.
#[derive(Debug, Clone)]
struct ImageGenInfo {
    /// The prompt used to generate the image.
    prompt: String,
    /// Resource IDs referenced during generation, e.g. ["R1", "R2"].
    reference_resource_ids: Vec<String>,
}

/// Parameters used for a video generation task.
#[derive(Debug, Clone)]
struct VideoGenInfo {
    /// The prompt describing the video.
    prompt: String,
    /// Optional resource IDs used as input images (image-to-video).
    reference_resource_ids: Vec<String>,
    /// Requested width.
    width: u32,
    /// Requested height.
    height: u32,
    /// Number of frames.
    num_frames: u32,
    /// Frame rate.
    frame_rate: u32,
    /// Optional negative prompt.
    negative_prompt: Option<String>,
    /// Optional seed.
    seed: Option<u64>,
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
    /// Client-side: texture IDs of images pasted/uploaded but not yet sent.
    uploaded_images: Vec<String>,
    /// Source (who created it) of each resource ID.
    resource_source: HashMap<String, ResourceSource>,
    /// For AI-generated images: the prompt + reference resources used + dimensions.
    image_gen_info: HashMap<String, ImageGenInfo>,
    /// Dimensions (width, height) of every stored image, keyed by resource ID.
    image_dimensions: HashMap<String, (u32, u32)>,
    /// Sequential resource counter. Next allocated resource gets format "R{n}".
    next_resource_id: u64,
    /// Ordered list of resource IDs the AI is currently allowed to "see".
    /// - User-uploaded images added on send (auto-visible)
    /// - AI can add more via view_resource tool
    /// - Cleared when streaming ends
    ai_visible_resources: Vec<String>,
    /// Cached image descriptions (resource_id -> Chinese text), used when an image
    /// is not in ai_visible_resources: the description gives the AI a rough idea
    /// of what the image contains without needing to call view_resource.
    image_descriptions: HashMap<String, String>,
    /// Status of pending video generation tasks, keyed by resource ID (e.g. "R3").
    /// The UI renders each video message differently based on its status.
    video_status: HashMap<String, VideoStatus>,
    /// Parameters used for each video generation task, keyed by resource ID.
    /// Used for annotation text and for the AI to understand what was requested.
    video_gen_info: HashMap<String, VideoGenInfo>,
    /// Active inline video players, keyed by resource ID.
    /// Created when user clicks ▶ 播放 on a completed video; removed when
    /// the player is stopped or the message falls out of view.
    video_players: HashMap<String, VideoPlayerState>,
    /// Resources whose video players are queued to start on the next update tick.
    /// Populated by render (immutable borrow), drained by update (mutable borrow).
    pending_video_starts: Vec<String>,
    /// Resource IDs queued for pause/resume toggle on the next update tick.
    pending_video_toggle_pause: Vec<String>,
    /// Resource IDs queued for stop on the next update tick.
    pending_video_stops: Vec<String>,
    /// Seek requests queued on the next update tick: (resource_id, target).
    pending_video_seeks: Vec<(String, std::time::Duration)>,
    /// Pending first-frame thumbnails extracted from completed videos: (resource_id, png_bytes).
    /// Populated by the poll worker thread after download, drained in update() to register textures.
    pending_video_thumbnails: Vec<(String, Vec<u8>)>,
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
            uploaded_images: Vec::new(),
            resource_source: HashMap::new(),
            image_gen_info: HashMap::new(),
            image_dimensions: HashMap::new(),
            next_resource_id: 0,
            ai_visible_resources: Vec::new(),
            image_descriptions: HashMap::new(),
            video_status: HashMap::new(),
            video_gen_info: HashMap::new(),
            video_players: HashMap::new(),
            pending_video_starts: Vec::new(),
            pending_video_toggle_pause: Vec::new(),
            pending_video_stops: Vec::new(),
            pending_video_seeks: Vec::new(),
            pending_video_thumbnails: Vec::new(),
        }
    }

    fn reset_settings(&mut self) {
        self.settings_api_key = self.config.api_key.clone();
    }
}

/// Generate a sequential resource ID ("R1", "R2", ...) from AppState.
fn gen_resource_id(state: &mut AppState) -> String {
    let n = state.next_resource_id;
    state.next_resource_id += 1;
    format!("R{}", n + 1)
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

/// If the clipboard contains an image, convert it to PNG bytes.
/// Used when user pastes an image into the chat input.
fn try_get_clipboard_image_png() -> Option<(Vec<u8>, u32, u32)> {
    use arboard::ImageData;

    // Open clipboard — on Windows this can fail if another process
    // holds the clipboard open.
    let mut cb = arboard::Clipboard::new().map_err(|e| {
        log_info(&format!("[paste] Clipboard::new() failed: {e}"));
        e
    }).ok()?;

    // Try get_image() first.
    // On Windows, screenshots may be in a format arboard can't convert.
    let img: ImageData = match cb.get_image() {
        Ok(img) => img,
        Err(e) => {
            log_info(&format!("[paste] get_image() failed: {e}, trying raw Win32 clipboard access..."));
            // On Windows, try to get image data via raw clipboard formats.
            // arboard's get_image() may fail for screenshots because Windows
            // stores them in CF_DIB format which arboard doesn't handle well.
            #[cfg(windows)]
            {
                use std::sync::OnceLock;

                static CB_LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
                let _guard = CB_LOCK.get_or_init(|| std::sync::Mutex::new(())).lock().unwrap();

                if unsafe { winapi::um::winuser::OpenClipboard(std::ptr::null_mut()) } != 0 {
                    // CF_DIBV5 (BITMAPV5HEADER, 108 bytes) — Windows screenshot tools use this
                    let dibv5_handle = unsafe { winapi::um::winuser::GetClipboardData(9) }; // CF_DIBV5 = 9
                    if !dibv5_handle.is_null() {
                        let lock = unsafe { winapi::um::winbase::GlobalLock(dibv5_handle) };
                        if !lock.is_null() {
                            unsafe {
                                let ptr = lock as *const u8;
                                let width_i32 = i32::from_ne_bytes([
                                    *ptr.add(4), *ptr.add(5), *ptr.add(6), *ptr.add(7),
                                ]);
                                let height_i32 = i32::from_ne_bytes([
                                    *ptr.add(8), *ptr.add(9), *ptr.add(10), *ptr.add(11),
                                ]);
                                let bit_count = u16::from_ne_bytes([
                                    *ptr.add(14), *ptr.add(15),
                                ]);
                                let compression = u32::from_ne_bytes([
                                    *ptr.add(16), *ptr.add(17), *ptr.add(18), *ptr.add(19),
                                ]);
                                let w = width_i32.max(0) as u32;
                                let h = height_i32.unsigned_abs();
                                let bytes_per_pixel = ((bit_count as usize) + 7) / 8;
                                let row_bytes = ((w as usize) * bytes_per_pixel + 3) & !3; // 4-byte aligned
                                let total_rows = h as usize;
                                let is_top_down = height_i32 < 0;

                                log_info(&format!(
                                    "[paste] CF_DIBV5: {}x{}, {}bpp, compression={}, row_bytes={}, top_down={}",
                                    w, h, bit_count, compression, row_bytes, is_top_down
                                ));

                                // Extract pixels row by row, converting to clean RGBA
                                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                                for y in 0..total_rows {
                                    let src_row = if is_top_down {
                                        y
                                    } else {
                                        total_rows - 1 - y
                                    };
                                    let src_base = src_row * row_bytes;
                                    for x in 0..w {
                                        let px_offset = (x as usize) * bytes_per_pixel;
                                        let pixel_src = src_base + px_offset;
                                        if bit_count == 32 {
                                            // DIB 32bpp pixel layout: [B, G, R, A] per pixel
                                            rgba.push(*ptr.add(pixel_src + 2)); // R
                                            rgba.push(*ptr.add(pixel_src + 1)); // G
                                            rgba.push(*ptr.add(pixel_src));     // B
                                            rgba.push(*ptr.add(pixel_src + 3)); // A
                                        } else {
                                            // Fallback: just copy raw bytes
                                            for b in 0..bytes_per_pixel {
                                                rgba.push(*ptr.add(pixel_src + b));
                                            }
                                            // Pad to 4 bytes if needed
                                            while rgba.len() % 4 != 0 && rgba.len() < (w * h * 4) as usize {
                                                rgba.push(0);
                                            }
                                        }
                                    }
                                }

                                winapi::um::winbase::GlobalUnlock(dibv5_handle);

                                let png_result = {
                                    let img = image::RgbaImage::from_raw(w, h, rgba)?;
                                    let mut png = Vec::new();
                                    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
                                    log_info(&format!("[paste] PNG encoded via CF_DIBV5: {} bytes", png.len()));
                                    Some((png, w, h))
                                };
                                winapi::um::winuser::CloseClipboard();
                                return png_result;
                            }
                        }
                    }

                    // Fallback: CF_DIB (BITMAPINFOHEADER, 40 bytes)
                    let dib_handle = unsafe { winapi::um::winuser::GetClipboardData(8) }; // CF_DIB = 8
                    if !dib_handle.is_null() {
                        let lock = unsafe { winapi::um::winbase::GlobalLock(dib_handle) };
                        if !lock.is_null() {
                            unsafe {
                                let ptr = lock as *const u8;
                                let header_size = u32::from_ne_bytes([
                                    *ptr.add(0), *ptr.add(1), *ptr.add(2), *ptr.add(3),
                                ]) as usize;
                                let width_i32 = i32::from_ne_bytes([
                                    *ptr.add(4), *ptr.add(5), *ptr.add(6), *ptr.add(7),
                                ]);
                                let height_i32 = i32::from_ne_bytes([
                                    *ptr.add(8), *ptr.add(9), *ptr.add(10), *ptr.add(11),
                                ]);
                                let bit_count = u16::from_ne_bytes([
                                    *ptr.add(14), *ptr.add(15),
                                ]);
                                let w = width_i32.max(0) as u32;
                                let h = height_i32.unsigned_abs();
                                let bytes_per_pixel = ((bit_count as usize) + 7) / 8;
                                let palette_entries = if bit_count < 16 { (1usize << bit_count) * 4 } else { 0 };
                                let bits_offset = header_size + palette_entries;
                                let row_bytes = ((w as usize) * bytes_per_pixel + 3) & !3;
                                let total_rows = h as usize;
                                let is_top_down = height_i32 < 0;

                                log_info(&format!(
                                    "[paste] CF_DIB: {}x{}, {}bpp, header_size={}, palette={}, row_bytes={}, top_down={}",
                                    w, h, bit_count, header_size, palette_entries, row_bytes, is_top_down
                                ));

                                let mut rgba = Vec::with_capacity((w * h * 4) as usize);
                                for y in 0..total_rows {
                                    let src_row = if is_top_down {
                                        y
                                    } else {
                                        total_rows - 1 - y
                                    };
                                    let src_base = src_row * row_bytes + bits_offset;
                                    for x in 0..w {
                                        let px_offset = (x as usize) * bytes_per_pixel;
                                        let pixel_src = src_base + px_offset;
                                        if bit_count == 32 {
                                            // DIB 32bpp pixel layout: [B, G, R, A] per pixel
                                            rgba.push(*ptr.add(pixel_src + 2)); // R
                                            rgba.push(*ptr.add(pixel_src + 1)); // G
                                            rgba.push(*ptr.add(pixel_src));     // B
                                            rgba.push(*ptr.add(pixel_src + 3)); // A
                                        } else {
                                            for b in 0..bytes_per_pixel {
                                                rgba.push(*ptr.add(pixel_src + b));
                                            }
                                            while rgba.len() % 4 != 0 && rgba.len() < (w * h * 4) as usize {
                                                rgba.push(0);
                                            }
                                        }
                                    }
                                }

                                winapi::um::winbase::GlobalUnlock(dib_handle);

                                let png_result = {
                                    let img = image::RgbaImage::from_raw(w, h, rgba)?;
                                    let mut png = Vec::new();
                                    img.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
                                    log_info(&format!("[paste] PNG encoded via CF_DIB: {} bytes", png.len()));
                                    Some((png, w, h))
                                };
                                winapi::um::winuser::CloseClipboard();
                                return png_result;
                            }
                        }
                    }

                    unsafe { winapi::um::winuser::CloseClipboard() };
                    log_info("[paste] Could not read CF_DIB/CF_DIBV5 from clipboard");
                }
            }

            return Err(e).ok()?;
        }
    };

    let w = img.width as u32;
    let h = img.height as u32;
    let bytes = img.bytes;

    log_info(&format!(
        "[paste] Clipboard image: {}x{}, {} bytes raw (expected {} = {}x{}x4)",
        w, h, bytes.len(),
        w as usize * h as usize * 4,
        w, h
    ));

    // DIAGNOSTIC: save raw PNG for visual inspection
    let expected = w as usize * h as usize * 4;
    if bytes.len() != expected {
        log_error(&format!(
            "[paste] Byte count mismatch: got {} != expected {}",
            bytes.len(), expected
        ));
        return None;
    }

    let bytes = bytes.as_ref();
    let rgba = image::RgbaImage::from_raw(w, h, bytes.to_vec())?;
    let mut png: Vec<u8> = Vec::new();
    rgba.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).ok()?;
    log_info(&format!("[paste] PNG encoded: {} bytes", png.len()));
    Some((png, w, h))
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

    /// Deep-clones conversation messages for API delivery.
    ///
    /// For messages that reference a resource (`ref_resource_id`):
    /// - **Visible** (in `ai_visible_resources`): deliver base64 image with a
    ///   short plain-Chinese label that mentions NO resource ID.  The model
    ///   sees the image directly and never feels the need to call view_resource.
    /// - **Invisible**: leave `image_urls: None`, replace content with an
    ///   annotation that mentions the resource ID so the AI can call view_resource.
    fn prepare_conversation_for_api(
        conversation: &[ChatMessage],
        image_bytes: &std::collections::HashMap<String, Vec<u8>>,
        ai_visible_resources: &[String],
        resource_source: &std::collections::HashMap<String, ResourceSource>,
        image_gen_info: &std::collections::HashMap<String, ImageGenInfo>,
        image_dimensions: &std::collections::HashMap<String, (u32, u32)>,
        image_descriptions: &std::collections::HashMap<String, String>,
        video_status: &std::collections::HashMap<String, VideoStatus>,
        video_gen_info: &std::collections::HashMap<String, VideoGenInfo>,
    ) -> Vec<ChatMessage> {
        conversation
            .iter()
            .map(|msg| {
                let mut msg = msg.clone();
                if let Some(ref res_id) = msg.ref_resource_id {
                    // Video resources get a completely different annotation
                    // — they are never "visible" to the AI as base64,
                    // and the AI should never call view_resource for them.
                    // Always show generation params so the AI understands context.
                    if let Some(vstatus) = video_status.get(res_id) {
                        let status_text = match vstatus {
                            VideoStatus::Pending => "正在生成中",
                            VideoStatus::Completed(_) => "已生成完毕",
                            VideoStatus::Failed(reason) => &format!("生成失败: {reason}"),
                        };
                        // Build params description.
                        let params_hint = if let Some(info) = video_gen_info.get(res_id) {
                            let mut parts = vec![format!("提示词: {}", info.prompt)];
                            if !info.reference_resource_ids.is_empty() {
                                parts.push(format!("参考资源: {}", info.reference_resource_ids.join(", ")));
                            }
                            parts.push(format!("{}x{}", info.width, info.height));
                            parts.push(format!("{}帧@{}fps", info.num_frames, info.frame_rate));
                            if let Some(ref np) = info.negative_prompt {
                                parts.push(format!("负面提示词: {np}"));
                            }
                            if let Some(s) = info.seed {
                                parts.push(format!("种子: {s}"));
                            }
                            format!("，参数: {}", parts.join("，"))
                        } else {
                            String::new()
                        };
                        // Add a strong directive telling the AI NOT to call any
                        // video-related tools for this resource — the video is
                        // already there (or its final status is known).
                        let tool_directive = match vstatus {
                            VideoStatus::Pending => {
                                "视频正在后台生成中，无需调用任何工具，只需正常回复文本即可"
                            }
                            VideoStatus::Completed(_) => {
                                "视频已生成完毕，无需再次调用 generate_video 工具，只需正常回复文本即可"
                            }
                            VideoStatus::Failed(_) => {
                                "视频生成已失败，如需重新生成请在用户再次要求时再调用 generate_video 工具"
                            }
                        };
                        msg.image_urls = None;
                        msg.content = MessageContent::Text(format!(
                            "[视频 {res_id}（{status_text}）{params_hint}。{tool_directive}]"
                        ));
                        return msg;
                    }

                    let source = resource_source.get(res_id);
                    let source_label = match source {
                        Some(ResourceSource::UserUpload) => "用户上传",
                        Some(ResourceSource::Generated) => "AI 生成",
                        None => "上传", // fallback for legacy/untracked resources
                    };
                    if ai_visible_resources.contains(res_id) {
                        // Resource is visible → deliver base64 image.
                        // Include the resource ID so the AI can pass it as
                        // reference_resource_ids to generate_image, but also
                        // tell the model the image is already visible so it
                        // won't redundantly call view_resource.
                        if let Some(bytes) = image_bytes.get(res_id) {
                            use base64::Engine;
                            let b64 =
                                base64::engine::general_purpose::STANDARD.encode(bytes);
                            msg.image_urls =
                                Some(vec![format!("data:image/png;base64,{b64}")]);
                            let n = res_id.trim_start_matches('R');
                            // For AI-generated images, add prompt + reference info.
                            let gen_hint = if let Some(info) = image_gen_info.get(res_id) {
                                let refs_hint = if info.reference_resource_ids.is_empty() {
                                    String::new()
                                } else {
                                    format!("，引用了以下资源：{}", info.reference_resource_ids.join("、"))
                                };
                                format!(
                                    "，使用的提示词为「{}」{}",
                                    info.prompt, refs_hint
                                )
                            } else {
                                String::new()
                            };
                            let action_hint = if matches!(source, Some(ResourceSource::Generated)) {
                                "它是你刚刚生成的，可直接引用"
                            } else {
                                "如需在生图时引用"
                            };
                            let dim_hint = image_dimensions
                                .get(res_id)
                                .map(|(w, h)| format!("（尺寸: {w}x{h}）"))
                                .unwrap_or_default();
                            msg.content = MessageContent::Text(
                                format!(
                                    "[这里有一张{source_label}的图片，已直接上传，资源 id 为 {res_id}{dim_hint}{gen_hint}。{action_hint}，reference_resource_ids 填 [{n}]；无需再调 view_resource]"
                                ),
                            );
                        }
                    } else {
                        // Resource is invisible → show annotation text prompting view_resource.
                        // If a description was pre-generated, include it so the AI
                        // can roughly understand the image without calling view_resource.
                        let n = res_id.trim_start_matches('R');
                        // For AI-generated images, include prompt + reference info so
                        // the model knows what it would see before calling view_resource.
                        let gen_hint = if let Some(info) = image_gen_info.get(res_id) {
                            let refs_hint = if info.reference_resource_ids.is_empty() {
                                String::new()
                            } else {
                                format!("，引用了以下资源：{}", info.reference_resource_ids.join("、"))
                            };
                            format!("，使用的提示词为「{}」{}", info.prompt, refs_hint)
                        } else {
                            String::new()
                        };
                        let dim_hint = image_dimensions
                            .get(res_id)
                            .map(|(w, h)| format!("（尺寸: {w}x{h}）"))
                            .unwrap_or_default();
                        let desc_hint = image_descriptions
                            .get(res_id)
                            .map(|d| format!("（内容描述：{d}）"))
                            .unwrap_or_default();
                        let annotation = if desc_hint.is_empty() {
                            format!(
                                "[{source_label}了 id 为 {res_id} 的资源{dim_hint}{gen_hint}。如需查看，请调用 view_resource({n})]"
                            )
                        } else {
                            format!(
                                "[{source_label}了 id 为 {res_id} 的资源{dim_hint}{gen_hint}{desc_hint}。如需查看原图，请调用 view_resource({n})]"
                            )
                        };
                        msg.image_urls = None;
                        msg.content = MessageContent::Text(annotation);
                    }
                }
                msg
            })
            .collect()
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

        // Add user messages: one per uploaded image, then one text-only message.
        const MAX_IMAGES: usize = 10;
        let mut uploaded = std::mem::take(&mut state.uploaded_images);
        // Hard cap at 10 images for sanity.
        if uploaded.len() > MAX_IMAGES {
            let extra: Vec<String> = uploaded.split_off(MAX_IMAGES);
            for id in &extra {
                state.image_bytes.remove(id);
                state.textures.remove(id);
            }
        }
        log_info(&format!(
            "Sending user message ({} images): {:?}",
            uploaded.len(),
            &input[..input.len().min(80)]
        ));
        // When user sends new images, reset the visible set and repopulate.
        // When user sends NO new images (text-only), keep the existing
        // visible set so the AI can still see previously-viewed images.
        if !uploaded.is_empty() {
            state.ai_visible_resources.clear();
        }

        for id in &uploaded {
            state.messages.push(ChatMessage {
                role: Role::User,
                // UI 渲染：content 为空，图片由 uploaded_image 字段渲染缩略图。
                // annotation text 只在 prepare_conversation_for_api 中生成，不会出现在 UI。
                content: MessageContent::Text(String::new()),
                tool_calls: None,
                tool_call_id: None,
                image_urls: None,
                uploaded_image: Some(id.clone()),
                ref_resource_id: Some(id.clone()),
            });
            // Auto-visible: user-uploaded images are immediately visible to AI
            state.ai_visible_resources.push(id.clone());
            state.resource_source.insert(id.clone(), ResourceSource::UserUpload);
        }
        state.messages.push(ChatMessage {
            role: Role::User,
            content: MessageContent::Text(input),
            tool_calls: None,
            tool_call_id: None,
            image_urls: None,
            uploaded_image: None,
            ref_resource_id: None,
        });
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
                    "You have access to three tools:\n\n\
                     1. view_resource — Call this with a resource number (integer) to view an image. \
                     When you see a message like '[用户上传了 id 为 R5 的资源。如需查看，请调用 view_resource(5)]', \
                     it means the image is NOT visible to you yet — call view_resource(5) to see it. \
                     Once viewed, the image stays visible in subsequent messages.\n\n\
                     2. generate_image — Two modes:\n\
                       • Text-to-image (brand-new image): write a full English description of what to create. Omit reference_resource_ids.\n\
                       • Image-editing (modify an existing image, e.g. 'remove text', 'change background'): pass the\n\
                         reference_resource_ids and write a CONCISE EDITING INSTRUCTION — only describe what to change,\n\
                         NOT a full scene rewrite. Example: prompt='Remove all text and logos from the image', reference_resource_ids=[5].\n\
                         The image model receives the reference image internally; your prompt should only describe the delta.\n\n\
                      3. generate_video — Generate a video from text, images, or keyframes. Current price: $0/second (free).\n\
                        • Text-to-video: write full prompt describing the video. Omit reference_resource_ids.\n\
                        • Image-to-video: pass one reference_resource_ids entry (e.g. [5]), prompt describes what should move.\n\
                        • Multi-image video: pass multiple reference_resource_ids (e.g. [5, 8]), prompt describes transitions between images.\n\
                        • Keyframe animation: pass multiple reference_resource_ids + is_keyframe=true, prompt describes cinematic transitions.\n\
                        Good prompt structure: [subject] + [action] + [scene] + [camera movement] + [lighting] + [style].\n\
                       You may optionally set width (default 1152), height (default 768), num_frames (default 121, must be 8n+1 ≤441),\n\
                       frame_rate (default 24), negative_prompt, and seed.\n\
                       Video generation is async — a placeholder is shown immediately and the video appears when ready.\n\
                       You may continue talking after calling generate_video — you do NOT need to wait for the video to complete.\n\n\
                     CRITICAL RULES:\
                     - If a message contains an image AND mentions '资源 id 为 RN' — the image IS already visible AND you may reference it via reference_resource_ids: [N]. DO NOT call view_resource for it.\
                     - Images in the user's CURRENT message are ALWAYS sent directly — NEVER call view_resource for them.\
                     - Only call view_resource when you see the '如需查看，请调用 view_resource(N)' annotation in a PREVIOUS message.\
                     - NEVER use generate_image when the user only wants to discuss or analyze an image — just respond with text.\
                      - NEVER use generate_video when the user only wants to discuss or analyze a video/image — just respond with text.\
                      - Only ONE video can be generated at a time. If a video is already being generated, you MUST wait for it to complete before requesting another. The tool will reject duplicate requests.\
                      - Resource IDs are integers only: R5 -> use 5; R12 -> use 12.\
                     - Text-to-image: full English description, no reference_resource_ids.\n\
                       Image-editing: CONCISE instruction (what to change only) + reference_resource_ids.\n\
                     - Generated images get the next resource ID (e.g. R6) and can be reused via reference_resource_ids.\n\
                     - When editing an image, you may optionally set `width` and `height` to control output size. \
                       If omitted, the output matches the reference image's dimensions. \
                       For text-to-image, default is 1024x768 if neither is specified. \
                       Preserve the reference image's aspect ratio when editing unless the user explicitly requests otherwise.",
                ),
            );
        }

        let config = state.config.clone();
        let messages = state.messages.clone();

        // Collect (resource_id, png_bytes) for parallel image description.
        // Only user-uploaded images need descriptions; AI-generated images
        // already have their gen_info (prompt/params) recorded.
        // Must be done BEFORE `state` is shadowed by `self.state.clone()` below.
        let to_describe: Vec<(String, Vec<u8>)> = uploaded
            .iter()
            .filter_map(|id| state.image_bytes.get(id).map(|b| (id.clone(), b.clone())))
            .collect();

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

                // Spawn parallel image description tasks (fire-and-forget).
                // Results are inserted into AppState.image_descriptions.
                // The main request does NOT wait for these — they're cached
                // for future turns when the image may not be visible.
                let describe_handles: Vec<tokio::task::JoinHandle<()>> = to_describe
                    .into_iter()
                    .map(|(res_id, png_bytes)| {
                        let base_url = base_url.clone();
                        let api_key = api_key.clone();
                        let app_state = state.clone();
                        tokio::spawn(async move {
                            match ai_client::describe_image(&base_url, &api_key, &png_bytes).await {
                                Ok(desc) => {
                                    log_info(&format!("describe_image {}: {}", res_id, desc));
                                    app_state
                                        .lock()
                                        .unwrap()
                                        .image_descriptions
                                        .insert(res_id, desc);
                                }
                                Err(e) => {
                                    log_warn(&format!("describe_image {} failed (all retries exhausted): {e}", res_id));
                                }
                            }
                        })
                    })
                    .collect();

                // Tool-use loop: continue until the model returns plain text
                // or an error occurs.
                loop {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(32);
                    let base_url2 = base_url.clone();
                    let api_key2 = api_key.clone();

                    // Snapshot visible set + inject base64 for visible resources.
                    let prepared_messages = {
                        let app_state = state.lock().unwrap();
                        let visible = app_state.ai_visible_resources.clone();
                        log_info(&format!(
                            "[prepare] ai_visible_resources = {:?}",
                            visible
                        ));
                        let msgs = AgnesApp::prepare_conversation_for_api(
                            &conversation,
                            &app_state.image_bytes,
                            &app_state.ai_visible_resources,
                            &app_state.resource_source,
                            &app_state.image_gen_info,
                            &app_state.image_dimensions,
                            &app_state.image_descriptions,
                            &app_state.video_status,
                            &app_state.video_gen_info,
                        );
                        // Log each message's content summary so we can see exactly
                        // what the model receives.
                        for (i, m) in msgs.iter().enumerate() {
                            let text_summary = match &m.content {
                                MessageContent::Text(s) => {
                                    // Use char-boundary-safe truncation (CJK = 3 bytes/char).
                                    let truncated: String =
                                        s.chars().take(80).collect();
                                    if s.chars().count() > 80 {
                                        format!("\"{truncated}...\"")
                                    } else {
                                        format!("\"{truncated}\"")
                                    }
                                }
                                MessageContent::Image { id } => {
                                    format!("Image{{{id}}}")
                                }
                                 MessageContent::Video { id } => {
                                    format!("Video{{{id}}}")
                                }
                                MessageContent::ToolResult { tool_name, .. } => {
                                    format!("ToolResult{{{tool_name}}}")
                                }
                            };
                            let has_images = m
                                .image_urls
                                .as_ref()
                                .map_or(false, |u| !u.is_empty());
                            log_info(&format!(
                                "[prepare] msg[{}]: role={}, content={}, has_image_urls={}",
                                i, m.role as i32, text_summary, has_images,
                            ));
                        }
                        msgs
                    };

                    let dispatch_handle =
                        tokio::task::spawn(async move {
                            ai_client::chat_stream_tools(
                                &base_url2,
                                &api_key2,
                                prepared_messages,
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
                        StreamResult::ToolCalls(ref tool_calls) => {
                            // Model wants to call one or more functions.
                            log_info(&format!(
                                "Model requested {} tool call(s):",
                                tool_calls.len()
                            ));
                            for tc in tool_calls {
                                log_info(&format!(
                                    "  -> tool={}, id={}, arguments={}",
                                    tc.name, tc.id, tc.arguments
                                ));
                            }

                            // Save any streaming text the model produced before
                            // deciding to call tools (e.g. "Let me think about
                            // this..." or "Here's what I see in the image...").
                            // This text is in assistant_buffer — preserve it
                            // so the conversation history shows the model's
                            // reasoning, not just the tool result.
                            let reasoning_text = {
                                let mut app_state = state.lock().unwrap();
                                if app_state.assistant_buffer.is_empty() {
                                    None
                                } else {
                                    let text = app_state.assistant_buffer.clone();
                                    app_state.assistant_buffer.clear();
                                    Some(text)
                                }
                            };
                            if let Some(text) = &reasoning_text {
                                // Push to both conversation (API) and state.messages (UI rendering).
                                conversation.push(ChatMessage::text(Role::Assistant, text));
                                state.lock().unwrap().messages.push(ChatMessage::text(Role::Assistant, text));
                            }

                            // Register the assistant message with tool_calls for
                            // protocol correctness.
                            tool_calls.iter().for_each(|tc| {
                                conversation.push(ChatMessage {
                                    role: Role::Assistant,
                                    content: MessageContent::Text(String::new()),
                                    tool_calls: Some(vec![tc.clone()]),
                                    tool_call_id: None,
                                    image_urls: None,
                                    uploaded_image: None,
                                    ref_resource_id: None,
                                });
                            });

                            // Execute each tool call.
                            for tc in tool_calls {
                                // --- view_resource: make a resource visible ---
                                if tc.name == "view_resource" {
                                    let raw_id: i64 = serde_json::from_str::<serde_json::Value>(&tc.arguments)
                                        .ok()
                                        .and_then(|v| v.get("id").and_then(|x| x.as_i64()))
                                        .unwrap_or(-1);
                                    if raw_id <= 0 {
                                        let err_msg = "Error: invalid resource ID".to_string();
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(err_msg.clone()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                            "view_resource", &tc.arguments, &err_msg,
                                        ));
                                        continue;
                                    }
                                    let res_id = format!("R{raw_id}");
                                    let exists = state.lock().unwrap().image_bytes.contains_key(&res_id);
                                    if exists {
                                        let mut app_state = state.lock().unwrap();
                                        if !app_state.ai_visible_resources.contains(&res_id) {
                                            app_state.ai_visible_resources.push(res_id.clone());
                                        }
                                        let ok_msg = format!("Resource R{raw_id} is now visible.");
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(ok_msg.clone()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        app_state.messages.push(ChatMessage::tool_result(
                                            "view_resource", &tc.arguments, &ok_msg,
                                        ));
                                    } else {
                                        let err_msg = format!("Error: resource R{raw_id} does not exist");
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(err_msg.clone()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                            "view_resource", &tc.arguments, &err_msg,
                                        ));
                                    }
                                    continue;
                                }

                                // --- generate_image ---
                                if tc.name == "generate_image" {
                                    // Parse prompt + reference_resource_ids + optional width/height.
                                    let (prompt, reference_ids, requested_w, requested_h) = match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                        Ok(v) => {
                                            let p = v.get("prompt").and_then(|p| p.as_str()).unwrap_or("").to_string();
                                            let refs: Vec<i64> = v.get("reference_resource_ids")
                                                .cloned()
                                                .and_then(|rv| serde_json::from_value(rv).ok())
                                                .unwrap_or_default();
                                            let w = v.get("width").and_then(|v| v.as_u64()).map(|v| v as u32);
                                            let h = v.get("height").and_then(|v| v.as_u64()).map(|v| v as u32);
                                            (p, refs, w, h)
                                        }
                                        Err(e) => {
                                            log_error(&format!("Failed to parse tool args: {e}"));
                                            conversation.push(ChatMessage {
                                                role: Role::Tool,
                                                content: MessageContent::Text(format!(
                                                    "Error parsing arguments: {e}"
                                                )),
                                                tool_calls: None,
                                                tool_call_id: Some(tc.id.clone()),
                                                image_urls: None,
                                                uploaded_image: None,
                                                ref_resource_id: None,
                                            });
                                            continue;
                                        }
                                    };

                                    if prompt.is_empty() {
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text("Error: empty prompt".into()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        continue;
                                    }

                                    // Collect reference image bytes from state.
                                    let reference_images: Vec<Vec<u8>> = {
                                        let mut app_state = state.lock().unwrap();
                                        let mut images = Vec::new();
                                        for rid in &reference_ids {
                                            let res_id = format!("R{rid}");
                                            if let Some(bytes) = app_state.image_bytes.get(&res_id) {
                                                images.push(bytes.clone());
                                                if !app_state.ai_visible_resources.contains(&res_id) {
                                                    app_state.ai_visible_resources.push(res_id);
                                                }
                                            }
                                        }
                                        images
                                    };

                                    // Determine output size: prefer AI's requested dimensions,
                                    // fall back to reference image dimensions, then 1024x768 default.
                                    let first_ref_dimensions: Option<(u32, u32)> = {
                                        let app_state = state.lock().unwrap();
                                        reference_ids.first().and_then(|rid| {
                                            let res_id = format!("R{rid}");
                                            app_state.image_dimensions.get(&res_id).copied()
                                        })
                                    };
                                    let (first_ref_width, first_ref_height) = match first_ref_dimensions {
                                        Some((w, h)) => (Some(w), Some(h)),
                                        None => (None, None),
                                    };
                                    let (out_w, out_h) = match (requested_w, requested_h) {
                                        (Some(w), Some(h)) => (w, h),
                                        (Some(w), None) => (w, first_ref_height.unwrap_or(768)),
                                        (None, Some(h)) => (first_ref_width.unwrap_or(1024), h),
                                        (None, None) => match first_ref_dimensions {
                                            Some((w, h)) => (w, h),
                                            None => (1024, 768),
                                        },
                                    };

                                    log_info(&format!(
                                        "Generating image with prompt: {:?} ({} reference image(s)), size={}x{}",
                                        &prompt[..prompt.len().min(80)],
                                        reference_images.len(),
                                        out_w, out_h
                                    ));

                                    // Show generating indicator.
                                    {
                                        let mut app_state = state.lock().unwrap();
                                        app_state.generating_image = true;
                                        app_state.assistant_buffer = "🎨 正在生成图片...".into();
                                    }
                                    ctx.request_repaint();

                                    // Call the image generation API with computed dimensions.
                                    let image_result = image_gen::ImageGenClient::new()
                                        .generate(&base_url, &api_key, &prompt, &reference_images, out_w, out_h)
                                        .await;

                                    match image_result {
                                        Ok(png_bytes) => {
                                            let image_id = {
                                                let mut app_state = state.lock().unwrap();
                                                gen_resource_id(&mut app_state)
                                            };
                                            // Decode once to get actual dimensions from the PNG.
                                            let (gen_w, gen_h) = match image::load_from_memory(&png_bytes) {
                                                Ok(img) => (img.width(), img.height()),
                                                Err(e) => {
                                                    log_warn(&format!(
                                                        "[agnes] Failed to decode generated image for dims: {e}, using requested {}x{}",
                                                        out_w, out_h
                                                    ));
                                                    (out_w, out_h)
                                                }
                                            };

                                            // If the model ignored our size request, force-rescale to target.
                                            let png_bytes = if gen_w != out_w || gen_h != out_h {
                                                log_warn(&format!(
                                                    "[agnes] Model returned {}x{} instead of requested {}x{}, rescaling to target",
                                                    gen_w, gen_h, out_w, out_h
                                                ));
                                                match image::load_from_memory(&png_bytes) {
                                                    Ok(img) => {
                                                        let resized = image::imageops::resize(
                                                            &img,
                                                            out_w,
                                                            out_h,
                                                            image::imageops::FilterType::Lanczos3,
                                                        );
                                                        let mut buf = Vec::new();
                                                        resized.write_to(
                                                            &mut std::io::Cursor::new(&mut buf),
                                                            image::ImageFormat::Png,
                                                        ).ok();
                                                        if buf.len() > png_bytes.len() / 10 {
                                                            // Use only if the resize produced a valid PNG (size should be similar or smaller than original)
                                                            buf
                                                        } else {
                                                            png_bytes
                                                        }
                                                    }
                                                    Err(_) => png_bytes,
                                                }
                                            } else {
                                                png_bytes
                                            };

                                            // Final dimensions after (optional) rescaling.
                                            let (final_w, final_h) = if gen_w != out_w || gen_h != out_h {
                                                (out_w, out_h)
                                            } else {
                                                (gen_w, gen_h)
                                            };

                                            log_info(&format!(
                                                "Image generated: {} ({} bytes), {}x{} (requested {}x{})",
                                                image_id,
                                                png_bytes.len(),
                                                final_w, final_h,
                                                out_w, out_h
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
                                                app_state.image_dimensions.insert(image_id.clone(), (final_w, final_h));
                                                app_state.messages.push(
                                                    ChatMessage::image(&image_id),
                                                );
                                                // Generated image is auto-visible (AI just created it).
                                                if !app_state.ai_visible_resources.contains(&image_id) {
                                                    app_state.ai_visible_resources.push(image_id.clone());
                                                }
                                                app_state.resource_source.insert(image_id.clone(), ResourceSource::Generated);
                                                // Record gen metadata (prompt + reference IDs + dimensions).
                                                let ref_res_ids: Vec<String> = reference_ids
                                                    .iter()
                                                    .map(|rid| format!("R{rid}"))
                                                    .collect();
                                                app_state.image_gen_info.insert(
                                                    image_id.clone(),
                                                    ImageGenInfo {
                                                        prompt: prompt.clone(),
                                                        reference_resource_ids: ref_res_ids,
                                                    },
                                                );
                                            }

                                            // Add tool response.  image_urls is None —
                                            // base64 is injected lazily by
                                            // prepare_conversation_for_api.
                                            // IMPORTANT: tell the model the task is done —
                                            // otherwise it often calls generate_image AGAIN
                                            // after we feed this response back into the loop.
                                            let ok_msg = format!("Image generated successfully (id: {image_id})");
                                            conversation.push(ChatMessage {
                                                role: Role::Tool,
                                                content: MessageContent::Text(format!(
                                                    "Image generated successfully (id: {}). Do NOT call generate_image again — the task is complete. Just reply with plain text describing the result (or NO_REPLY if you have nothing to add).",
                                                    image_id
                                                )),
                                                image_urls: None,
                                                ref_resource_id: Some(image_id.clone()),
                                                tool_calls: None,
                                                tool_call_id: Some(tc.id.clone()),
                                                uploaded_image: None,
                                            });

                                            // Show tool result in UI.
                                            state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                                "generate_image", &tc.arguments, &ok_msg,
                                            ));

                                            ctx.request_repaint();
                                        }
                                        Err(e) => {
                                            log_error(&format!(
                                                "Image generation failed: {e}"
                                            ));
                                             let err_msg = format!("Image generation failed: {e}");
                                             conversation.push(ChatMessage {
                                                role: Role::Tool,
                                                content: MessageContent::Text(err_msg.clone()),
                                                tool_calls: None,
                                                tool_call_id: Some(tc.id.clone()),
                                                image_urls: None,
                                                ref_resource_id: None,
                                                uploaded_image: None,
                                            });
                                             state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                                "generate_image", &tc.arguments, &err_msg,
                                            ));
                                        }
                                    }
                                    continue; // move to next tool call
                                }

                                // --- generate_video ---
                                if tc.name == "generate_video" {
                                    // Check if a video is already being generated
                                    let has_pending = {
                                        let app_state = state.lock().unwrap();
                                        app_state
                                            .video_status
                                            .values()
                                            .any(|s| matches!(s, VideoStatus::Pending))
                                    };
                                    if has_pending {
                                        let existing_id = {
                                            let app_state = state.lock().unwrap();
                                            app_state
                                                .video_status
                                                .iter()
                                                .find(|(_, s)| matches!(s, VideoStatus::Pending))
                                                .map(|(id, _)| id.clone())
                                                .unwrap_or_default()
                                        };
                                        let err_msg = format!(
                                            "A video is already being generated ({existing_id}). \
                                             Please wait for it to complete before requesting another video."
                                        );
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text(err_msg.clone()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                            "generate_video", &tc.arguments, &err_msg,
                                        ));
                                        continue;
                                    }

                                    let (prompt, reference_ids, is_keyframe, vid_w, vid_h, num_frames, frame_rate, negative_prompt, seed_val) =
                                        match serde_json::from_str::<serde_json::Value>(&tc.arguments) {
                                            Ok(v) => {
                                                let p = v.get("prompt").and_then(|p| p.as_str()).unwrap_or("").to_string();
                                                let refs: Vec<i64> = v.get("reference_resource_ids")
                                                    .cloned()
                                                    .and_then(|rv| serde_json::from_value(rv).ok())
                                                    .unwrap_or_default();
                                                let kf = v.get("is_keyframe").and_then(|v| v.as_bool()).unwrap_or(false);
                                                let w = v.get("width").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(1152);
                                                let h = v.get("height").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(768);
                                                let nf = v.get("num_frames").and_then(|v| v.as_u64()).map(|v| v as u32).unwrap_or(121);
                                                let fr = v.get("frame_rate").and_then(|v| v.as_f64()).map(|v| v as u32).unwrap_or(24);
                                                let np = v.get("negative_prompt").and_then(|v| v.as_str()).map(|s| s.to_string());
                                                let sv = v.get("seed").and_then(|v| v.as_u64());
                                                (p, refs, kf, w, h, nf, fr, np, sv)
                                            }
                                            Err(e) => {
                                                log_error(&format!("Failed to parse video tool args: {e}"));
                                                conversation.push(ChatMessage {
                                                    role: Role::Tool,
                                                    content: MessageContent::Text(format!(
                                                        "Error parsing arguments: {e}"
                                                    )),
                                                    tool_calls: None,
                                                    tool_call_id: Some(tc.id.clone()),
                                                    image_urls: None,
                                                    uploaded_image: None,
                                                    ref_resource_id: None,
                                                });
                                                continue;
                                            }
                                        };

                                    if prompt.is_empty() {
                                        conversation.push(ChatMessage {
                                            role: Role::Tool,
                                            content: MessageContent::Text("Error: empty prompt".into()),
                                            tool_calls: None,
                                            tool_call_id: Some(tc.id.clone()),
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: None,
                                        });
                                        continue;
                                    }

                                    // Prepare image data URIs for all reference images.
                                    // All references become visible to the AI (they are now context for the video).
                                    let image_data_uris: Vec<String> = {
                                        let mut app_state = state.lock().unwrap();
                                        let mut uris = Vec::new();
                                        for rid in &reference_ids {
                                            let res_id = format!("R{rid}");
                                            if let Some(bytes) = app_state.image_bytes.get(&res_id) {
                                                use base64::Engine;
                                                let encoded = base64::engine::general_purpose::STANDARD.encode(bytes);
                                                uris.push(format!("data:image/png;base64,{encoded}"));
                                                if !app_state.ai_visible_resources.contains(&res_id) {
                                                    app_state.ai_visible_resources.push(res_id);
                                                }
                                            }
                                        }
                                        uris
                                    };

                                    // Set placeholder state.
                                    let video_resource_id = {
                                        let mut app_state = state.lock().unwrap();
                                        let id = gen_resource_id(&mut app_state);
                                        app_state.video_status.insert(id.clone(), VideoStatus::Pending);
                                        // Record generation params for annotation + AI context.
                                        let ref_res_ids: Vec<String> = reference_ids
                                            .iter()
                                            .map(|rid| format!("R{rid}"))
                                            .collect();
                                        app_state.video_gen_info.insert(
                                            id.clone(),
                                            VideoGenInfo {
                                                prompt: prompt.clone(),
                                                reference_resource_ids: ref_res_ids,
                                                width: vid_w,
                                                height: vid_h,
                                                num_frames,
                                                frame_rate,
                                                negative_prompt: negative_prompt.clone(),
                                                seed: seed_val,
                                            },
                                        );
                                        app_state.messages.push(ChatMessage {
                                            role: Role::Assistant,
                                            content: MessageContent::Video { id: id.clone() },
                                            tool_calls: None,
                                            tool_call_id: None,
                                            image_urls: None,
                                            uploaded_image: None,
                                            ref_resource_id: Some(id.clone()),
                                        });
                                        id
                                    };

                                    log_info(&format!(
                                        "Creating video task {} with prompt: {:?} ({} reference image(s)), {}x{}, {} frames, {} fps",
                                        video_resource_id,
                                        &prompt[..prompt.len().min(80)],
                                        reference_ids.len(),
                                        vid_w, vid_h, num_frames, frame_rate
                                    ));

                                    // Create the video task with exponential backoff retry
                                    // (up to 10 times on 503 "service busy", with 1s→512s backoff).
                                    // Uses create_with_retry_multi which dispatches between:
                                    // - text-to-video (no images)
                                    // - single-image-to-video (1 image → `image` field)
                                    // - multi-image/keyframe (2+ images → `extra_body.image` array)
                                    let create_result = video_gen::VideoGenClient::new().create_with_retry_multi(
                                        &base_url,
                                        &api_key,
                                        &prompt,
                                        &image_data_uris,
                                        vid_w,
                                        vid_h,
                                        num_frames,
                                        frame_rate,
                                        negative_prompt.as_deref(),
                                        seed_val,
                                        is_keyframe,
                                        10, // max_retries
                                    ).await;

                                    let video_api_id = match create_result {
                                        Ok(id) => id,
                                        Err(e) => {
                                            log_error(&format!("Failed to create video task: {e}"));
                                         let err_msg = format!("Video generation task creation failed: {e}");
                                             let mut app_state = state.lock().unwrap();
                                             app_state.video_status.insert(
                                                 video_resource_id.clone(),
                                                 VideoStatus::Failed(format!("Failed to create task: {e}")),
                                             );
                                              ctx.request_repaint();
                                              conversation.push(ChatMessage {
                                                 role: Role::Tool,
                                                 content: MessageContent::Text(err_msg.clone()),
                                                 tool_calls: None,
                                                 tool_call_id: Some(tc.id.clone()),
                                                 image_urls: None,
                                                 uploaded_image: None,
                                                 ref_resource_id: None,
                                             });
                                             app_state.messages.push(ChatMessage::tool_result(
                                                 "generate_video", &tc.arguments, &err_msg,
                                             ));
                                             continue;
                                        }
                                    };

                                    log_info(&format!(
                                        "Video task created: api_id={}, resource_id={}",
                                        video_api_id, video_resource_id
                                    ));

                                    // Spawn background polling on a DEDICATED THREAD with its
                                    // own tokio runtime. Using std::thread (not tokio::spawn)
                                    // ensures the poller keeps running after the parent
                                    // worker thread exits (which would kill a tokio::spawn
                                    // task when its runtime is dropped).
                                    let poll_state = state.clone();
                                    let poll_ctx = ctx.clone();
                                    let poll_resource_id = video_resource_id.clone();
                                    let poll_base_url = base_url.clone();
                                    let poll_api_key = api_key.clone();
                                    let poll_video_api_id = video_api_id.clone();
                                    std::thread::spawn(move || {
                                        let rt = tokio::runtime::Runtime::new().unwrap();
                                        rt.block_on(async move {
                                            let client = video_gen::VideoGenClient::new();
                                            let temp_dir = std::env::temp_dir();
                                            let file_path = temp_dir
                                                .join(format!("{poll_resource_id}.mp4"))
                                                .to_string_lossy()
                                                .to_string();

                                            let max_wait = 600u64; // 10 minutes total timeout
                                            let poll_interval = 5u64; // 5 seconds between polls
                                            let start_time = std::time::Instant::now();
                                            let timeout = std::time::Duration::from_secs(max_wait);

                                            loop {
                                                if start_time.elapsed() > timeout {
                                                    log_error(&format!(
                                                        "Video poll timed out after {max_wait}s for {}",
                                                        poll_resource_id
                                                    ));
                                                    let mut st = poll_state.lock().unwrap();
                                                    st.video_status.insert(
                                                        poll_resource_id.clone(),
                                                        VideoStatus::Failed("Generation timed out (10 minutes)".into()),
                                                    );
                                                    poll_ctx.request_repaint();
                                                    break;
                                                }

                                                tokio::time::sleep(
                                                    std::time::Duration::from_secs(poll_interval),
                                                )
                                                .await;

                                                match client
                                                    .poll(&poll_base_url, &poll_api_key, &poll_video_api_id)
                                                    .await
                                                {
                                                    Ok(video_gen::PollResult::Pending) => {
                                                        // Still processing — keep polling.
                                                        continue;
                                                    }
                                                    Ok(video_gen::PollResult::Completed(video_url)) => {
                                                        log_info(&format!(
                                                            "Video completed! Downloading from: {}",
                                                            &video_url[..video_url.len().min(100)]
                                                        ));
                                                        match client
                                                            .download_video(&video_url, &file_path)
                                                            .await
                                                        {
                                                            Ok(()) => {
                                                                log_info(&format!(
                                                                    "Video saved to {file_path}"
                                                                ));
                                                                // Extract first frame as thumbnail (blocking, on worker thread)
                                                                let thumb_resource_id = poll_resource_id.clone();
                                                                let thumb_path = file_path.clone();
                                                                let thumb_png = video_player::extract_first_frame_png(&thumb_path);
                                                                let mut st = poll_state.lock().unwrap();
                                                                st.video_status.insert(
                                                                    poll_resource_id.clone(),
                                                                    VideoStatus::Completed(file_path.clone()),
                                                                );
                                                                if let Ok(png_bytes) = thumb_png {
                                                                    st.pending_video_thumbnails.push((thumb_resource_id, png_bytes));
                                                                    log_info("Video thumbnail extracted");
                                                                } else {
                                                                    log_warn(&format!(
                                                                        "Failed to extract thumbnail for {poll_resource_id}: {}",
                                                                        thumb_png.unwrap_err()
                                                                    ));
                                                                }
                                                            }
                                                            Err(e) => {
                                                                log_error(&format!(
                                                                    "Video download failed: {e}"
                                                                ));
                                                                let mut st = poll_state.lock().unwrap();
                                                                st.video_status.insert(
                                                                    poll_resource_id.clone(),
                                                                    VideoStatus::Failed(format!("Download failed: {e}")),
                                                                );
                                                            }
                                                        }
                                                        poll_ctx.request_repaint();
                                                        break;
                                                    }
                                                    Ok(video_gen::PollResult::Failed(reason)) => {
                                                        log_error(&format!(
                                                            "Video generation failed: {reason}"
                                                        ));
                                                        let mut st = poll_state.lock().unwrap();
                                                        st.video_status.insert(
                                                            poll_resource_id.clone(),
                                                            VideoStatus::Failed(reason),
                                                        );
                                                        poll_ctx.request_repaint();
                                                        break;
                                                    }
                                                    Err(e) => {
                                                        // Network error — retry silently up to timeout.
                                                        log_warn(&format!(
                                                            "Video poll error for {poll_resource_id}: {e} (will retry)"
                                                        ));
                                                        continue;
                                                    }
                                                }
                                            }
                                        });
                                    });

                                    // Push tool response — task is accepted, AI can continue.
                                    let ok_msg = format!("Video generation task created (resource id: {video_resource_id})");
                                    conversation.push(ChatMessage {
                                        role: Role::Tool,
                                        content: MessageContent::Text(format!(
                                            "Video generation task created (resource id: {video_resource_id}). \
                                             Generation is in progress — it will take some time. \
                                             The placeholder video card will automatically turn into \
                                             a playable video once it is ready. Continue with normal text \
                                             — do NOT call generate_video again."
                                        )),
                                        tool_calls: None,
                                        tool_call_id: Some(tc.id.clone()),
                                        image_urls: None,
                                        uploaded_image: None,
                                        ref_resource_id: Some(video_resource_id.clone()),
                                    });

                                    // Show tool result in UI.
                                    state.lock().unwrap().messages.push(ChatMessage::tool_result(
                                        "generate_video", &tc.arguments, &ok_msg,
                                    ));

                                    ctx.request_repaint();
                                    continue; // move to next tool call
                                }

                                // --- unknown tool ---
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
                                    image_urls: None,
                                    uploaded_image: None,
                                    ref_resource_id: None,
                                });
                            }

                            // Clear generating indicator.
                            {
                                let mut app_state = state.lock().unwrap();
                                app_state.generating_image = false;
                                app_state.assistant_buffer.clear();
                            }
                            ctx.request_repaint();

                            // Always continue the loop — the model needs to see
                            // tool results (including error messages) so it can
                            // analyze the failure and respond to the user.
                            continue;
                        }
                        StreamResult::Text(text) => {
                            let mut app_state = state.lock().unwrap();
                            app_state.streaming = false;
                            let trimmed = text.trim();
                            // If the model replies with "NO_REPLY", it means it has
                            // nothing to add — don't push an empty or placeholder msg.
                            if !trimmed.is_empty() && trimmed != "NO_REPLY" {
                                app_state.messages.push(ChatMessage::text(
                                    Role::Assistant,
                                    &text,
                                ));
                            }
                            app_state.assistant_buffer.clear();
                            app_state.generating_image = false;
                            app_state.ai_visible_resources.clear();
                            break;
                        }
                        StreamResult::Error(e) => {
                            log_error(&format!("Stream error: {e}"));
                            let mut app_state = state.lock().unwrap();
                            app_state.streaming = false;
                            app_state.generating_image = false;
                            app_state.assistant_buffer.clear();
                            app_state.error = Some(format!("Request failed: {e}"));
                            app_state.ai_visible_resources.clear();
                            break;
                        }
                    }
                }

                // After the main conversation ends, wait for all description
                // tasks to finish before the worker thread exits.  This ensures
                // the cached descriptions are available for the next user turn —
                // blocking here is fine because the user can't send a new message
                // until streaming = false anyway.
                if !describe_handles.is_empty() {
                    log_info(&format!(
                        "Waiting for {} image description(s)...",
                        describe_handles.len()
                    ));
                }
                for h in describe_handles {
                    let _ = h.await;
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

        // Register pending video thumbnails as egui textures.
        let pending_thumbs = {
            let mut state = self.state.lock().unwrap();
            std::mem::take(&mut state.pending_video_thumbnails)
        };
        if !pending_thumbs.is_empty() {
            for (id, png_bytes) in pending_thumbs {
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

        // ── Video player orchestration (runs on UI thread) ──
        // Drain pending control requests into active players, poll frames,
        // and upload decoded RGBA buffers as egui textures.
        {
            let mut state = self.state.lock().unwrap();

            // Drain all pending queues into local Vecs first (avoids borrow conflicts).
            let starts: Vec<String> = state.pending_video_starts.drain(..).collect();
            let toggles: Vec<String> = state.pending_video_toggle_pause.drain(..).collect();
            let stops: Vec<String> = state.pending_video_stops.drain(..).collect();
            let seeks: Vec<(String, std::time::Duration)> = state.pending_video_seeks.drain(..).collect();

            // 1. Handle start requests (always create a fresh decoder to support replay)
            for res_id in starts {
                let file_path = match state.video_status.get(&res_id) {
                    Some(VideoStatus::Completed(path)) => path.clone(),
                    _ => continue,
                };
                // Stop and remove existing player (if any) to support replay
                if let Some(mut old) = state.video_players.remove(&res_id) {
                    old.scheduled_stop();
                }
                state.textures.retain(|k, _| k != &res_id);
                log_info(&format!("[video_player] Starting inline playback for {res_id}"));
                let mut player = VideoPlayerState::new(res_id.clone(), file_path);
                player.start();
                state.video_players.insert(res_id, player);
            }

            // 2. Handle toggle-pause
            for res_id in toggles {
                if let Some(player) = state.video_players.get(&res_id) {
                    let is_playing = player.is_playing();
                    let _ = if is_playing { player.pause() } else { player.resume() };
                }
            }

            // 3. Handle stops
            for res_id in stops {
                if let Some(mut player) = state.video_players.remove(&res_id) {
                    player.scheduled_stop();
                }
                state.textures.retain(|k, _| k != &res_id);
            }

            // 4. Handle seeks
            for (res_id, target) in seeks {
                if let Some(player) = state.video_players.get(&res_id) {
                    let _ = player.seek(target);
                }
            }

            // 5. Poll frames from all active players and upload as textures.
            //    Detect EOF: drain remaining frames, then remove finished players
            //    so the UI reverts to the idle "play" card.
            let mut textures_to_update: Vec<(String, video_player::FrameData)> = Vec::new();
            let mut finished: Vec<String> = Vec::new();
            for (res_id, player) in state.video_players.iter_mut() {
                if let Some(frame) = player.poll_frames() {
                    textures_to_update.push((res_id.clone(), frame));
                }
                // Only mark as finished if we've received at least one frame
                // (decoder was truly running) and it's no longer playing.
                // This avoids killing the player before the decoder thread
                // even starts (race condition on is_playing).
                if player.has_frame() && !player.is_playing() {
                    finished.push(res_id.clone());
                }
            }
            for res_id in &finished {
                if let Some(mut p) = state.video_players.remove(res_id) {
                    p.scheduled_stop();
                }
            }
            // Release lock before calling ctx.load_texture()
            drop(state);

            for (res_id, frame) in textures_to_update {
                let size = [frame.width as usize, frame.height as usize];
                let color_img = egui::ColorImage::from_rgba_unmultiplied(size, &frame.rgba);
                let mut state = self.state.lock().unwrap();
                if let Some(handle) = state.textures.get_mut(&res_id) {
                    handle.set(color_img, egui::TextureOptions::default());
                } else {
                    let handle = ctx.load_texture(&res_id, color_img, egui::TextureOptions::default());
                    state.textures.insert(res_id, handle);
                }
            }

            // 6. Request repaint if any active player is playing (for frame updates)
            let should_repaint = {
                let state = self.state.lock().unwrap();
                state.video_players.values().any(|p| p.is_playing())
            };
            if should_repaint {
                ctx.request_repaint();
            }
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

        // Bottom input bar — custom input box with image paste support.
        egui::TopBottomPanel::bottom("input_bar").show(ctx, |ui| {
            ui.add_space(6.0);

            let input_box_id = egui::Id::new("agnes_input_box");
            let mut ib_state = input_box::InputBoxState::load(ctx, input_box_id);

            // Lock app state for read + write.
            let mut state = self.state.lock().unwrap();

            // No need to track input length anymore — paste detection
            // now happens inside input_box() by intercepting Ctrl+V.

            // Build a list of (texture, id) for all uploaded images.
            let uploaded_tex: Vec<(&egui::TextureHandle, &str)> = state
                .uploaded_images
                .iter()
                .filter_map(|id| state.textures.get(id).map(|tex| (tex, id.as_str())))
                .collect();

            // Error message (draw above input).
            if let Some(ref err) = state.error {
                ui.colored_label(Color32::RED, err);
            }

            // Render custom input box.
            // `is_streaming` turns the send button into a stop button.
            let actions = input_box::input_box(
                &mut ib_state,
                &uploaded_tex,
                "Type a message...",
                500.0,
                state.streaming,
                ui,
            );

            // Sync text back to app state.
            state.input = ib_state.text.clone();

            // Process actions returned by the input box.
            for action in actions {
                match action {
                    input_box::InputAction::Send => {
                        ib_state.text.clear();
                        ib_state.cursor_char = 0;
                        ib_state.selection_active = false;
                        ib_state.selection_start = None;
                        ib_state.clone().store(ctx, input_box_id);
                        drop(state);
                        self.send_message(ctx.clone());
                        return;
                    }
                    input_box::InputAction::Stop => {
                        drop(state);
                        self.stop_streaming();
                        ib_state.cursor_char = 0;
                        ib_state.selection_active = false;
                        ib_state.selection_start = None;
                        ib_state.clone().store(ctx, input_box_id);
                        return;
                    }
                    input_box::InputAction::ImagePastePending => {
                        // Ctrl+V was pressed inside the TextEdit — the
                        // widget already consumed the key event so the
                        // TextEdit won't insert anything.  Now check the
                        // clipboard for an image.
                        const MAX_IMAGES: usize = 10;
                        if state.uploaded_images.len() < MAX_IMAGES {
                            log_info("[paste] ImagePastePending: checking clipboard...");
                            match try_get_clipboard_image_png() {
                                Some((png_bytes, width, height)) => {
                                    let id = gen_resource_id(&mut state);
                                    log_info(&format!(
                                        "[paste] SUCCESS: stored image {} ({} bytes), {}x{}",
                                        id, png_bytes.len(), width, height
                                    ));
                                    state.uploaded_images.push(id.clone());
                                    state.image_bytes.insert(id.clone(), png_bytes.clone());
                                    state.image_dimensions.insert(id.clone(), (width, height));
                                    state.pending_images.push((id, png_bytes));
                                }
                                None => {
                                    log_info("[paste] Clipboard had no image — doing nothing");
                                }
                            }
                        } else {
                            log_info(&format!(
                                "[paste] ImagePastePending: already at max ({MAX_IMAGES}), ignoring"
                            ));
                        }
                    }
                    input_box::InputAction::CancelImage(image_id) => {
                        state.uploaded_images.retain(|id| id != &image_id);
                        state.image_bytes.remove(&image_id);
                        state.textures.remove(&image_id);
                        log_info(&format!("[upload] Canceled image {image_id}"));
                    }
                }
            }

            // Persist input_box state for next frame.
            ib_state.clone().store(ctx, input_box_id);

            drop(state);
            ui.add_space(6.0);
        });

        // Main area: messages only (takes all remaining space)
        egui::CentralPanel::default().show(ctx, |ui| {
            // Lock state mutably so render can push to pending_video_* queues.
            let mut state = self.state.lock().unwrap();

            // Message list (scrollable)
            egui::ScrollArea::vertical()
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    self.render_messages(ui, &mut state);
                });
            // Lock is dropped here; orchestration block below drains the queues.
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

        // Request repaint during streaming, image generation, or pending video tasks.
        {
            let state = self.state.lock().unwrap();
            let has_pending_video = state
                .video_status
                .values()
                .any(|s| matches!(s, VideoStatus::Pending));
            if state.streaming || state.generating_image || has_pending_video {
                ctx.request_repaint();
            }
        }
    }
}

impl AgnesApp {
    fn render_messages(&self, ui: &mut Ui, state: &mut AppState) {
        // Render completed messages (skip System role messages).
        // Safety: messages vec is not mutated during iteration; borrow is split via raw ptr.
        let msg_count = state.messages.len();
        for i in 0..msg_count {
            if state.messages[i].role == Role::System {
                continue;
            }
            let msg_ptr = &state.messages[i] as *const ChatMessage;
            let msg_ref: &ChatMessage = unsafe { &*msg_ptr };
            self.render_message(ui, msg_ref, state);
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

    fn render_message(&self, ui: &mut Ui, msg: &ChatMessage, state: &mut AppState) {
        let is_user = msg.role == Role::User;

        if is_user {
            // User message: render as a light bubble on the right side.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                egui::Frame::NONE
                    .fill(egui::Color32::from_rgb(0xE3, 0xF2, 0xFD)) // light blue bubble
                    .corner_radius(egui::CornerRadius::same(12))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .outer_margin(egui::Margin::symmetric(40, 0))
                    .show(ui, |ui| {
                        ui.set_max_width(ui.available_width() - 20.0);
                        // Use dark text on light background.
                        ui.style_mut().visuals.override_text_color =
                            Some(egui::Color32::from_rgb(0x1a, 0x1a, 0x1a));
                        self.render_message_content(ui, msg, state);
                    });
            });
        } else {
            // Assistant message: render as plain text.
            self.render_message_content(ui, msg, state);
        }

        ui.allocate_space(egui::vec2(0.0, 8.0));
    }

    /// Render the content of a message (text + optional inline thumbnail).
    fn render_message_content(&self, ui: &mut Ui, msg: &ChatMessage, state: &mut AppState) {
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

                // Show inline thumbnail for uploaded image (user pasted).
                if let Some(ref att_id) = msg.uploaded_image {
                    ui.add_space(6.0);
                    if let Some(handle) = state.textures.get(att_id) {
                        // Thumbnail size: max 220px wide, max 165px tall, keep aspect ratio.
                        let max_width = 220.0_f32.min(ui.available_width());
                        let thumb_size = image_fit_size(handle, max_width);
                        // Cap thumbnail height at 165px.
                        let aspect = thumb_size.x / thumb_size.y;
                        let thumb_size = if thumb_size.y > 165.0 {
                            egui::vec2(165.0 * aspect, 165.0)
                        } else {
                            thumb_size
                        };
                        let tex_ref = SizedTexture::new(handle.id(), thumb_size);
                        let (thumb_rect, resp) =
                            ui.allocate_exact_size(thumb_size, egui::Sense::click());
                        egui::Image::from_texture(tex_ref)
                            .corner_radius(egui::CornerRadius::same(10))
                            .paint_at(ui, thumb_rect);
                        // Right-click context menu.
                        resp.context_menu(|ui| {
                            if ui.button("📋 复制图片").clicked() {
                                if let Some(bytes) = state.image_bytes.get(att_id) {
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
            MessageContent::Image { id } => {
                // Render the generated image (full render with copy context menu).
                if let Some(handle) = state.textures.get(id) {
                    // Scale image to fit within the available width.
                    let avail_width = ui.available_width();
                    let display_size = image_fit_size(handle, avail_width);

                    let tex = SizedTexture::new(handle.id(), display_size);
                    // Allocate a clickable area so right-clicks are detected.
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
            MessageContent::ToolResult {
                tool_name,
                args_display,
                result,
            } => {
                // Render tool call result as a collapsible card.
                ui.group(|ui| {
                    ui.horizontal(|ui| {
                        let icon = if result.starts_with("Error") || result.starts_with("❌") {
                            "❌"
                        } else {
                            "✅"
                        };
                        ui.label(RichText::new(icon).size(14.0));
                        ui.label(
                            RichText::new(format!("Tool: {tool_name}"))
                                .size(12.0)
                                .color(Color32::GRAY),
                        );
                    });
                    if !args_display.is_empty() {
                        ui.label(
                            RichText::new(format!("Args: {args_display}"))
                                .size(11.0)
                                .color(Color32::GRAY),
                        );
                    }
                    let result_color = if result.starts_with("Error") || result.starts_with("❌") {
                        Color32::from_rgb(0xFF, 0x44, 0x44)
                    } else {
                        Color32::from_rgb(0x44, 0xBB, 0x44)
                    };
                    ui.label(RichText::new(result).size(12.0).color(result_color));
                });
            }
            MessageContent::Video { id } => {
                // Render video card based on current status.
                let status = state.video_status.get(id);
                match status {
                    Some(VideoStatus::Pending) | None => {
                        // Show a waiting card while video is being generated.
                        ui.horizontal(|ui| {
                            ui.label(RichText::new("🎬").size(20.0));
                            ui.vertical(|ui| {
                                ui.label(
                                    RichText::new("视频生成中…")
                                        .size(14.0)
                                        .color(Color32::LIGHT_GRAY),
                                );
                                ui.label(
                                    RichText::new("完成后将自动显示播放按钮")
                                        .size(11.0)
                                        .color(Color32::GRAY),
                                );
                            });
                        });
                    }
                    Some(VideoStatus::Completed(file_path)) => {
                        // Inline video playback UI. Render is read-only (state is &AppState);
                        // mutations go into pending_* queues drained in update() each tick.

                        let player = state.video_players.get(id);
                        let is_playing = player.map(|p| p.is_playing()).unwrap_or(false);

                        // Lock-free reads from shared atomics:
                        let total_dur = player.and_then(|p| {
                            p.shared.total_duration.lock().ok().and_then(|g| *g)
                        });
                        let current_secs = player.map(|p| {
                            p.shared.current_pts.lock().map(|g| g.as_secs_f64()).unwrap_or(0.0)
                        }).unwrap_or(0.0);
                        let current_pts = std::time::Duration::from_secs_f64(current_secs);

                        if player.is_none() {
                            // ── Idle state: show thumbnail + play overlay ──
                            let thumb_tex = state.textures.get(id);
                            let max_w = ui.available_width().min(380.0);
                            // Use actual thumbnail aspect ratio if available, else 16:9
                            let (disp_w, disp_h) = if let Some(tex) = thumb_tex {
                                let sz = image_fit_size(tex, max_w);
                                (sz.x, sz.y)
                            } else {
                                (max_w, max_w * 9.0 / 16.0)
                            };
                            let (rect, resp) = ui.allocate_exact_size(
                                egui::vec2(disp_w, disp_h),
                                egui::Sense::click(),
                            );
                            if let Some(tex) = thumb_tex {
                                // Draw the actual video frame with rounded corners
                                let sized_tex = SizedTexture::new(tex.id(), egui::vec2(disp_w, disp_h));
                                egui::Image::from_texture(sized_tex)
                                    .corner_radius(egui::CornerRadius::same(10))
                                    .paint_at(ui, rect);
                                // Semi-transparent overlay for play button visibility
                                ui.painter().rect_filled(
                                    rect,
                                    egui::CornerRadius::same(10),
                                    Color32::from_rgba_premultiplied(0x00, 0x00, 0x00, 0x20),
                                );
                            } else {
                                // No thumbnail yet — dark placeholder
                                ui.painter().rect_filled(
                                    rect,
                                    egui::CornerRadius::same(10),
                                    Color32::from_rgb(0x11, 0x11, 0x18),
                                );
                            }
                            // Play icon overlay (circle + triangle)
                            let center = rect.center();
                            ui.painter().circle_filled(
                                center,
                                22.0,
                                Color32::from_rgba_premultiplied(0xff, 0xff, 0xff, 0x33),
                            );
                            let tri_size = 12.0_f32;
                            let offset_x = tri_size * 0.15;
                            ui.painter().add(egui::Shape::convex_polygon(
                                vec![
                                    egui::pos2(center.x + offset_x - tri_size * 0.5, center.y - tri_size),
                                    egui::pos2(center.x + offset_x - tri_size * 0.5, center.y + tri_size),
                                    egui::pos2(center.x + offset_x + tri_size * 0.7, center.y),
                                ],
                                Color32::WHITE,
                                egui::Stroke::NONE,
                            ));
                            if resp.clicked() {
                                state.pending_video_starts.push(id.clone());
                            }
                            resp.context_menu(|ui| {
                                if ui.button("📂 在外部播放器打开").clicked() {
                                    let fp = file_path.clone();
                                    std::thread::spawn(move || {
                                        let _ = open::that(&fp);
                                    });
                                    ui.close();
                                }
                            });
                        } else {
                            // ── Playing / paused state: frame + controls ──

                            // First, see if we have a frame to render. Texture upload happens
                            // in update() via pending_video_upload. Here we check if the
                            // texture for this resource exists and draw it.
                            let tex_uploaded = state.textures.contains_key(id);

                            ui.group(|ui| {
                                ui.set_min_width(ui.available_width().min(400.0));
                                ui.vertical(|ui| {
                                    // Frame display
                                    let max_w = ui.available_width().min(380.0);
                                    if tex_uploaded {
                                        if let Some(tex) = state.textures.get(id) {
                                            let sz = image_fit_size(tex, max_w);
                                            let sized_tex = SizedTexture::new(tex.id(), sz);
                                            let (rect, resp) = ui.allocate_exact_size(sz, egui::Sense::click());
                                            egui::Image::from_texture(sized_tex)
                                                .corner_radius(egui::CornerRadius::same(8))
                                                .paint_at(ui, rect);
                                            // Click frame → toggle play/pause
                                            if resp.clicked() {
                                                state.pending_video_toggle_pause.push(id.clone());
                                            }
                                            resp.context_menu(|ui| {
                                                if ui.button("📂 在外部播放器打开").clicked() {
                                                    let fp = file_path.clone();
                                                    std::thread::spawn(move || {
                                                        let _ = open::that(&fp);
                                                    });
                                                    ui.close();
                                                }
                                            });
                                        }
                                    } else {
                                        // No texture yet — show dark placeholder (clickable to toggle play/pause)
                                        let frame_h = max_w * 9.0 / 16.0;
                                        let (rect, resp) = ui.allocate_exact_size(
                                            egui::vec2(max_w, frame_h),
                                            egui::Sense::click(),
                                        );
                                        if resp.clicked() {
                                            state.pending_video_toggle_pause.push(id.clone());
                                        }
                                        ui.painter().rect_filled(
                                            rect,
                                            egui::CornerRadius::same(8),
                                            Color32::from_rgb(0x11, 0x11, 0x11),
                                        );
                                        ui.painter().text(
                                            rect.center(),
                                            egui::Align2::CENTER_CENTER,
                                            "🎬",
                                            egui::FontId::proportional(28.0),
                                            Color32::GRAY,
                                        );
                                        let _ = resp;
                                    }

                                    ui.add_space(4.0);

                                    // ── Control bar ──
                                    ui.horizontal(|ui| {
                                        // Play / pause toggle
                                        let label = if is_playing { "⏸" } else { "▶" };
                                        if ui.button(RichText::new(label).size(16.0)).clicked() {
                                            state.pending_video_toggle_pause.push(id.clone());
                                        }
                                        // Stop → close player & clean up
                                        if ui.button(RichText::new("⏹").size(16.0)).clicked() {
                                            state.pending_video_stops.push(id.clone());
                                        }
                                        // Open file location
                                        if ui.button(RichText::new("📂").size(16.0)).on_hover_text("打开文件所在位置").clicked() {
                                            let fp = file_path.clone();
                                            std::thread::spawn(move || {
                                                #[cfg(windows)]
                                                {
                                                    let _ = std::process::Command::new("explorer")
                                                        .args(&["/select,", &fp])
                                                        .spawn();
                                                }
                                                #[cfg(not(windows))]
                                                {
                                                    let _ = open::that(fp);
                                                }
                                            });
                                        }

                                        // Seek slider + timestamp
                                        if let Some(total) = total_dur {
                                            if total > Duration::ZERO {
                                                let frac = (current_secs / total.as_secs_f64()).clamp(0.0, 1.0);
                                                let mut f = frac;
                                                let slider = egui::Slider::new(&mut f, 0.0..=1.0)
                                                    .show_value(false)
                                                    .trailing_fill(true);
                                                let s_resp = ui.add(slider);
                                                if s_resp.dragged() {
                                                    state.pending_video_seeks.push((
                                                        id.clone(),
                                                        Duration::from_secs_f64(f * total.as_secs_f64()),
                                                    ));
                                                }
                                                ui.label(
                                                    RichText::new(format!(
                                                        "{} / {}",
                                                        video_player::format_duration(current_pts),
                                                        video_player::format_duration(total),
                                                    ))
                                                    .size(11.0)
                                                    .color(Color32::GRAY),
                                                );
                                            }
                                        }
                                    });
                                });
                            });
                        }
                    }
                    Some(VideoStatus::Failed(reason)) => {
                        ui.colored_label(
                            Color32::from_rgb(0xFF, 0x44, 0x44),
                            format!("❌ 视频生成失败: {reason}"),
                        );
                    }
                }
            }
        }
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

// ─── Tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod image_tests {
    use image::ImageFormat;

    /// Simulate the full clipboard paste pipeline:
    /// arboard-style top-down RGBA bytes → RgbaImage::from_raw → PNG encode
    /// → PNG decode → into_rgba8. Verifies the final pixel layout matches.
    #[test]
    fn test_full_clipboard_pipeline_orientation() {
        let w = 4u32;
        let h = 4u32;

        // Simulate arboard returning top-down RGBA bytes.
        // Row 0 = top, row h-1 = bottom.
        let mut rgba_bytes: Vec<u8> = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let color = if x < 2 && y < 2 {
                    [255u8, 0, 0, 255]   // red — top-left
                } else if x >= 2 && y < 2 {
                    [0, 255, 0, 255]     // green — top-right
                } else if x < 2 && y >= 2 {
                    [0, 0, 255, 255]     // blue — bottom-left
                } else {
                    [255, 255, 0, 255]   // yellow — bottom-right
                };
                rgba_bytes.extend_from_slice(&color);
            }
        }

        // Step 1: RgbaImage::from_raw (what our code does)
        let img = image::RgbaImage::from_raw(w, h, rgba_bytes).unwrap();

        // Step 2: Encode to PNG
        let mut png_buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_buf), ImageFormat::Png)
            .unwrap();

        // Step 3: Decode back (what happens when texture is created)
        let decoded = image::load_from_memory(&png_buf).unwrap();
        let decoded_rgba = decoded.into_rgba8();

        // Verify quadrants
        assert_eq!(decoded_rgba.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        assert_eq!(decoded_rgba.get_pixel(3, 0), &image::Rgba([0, 255, 0, 255]));
        assert_eq!(decoded_rgba.get_pixel(0, 3), &image::Rgba([0, 0, 255, 255]));
        assert_eq!(decoded_rgba.get_pixel(3, 3), &image::Rgba([255, 255, 0, 255]));
    }

    /// Verify that the PNG roundtrip preserves image orientation.
    /// Creates a 4x4 image with distinct quadrants (R,G,B,Y) and checks
    /// that after RgbaImage→PNG→load→into_rgba8 the colors are in the
    /// same positions.
    #[test]
    fn test_png_roundtrip_preserves_orientation() {
        // Create a 4x4 image: top-left=red, top-right=green,
        // bottom-left=blue, bottom-right=yellow.
        let w = 4u32;
        let h = 4u32;
        let mut img = image::RgbaImage::new(w, h);
        for y in 0..h {
            for x in 0..w {
                let color = if x < 2 && y < 2 {
                    image::Rgba([255, 0, 0, 255]) // red — top-left
                } else if x >= 2 && y < 2 {
                    image::Rgba([0, 255, 0, 255]) // green — top-right
                } else if x < 2 && y >= 2 {
                    image::Rgba([0, 0, 255, 255]) // blue — bottom-left
                } else {
                    image::Rgba([255, 255, 0, 255]) // yellow — bottom-right
                };
                img.put_pixel(x, y, color);
            }
        }

        // Encode to PNG.
        let mut png_buf = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut png_buf), ImageFormat::Png)
            .unwrap();

        // Decode back.
        let decoded = image::load_from_memory(&png_buf).unwrap();
        let decoded_rgba = decoded.into_rgba8();

        // Check each quadrant corner pixel.
        // Top-left (0,0) should be red.
        assert_eq!(decoded_rgba.get_pixel(0, 0), &image::Rgba([255, 0, 0, 255]));
        // Top-right (3,0) should be green.
        assert_eq!(decoded_rgba.get_pixel(3, 0), &image::Rgba([0, 255, 0, 255]));
        // Bottom-left (0,3) should be blue.
        assert_eq!(decoded_rgba.get_pixel(0, 3), &image::Rgba([0, 0, 255, 255]));
        // Bottom-right (3,3) should be yellow.
        assert_eq!(decoded_rgba.get_pixel(3, 3), &image::Rgba([255, 255, 0, 255]));
    }
}
