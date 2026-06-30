# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

**Agnes AI Chat** — a desktop chat client built in Rust using egui/eframe, streaming AI responses from the Agnes AI API.

## Build & Run

```bash
cargo build          # Compile
cargo run            # Run the app
cargo check          # Quick compile check
```

No test suite exists yet.

## Architecture

```
src/
├── main.rs        # App entry, Egui UI, AppState, AgnesApp impl
├── config.rs      # TOML config (api_key + base_url), persisted to OS config dir
├── ai_client.rs   # Streaming HTTP client (OpenAI-compatible chat completions)
└── markdown.rs    # Markdown → RichText renderer for egui (comrak → HTML → styled segments)
```

**Key patterns:**

- `AppState` holds all mutable state behind `Arc<Mutex<>>`, shared between the egui UI thread and async worker threads.
- `send_message()` spawns a `std::thread` with its own `tokio::Runtime` per user message, calls `AiClient::chat_stream_collect()`, then writes the accumulated result back into `AppState`.
- `markdown::render_markdown()` converts markdown to HTML via comrak, then hand-rolls a simple HTML-to-RichText converter supporting bold, italic, code, strikethrough, headings, lists, and blockquotes.
- Config is stored in `<OS-config-dir>/agnes-is-free/config.toml` and loaded on startup with a default API URL of `https://apihub.agnes-ai.com/v1`.

## Maintenance

After making code changes, update the corresponding section of this file so it stays accurate. This file is the source of truth for future sessions — keep it in sync.

## Known Issues & Fixes

### Chinese Characters Not Displaying (CJK Rendering)

**Problem:** egui defaults to Latin-only fonts, so Chinese (and other CJK) characters render as blank squares or invisible.

**Root cause:** `egui::FontDefinitions` only contains the default Latin font set. CJK glyphs are not covered.

**Fix:** In `src/main.rs`, added `chinese_font_data()` to load a system CJK font (Microsoft YaHei on Windows, PingFang SC on macOS, WenQuanYi Zenhei on Linux) and registered it as the fallback for `FontFamily::Proportional` via `cc.egui_ctx.set_fonts(fonts)` in the `app_creator` closure.

**Files changed:** `src/main.rs` — added `chinese_font_data()` function, modified `main()` to call `cc.egui_ctx.set_fonts()`.

---

> **记录规范：** 每次遇到问题并解决后，都应在此处记录问题的现象、根因、修复方案和影响文件。这能让后续会话快速定位和修复同类问题，避免重复排查。
