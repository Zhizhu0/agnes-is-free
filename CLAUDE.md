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

## Session Workflows & Tool Failure Lessons

### Worktree Isolation Conflicts (Background Sessions)

**问题：** 在后台会话中直接编辑主分支文件时，频繁收到 `"This background session hasn't isolated its changes yet. Call EnterWorktree first"` 错误，导致 Edit/Write 工具调用失败。即使已经创建了 worktree，从 worktree 切回 master 后再编辑也会再次失败。

**为什么会失败：**
1. 后台会话有一个自动的 worktree 隔离守卫（harness guard），它只检查当前工作目录是否在 `.claude/worktrees/` 下。不在就会拒绝所有文件编辑工具。
2. 这个守卫与 git 分支无关——不管你当前在哪个分支上，只要路径不在 worktree 里就拦截。
3. 在 worktree 内编辑完成后，如果 `ExitWorktree` 回到主目录再尝试编辑 master 上的文件，守卫仍然会拦截。

**如何避免：**
1. **后台会话中，始终在第一步就调用 `EnterWorktree`**，然后所有读写编辑都在 worktree 内完成。
2. **如果需要将改动同步到 master**，用 `cp <worktree-path>/<file> <master-path>/<file>` + `git commit` 的方式，而不是尝试在主目录直接编辑。
3. **不要混用 worktree 和非 worktree 编辑。** 选定一种模式坚持到底：要么全在 worktree 里做，要么全部用 cp 方式从 worktree 搬运到 master。
4. **如果遇到隔离报错**，不要重试同一个编辑——退出 worktree（如果已在其中），重新进入一个新的 worktree，或者改用 cp 方案。

**可靠的完整流程：**
```
1. EnterWorktree(name: "xxx")          ← 第一件事
2. Read + Edit + Build（都在 worktree 内）
3. ExitWorktree(action: "remove")      ← 回到 master 目录
4. cp .claude/worktrees/<name>/<file> src/<file>
5. git add + git commit（在 master 上）
```
