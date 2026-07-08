use egui::{Color32, RichText};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use syntect::easy::HighlightLines;
use syntect::highlighting::{Style as SyntectStyle, Theme};
use syntect::parsing::{SyntaxReference, SyntaxSet};

const CODE_BLOCK_START: &str = "\x00CB_START";
const CODE_BLOCK_END: &str = "\x00CB_END";
const DEFAULT_TEXT_COLOR: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const INLINE_CODE_COLOR: Color32 = Color32::from_rgb(0x1a, 0x1a, 0x1a);
const THEME_NAME: &str = "base16-ocean.light";

#[derive(Clone, Debug, Default)]
struct MarkdownStyle {
    bold: bool,
    italic: bool,
    code: bool,
    strikethrough: bool,
    heading_level: u8,
    color: Option<Color32>,
}

struct SyntectCache {
    syntax_set: SyntaxSet,
    theme: Theme,
}

impl SyntectCache {
    fn new() -> Self {
        let syntax_set = SyntaxSet::load_defaults_nonewlines();
        let theme_set = syntect::highlighting::ThemeSet::load_defaults();
        let theme = theme_set
            .themes
            .get(THEME_NAME)
            .cloned()
            .unwrap_or_else(|| {
                theme_set
                    .themes
                    .values()
                    .next()
                    .cloned()
                    .expect("at least one theme")
            });
        Self { syntax_set, theme }
    }

    fn highlight_code(&self, code: &str, lang: &str) -> Vec<RichText> {
        let syntax: Option<&SyntaxReference> = self.syntax_set.find_syntax_by_token(lang);
        let syntax = match syntax {
            Some(s) => s,
            None => {
                return vec![RichText::new(code.to_string())
                    .family(egui::FontFamily::Monospace)
                    .size(12.0)
                    .color(INLINE_CODE_COLOR)];
            }
        };

        let mut highlighter = HighlightLines::new(syntax, &self.theme);
        let mut result = Vec::new();
        let trimmed = code.trim_end_matches('\n');

        for line in trimmed.lines() {
            let ranges: Vec<(SyntectStyle, &str)> =
                match highlighter.highlight_line(line, &self.syntax_set) {
                    Ok(r) => r,
                    Err(_) => {
                        result.push(
                            RichText::new(line.to_string())
                                .family(egui::FontFamily::Monospace)
                                .size(12.0)
                                .color(INLINE_CODE_COLOR),
                        );
                        result.push(RichText::new("\n").size(14.0));
                        continue;
                    }
                };

            let mut line_text = String::new();
            let mut line_color: Option<Color32> = None;
            let flush_line_token = |text: &str, color: Color32, out: &mut Vec<RichText>| {
                if !text.is_empty() {
                    out.push(
                        RichText::new(text.to_string())
                            .family(egui::FontFamily::Monospace)
                            .size(12.0)
                            .color(color),
                    );
                }
            };

            for (style, text) in ranges {
                let fg = style.foreground;
                let color = Color32::from_rgb(fg.r, fg.g, fg.b);
                match line_color {
                    Some(c) if c == color => {
                        line_text.push_str(text);
                    }
                    _ => {
                        flush_line_token(&line_text, line_color.unwrap_or(color), &mut result);
                        line_text = text.to_string();
                        line_color = Some(color);
                    }
                }
            }
            flush_line_token(&line_text, line_color.unwrap_or(INLINE_CODE_COLOR), &mut result);
            result.push(RichText::new("\n").size(14.0));
        }

        result
    }
}

