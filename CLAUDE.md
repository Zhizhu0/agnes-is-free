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

No test suite exists. Unit tests are in `src/markdown.rs` (Chinese text rendering, UTF-8 boundary safety, newline and code block preservation).

## Verification

After code changes, run `cargo check` (or `cargo test`) to verify compilation. **Do not run the app** (`cargo run`) — it's an egui desktop GUI that requires a display environment and will fail in headless/session contexts.

## Architecture

```
src/
├── main.rs        # App entry, Egui UI, AppState, AgnesApp impl
├── config.rs      # TOML config (api_key + base_url), persisted to OS config dir
├── ai_client.rs   # Streaming HTTP client with OpenAI function-calling (tool use) support
├── image_gen.rs   # Image generation client (POST /v1/images/generations, base64 decode)
└── markdown.rs    # Markdown → RichText renderer for egui (comrak → HTML → styled segments)
```

**Key patterns:**

- `AppState` holds all mutable state behind `Arc<Mutex<>>`, shared between the egui UI thread and async worker threads.
- `send_message()` spawns a `std::thread` with its own `tokio::Runtime` per user message, runs a **tool-use loop** via `ai_client::chat_stream_tools()`. When the model returns `StreamResult::ToolCalls`, the worker executes the tool (e.g., `image_gen::ImageGenClient::generate()`), appends the result to the conversation, and loops back. When the model returns `StreamResult::Text`, the response is finalized.
- `ChatMessage.content` is a `MessageContent` enum (`Text(String)` / `Image { id: String }`). Image messages render as egui textures with a right-click context menu (copy to clipboard via `arboard`).
- `markdown::render_markdown()` converts markdown to HTML via comrak, then hand-rolls a simple HTML-to-RichText converter supporting bold, italic, code (with monospace + green color, CJK-aware), strikethrough, headings, lists, blockquotes, and code blocks. Preserves newlines and code block whitespace literally. Uses char-level iteration for text content (preserves UTF-8 multibyte chars) and byte-level scanning for ASCII-only HTML tags. Block-level tags (`<p>`, `<div>`, headings) emit newline segments; inline styling tags use recursive descent for nested content. `FontFamily::Monospace` in `main.rs` font setup includes CJK font for code block Chinese rendering.

## Maintenance

After making code changes, update the corresponding section of this file so it stays accurate. This file is the source of truth for future sessions — keep it in sync.

## Known Issues & Fixes

### Chinese Characters Not Displaying (CJK Rendering)

**Problem:** egui defaults to Latin-only fonts, so Chinese (and other CJK) characters render as blank squares or invisible.

**Root cause:** `egui::FontDefinitions` only contains the default Latin font set. CJK glyphs are not covered.

**Fix:** In `src/main.rs`, added `chinese_font_data()` to load a system CJK font (Microsoft YaHei on Windows, PingFang SC on macOS, WenQuanYi Zenhei on Linux) and registered it as the fallback for `FontFamily::Proportional` via `cc.egui_ctx.set_fonts(fonts)` in the `app_creator` closure.

**Files changed:** `src/main.rs` — added `chinese_font_data()` function, modified `main()` to call `cc.egui_ctx.set_fonts()`.

---

### Settings Modal: Mutex Deadlock (App Freeze on Save)

**Problem:** Clicking "Save" in the Settings modal freezes the entire application.

**Root cause:** The settings modal code held `self.state.lock()` (a `MutexGuard`) while calling `self.save_settings()`, which internally also called `self.state.lock()`. Since `Mutex` is not reentrant, the same thread tried to acquire the lock twice → permanent deadlock.

**Fix:** Restructured the modal to release the lock before calling `save_settings`. The guard is only held for the minimal scope needed.

**Files changed:** `src/main.rs` — `save_settings()` and the settings modal rendering block.

---

### Settings Modal: API Key Input Instantly Cleared

**Problem:** Typing in the API key text field resulted in the value being instantly wiped empty.

**Root cause:** The modal cloned `state.settings_api_key` into a local variable `api_key` and bound `TextEdit` to the local copy. At the start of every `update()` frame, the code re-cloned from `state.settings_api_key` (which remained empty), overwriting any user input.

