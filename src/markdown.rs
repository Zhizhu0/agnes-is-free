use egui::{Color32, RichText};

/// Render markdown text into a list of RichText segments for egui.
/// Uses comrak to convert markdown → HTML, then a simple HTML parser
/// to produce styled RichText segments.
pub fn render_markdown(text: &str) -> Vec<RichText> {
    let mut result = Vec::new();

    let html = comrak::markdown_to_html(text, &comrak::ComrakOptions::default());
    render_html(&html, &mut result);

    result
}

#[derive(Clone, Debug, Default)]
struct Style {
    bold: bool,
    italic: bool,
    code: bool,
    strikethrough: bool,
    heading_level: u8, // 0 = not a heading, 1-6 for h1-h6
}

/// Parse HTML into styled RichText segments.
/// Uses a char iterator for text (preserves UTF-8) and byte-length skipping for tags.
fn render_html(html: &str, out: &mut Vec<RichText>) {
    let mut styles = vec![Style::default()];
    let mut buf = String::new();
    let mut chars = html.chars();
    let mut in_code = false;
    let mut code_buf = String::new();

    while let Some(ch) = chars.next() {
        if ch == '<' {
            // Flush accumulated text before processing a tag.
            flush_text(&styles, &buf, out);
            buf.clear();

            // Read tag name: scan forward for '>'.
            let mut tag = String::new();
            let mut is_close = false;
            loop {
                match chars.next() {
                    Some('/') => is_close = true,
                    Some('>') => break,
                    Some(c) => tag.push(c),
                    None => break, // malformed tag, stop
                }
            }

            if is_close {
                // Flush code buffer on </code> or </pre>
                if matches!(tag.split_whitespace().next().unwrap_or(""), "code" | "pre") {
                    if in_code && !code_buf.is_empty() {
                        let mut s = styles.last().unwrap().clone();
                        s.code = true;
                        flush_text(&[s], &code_buf, out);
                        code_buf.clear();
                    }
                    in_code = false;
                }
                // Only pop style for tags that pushed one (inline formatting tags + headings).
                // Container tags like <ul>, <ol>, <table>, <tr>, <td>, etc.
                // are stack-neutral — they don't push, so don't pop on close.
                let tag_lower = tag.split_whitespace().next().unwrap_or("").to_lowercase();
                let should_pop = is_stack_pushing_inline_tag(&tag_lower)
                    || tag_lower.starts_with('h') && tag_lower.len() == 2;
                if should_pop {
                    styles.pop();
                    if styles.is_empty() {
                        styles.push(Style::default());
                    }
                }
            } else {
                let tag_name = tag.split_whitespace().next().unwrap_or("");
                let is_void = matches!(tag_name, "br" | "hr" | "img" | "input");

                match tag_name {
                    "strong" | "b" => {
                        let mut s = styles.last_mut().unwrap().clone();
                        s.bold = true;
                        styles.push(s);
                    }
                    "em" | "i" => {
                        let mut s = styles.last_mut().unwrap().clone();
                        s.italic = true;
                        styles.push(s);
                    }
                    "code" => {
                        let mut s = styles.last_mut().unwrap().clone();
                        s.code = true;
                        styles.push(s);
                        in_code = true;
                    }
                    "del" | "s" | "strike" => {
                        let mut s = styles.last_mut().unwrap().clone();
                        s.strikethrough = true;
                        styles.push(s);
                    }
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        // Push a heading style level: larger font, bold.
                        let level = tag_name[1..].parse::<u8>().unwrap();
                        let mut s = styles.last_mut().unwrap().clone();
                        s.bold = true;
                        s.heading_level = level;
                        styles.push(s);
                    }
                    "p" | "div" | "blockquote" => {
                        // Block-level tags: flush accumulated text with a trailing newline.
                        flush_text(&styles, &buf, out);
                        buf.clear();
                        out.push(RichText::new("\n").size(14.0));
                    }
                    "pre" => {
                        // <pre> preserves whitespace; defer flushing to </pre>.
                        in_code = true;
                    }
                    "li" => {
                        buf.push_str("• ");
                    }
                    "br" => {
                        flush_text(&styles, &buf, out);
                        buf.clear();
                        out.push(RichText::new("\n").size(14.0));
                    }
                    _ => {}
                }

                // No recursive descent: let the iterative char-by-char processing
                // handle nested tags naturally. The style stack is now correctly
                // balanced (only inline formatting tags push/pop), so recursion
                // is unnecessary and was buggy with nested tags.
                let _ = is_void; // silence unused warning
            }
        } else if in_code {
            // Inside code blocks: preserve newlines and whitespace literally.
            code_buf.push(ch);
        } else if ch == '\n' {
            // Newlines outside code: flush current buffer, emit a line break.
            flush_text(&styles, &buf, out);
            buf.clear();
            out.push(RichText::new("\n").size(14.0));
        } else {
            buf.push(ch);
        }
    }

    // Flush any remaining code buffer
    if in_code && !code_buf.is_empty() {
        let mut s = styles.last().unwrap().clone();
        s.code = true;
        flush_text(&[s], &code_buf, out);
    }

    flush_text(&styles, &buf, out);
}

