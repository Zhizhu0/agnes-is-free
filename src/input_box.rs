//! Custom input box with cursor positioning, text selection, and IME support.
//!
//! Features:
//!   • Typing a character → insert at cursor position
//!   • Backspace → delete before cursor (pre-edit first, then committed)
//!   • Delete → delete after cursor
//!   • Arrow keys / Home / End → move cursor (Shift for selection)
//!   • Click → reposition cursor to nearest character
//!   • Shift+click / Shift+arrow → text selection with highlight
//!   • Enter → emit Send action
//!   • Ctrl+V → check clipboard for image; if found, emit
//!     ImagePastePending.  Otherwise paste text at cursor.
//!   • IME support for CJK / emoji / accented text.
//!
//! Focus is managed via `wants_focus` set on click, forwarded to egui's
//! memory so the context receives keyboard events.  This preserves the
//! original "click to focus" behavior — hovering does NOT gain focus.

use egui::{Color32, CornerRadius, CursorIcon, FontId, Id, Rangef, Rect, Stroke, TextureHandle, Ui};

/// High-level actions the widget wants the caller to handle.
#[derive(Debug, Clone)]
pub enum InputAction {
    /// Enter was pressed or send-button clicked with non-empty text.
    Send,
    /// Stop-button was clicked while streaming.
    Stop,
    /// Ctrl+V was pressed and the clipboard has an image.
    ImagePastePending,
    /// User clicked the delete button on an uploaded image thumbnail.
    CancelImage(String),
}

/// Persistent widget state.
#[derive(Debug, Clone, Default)]
pub struct InputBoxState {
    /// Current text content.
    pub text: String,
    /// Whether we should request focus on the inner area this frame.
    pub wants_focus: bool,
    /// Pending IME pre-edit text (composition string).
    pub ime_preedit: String,
    /// True when the IME just consumed a backspace this frame.
    pub ime_skip_next_backspace: bool,
    /// Cursor position measured in **chars** into `text`.
    pub cursor_char: usize,
    /// Start of selection range (char index).  None when no selection is active.
    pub selection_start: Option<usize>,
    /// True when the user is holding shift and selecting.
    pub selection_active: bool,
}

impl InputBoxState {
    pub fn load(ctx: &egui::Context, id: Id) -> Self {
        ctx.data_mut(|d| d.get_persisted(id).unwrap_or_default())
    }

    pub fn store(self, ctx: &egui::Context, id: Id) {
        ctx.data_mut(|d| d.insert_persisted(id, self));
    }
}

// ── Layout constants ──
const BAR_HEIGHT: f32 = 44.0;
const BAR_ROUNDING: u8 = 14;
const PADDING_LEFT: f32 = 18.0;
const PADDING_RIGHT: f32 = 50.0;
const THUMB_SIZE: f32 = 30.0;
const THUMB_ROUNDING: u8 = 8;
const CURSOR_WIDTH: f32 = 1.5;