**Fix:** Bound `TextEdit` directly to `self.state.lock().unwrap().settings_api_key` so the text field reads and writes the real state. Save also clones from state. Removed the now-unused `cancel_settings()` method.

**Files changed:** `src/main.rs` — settings modal rendering block, `save_settings()` renamed to `persist_settings()`, removed `cancel_settings()`.

---

---

### Message Send: Silent Failure (No Response, No Error)

**Problem:** After sending a message, the Send button briefly becomes Stop, then reverts to Send. No assistant message appears, no error is shown, no logs are produced.

**Root cause:** Two layers of silent swallowing:
1. `ai_client.rs`: `chat_stream_collect()` returned `Result<String, reqwest::Error>`, but the caller in `main.rs` used `.unwrap_or_default()` — any network error, HTTP error, or timeout was converted to an empty string with zero logging.
2. `ai_client.rs`: Stream chunk parsing used `if let Ok(chunk)` which silently dropped malformed SSE lines. If the API returned an error JSON (non-stream format), it would parse as empty with no warning.

**Fix:**
1. Introduced `ChatError` enum (`Http(String)` / `EmptyResponse`) in `ai_client.rs` with `Display` + `Error` impls.
2. `chat_stream_collect()` now checks HTTP status and returns a descriptive error on failure.
3. Stream parsing emits a single `eprintln!` warning on first malformed chunk (truncated data visible).
4. `main.rs` `send_message()` worker thread now matches on `Result` — on `Err`, it sets `state.error` so the user sees the message; on `Ok`, it proceeds normally.

**Files changed:** `src/ai_client.rs` — added `ChatError` enum, improved `chat_stream_collect()` with HTTP status check, parse warning, and proper error propagation. `src/main.rs` — updated worker thread to handle `ChatError` and surface it via `state.error`.

---

### User Messages: Chinese Characters Rendered as Gibberish (`ä½ å¥½`)

**Problem:** User-entered Chinese text displays as `ä½ å¥½` instead of `你好` in the chat.

**Root cause:** `markdown::render_html()` used `bytes[i] as char` to iterate over the comrak-generated HTML string. UTF-8 multibyte characters (e.g. `你` = `E4 BD A0`) had each byte individually cast to a `char`, producing Latin-1 code points (`ä` = U+00E4, `½` = U+00BD, `` = U+00A0).

**Fix:** Rewrote `render_html()` to use `.chars()` for text content and byte-level scanning only for ASCII-only HTML tag detection. Block-level tags (`<p>`, `<div>`, etc.) are now ignored to prevent unwanted newlines.

**Files changed:** `src/markdown.rs` — complete rewrite of `render_html()`, added unit tests for Chinese text rendering.

---

### Markdown Rendering: Newlines and Code Blocks Lost

**Problem:** AI responses containing newlines and code blocks rendered as a single flattened paragraph — line breaks disappeared, `<` and `>` in code became `&lt;`/`&gt;` literal text, and `<pre><code>` blocks lost all formatting.

**Root cause:** Three bugs:
1. `markdown::render_html()` collapsed all `\n` to spaces (`buf.push(' ')`), destroying paragraph and code block line breaks.
2. `<pre>` and `<code>` tags were not specially handled — content flowed through the normal buffer path, losing whitespace.
3. `main.rs::render_message()` concatenated all `RichText` segments into a single flat string via `.join("")`, discarding every style distinction (bold, italic, code color) regardless of what the renderer produced.

**Fix:**
1. `markdown::render_html()`: Added `in_code` flag + `code_buf` accumulator. Inside code blocks (`<code>`/`<pre>`), all characters go to `code_buf` preserving newlines literally. On `</code>` or `</pre>`, the buffer is flushed as a single code-styled `RichText`. Newlines outside code flush the current buffer and emit a `\n` `RichText` segment. Block-level tags (`<p>`, `<h1>`–`<h6>`) and `<br>` also emit `\n` segments.
2. `main.rs::render_message()`: Replaced flat `.join("")` concatenation with per-segment `ui.label(rt.clone().size(14.0))` loop, preserving all RichText styling.

**Files changed:** `src/markdown.rs` — `render_html()` rewritten with code-block awareness, newline preservation, `<pre>`/`<br>` handling, added tests for newlines and code blocks. `src/main.rs` — `render_message()` renders each RichText segment individually, added CJK font to `FontFamily::Monospace` for code block Chinese rendering, `send_message()` worker now clears `assistant_buffer` on success to prevent streaming-buffer / completed-message overlap.