pub fn render_markdown(text: &str) -> Vec<RichText> {
    let mut result = Vec::new();

    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let parser = Parser::new_ext(text, options);

    let mut styles = vec![MarkdownStyle::default()];
    let mut in_code_block = false;
    let mut code_lang = String::new();
    let mut code_buf = String::new();

    for event in parser {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { level, .. } => {
                    let mut s = styles.last().unwrap().clone();
                    s.bold = true;
                    s.heading_level = level as u8;
                    styles.push(s);
                }
                Tag::Paragraph => {}
                Tag::BlockQuote(_) => {}
                Tag::CodeBlock(kind) => {
                    in_code_block = true;
                    code_buf.clear();
                    if let CodeBlockKind::Fenced(lang) = kind {
                        code_lang = lang.to_string();
                    } else {
                        code_lang = String::new();
                    }
                    let mut s = styles.last().unwrap().clone();
                    s.code = true;
                    styles.push(s);
                }
                Tag::List(_) => {}
                Tag::Item => {
                    result.push(RichText::new("• ").size(14.0).color(DEFAULT_TEXT_COLOR));
                }
                Tag::Emphasis => {
                    let mut s = styles.last().unwrap().clone();
                    s.italic = true;
                    styles.push(s);
                }
                Tag::Strong => {
                    let mut s = styles.last().unwrap().clone();
                    s.bold = true;
                    styles.push(s);
                }
                Tag::Strikethrough => {
                    let mut s = styles.last().unwrap().clone();
                    s.strikethrough = true;
                    styles.push(s);
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Heading(_) => {
                    result.push(RichText::new("\n").size(14.0));
                    if styles.len() > 1 {
                        styles.pop();
                    }
                }
                TagEnd::Paragraph => {
                    result.push(RichText::new("\n").size(14.0));
                }
                TagEnd::BlockQuote(_) => {}
                TagEnd::CodeBlock => {
                    if in_code_block {
                        result.push(
                            RichText::new(CODE_BLOCK_START)
                                .size(0.0)
                                .color(Color32::TRANSPARENT),
                        );
                        let cache = SyntectCache::new();
                        let highlighted = cache.highlight_code(&code_buf, &code_lang);
                        result.extend(highlighted);
                        result.push(
                            RichText::new(CODE_BLOCK_END)
                                .size(0.0)
                                .color(Color32::TRANSPARENT),
                        );
                        in_code_block = false;
                        code_buf.clear();
                    }
                    if styles.len() > 1 {
                        styles.pop();
                    }
                }
                TagEnd::List(_) => {
                    result.push(RichText::new("\n").size(14.0));
                }
                TagEnd::Item => {
                    result.push(RichText::new("\n").size(14.0));
                }
                TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => {
                    if styles.len() > 1 {
                        styles.pop();
                    }
                }
                _ => {}
            },
            Event::Text(t) => {
                if in_code_block {
                    code_buf.push_str(&t);
                } else {
                    let s = styles.last().unwrap().clone();
                    let text_str: &str = &t;
                    if text_str.ends_with('\n') {
                        let trimmed = text_str.trim_end_matches('\n');
                        if !trimmed.is_empty() {
                            result.push(build_rich_text(trimmed, &s));
                        }
                        result.push(RichText::new("\n").size(14.0));
                    } else {
                        result.push(build_rich_text(text_str, &s));
                    }
                }
            }
            Event::Code(t) => {
                let mut s = styles.last().unwrap().clone();
                s.code = true;
                result.push(build_rich_text(&t.to_string(), &s));
            }
            Event::SoftBreak => {
                result.push(RichText::new("\n").size(14.0));
            }
            Event::HardBreak => {
                result.push(RichText::new("\n").size(14.0));
            }
            _ => {}
        }
    }

    if in_code_block && !code_buf.is_empty() {
        result.push(
            RichText::new(CODE_BLOCK_START)
                .size(0.0)
                .color(Color32::TRANSPARENT),
        );
        let cache = SyntectCache::new();
        let highlighted = cache.highlight_code(&code_buf, &code_lang);
        result.extend(highlighted);
        result.push(
            RichText::new(CODE_BLOCK_END)
                .size(0.0)
                .color(Color32::TRANSPARENT),
        );
    }

    result
}

fn build_rich_text(text: &str, style: &MarkdownStyle) -> RichText {
    if text.is_empty() {
        return RichText::new("").size(14.0);
    }

    let mut size = 14.0;
    if style.heading_level > 0 {
        size = match style.heading_level {
            1 => 22.0,
            2 => 18.0,
            3 => 16.0,
            4 => 15.0,
            5 => 14.0,
            6 => 13.0,
            _ => 14.0,
        };
    }
    if style.code {
        size = 12.0;
    }

    let color = style.color.unwrap_or(if style.code {
        INLINE_CODE_COLOR
    } else {
        DEFAULT_TEXT_COLOR
    });

    let mut rt = RichText::new(text).size(size).color(color);

    if style.bold {
        rt = rt.strong();
    }
    if style.italic {
        rt = rt.italics();
    }
    if style.code {
        rt = rt.family(egui::FontFamily::Monospace);
    }
    if style.strikethrough {
        rt = rt.strikethrough();
    }

    rt
}

