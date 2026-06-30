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
}

fn render_html(html: &str, out: &mut Vec<RichText>) {
    let mut styles = vec![Style::default()];
    let mut buf = String::new();
    let mut i = 0;
    let bytes = html.as_bytes();

    while i < bytes.len() {
        let ch = bytes[i] as char;
        if ch == '<' {
            flush_text(&styles, &buf, out);
            buf.clear();
            i += 1;

            // Read tag
            let mut tag = String::new();
            let mut is_close = false;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c == '/' {
                    is_close = true;
                } else if c == '>' {
                    i += 1;
                    break;
                }
                tag.push(c);
                i += 1;
            }

            if is_close {
                styles.pop();
                if styles.is_empty() {
                    styles.push(Style::default());
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
                    }
                    "del" | "s" | "strike" => {
                        let mut s = styles.last_mut().unwrap().clone();
                        s.strikethrough = true;
                        styles.push(s);
                    }
                    "p" | "div" | "blockquote" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        flush_text(&styles, "\n", out);
                    }
                    "li" => {
                        buf.push_str("• ");
                    }
                    _ => {}
                }

                if !is_void {
                    let close_tag = format!("</{}>", tag_name);
                    if let Some(close_pos) = html[i..].find(&close_tag) {
                        let inner = &html[i..i + close_pos];
                        render_html(inner, out);
                        i += close_pos + close_tag.len();
                    }
                }
            }
        } else if ch == '\n' {
            buf.push(' ');
            i += 1;
        } else {
            buf.push(ch);
            i += 1;
        }
    }

    flush_text(&styles, &buf, out);
}

fn flush_text(styles: &[Style], text: &str, out: &mut Vec<RichText>) {
    if text.is_empty() {
        return;
    }

    let mut rt = RichText::new(text).size(14.0);

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
                .color(Color32::LIGHT_GREEN);
        }
        if s.strikethrough {
            rt = rt.strikethrough();
        }
    }

    out.push(rt);
}