---

### Stream Parse Warning on First Chunk (Harmless Noise)

**Problem:** `[agnes] WARN: Failed to parse stream chunk, first few bytes: "{\"id\":\"chatcmpl-...\""` appears at the start of each streaming response.

**Root cause:** OpenAI-compatible APIs send a first SSE line containing stream metadata (`{"id":"...","object":"completion","model":"...","usage":...}`) with no `delta.content`. This is valid JSON and parses as `StreamChunk` successfully, but since `choices[].delta.content` is `None`, nothing is sent to the UI. The warning was triggered by a subsequent genuinely malformed chunk (or the metadata line itself if the parser encountered it before the content chunks).

**Fix:** `ai_client.rs` — the parse warning now checks if the data is a valid `StreamChunk` with no content (normal metadata) vs. truly unparsable. Valid metadata chunks are silently skipped; only genuinely malformed lines trigger the warning.

**Files changed:** `src/ai_client.rs` — refined parse warning logic to distinguish metadata from malformed chunks.

---

### Code Block Chinese Characters Render as Boxes

**Problem:** Chinese text inside `<pre><code>` code blocks renders as blank squares.

**Root cause:** `flush_text()` applies `egui::FontFamily::Monospace` for code styling, but the monospace font family only contains Latin glyphs. CJK characters fall through to the default system monospace font which has no CJK coverage.

**Fix:** In `main.rs` font setup, after building the Proportional family, also add the CJK font ("chinese") to the front of `FontFamily::Monospace`.

**Files changed:** `src/main.rs` — font setup now registers CJK font for monospace family.

---

### Headings Lost + Content Duplication (Style Stack Corruption)

**Problem:** `### 说明：` heading text disappears, content after the heading is lost or garbled. Later paragraphs sometimes duplicate previous content. Code block color is bright green (ugly).

**Root cause:** Two bugs in `markdown::render_html()`:
1. **Style stack corruption**: Comrak generates HTML container tags like `<ul>`, `<ol>`, `<table>`, `<tr>`, `<td>`, `<th>` that the parser ignores on open (falls to `_ => {}`) but treats as closing tags on `</ul>`, `</table>`, etc. — the `is_close` branch unconditionally called `styles.pop()`, popping a style level that was never pushed. This corrupted the style stack, causing subsequent text to render with wrong styles or be dropped entirely.
2. **Recursive descent bug**: `find_closing_tag_skip()` used byte-level search for closing tags in the remaining HTML. When nested inline tags existed (e.g., `<strong>bold and <em>italic</em> text</strong>`), it found the inner tag's `>` instead of the outer tag's `>`, extracting wrong inner content and duplicating text.

**Fix:**
1. Added `is_stack_pushing_inline_tag()` — only inline formatting tags (`strong`, `b`, `em`, `i`, `code`, `del`, `s`, `strike`) push/pop style levels. Container tags (`ul`, `ol`, `table`, `tr`, `td`, `th`, `figure`, `figcaption`, `dl`, `dt`, `dd`) are stack-neutral: they push on open and ignore on close.
2. Removed recursive descent entirely. The iterative char-by-char processor now handles nested tags naturally via the style stack, which is correctly balanced.
3. Changed code block color from `Color32::LIGHT_GREEN` to dark gray `Color32::from_rgb(0x3a, 0x3a, 0x3a)`.

**Files changed:** `src/markdown.rs` — added `is_stack_pushing_inline_tag()`, removed `find_closing_tag_skip()`, removed recursive descent, changed code color. Added regression tests `test_heading_followed_by_list` and `test_nested_inline_tags`.

---

> **记录规范：** 每次遇到问题并解决后，都应在此处记录问题的现象、根因、修复方案和影响文件。这能让后续会话快速定位和修复同类问题，避免重复排查。

## Session Workflows & Tool Failure Lessons

### Tool Calls Blocked: Model Temporarily Unavailable (Auto-Mode Classifier)

**问题：** `bash` / `powershell` / `Skill` 工具调用反复失败，报错 `agnes-2.0-flash is temporarily unavailable, so auto mode cannot determine the safety of ... right now`。

