# Syntax Highlighting for Code Blocks — Design Spec

**Date:** 2026-07-08
**Status:** Approved

## Goal

Add syntax highlighting to code blocks in the chat UI, replacing the current HTML-intermediate rendering pipeline with a direct markdown → RichText approach.

## Context

- Currently: `comrak` converts markdown → HTML string → custom HTML parser walks the string → `Vec<RichText>` → egui renders segments.
- Code blocks are rendered as monospace + near-black (`#1a1a1a`) with zero distinction between inline code and fenced code blocks.
- No syntax highlighting, no background panel, no visual separation.
- The user wants: syntect-powered syntax highlighting, light theme, background panel on code blocks, and a "pure" architecture without mixing HTML and non-HTML paths.

## Architecture

**New pipeline:**
```
markdown → pulldown-cmark events → walk events, maintain Style stack → Vec<RichText> → egui
```

**Removed:** `comrak`, the custom HTML parser (`render_html()`), `decode_html_entities()`, `is_stack_pushing_inline_tag()`.

**Added:** `pulldown-cmark`, `syntect`.

## Components

### 1. Event Walker + Style Stack (markdown.rs)

Walk pullddown-cmark `Event` stream, maintain a `Style` stack:

| Event | Action |
|-------|--------|
| `Start(Strong)` / `Start(Emph)` | push bold/italic |
| `Start(Heading(level))` | push heading style (size + bold) |
| `Start(Strikethrough)` | push strikethrough |
| `Start(Code)` | push code style (monospace, no highlighting) |
| `Start(CodeBlock(Fenced(lang)))` | record language tag, push code style |
| `Text(t)` | emit `RichText::new(t)` with current style |
| `SoftBreak` / `HardBreak` | emit `RichText::new("\n")` |
| `End(...)` | pop corresponding style |
| `Start(Item)` | emit `"• "` for list items |

Style stack is balanced by pulldown-cmark's well-formed event stream — no manual tag matching needed.

Supported elements (v1): headings, bold, italic, strikethrough, inline code, fenced code blocks, lists, paragraphs, line breaks.

### 2. Syntax Highlighting (markdown.rs)

On `Start(CodeBlock(Fenced(lang)))`:
- Record `lang` string (e.g. `"rust"`, `"python"`)
- Collect all `Text` events until matching `End(CodeBlock)`
- Feed collected code to syntect:
  - `SyntaxSet::load_defaults_nonewlines()` for syntax definitions
  - `ThemeSet::load_defaults()` for themes
  - Use `base16-ocean.light` theme (light background UI)
  - `find_syntax_by_token(lang)` to resolve language; fall back to plain text if unknown
  - `HighlightLines::highlight()` per line
- Each token becomes its own `RichText` segment with the syntax color from the theme
- Tokens within a line flow inline via the existing `render_line()` mechanism

Inline code (`Start(Code)`) does NOT get syntax highlighting — just monospace font.

### 3. Code Block Background Frame (main.rs)

Since egui segments are flat (`Vec<RichText>`), use sentinel segments to mark code block boundaries:

- `"\x00CB_START"` — marks beginning of a code block
- `"\x00CB_END"` — marks end of a code block

In `render_message_content()`:
- On `CB_START`: open an `egui::Frame` with light gray background, rounded corners, internal padding
- Render token segments normally inside the frame
- On `CB_END`: close the frame

The `\x00` null character is invisible to users and doesn't interfere with newline splitting logic.

### 4. Dependencies

**Cargo.toml changes:**
```toml
# Remove:
# comrak = "0.36"

# Add:
pulldown-cmark = "0.12"
syntect = { version = "5.2", features = ["default-syntaxes", "default-themes"] }
```

### 5. Testing

Rewrite existing tests — most current tests assume HTML intermediate behavior (entity encoding, tag stack balancing). New tests verify input/output behavior:

- Chinese text rendering
- Code block content preserved (no entity encoding artifacts)
- Heading style doesn't leak to following text
- Nested inline tags (bold + italic)
- Syntax highlighting produces multiple colored tokens for known languages
- Sentinel segments correctly mark code block boundaries
- Chat content flows correctly with mixed code blocks and normal text

## Migration Notes

- pulldown-cmark's default parser does not enable extensions. Add `Options::ENABLE_TABLES`, `ENABLE_STRIKETHROUGH`, etc., as needed in v1.
- pulldown-cmark does not auto-link URLs (comrak did neither — the existing parser also didn't handle `<a>` tags). No regression.
- Existing `MarkdownStyle` struct and `flush_text()` are replaced by event-driven token emission.

## Out of Scope

- Dark mode / theme switching
- Tables, footnotes, task lists (pulldown-cmark supports them but we don't enable them in v1)
- Copy-to-clipboard for code blocks
- Language label display (e.g. showing "Rust" badge on the code block)
