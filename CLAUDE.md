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
