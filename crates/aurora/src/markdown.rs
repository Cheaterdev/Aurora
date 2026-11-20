//! Markdown to HTML for document bodies, safe by construction.
//!
//! `pulldown-cmark` only parses; this module writes every tag itself, so the
//! output holds nothing but the tags and attributes listed here:
//!
//! * Raw HTML in the source (`Html`, `InlineHtml` events and HTML blocks) is
//!   never passed through: it is written as escaped text, so it shows up
//!   literally on the page.
//! * Links are kept only for `http`, `https`, `mailto` and relative
//!   addresses (no scheme, not starting `//` or a backslash). Anything else,
//!   whatever the case, entities or whitespace tricks in the source (the
//!   parser decodes entities first, control characters are refused), is
//!   written as plain text without a link. Links to `http` and `https` get
//!   `rel="noopener noreferrer nofollow"`.
//! * Images are never loaded: `![alt](url)` becomes a plain link to the image
//!   address (or just the alt text when the address is not allowed or the
//!   image sits inside a link), so the page never fetches anything from a
//!   third party and the content security policy needs no `img-src` change.
//! * Headings carry no ids and no attributes, code blocks only a
//!   `language-…` class made of safe characters, table alignment is a class
//!   (never a style attribute).
//!
//! [`render`] returns `maud::Markup`, which is `PreEscaped<String>`:
//! **this is the one place in the program that is allowed to build
//! `PreEscaped` from user-controlled text, and only from this function's
//! output.** Everything else must go through maud's normal escaping.

use maud::{Markup, PreEscaped};
use pulldown_cmark::{Alignment, CodeBlockKind, Event, LinkType, Options, Parser, Tag, TagEnd};

/// Escapes text for element content and for double-quoted attribute values.
fn escape(text: &str, out: &mut String) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c => out.push(c),
        }
    }
}

/// The part of `url` before the first `:` when that colon comes before any
/// `/`, `?` or `#` (so it is where a scheme would be), lowercased.
fn scheme_of(url: &str) -> Option<String> {
    let colon = url.find(':')?;
    let head = &url[..colon];
    if head.contains(['/', '?', '#']) {
        return None;
    }
    Some(head.to_ascii_lowercase())
}

/// What a link address is allowed to be.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    /// `http` or `https`.
    External,
    Mail,
    Relative,
}

fn classify(raw: &str) -> Option<Target> {
    let url = raw.trim();
    // Browsers drop tabs and newlines inside an address, so "java\tscript:" would
    // still be a script address: no control character is accepted at all.
    if url.is_empty() || url.chars().any(|c| c.is_ascii_control()) {
        return None;
    }
    if url.starts_with("//") || url.starts_with('\\') || url.starts_with("/\\") {
        return None;
    }
    match scheme_of(url).as_deref() {
        None => Some(Target::Relative),
        Some("http" | "https") => Some(Target::External),
        Some("mailto") => Some(Target::Mail),
        Some(_) => None,
    }
}

/// The language class of a fenced code block: the first word of the info
/// string, kept only when it is made of letters, digits and `_+-#.`.
fn language_of(info: &str) -> Option<&str> {
    let word = info.split_whitespace().next()?;
    word.chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '#' | '.'))
        .then_some(word)
}

struct Image {
    target: Option<String>,
    alt: String,
}

#[derive(Default)]
struct Writer {
    out: String,
    alignments: Vec<Alignment>,
    cell: usize,
    in_head: bool,
    body_open: bool,
    /// Inside a link: whether its `<a>` was written (false: plain-text fallback).
    link: Option<bool>,
    image: Option<Image>,
    nested_images: usize,
}