/// Render the input box.  Returns actions for the caller.
///
/// `uploaded_images` is a slice of `(texture, image_id)` pairs for all
/// pending images (0‥10).  Thumbnails are laid out to the left of the
/// send/stop button and expand the bar height to fit.
pub fn input_box(
    state: &mut InputBoxState,
    uploaded_images: &[(&TextureHandle, &str)],
    hint: &str,
    desired_width: f32,
    is_streaming: bool,
    ui: &mut Ui,
) -> Vec<InputAction> {
    let mut actions = Vec::new();

    let available_width = desired_width.min(ui.available_width());

    // ── Two-row layout ──────────────────────────────────────────────────
    // When thumbnails are present the bar is split into:
    //   • top row    — thumbnails (fixed height)
    //   • bottom row — text input + send button (always BAR_HEIGHT tall)
    let thumbnails_per_row = 4usize;
    let num_thumbs = uploaded_images.len();
    let thumb_rows = if num_thumbs == 0 {
        0
    } else {
        (num_thumbs + thumbnails_per_row - 1) / thumbnails_per_row
    };
    let thumb_h: f32 = if num_thumbs == 0 {
        0.0
    } else {
        // Top padding + N rows of thumbnails + gap before bottom row
        6.0 + thumb_rows as f32 * (THUMB_SIZE + 6.0)
    };
    let bar_height = BAR_HEIGHT + thumb_h; // bottom row is always BAR_HEIGHT
    let num_thumb_cols = thumbnails_per_row.min(num_thumbs.max(1));
    let _thumb_total_w = if num_thumbs == 0 {
        0.0
    } else {
        num_thumb_cols as f32 * (THUMB_SIZE + 6.0) + 8.0
    };
    let text_area_width =
        (available_width - PADDING_LEFT - PADDING_RIGHT).max(50.0);

    // ── Compute bottom-row geometry (used by text, cursor, button, IME) ──
    // rectangle.top is not known until after allocate; store thumb_h so
    // draw code can derive the bottom-row Y origin.
    let thumb_h = thumb_h;

    // ── Allocate bar rectangle ──
    let desired_size = egui::vec2(available_width, bar_height);
    let (rect, _alloc) = ui.allocate_at_least(desired_size, egui::Sense::hover());

    let bottom_row_top = rect.min.y + thumb_h;
    let bottom_row_center = bottom_row_top + BAR_HEIGHT / 2.0;

    // ── Register an interactable with the SAME id we use for request_focus ──
    // The accessibility tree (accesskit) walks the widget Ids to find the
    // focused node.  If request_focus targets an Id with no registered
    // widget, accesskit's validate_global panics with
    // "Focused ID … is not in the node list".  ui.interact here creates that
    // node AND gives us a properly-keyed response.
    let edit_id = ui.id().with("input_edit");
    let response = ui.interact(rect, edit_id, egui::Sense::click());

    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
    }

    // ── Focus management ──
    // Click on the registered interactable → request focus on the same Id.
    if response.clicked() {
        ui.ctx().memory_mut(|mem| mem.request_focus(edit_id));
    }
    // Forward stale wants_focus from persisted state; clear it afterward.
    if state.wants_focus {
        ui.ctx().memory_mut(|mem| mem.request_focus(edit_id));
        state.wants_focus = false;
    }

    let focused = ui.ctx().memory(|mem| mem.has_focus(edit_id));

    // ── Painter ──
    let painter = ui.painter();

    if focused {
        // ── Mouse: click to reposition cursor ──
        if response.clicked() {
            if let Some(pos) = ui.ctx().pointer_interact_pos() {
                // Only respond to clicks inside the bottom (text) row.
                let text_right = rect.max.x - PADDING_RIGHT - 36.0;
                if pos.x >= rect.min.x + PADDING_LEFT && pos.x <= text_right
                    && pos.y >= bottom_row_top + 4.0
                    && pos.y <= rect.max.y - 4.0
                {
                    let offset_x = pos.x - rect.min.x - PADDING_LEFT;
                    let char_idx = text_index_at_x(&state.text, offset_x, ui);
                    state.cursor_char = char_idx.clamp(0, state.text.chars().count());
                    // If shift is held, start selection from current cursor.
                    ui.ctx().input(|i| {
                        if i.modifiers.shift {
                            state.selection_active = true;
                            if state.selection_start.is_none() {
                                state.selection_start = Some(state.cursor_char);
                            }
                        } else {
                            state.selection_active = false;
                            state.selection_start = None;
                        }
                    });
                }
            }
        }

        // ── Process keyboard / IME events ──
        let ctx = ui.ctx().clone();
        ctx.input(|i| {
            for event in &i.events {
                match event {
                    egui::Event::Ime(egui::ImeEvent::Preedit(text)) => {
                        let was_non_empty = !state.ime_preedit.is_empty();
                        state.ime_preedit = text.clone();
                        if was_non_empty && text.is_empty() {
                            state.ime_skip_next_backspace = true;
                        }
                    }
                    egui::Event::Ime(egui::ImeEvent::Commit(text)) => {
                        // Insert committed text at cursor position.
                        let byte_idx = char_char_to_byte(&state.text, state.cursor_char);
                        state.text.insert_str(byte_idx, text);
                        state.cursor_char += text.chars().count();
                        state.ime_preedit.clear();
                        if !text.is_empty() {
                            state.ime_skip_next_backspace = false;
                        }
                    }
                    egui::Event::Ime(egui::ImeEvent::Enabled) => {
                        state.ime_skip_next_backspace = false;
                    }
                    egui::Event::Ime(egui::ImeEvent::Disabled) => {
                        state.ime_preedit.clear();
                    }
                    egui::Event::Key {
                        key: egui::Key::Backspace,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.ctrl && !modifiers.alt => {
                        if state.ime_skip_next_backspace {
                            state.ime_skip_next_backspace = false;
                        } else if !state.ime_preedit.is_empty() {
                            // Still composing — IME handles it.
                        } else if state.selection_active && state.selection_start.is_some() {
                            // Delete selection.
                            let s = state.selection_start.unwrap().min(state.cursor_char);
                            let e = state.selection_start.unwrap().max(state.cursor_char);
                            let byte_s = char_char_to_byte(&state.text, s);
                            let byte_e = char_char_to_byte(&state.text, e);
                            state.text.drain(byte_s..byte_e);
                            state.cursor_char = s;
                            state.selection_active = false;
                            state.selection_start = None;
                        } else if state.cursor_char > 0 {
                            let byte_idx = char_char_to_byte(&state.text, state.cursor_char - 1);
                            state.text.remove(byte_idx);
                            state.cursor_char = state.cursor_char.saturating_sub(1);
                        }
                    }
                    egui::Event::Key {
                        key: egui::Key::Delete,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.ctrl && !modifiers.alt => {
                        if state.selection_active && state.selection_start.is_some() {
                            let s = state.selection_start.unwrap().min(state.cursor_char);
                            let e = state.selection_start.unwrap().max(state.cursor_char);
                            let byte_s = char_char_to_byte(&state.text, s);
                            let byte_e = char_char_to_byte(&state.text, e);
                            state.text.drain(byte_s..byte_e);
                            state.cursor_char = s;
                            state.selection_active = false;
                            state.selection_start = None;
                        } else if state.cursor_char < state.text.chars().count() {
                            let byte_idx = char_char_to_byte(&state.text, state.cursor_char);
                            if byte_idx < state.text.len() {
                                let ch_len = state.text[byte_idx..].chars().next().unwrap().len_utf8();
                                state.text.drain(byte_idx..byte_idx + ch_len);
                            }
                        }
                    }
                    egui::Event::Key {
                        key: egui::Key::ArrowLeft,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift {
                            state.selection_active = true;
                            if state.selection_start.is_none() {
                                state.selection_start = Some(state.cursor_char);
                            }
                        } else {
                            state.selection_active = false;
                        }
                        state.cursor_char = state.cursor_char.saturating_sub(1);
                    }
                    egui::Event::Key {
                        key: egui::Key::ArrowRight,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift {
                            state.selection_active = true;
                            if state.selection_start.is_none() {
                                state.selection_start = Some(state.cursor_char);
                            }
                        } else {
                            state.selection_active = false;
                        }
                        state.cursor_char = (state.cursor_char + 1).min(state.text.chars().count());
                    }
                    egui::Event::Key {
                        key: egui::Key::Home,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift {
                            state.selection_active = true;
                            if state.selection_start.is_none() {
                                state.selection_start = Some(state.cursor_char);
                            }
                        } else {
                            state.selection_active = false;
                        }
                        state.cursor_char = 0;
                    }
                    egui::Event::Key {
                        key: egui::Key::End,
                        pressed: true,
                        modifiers,
                        ..
                    } => {
                        if modifiers.shift {
                            state.selection_active = true;
                            if state.selection_start.is_none() {
                                state.selection_start = Some(state.cursor_char);
                            }
                        } else {
                            state.selection_active = false;
                        }
                        state.cursor_char = state.text.chars().count();
                    }
                    egui::Event::Text(text) => {
                        if state.selection_active && state.selection_start.is_some() {
                            let s = state.selection_start.unwrap().min(state.cursor_char);
                            let e = state.selection_start.unwrap().max(state.cursor_char);
                            let byte_s = char_char_to_byte(&state.text, s);
                            let byte_e = char_char_to_byte(&state.text, e);
                            state.text.drain(byte_s..byte_e);
                            state.cursor_char = s;
                            state.selection_active = false;
                            state.selection_start = None;
                        }
                        let byte_idx = char_char_to_byte(&state.text, state.cursor_char);
                        state.text.insert_str(byte_idx, text);
                        state.cursor_char += text.chars().count();
                    }
                    egui::Event::Paste(text) => {
                        if text != "default" {
                            let byte_idx = char_char_to_byte(&state.text, state.cursor_char);
                            state.text.insert_str(byte_idx, text);
                            state.cursor_char += text.chars().count();
                        } else {
                            actions.push(InputAction::ImagePastePending);
                        }
                    }
                    egui::Event::Key {
                        key: egui::Key::Enter,
                        pressed: true,
                        modifiers,
                        ..
                    } if !modifiers.shift && !modifiers.ctrl && !modifiers.alt => {
                        actions.push(InputAction::Send);
                    }
                    egui::Event::Key {
                        key: egui::Key::V,
                        pressed: false,
                        modifiers,
                        ..
                    } if modifiers.ctrl && !modifiers.shift => {
                        actions.push(InputAction::ImagePastePending);
                    }
                    _ => {}
                }
            }
        });
    }

    // Clamp cursor to text bounds.
    state.cursor_char = state.cursor_char.min(state.text.chars().count());

    // ── Draw background ──
    let bg = Color32::from_rgb(0xF0, 0xF0, 0xF2);
    let border = Color32::from_rgb(0xCC, 0xCC, 0xCC);
    let bw = 1.0;
    painter.rect_filled(rect, CornerRadius::same(BAR_ROUNDING), bg);
    painter.rect_stroke(
        rect,
        CornerRadius::same(BAR_ROUNDING),
        Stroke::new(bw, border),
        egui::StrokeKind::Middle,
    );

    // ── Draw selection highlight (bottom row only) ──
    if state.selection_active && state.selection_start.is_some() && !state.text.is_empty() {
        let start = state.selection_start.unwrap().min(state.cursor_char);
        let end = state.selection_start.unwrap().max(state.cursor_char);
        if start < end {
            let sel_top = bottom_row_top + 4.0;
            let sel_bot = rect.max.y - 4.0;
            draw_selection_rect(
                painter, &state.text, start, end,
                rect, PADDING_LEFT, text_area_width,
                sel_top, sel_bot,
            );
        }
    }

    // ── Draw text content (bottom row) ──
    let has_preedit = focused && !state.ime_preedit.is_empty();
    let display_text = if has_preedit {
        // Insert preedit at cursor position (not at the end).
        let byte_idx = char_char_to_byte(&state.text, state.cursor_char);
        let mut s = state.text.clone();
        s.insert_str(byte_idx, &state.ime_preedit);
        s
    } else {
        state.text.clone()
    };

    let text_y = bottom_row_center;
    if !display_text.is_empty() {
        painter.text(
            egui::pos2(rect.min.x + PADDING_LEFT, text_y),
            egui::Align2::LEFT_CENTER,
            &display_text,
            FontId::proportional(14.0),
            Color32::BLACK,
        );
    } else if !focused {
        // Draw hint text
        painter.text(
            egui::pos2(rect.min.x + PADDING_LEFT, text_y),
            egui::Align2::LEFT_CENTER,
            hint,
            FontId::proportional(14.0),
            Color32::from_rgb(0xAA, 0xAA, 0xAA),
        );
    }

    // ── Draw underline beneath pre-edit string (at cursor position) ──
    if has_preedit {
        let font_id = FontId::proportional(14.0);
        let text_before_cursor: String = state.text.chars().take(state.cursor_char).collect();
        let cursor_text_width = painter
            .layout(text_before_cursor, font_id.clone(), Color32::BLACK, text_area_width)
            .rect
            .width();

        let text_baseline = text_y + 4.0;
        let underline_start = rect.min.x + PADDING_LEFT + cursor_text_width;
        let underline_end = underline_start + painter
            .layout(state.ime_preedit.clone(), font_id, Color32::BLACK, text_area_width)
            .rect
            .width();
        painter.hline(
            Rangef::new(underline_start, underline_end),
            text_baseline,
            Stroke::new(1.5, Color32::BLACK),
        );
    }

    // ── Draw cursor line (bottom row) ──
    let mut cursor_rect = Rect::NOTHING;
    if focused {
        // Measure width of text up to cursor_char (+ preedit if composing).
        let mut text_before_cursor: String = state.text.chars().take(state.cursor_char).collect();
        if has_preedit {
            text_before_cursor.push_str(&state.ime_preedit);
        }
        let cursor_x_offset = painter
            .layout(text_before_cursor, FontId::proportional(14.0), Color32::BLACK, text_area_width)
            .rect
            .width();

        let cursor_x = rect.min.x + PADDING_LEFT + cursor_x_offset + 1.0;
        let cursor_top = bottom_row_top + 4.0;
        let cursor_bot = rect.max.y - 4.0;
        painter.line_segment(
            [
                egui::pos2(cursor_x, cursor_top),
                egui::pos2(cursor_x, cursor_bot),
            ],
            Stroke::new(CURSOR_WIDTH, Color32::BLACK),
        );
        cursor_rect = Rect::from_min_max(
            egui::pos2(cursor_x, cursor_top),
            egui::pos2(cursor_x + CURSOR_WIDTH, cursor_bot),
        );
    }

    // ── Tell winit to keep IME enabled while we're focused ──
    if focused {
        let to_global = ui
            .ctx()
            .layer_transform_to_global(ui.layer_id())
            .unwrap_or_default();
        let global_rect = to_global * rect;
        let global_cursor = to_global * cursor_rect;
        ui.ctx().output_mut(|o| {
            o.ime = Some(egui::output::IMEOutput {
                rect: global_rect,
                cursor_rect: global_cursor,
            });
        });
    }

    // ── Draw uploaded-image thumbnails (top row if any) ──
    for (idx, &(tex, image_id)) in uploaded_images.iter().enumerate() {
        let col = (idx % thumbnails_per_row) as f32;
        let row = (idx / thumbnails_per_row) as f32;
        let thumb_x = rect.min.x + 6.0 + col * (THUMB_SIZE + 6.0);
        let thumb_y = rect.min.y + 6.0 + row * (THUMB_SIZE + 6.0);

        let thumb_rect = Rect::from_min_size(
            egui::pos2(thumb_x, thumb_y),
            egui::vec2(THUMB_SIZE, THUMB_SIZE),
        );

        egui::Image::from_texture((tex.id(), egui::vec2(THUMB_SIZE, THUMB_SIZE)))
            .fit_to_exact_size(egui::vec2(THUMB_SIZE, THUMB_SIZE))
            .corner_radius(CornerRadius::same(THUMB_ROUNDING))
            .paint_at(ui, thumb_rect);

        // Delete button
        let del_size = 14.0;
        let del_rect = Rect::from_min_size(
            egui::pos2(
                thumb_rect.max.x - del_size / 2.0,
                thumb_rect.min.y - del_size / 2.0,
            ),
            egui::vec2(del_size, del_size),
        );
        let del_resp = ui.interact(
            del_rect,
            ui.id().with(format!("del_{image_id}")),
            egui::Sense::click(),
        );
        if del_resp.clicked() {
            actions.push(InputAction::CancelImage(image_id.to_string()));
        }
        if del_resp.hovered() {
            let painter = ui.painter();
            painter.circle_filled(del_rect.center(), del_size / 2.0, Color32::from_rgb(0xFF, 0x55, 0x55));
            painter.text(
                del_rect.center(),
                egui::Align2::CENTER_CENTER,
                "\u{00D7}",
                FontId::proportional(11.0),
                Color32::WHITE,
            );
        }
    }

    // ── Draw send / stop button (bottom row) ──
    let send_size = 30.0;
    let send_rect = Rect::from_min_size(
        egui::pos2(
            rect.max.x - send_size - 6.0,
            bottom_row_top + BAR_HEIGHT / 2.0 - send_size / 2.0,
        ),
        egui::vec2(send_size, send_size),
    );
    let send_resp = ui.interact(send_rect, ui.id().with("send_btn"), egui::Sense::click());
    let can_interact = if is_streaming {
        true
    } else {
        !state.text.trim().is_empty()
    };
    let btn_bg = if is_streaming {
        if send_resp.hovered() {
            Color32::from_rgb(0xB2, 0x22, 0x22)
        } else {
            Color32::from_rgb(0xD0, 0x30, 0x30)
        }
    } else if can_interact {
        if send_resp.hovered() {
            Color32::from_rgb(0x3D, 0x7A, 0xE0)
        } else {
            Color32::from_rgb(0x5B, 0x9B, 0xF5)
        }
    } else {
        Color32::from_rgb(0xCC, 0xCC, 0xCC)
    };
    {
        let painter = ui.painter();
        painter.circle_filled(send_rect.center(), send_size / 2.0, btn_bg);
        if is_streaming {
            let sq = 12.0;
            let square_rect = Rect::from_center_size(send_rect.center(), egui::vec2(sq, sq));
            painter.rect_filled(square_rect, egui::CornerRadius::same(2), Color32::WHITE);
        } else {
            painter.text(
                egui::pos2(send_rect.center().x, send_rect.center().y - 0.5),
                egui::Align2::CENTER_CENTER,
                "\u{27A4}",
                FontId::proportional(14.0),
                Color32::WHITE,
            );
        }
    }
    if send_resp.clicked() && can_interact {
        if is_streaming {
            actions.push(InputAction::Stop);
        } else {
            actions.push(InputAction::Send);
        }
    }

    actions
}

// ── Helper functions ──

/// Given an offset-x from the left padding, find the character index
/// in `text` that the click landed on.
fn text_index_at_x(text: &str, offset_x: f32, ui: &Ui) -> usize {
    let font_id = FontId::proportional(14.0);
    let total_chars = text.chars().count();
    if total_chars == 0 {
        return 0;
    }

    let painter = ui.painter();
    for char_idx in 0..total_chars {
        let prefix: String = text.chars().take(char_idx + 1).collect();
        let laid = painter.layout_no_wrap(prefix, font_id.clone(), Color32::BLACK);
        if laid.rect.width() > offset_x {
            return char_idx;
        }
    }

    total_chars
}

/// Convert a char index to a byte index in the string.
fn char_char_to_byte(text: &str, char_idx: usize) -> usize {
    text.char_indices()
        .nth(char_idx)
        .map(|(i, _)| i)
        .unwrap_or(text.len())
}

/// Draw a selection highlight rectangle over characters [start..end].
/// `sel_top` / `sel_bot` are the vertical bounds (already in bar-local
/// coordinates) so the caller can restrict the highlight to the text row.
fn draw_selection_rect(
    painter: &egui::Painter,
    text: &str,
    start: usize,
    end: usize,
    rect: Rect,
    padding_left: f32,
    wrap_width: f32,
    sel_top: f32,
    sel_bot: f32,
) {
    let prefix_a: String = text.chars().take(start.min(text.chars().count())).collect();
    let clamped_end = end.min(text.chars().count());
    let prefix_b: String = text.chars().take(clamped_end).collect();

    let laid_a = painter.layout(prefix_a, FontId::proportional(14.0), Color32::BLACK, wrap_width);
    let laid_b = painter.layout(prefix_b, FontId::proportional(14.0), Color32::BLACK, wrap_width);

    let sel_rect = Rect::from_min_size(
        egui::pos2(rect.min.x + padding_left + laid_a.rect.width(), sel_top),
        egui::vec2(laid_b.rect.width() - laid_a.rect.width(), sel_bot - sel_top),
    );

    // Light blue background.
    painter.rect_filled(
        sel_rect,
        2.0,
        Color32::from_rgb(0xBB, 0xDD, 0xFF).linear_multiply(0.35),
    );
    painter.rect_stroke(
        sel_rect,
        2.0,
        Stroke::new(0.5, Color32::from_rgb(0x99, 0xBB, 0xEE)),
        egui::StrokeKind::Middle,
    );
}