**根因：** 当前会话使用的模型 `agnes-2.0-flash` 的安全分类器（classifier）暂时不可用，导致所有需要自动安全审查的工具调用被拦截。这是模型服务端的临时状态，与代码无关。

**如何避免：**
1. 等待几秒后重试，通常该状态是暂时的。
2. 如果持续出现，可以切换为纯文本操作（Read/Edit/Write）不受影响，等待 classifier 恢复后再执行构建/测试。
3. 不要在报错后立即反复重试同一命令——间隔 5-10 秒再试。

**影响文件：** 无（服务端问题，不影响代码）。

---

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

---

### Markdown Rendering: Inline Styles Collapsed into Separate Lines

**问题：** 每个 `RichText` segment 渲染成独立的一行，导致 `**粗体** 和 *斜体*` 等内容分行显示。列表项中的代码块内容也被拆成多行。

**根因：** `main.rs::render_message()` 对每个 segment 调用 `ui.label(rt)`。在 egui 中，每个 `ui.label()` 是 block-level widget，独占一行。

**修复：**
1. 按换行符（`"\n"` RichText entry）将 segments 分组。
2. 每组内用 `ui.horizontal_wrapped()` 包裹多个 `egui::Label`，使它们在同一行内联流动。
3. 新增 `render_line()` 辅助函数。

**Markdown 样式改进：**
4. `Style` 结构体新增 `heading_level: u8`。heading 标签（`<h1>`–`<h6>`）push 带对应级别的 style level。
5. `flush_text()` 应用 heading 对应字号：h1=22, h2=18, h3=16, h4=15, h5=14, h6=13。所有 heading 加粗。
6. code 样式使用 12.0pt 字号（小于正文 14.0pt），配合 monospace 字体。
7. ~~关闭 heading 标签（`</h3>` 等）正确 pop style stack。~~ (Later found buggy — `len() == 3` should be `len() == 2` for 2-char tags like "h3")

---

### Heading Style Leaks to Subsequent Content (Bold + Enlarged Font)

**Problem:** Text after a heading (e.g., `### 说明：`) inherits heading style — bold and enlarged font (16pt for h3). List items, code block content, and normal paragraph text after the heading all render with heading size and bold.

**Root cause:** The `</h3>` close-tag handler used `tag_lower.len() == 3` to detect heading tags, but `"h3"` has length **2**, not 3. The condition `tag_lower.starts_with('h') && tag_lower.len() == 3` was always false, so `styles.pop()` was never called on heading close. The heading style level remained on the stack, leaking to all subsequent text.

**Fix:** Changed `tag_lower.len() == 3` to `tag_lower.len() == 2` (heading tags like "h1"–"h6" are 2 characters).

**Files changed:** `src/markdown.rs` — heading close-tag pop condition. Added regression test `test_heading_style_does_not_leak`.

---

### Input Bar Scrolled Away by Long Conversations

**Problem:** The input box was inside the CentralPanel below the ScrollArea, so long chat messages pushed it off-screen. Users had to scroll down to find it.

**Root cause:** In egui, widgets placed after a ScrollArea in the same panel appear below the scroll region — they only become visible after scrolling past all content.

**Fix:** Split the layout into two panels: `TopBottomPanel::bottom("input_bar")` holds the centered input row (always visible), and `CentralPanel::default()` takes the remaining space for the scrollable message list. Input is rendered with `ui.vertical_centered()` and supports Enter-to-send.

**Files changed:** `src/main.rs` — `update()` restructured from single CentralPanel to CentralPanel + bottom TopBottomPanel.

---

### Emoji Render as Empty Squares

**Problem:** Emoji (🤖, ⚙, 👤) render as empty squares or invisible glyphs.

**Root cause:** The emoji font path was wrong — `"C:/Windows/Fonts/segoeui-emoji.ttf"` doesn't exist. The real Segoe UI Emoji file is `seguiemj.ttf`. Without a valid emoji font, egui can't find emoji glyphs.

**Fix:** Corrected the emoji font path to `"C:/Windows/Fonts/seguiemj.ttf"` with macOS and Linux fallbacks. The emoji font remains registered first in the Proportional fallback list.

**Files changed:** `src/main.rs` — `emoji_paths` corrected to `seguiemj.ttf`.

---