#[cfg(test)]
mod tests {
    use super::*;

    fn combined(result: &[RichText]) -> String {
        result
            .iter()
            .map(|rt| {
                let t = rt.text();
                if t == CODE_BLOCK_START || t == CODE_BLOCK_END {
                    ""
                } else {
                    t
                }
            })
            .collect()
    }

    #[test]
    fn test_chinese_text() {
        let result = render_markdown("你好");
        let text = combined(&result);
        assert_eq!(text.trim(), "你好");
    }

    #[test]
    fn test_chinese_with_bold() {
        let result = render_markdown("**你好**");
        let text = combined(&result);
        assert!(text.contains("你好"));
    }

    #[test]
    fn test_no_panic_on_multibyte() {
        render_markdown("Hello 世界");
    }

    #[test]
    fn test_newlines_preserved() {
        let result = render_markdown("line1\nline2");
        let text = combined(&result);
        assert!(text.contains('\n'), "expected newline in '{}'", text);
    }

    #[test]
    fn test_code_block_preserved() {
        let md = "```\n<html>\n<body>\n</body>\n</html>\n```";
        let result = render_markdown(md);
        let text = combined(&result);
        assert!(
            text.contains("<html>"),
            "expected '<html>' in code block: {}",
            text
        );
        assert!(text.contains('\n'), "expected newlines in code block: {}", text);
    }

    #[test]
    fn test_heading_followed_by_list() {
        let md = "### 说明：\n- `<!DOCTYPE html>`：声明文档类型。";
        let result = render_markdown(md);
        let text = combined(&result);
        assert!(text.contains("说明"), "heading text lost: {}", text);
        assert!(text.contains("声明"), "list item text lost: {}", text);
        assert!(text.contains("DOCTYPE"), "code content lost: {}", text);
    }

    #[test]
    fn test_heading_style_does_not_leak() {
        let md = "### Heading\nNormal text";
        let result = render_markdown(md);
        let texts: Vec<&str> = result.iter().map(|rt| rt.text()).collect();
        assert!(
            texts.iter().any(|t| *t == "Heading"),
            "heading text missing: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| t.contains("Normal text")),
            "normal text missing: {:?}",
            texts
        );
    }

    #[test]
    fn test_nested_inline_tags() {
        let md = "**bold and *italic* text**";
        let result = render_markdown(md);
        let text = combined(&result);
        assert!(text.contains("bold"), "lost 'bold': {}", text);
        assert!(text.contains("italic"), "lost 'italic': {}", text);
        assert!(text.contains("text"), "lost 'text': {}", text);
    }

    #[test]
    fn test_code_block_sentinel_present() {
        let md = "```rust\nfn main() {}\n```";
        let result = render_markdown(md);
        let texts: Vec<&str> = result.iter().map(|rt| rt.text()).collect();
        assert!(
            texts.iter().any(|t| *t == CODE_BLOCK_START),
            "missing CODE_BLOCK_START sentinel: {:?}",
            texts
        );
        assert!(
            texts.iter().any(|t| *t == CODE_BLOCK_END),
            "missing CODE_BLOCK_END sentinel: {:?}",
            texts
        );
    }

    #[test]
    fn test_syntax_highlight_produces_colored_tokens() {
        let md = "```rust\nfn main() {}\n```";
        let result = render_markdown(md);
        let code_segments: Vec<_> = result
            .iter()
            .skip_while(|rt| rt.text() != CODE_BLOCK_START)
            .skip(1)
            .take_while(|rt| rt.text() != CODE_BLOCK_END)
            .collect();
        assert!(
            !code_segments.is_empty(),
            "no code segments between sentinels"
        );
        assert!(
            code_segments.len() > 1,
            "expected multiple color tokens from syntax highlighting, got {}: {:?}",
            code_segments.len(),
            code_segments.iter().map(|rt| rt.text()).collect::<Vec<_>>()
        );
    }
}