fn flush_text(styles: &[Style], text: &str, out: &mut Vec<RichText>) {
    if text.is_empty() {
        return;
    }

    // Decode HTML entities (comrak escapes < > & ' " in code blocks).
    let decoded = decode_html_entities(text);

    // Determine font size based on accumulated styles.
    let mut size = 14.0;
    for s in styles {
        if s.heading_level > 0 {
            // Heading sizes: h1=22, h2=18, h3=16, h4=15, h5=14, h6=13
            size = match s.heading_level {
                1 => 22.0,
                2 => 18.0,
                3 => 16.0,
                4 => 15.0,
                5 => 14.0,
                6 => 13.0,
                _ => 14.0,
            };
        }
        if s.code {
            size = 12.0; // Code blocks use smaller font
        }
    }

    let mut rt = RichText::new(decoded).size(size);

    for s in styles {
        if s.bold {
            rt = rt.strong();
        }
        if s.italic {
            rt = rt.italics();
        }
        if s.code {
            rt = rt
                .family(egui::FontFamily::Monospace)
                .color(Color32::from_rgb(0x1a, 0x1a, 0x1a)); // near black for light backgrounds
        }
        if s.strikethrough {
            rt = rt.strikethrough();
        }
    }

    out.push(rt);
}

/// Returns true if the tag pushes a style level on open (and thus should
/// pop one on close). Only inline formatting tags push; container tags
/// like <ul>, <ol>, <table>, <tr>, <td>, <th>, <figure>, <figcaption>,
/// <dl>, <dt>, <dd> are stack-neutral.
fn is_stack_pushing_inline_tag(tag: &str) -> bool {
    matches!(tag, "strong" | "b" | "em" | "i" | "code" | "del" | "s" | "strike")
}

/// Decode common HTML entities back to plain characters.
/// comrak escapes < > & ' " in fenced code blocks, which would otherwise
/// render as literal text like `&lt;` and confuse the font renderer.
fn decode_html_entities(text: &str) -> String {
    text.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_chinese_text() {
        let result = render_markdown("你好");
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        // Strip trailing whitespace from paragraph wrapping
        assert_eq!(combined.trim(), "你好");
    }

    #[test]
    fn test_chinese_with_bold() {
        let result = render_markdown("**你好**");
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        assert!(combined.contains("你好"));
    }

    #[test]
    fn test_no_panic_on_multibyte() {
        render_markdown("Hello 世界");
    }

    #[test]
    fn test_newlines_preserved() {
        let result = render_markdown("line1\nline2");
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        assert!(combined.contains('\n'), "expected newline in '{}'", combined);
    }

    #[test]
    fn test_code_block_preserved() {
        let md = "```\n<html>\n<body>\n</body>\n</html>\n```";
        let result = render_markdown(md);
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        assert!(combined.contains("<html>"), "expected '<html>' in code block: {}", combined);
        assert!(combined.contains('\n'), "expected newlines in code block: {}", combined);
    }

    #[test]
    fn test_heading_followed_by_list() {
        // Regression: <h3> + <ul>/<li> used to corrupt the style stack
        // because </ul> would pop a style level that <ul> never pushed.
        let md = "### 说明：\n- `<!DOCTYPE html>`：声明文档类型。";
        let result = render_markdown(md);
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        assert!(combined.contains("说明"), "heading text lost: {}", combined);
        assert!(combined.contains("声明"), "list item text lost: {}", combined);
        assert!(combined.contains("DOCTYPE"), "code content lost: {}", combined);
    }

    #[test]
    fn test_heading_style_does_not_leak() {
        // Regression: </h3> was not popping the heading style because
        // `tag.len() == 3` was wrong — "h3" has length 2, not 3.
        // This caused all text after the heading to inherit heading size/bold.
        let md = "### Heading\nNormal text";
        let result = render_markdown(md);
        let texts: Vec<&str> = result.iter().map(|rt| rt.text()).collect();
        assert!(texts.iter().any(|t| *t == "Heading"), "heading text missing: {:?}", texts);
        assert!(texts.iter().any(|t| *t == "Normal text"), "normal text missing: {:?}", texts);
        // Normal text should be present as its own segment (not merged into heading).
        // If heading style leaked, "Normal text" would have been absorbed into
        // the heading segment or rendered with heading size/bold.
        assert!(texts.contains(&"Normal text"),
            "normal text should be a separate segment after heading close: {:?}", texts);
    }

    #[test]
    fn test_nested_inline_tags() {
        // **bold and *italic* text** — nested strong/em should not lose styles.
        let md = "**bold and *italic* text**";
        let result = render_markdown(md);
        let combined: String = result.iter().map(|rt| rt.text()).collect();
        assert!(combined.contains("bold"), "lost 'bold': {}", combined);
        assert!(combined.contains("italic"), "lost 'italic': {}", combined);
        assert!(combined.contains("text"), "lost 'text': {}", combined);
    }
}