impl Writer {
    fn text(&mut self, text: &str) {
        match &mut self.image {
            Some(image) => image.alt.push_str(text),
            None => escape(text, &mut self.out),
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        if self.image.is_some() {
            // Only the text of an image's description is kept; an image inside
            // it is counted so that its end does not close the outer one.
            if matches!(tag, Tag::Image { .. }) {
                self.nested_images += 1;
            }
            return;
        }
        match tag {
            Tag::Paragraph => self.out.push_str("<p>"),
            Tag::Heading { level, .. } => {
                self.out.push('<');
                self.out.push_str(&level.to_string());
                self.out.push('>');
            }
            Tag::BlockQuote(_) => self.out.push_str("<blockquote>\n"),
            Tag::CodeBlock(kind) => {
                self.out.push_str("<pre><code");
                if let CodeBlockKind::Fenced(info) = &kind
                    && let Some(language) = language_of(info)
                {
                    self.out.push_str(" class=\"language-");
                    escape(language, &mut self.out);
                    self.out.push('"');
                }
                self.out.push('>');
            }
            Tag::HtmlBlock => self.out.push_str("<p class=\"raw-html\">"),
            Tag::List(Some(1)) => self.out.push_str("<ol>\n"),
            Tag::List(Some(first)) => {
                self.out.push_str(&format!("<ol start=\"{first}\">\n"));
            }
            Tag::List(None) => self.out.push_str("<ul>\n"),
            Tag::Item => self.out.push_str("<li>"),
            Tag::Table(alignments) => {
                self.alignments = alignments;
                self.body_open = false;
                self.out.push_str("<div class=\"table-wrap\"><table>\n");
            }
            Tag::TableHead => {
                self.in_head = true;
                self.cell = 0;
                self.out.push_str("<thead><tr>");
            }
            Tag::TableRow => {
                if !self.body_open {
                    self.body_open = true;
                    self.out.push_str("<tbody>\n");
                }
                self.cell = 0;
                self.out.push_str("<tr>");
            }
            Tag::TableCell => {
                self.out.push_str(if self.in_head { "<th" } else { "<td" });
                match self.alignments.get(self.cell) {
                    Some(Alignment::Left) => self.out.push_str(" class=\"align-left\""),
                    Some(Alignment::Center) => self.out.push_str(" class=\"align-center\""),
                    Some(Alignment::Right) => self.out.push_str(" class=\"align-right\""),
                    _ => {}
                }
                self.out.push('>');
            }
            Tag::Emphasis => self.out.push_str("<em>"),
            Tag::Strong => self.out.push_str("<strong>"),
            Tag::Strikethrough => self.out.push_str("<del>"),
            Tag::Link {
                link_type,
                dest_url,
                title,
                ..
            } => {
                let address = if link_type == LinkType::Email {
                    format!("mailto:{dest_url}")
                } else {
                    dest_url.to_string()
                };
                match classify(&address) {
                    Some(target) => {
                        self.out.push_str("<a href=\"");
                        escape(address.trim(), &mut self.out);
                        self.out.push('"');
                        if !title.is_empty() {
                            self.out.push_str(" title=\"");
                            escape(&title, &mut self.out);
                            self.out.push('"');
                        }
                        if target == Target::External {
                            self.out.push_str(" rel=\"noopener noreferrer nofollow\"");
                        }
                        self.out.push('>');
                        self.link = Some(true);
                    }
                    None => self.link = Some(false),
                }
            }
            Tag::Image { dest_url, .. } => {
                // Inside a link the image is only its text: no nested anchors.
                let target = match (self.link, classify(&dest_url)) {
                    (None, Some(_)) => Some(dest_url.trim().to_string()),
                    _ => None,
                };
                self.image = Some(Image {
                    target,
                    alt: String::new(),
                });
            }
            // Not enabled in the parser, so none of these is produced.
            Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::Superscript
            | Tag::Subscript
            | Tag::MetadataBlock(_) => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        if let TagEnd::Image = tag {
            if self.nested_images > 0 {
                self.nested_images -= 1;
                return;
            }
            if let Some(image) = self.image.take() {
                let alt = if image.alt.trim().is_empty() {
                    "image"
                } else {
                    image.alt.as_str()
                };
                match image.target {
                    Some(target) => {
                        self.out.push_str("<a class=\"image-link\" href=\"");
                        escape(&target, &mut self.out);
                        self.out
                            .push_str("\" rel=\"noopener noreferrer nofollow\">");
                        escape(alt, &mut self.out);
                        self.out.push_str("</a>");
                    }
                    None => escape(alt, &mut self.out),
                }
            }
            return;
        }
        if self.image.is_some() {
            return;
        }
        match tag {
            TagEnd::Paragraph => self.out.push_str("</p>\n"),
            TagEnd::Heading(level) => {
                self.out.push_str("</");
                self.out.push_str(&level.to_string());
                self.out.push_str(">\n");
            }
            TagEnd::BlockQuote(_) => self.out.push_str("</blockquote>\n"),
            TagEnd::CodeBlock => self.out.push_str("</code></pre>\n"),
            TagEnd::HtmlBlock => self.out.push_str("</p>\n"),
            TagEnd::List(true) => self.out.push_str("</ol>\n"),
            TagEnd::List(false) => self.out.push_str("</ul>\n"),
            TagEnd::Item => self.out.push_str("</li>\n"),
            TagEnd::Table => {
                if self.body_open {
                    self.out.push_str("</tbody>\n");
                }
                self.out.push_str("</table></div>\n");
            }
            TagEnd::TableHead => {
                self.in_head = false;
                self.out.push_str("</tr></thead>\n");
            }
            TagEnd::TableRow => self.out.push_str("</tr>\n"),
            TagEnd::TableCell => {
                self.out
                    .push_str(if self.in_head { "</th>" } else { "</td>" });
                self.cell += 1;
            }
            TagEnd::Emphasis => self.out.push_str("</em>"),
            TagEnd::Strong => self.out.push_str("</strong>"),
            TagEnd::Strikethrough => self.out.push_str("</del>"),
            TagEnd::Link => {
                if self.link.take() == Some(true) {
                    self.out.push_str("</a>");
                }
            }
            TagEnd::Image
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::Superscript
            | TagEnd::Subscript
            | TagEnd::MetadataBlock(_) => {}
        }
    }

    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            // Raw HTML is text, never markup.
            Event::Text(text)
            | Event::Html(text)
            | Event::InlineHtml(text)
            | Event::InlineMath(text)
            | Event::DisplayMath(text) => self.text(&text),
            Event::Code(code) => {
                if self.image.is_some() {
                    self.text(&code);
                } else {
                    self.out.push_str("<code>");
                    escape(&code, &mut self.out);
                    self.out.push_str("</code>");
                }
            }
            Event::FootnoteReference(label) => self.text(&format!("[^{label}]")),
            Event::SoftBreak => self.text("\n"),
            Event::HardBreak => {
                if self.image.is_some() {
                    self.text(" ");
                } else {
                    self.out.push_str("<br>\n");
                }
            }
            Event::Rule => {
                if self.image.is_none() {
                    self.out.push_str("<hr>\n");
                }
            }
            Event::TaskListMarker(checked) => {
                if self.image.is_none() {
                    self.out.push_str(if checked {
                        "<input type=\"checkbox\" disabled checked> "
                    } else {
                        "<input type=\"checkbox\" disabled> "
                    });
                }
            }
        }
    }
}

/// Renders `markdown` to HTML that holds no script, no raw source HTML, no
/// third-party loads and no address but `http`, `https`, `mailto` or relative.
pub fn render(markdown: &str) -> Markup {
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    let mut writer = Writer::default();
    for event in Parser::new_ext(markdown, options) {
        writer.event(event);
    }
    // Safe: see the module documentation; this is the only PreEscaped of user text.
    PreEscaped(writer.out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn html(markdown: &str) -> String {
        render(markdown).into_string()
    }

    const TAGS: &[&str] = &[
        "p",
        "h1",
        "h2",
        "h3",
        "h4",
        "h5",
        "h6",
        "blockquote",
        "pre",
        "code",
        "ol",
        "ul",
        "li",
        "table",
        "thead",
        "tbody",
        "tr",
        "th",
        "td",
        "em",
        "strong",
        "del",
        "a",
        "br",
        "hr",
        "div",
        "input",
    ];
    const ATTRIBUTES: &[&str] = &[
        "href", "title", "rel", "class", "start", "type", "disabled", "checked",
    ];

    /// Every `<` in the output starts a tag the renderer itself wrote (source `<` is
    /// escaped), so scanning tags and their attributes covers everything executable.
    fn assert_only_safe_markup(output: &str) {
        let mut rest = output;
        while let Some(at) = rest.find('<') {
            let tail = &rest[at + 1..];
            let end = tail
                .find('>')
                .unwrap_or_else(|| panic!("open tag in {output}"));
            let tag = &tail[..end];
            let (name, mut attributes) = match tag.split_once(' ') {
                Some((name, attributes)) => (name, attributes),
                None => (tag, ""),
            };
            assert!(
                TAGS.contains(&name.trim_start_matches('/')),
                "tag <{tag}> in {output}"
            );
            while !attributes.is_empty() {
                let attributes_trimmed = attributes.trim_start();
                let (attribute, after) = match attributes_trimmed.split_once('=') {
                    Some((attribute, after)) if !attribute.contains(' ') => (attribute, after),
                    _ => {
                        let word = attributes_trimmed.split(' ').next().unwrap_or("");
                        assert!(
                            ATTRIBUTES.contains(&word),
                            "attribute {word:?} in <{tag}> in {output}"
                        );
                        attributes = &attributes_trimmed[word.len()..];
                        continue;
                    }
                };
                assert!(
                    ATTRIBUTES.contains(&attribute),
                    "attribute {attribute:?} in <{tag}> in {output}"
                );
                let value_start = after.strip_prefix('"').unwrap_or_else(|| panic!("{tag}"));
                let close = value_start
                    .find('"')
                    .unwrap_or_else(|| panic!("unterminated value in <{tag}>"));
                let value = &value_start[..close];
                if attribute == "href" {
                    let lower = value.to_ascii_lowercase();
                    assert!(
                        lower.starts_with("http://")
                            || lower.starts_with("https://")
                            || lower.starts_with("mailto:")
                            || !lower.contains(':'),
                        "href {value:?} in {output}"
                    );
                }
                attributes = &value_start[close + 1..];
            }
            rest = &tail[end + 1..];
        }
    }

    struct Case {
        name: &'static str,
        source: &'static str,
        /// Text that must be in the output.
        has: &'static [&'static str],
        /// Text that must not be in the output.
        lacks: &'static [&'static str],
    }

    #[test]
    fn malicious_sources_produce_no_executable_html() {
        let cases = [
            Case {
                name: "script element",
                source: "<script>alert(1)</script>",
                has: &["&lt;script&gt;alert(1)&lt;/script&gt;"],
                lacks: &["<script"],
            },
            Case {
                name: "script after text",
                source: "hello <script>alert(1)</script> world",
                has: &["hello &lt;script&gt;", "world"],
                lacks: &["<script"],
            },
            Case {
                name: "upper-case and split script",
                source: "<SCRIPT SRC=//evil.example/x.js></SCRIPT>\n\n<scr\nipt>alert(1)</script>",
                has: &["&lt;SCRIPT"],
                lacks: &["<SCRIPT", "<script", "<scr"],
            },
            Case {
                name: "img with onerror",
                source: "<img src=x onerror=alert(1)>",
                has: &["&lt;img src=x onerror=alert(1)&gt;"],
                lacks: &["<img"],
            },
            Case {
                name: "raw anchor with a script address",
                source: "<a href=\"javascript:alert(1)\">x</a>",
                has: &["&lt;a href=&quot;javascript:alert(1)&quot;&gt;"],
                lacks: &["<a "],
            },
            Case {
                name: "iframe",
                source: "<iframe src=\"https://evil.example\"></iframe>",
                has: &["&lt;iframe"],
                lacks: &["<iframe"],
            },
            Case {
                name: "svg, style and handlers",
                source: "<svg onload=alert(1)>\n\n<style>body{display:none}</style>\n\n<div onclick=x>y</div>",
                has: &["&lt;svg onload=alert(1)&gt;", "&lt;style&gt;"],
                lacks: &["<svg", "<style", "<div onclick"],
            },
            Case {
                name: "html comment and doctype tricks",
                source: "<!-- <script>alert(1)</script> -->\n\n<![CDATA[<script>]]>",
                has: &["&lt;!--"],
                lacks: &["<!--", "<script", "<!["],
            },
            Case {
                name: "html inside a list",
                source: "- <script>alert(1)</script>\n- <img src=x onerror=alert(2)>\n  - <iframe></iframe>",
                has: &["<li>", "&lt;script&gt;"],
                lacks: &["<script", "<img", "<iframe"],
            },
            Case {
                name: "html inside a table",
                source: "| a | b |\n|---|---|\n| <img src=x onerror=alert(1)> | <script>alert(2)</script> |",
                has: &["<table>", "&lt;img src=x"],
                lacks: &["<img", "<script"],
            },
            Case {
                name: "html in link text and in a heading and a quote",
                source: "[<img src=x onerror=alert(1)>](https://ok.example)\n\n# <b>x</b>\n\n> <script>1</script>",
                has: &["<a href=\"https://ok.example\"", "&lt;b&gt;x&lt;/b&gt;"],
                lacks: &["<img", "<b>", "<script"],
            },
            Case {
                name: "javascript link",
                source: "[click](javascript:alert(1))",
                has: &["click"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "javascript link with mixed case",
                source: "[click](JaVaScRiPt:alert(1))",
                has: &["click"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "javascript link with entities",
                source: "[a](jav&#x61;script:alert(1)) [b](&#106;avascript:alert(1)) [c](javascript&colon;alert(1)) [d](javascript&#58;alert(1))",
                has: &["a", "b", "c", "d"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "javascript link with a tab or newline inside",
                source: "[a](<java\tscript:alert(1)>) [b](jav&#9;ascript:alert(1)) [c](jav&#10;ascript:alert(1)) [d](java&#13;script:alert(1))",
                has: &["a", "b", "c", "d"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "javascript link with leading space or control",
                source: "[a](< javascript:alert(1)>) [b](&#1;javascript:alert(1)) [c](&#x20;javascript:alert(1))",
                has: &["a", "b", "c"],
                lacks: &["href=\"javascript", "href=\" javascript", "<a href=\"\u{1}"],
            },
            Case {
                name: "data, vbscript, file and blob addresses",
                source: "[a](data:text/html;base64,PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0Pg==) [b](vbscript:msgbox(1)) [c](file:///etc/passwd) [d](blob:https://x/y) [e](ftp://x/y)",
                has: &["a", "b", "c", "d", "e"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "scheme-relative and backslash addresses",
                source: "[a](//evil.example/x) [b](/\\evil.example) [c](\\\\evil.example)",
                has: &["a", "b", "c"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "autolink with a script scheme",
                source: "<javascript:alert(1)>",
                has: &["javascript:alert(1)"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "reference-style link to a script address",
                source: "[x][r] and [r]\n\n[r]: javascript:alert(1)",
                has: &["x"],
                lacks: &["href", "<a "],
            },
            Case {
                name: "script image",
                source: "![alt](javascript:alert(1))",
                has: &["alt"],
                lacks: &["<img", "href", "<a "],
            },
            Case {
                name: "image from a third party becomes a plain link",
                source: "![logo](https://evil.example/pixel.png)",
                has: &[
                    "<a class=\"image-link\" href=\"https://evil.example/pixel.png\" rel=\"noopener noreferrer nofollow\">logo</a>",
                ],
                lacks: &["<img", "src="],
            },
            Case {
                name: "image with a data address and an image inside a link",
                source: "![x](data:image/png;base64,AAAA) ![<svg onload=alert(1)>](data:text/html,x) [![inner](https://ok.example/i.png)](https://ok.example)",
                has: &["x", "inner"],
                lacks: &["<img", "data:image", "<svg", "image-link"],
            },
            Case {
                name: "image with no description",
                source: "![](/pic.png)",
                has: &["<a class=\"image-link\" href=\"/pic.png\""],
                lacks: &["<img"],
            },
            Case {
                name: "attribute breakout through a title or an address",
                source: "[a](https://ok.example \"x\\\" onmouseover=alert(1) y=\\\"\") [b](https://ok.example/\"onmouseover=\"alert(1))",
                has: &["&quot;"],
                lacks: &["\" onmouseover", "\"onmouseover"],
            },
            Case {
                name: "code fence language breakout",
                source: "```\"><script>alert(1)</script>\ncode\n```\n\n```rust\" onclick=\"x\nmore\n```",
                has: &["<pre><code>"],
                lacks: &["<script", "language-\"", "onclick="],
            },
            Case {
                name: "inline code and emphasis around markup",
                source: "`<script>alert(1)</script>` *<script>*  **<img src=x onerror=1>**",
                has: &["<code>&lt;script&gt;", "<em>&lt;script&gt;</em>"],
                lacks: &["<script", "<img"],
            },
            Case {
                name: "heading attributes are not parsed",
                source: "# Title {#id .class onclick=alert(1)}",
                has: &["<h1>Title {#id .class onclick=alert(1)}</h1>"],
                lacks: &["id=", "class=\"class\""],
            },
            Case {
                name: "table alignment is a class, not a style",
                source: "| l | c | r |\n|:--|:-:|--:|\n| 1 | 2 | 3 |",
                has: &[
                    "class=\"align-left\"",
                    "class=\"align-center\"",
                    "class=\"align-right\"",
                ],
                lacks: &["style="],
            },
            Case {
                name: "footnote and math syntax carry no markup",
                source: "x[^1] $<script>$\n\n[^1]: note",
                has: &["$&lt;script&gt;$"],
                lacks: &["<script"],
            },
        ];
        for case in cases {
            let output = html(case.source);
            for needle in case.has {
                assert!(
                    output.contains(needle),
                    "{}: expected {needle:?} in\n{output}",
                    case.name
                );
            }
            for needle in case.lacks {
                assert!(
                    !output.contains(needle),
                    "{}: found {needle:?} in\n{output}",
                    case.name
                );
            }
            assert_only_safe_markup(&output);
            for forbidden in [
                "<script", "<iframe", "<img", "<svg", "<style", "<object", "<embed",
            ] {
                assert!(
                    !output.to_ascii_lowercase().contains(forbidden),
                    "{}: {forbidden} in\n{output}",
                    case.name
                );
            }
        }
    }

    #[test]
    fn address_classes() {
        let table = [
            ("https://example.com/a?b=c#d", Some(Target::External)),
            ("HTTP://EXAMPLE.COM", Some(Target::External)),
            ("mailto:a@b.example", Some(Target::Mail)),
            ("/p/TRK/3", Some(Target::Relative)),
            ("../other", Some(Target::Relative)),
            ("page.md", Some(Target::Relative)),
            ("#section", Some(Target::Relative)),
            ("?q=1", Some(Target::Relative)),
            ("dir/file:name", Some(Target::Relative)),
            ("#a:b", Some(Target::Relative)),
            ("javascript:alert(1)", None),
            ("JAVASCRIPT:alert(1)", None),
            (" javascript:alert(1)", None),
            ("java\tscript:alert(1)", None),
            ("data:text/html,x", None),
            ("vbscript:x", None),
            ("//evil.example", None),
            ("\\\\evil.example", None),
            ("/\\evil.example", None),
            ("", None),
            ("   ", None),
        ];
        for (address, expected) in table {
            assert_eq!(classify(address), expected, "{address:?}");
        }
    }

    #[test]
    fn ordinary_markdown_renders() {
        let output = html(
            "# Title\n\nSome *emphasis*, **strong**, ~~gone~~ and `code`.\n\n\
             - one\n- two\n\n3. three\n4. four\n\n> quoted\n\n---\n\n- [x] done\n- [ ] todo\n\n\
             ```rust\nfn main() {}\n```\n\n\
             [site](https://example.com \"Home\") [local](/p/TRK/3) [mail](mailto:a@b.example) <https://auto.example> <me@example.org>\n",
        );
        for needle in [
            "<h1>Title</h1>",
            "<em>emphasis</em>",
            "<strong>strong</strong>",
            "<del>gone</del>",
            "<code>code</code>",
            "<ul>\n<li>one</li>",
            "<ol start=\"3\">",
            "<blockquote>",
            "<hr>",
            "<input type=\"checkbox\" disabled checked>",
            "<input type=\"checkbox\" disabled>",
            "<pre><code class=\"language-rust\">fn main() {}\n</code></pre>",
            "<a href=\"https://example.com\" title=\"Home\" rel=\"noopener noreferrer nofollow\">site</a>",
            "<a href=\"/p/TRK/3\">local</a>",
            "<a href=\"mailto:a@b.example\">mail</a>",
            "<a href=\"https://auto.example\" rel=\"noopener noreferrer nofollow\">https://auto.example</a>",
            "<a href=\"mailto:me@example.org\">me@example.org</a>",
        ] {
            assert!(output.contains(needle), "{needle:?} missing in\n{output}");
        }
        assert_only_safe_markup(&output);
        assert_eq!(html(""), "");
    }

    #[test]
    fn tables_scroll_inside_a_wrapper_and_keep_their_head() {
        let output = html("| a | b |\n|---|---|\n| 1 | 2 |\n| 3 | 4 |");
        assert!(
            output.starts_with("<div class=\"table-wrap\"><table>"),
            "{output}"
        );
        assert!(output.contains("<thead><tr><th>a</th><th>b</th></tr></thead>"));
        assert!(output.contains(
            "<tbody>\n<tr><td>1</td><td>2</td></tr>\n<tr><td>3</td><td>4</td></tr>\n</tbody>"
        ));
        assert!(output.ends_with("</table></div>\n"));
    }

    #[test]
    fn output_for_large_hostile_input_is_bounded_and_safe() {
        let nested = ">".repeat(2000) + " <script>alert(1)</script>";
        let output = html(&nested);
        assert!(!output.contains("<script"));
        let brackets = "[".repeat(5000) + &"](javascript:x)".repeat(50);
        assert!(!html(&brackets).contains("href"));
    }
}
