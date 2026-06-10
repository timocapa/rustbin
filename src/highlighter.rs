use std::{borrow::Cow, fmt::Write};

use pulldown_cmark::{CodeBlockKind, CowStr, Event, Options, Parser, Tag, TagEnd};
use syntect::{
    easy::HighlightLines,
    html::{IncludeBackground, append_highlighted_html_for_styled_line},
    parsing::SyntaxReference,
    util::LinesWithEndings,
};

use crate::{enry_ffi, state::AppState};

/// Markdown extensions enabled for parsing. Shared with the preview renderer
/// (`preview::markdown_to_styled_lines`) so the HTML view and the OG image
/// always parse pastes with the same grammar — security-relevant changes like
/// the removal of `ENABLE_HEADING_ATTRIBUTES` must apply to both.
pub(crate) const MARKDOWN_OPTIONS: Options = Options::ENABLE_TABLES
    .union(Options::ENABLE_FOOTNOTES)
    .union(Options::ENABLE_STRIKETHROUGH)
    .union(Options::ENABLE_TASKLISTS)
    .union(Options::ENABLE_SMART_PUNCTUATION);

/// Render a paste's full HTML view: every line gets a number and an `#L<n>`
/// anchor, grouped into fixed-size chunks that the browser lazy-renders via
/// `content-visibility: auto`.
///
/// Two things make this scale to multi-MB pastes where naive per-line markup
/// froze the tab. The markup is minimal — two elements and ~50 bytes per line
/// instead of three elements and ~215 — and off-screen chunks cost zero
/// layout/paint: the `contain-intrinsic-size` hint stands in for their
/// geometry until they scroll near. Skipped chunks stay in the DOM, so
/// find-in-page, `#L<n>` fragment navigation, selection and copying keep
/// working over the whole paste (the CSS containment spec requires it).
pub fn render_paste_html(state: &AppState, extension: Option<&str>, content: &str) -> String {
    match syntax_for_rendering(state, extension, content) {
        Some(syntax) => {
            let mut highlighter = HighlightLines::new(syntax, state.theme.as_ref());
            render_lines_chunked(content, |line, out| {
                match highlighter.highlight_line(line, &state.syntax_set) {
                    Ok(regions) => {
                        let start = out.len();
                        if append_highlighted_html_for_styled_line(
                            &regions,
                            IncludeBackground::No,
                            out,
                        )
                        .is_err()
                        {
                            out.truncate(start);
                            push_escaped_html(out, trim_line_ending(line));
                        }
                    }
                    Err(_) => push_escaped_html(out, trim_line_ending(line)),
                }
            })
        }
        None => render_lines_chunked(content, |line, out| {
            push_escaped_html(out, trim_line_ending(line));
        }),
    }
}

pub fn is_markdown(extension: Option<&str>) -> bool {
    extension.is_some_and(|e| {
        let e = e.trim();
        e.eq_ignore_ascii_case("md")
            || e.eq_ignore_ascii_case("markdown")
            || e.eq_ignore_ascii_case("mdown")
            || e.eq_ignore_ascii_case("mkd")
            || e.eq_ignore_ascii_case("mkdn")
    })
}

pub fn render_markdown(state: &AppState, content: &str) -> String {
    render_markdown_with(content, |lang, code| {
        render_markdown_code_block(state, lang, code)
    })
}

/// Render markdown to HTML, delegating fenced/indented code blocks to
/// `render_code_block`. Split out from [`render_markdown`] so the
/// XSS-neutralizing event handling can be tested without an `AppState`.
fn render_markdown_with(
    content: &str,
    render_code_block: impl FnMut(Option<&str>, &str) -> String,
) -> String {
    let parser = Parser::new_ext(content, MARKDOWN_OPTIONS);
    let mut html_output = String::new();
    pulldown_cmark::html::push_html(
        &mut html_output,
        SanitizedEvents {
            inner: parser,
            render_code_block,
            code_text: String::new(),
        },
    );
    html_output
}

/// Streaming event adapter feeding `push_html` directly, so the document's
/// event stream is never buffered. It collapses each code block into a single
/// pre-rendered `Html` event via the callback, re-emits raw HTML as plain text
/// so it is escaped on output (stored XSS in user markdown, e.g. `<script>` or
/// `<img onerror=...>`), and strips dangerous URL schemes (`javascript:`,
/// `data:`, ...) from link/image targets, which pulldown-cmark does not
/// sanitize.
struct SanitizedEvents<I, F> {
    inner: I,
    render_code_block: F,
    code_text: String,
}

impl<'a, I, F> Iterator for SanitizedEvents<I, F>
where
    I: Iterator<Item = Event<'a>>,
    F: FnMut(Option<&str>, &str) -> String,
{
    type Item = Event<'a>;

    fn next(&mut self) -> Option<Event<'a>> {
        Some(match self.inner.next()? {
            Event::Start(Tag::CodeBlock(kind)) => {
                let lang = match &kind {
                    CodeBlockKind::Fenced(lang) => lang
                        .split_whitespace()
                        .next()
                        .filter(|l| !l.is_empty())
                        .map(str::to_string),
                    CodeBlockKind::Indented => None,
                };
                self.code_text.clear();
                for event in self.inner.by_ref() {
                    match event {
                        Event::Text(text) => self.code_text.push_str(&text),
                        Event::End(TagEnd::CodeBlock) => break,
                        _ => {}
                    }
                }
                Event::Html((self.render_code_block)(lang.as_deref(), &self.code_text).into())
            }
            Event::Html(html) | Event::InlineHtml(html) => Event::Text(html),
            Event::Start(Tag::Link {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Link {
                link_type,
                dest_url: sanitize_markdown_url(dest_url),
                title,
                id,
            }),
            Event::Start(Tag::Image {
                link_type,
                dest_url,
                title,
                id,
            }) => Event::Start(Tag::Image {
                link_type,
                dest_url: sanitize_markdown_url(dest_url),
                title,
                id,
            }),
            event => event,
        })
    }
}

/// Replace a link/image URL with `"#"` unless its scheme is known-safe, so a
/// markdown paste can't execute script via `javascript:`/`data:`/etc.
fn sanitize_markdown_url(url: CowStr<'_>) -> CowStr<'_> {
    if is_safe_markdown_url(&url) {
        url
    } else {
        CowStr::Borrowed("#")
    }
}

fn is_safe_markdown_url(url: &str) -> bool {
    // Single allocation-free pass. Whitespace/control chars are skipped so they
    // can't hide the scheme (e.g. `java\tscript:` or a leading newline). A
    // scheme is only present if ':' comes before any '/', '?' or '#'; anything
    // else is a relative path, query or fragment and can't run script.
    const MAX_SCHEME_LEN: usize = 6; // longest allowed scheme: "mailto"
    let mut scheme = [0u8; MAX_SCHEME_LEN];
    let mut len = 0usize;
    let mut overlong = false;

    for c in url.chars() {
        if c.is_whitespace() || c.is_control() {
            continue;
        }
        match c {
            ':' => {
                if overlong {
                    return false;
                }
                let scheme = &scheme[..len];
                return scheme == b"http"
                    || scheme == b"https"
                    || scheme == b"mailto"
                    || scheme == b"tel"
                    || scheme == b"ftp";
            }
            '/' | '?' | '#' => return true,
            _ => {
                if c.is_ascii() && len < MAX_SCHEME_LEN {
                    scheme[len] = (c as u8).to_ascii_lowercase();
                    len += 1;
                } else {
                    // Longer than any safe scheme (or non-ASCII): unsafe if a
                    // ':' still follows, irrelevant otherwise.
                    overlong = true;
                }
            }
        }
    }

    true
}

fn render_markdown_code_block(state: &AppState, lang: Option<&str>, code: &str) -> String {
    // syntax_for_rendering (not resolve_syntax) so fenced blocks respect the
    // highlight_max_bytes cap like every other syntect path; oversized blocks
    // fall back to escaped plain text.
    let syntax = syntax_for_rendering(state, lang, code);

    let mut html = String::new();
    match lang {
        Some(l) => {
            html.push_str("<pre><code class=\"language-");
            push_escaped_html(&mut html, l);
            html.push_str("\">");
        }
        None => html.push_str("<pre><code>"),
    }

    if let Some(syntax) = syntax {
        let mut highlighter = HighlightLines::new(syntax, state.theme.as_ref());
        for line in LinesWithEndings::from(code) {
            match highlighter.highlight_line(line, &state.syntax_set) {
                Ok(regions) => {
                    let mut line_html = String::new();
                    if append_highlighted_html_for_styled_line(
                        &regions,
                        IncludeBackground::No,
                        &mut line_html,
                    )
                    .is_err()
                    {
                        push_escaped_html(&mut html, line);
                    } else {
                        html.push_str(&line_html);
                    }
                }
                Err(_) => push_escaped_html(&mut html, line),
            }
        }
    } else {
        push_escaped_html(&mut html, code);
    }

    html.push_str("</code></pre>\n");
    html
}

pub enum LanguageDetection {
    /// Detection finished using only cheap in-memory lookups.
    Resolved(Option<String>),
    /// The content-based classifier (a blocking FFI call) is needed; run
    /// [`classify_language`] on the blocking pool.
    NeedsClassifier,
}

/// The non-blocking part of language detection: a filename extension resolves
/// via a hashmap lookup, and oversized content skips classification entirely.
/// Only when neither short-circuits is the blocking enry classifier required.
pub fn detect_language_fast(
    state: &AppState,
    filename: Option<&str>,
    content: &str,
) -> LanguageDetection {
    if let Some(extension) = filename_extension(filename) {
        if resolve_syntax(state, Some(&extension)).is_some() {
            return LanguageDetection::Resolved(Some(extension));
        }
        return LanguageDetection::Resolved(None);
    }

    if content.len() > state.classifier_max_bytes {
        return LanguageDetection::Resolved(None);
    }

    LanguageDetection::NeedsClassifier
}

/// Content-based detection via the enry classifier. This is a blocking cgo FFI
/// call — keep it off the async executor (`tokio::task::spawn_blocking`).
pub fn classify_language(state: &AppState, content: &str) -> Option<String> {
    if let Some(classifier_extensions) = enry_ffi::detect_language_by_classifier(content) {
        for extension in classifier_extensions.split('\n') {
            let extension = extension.trim();
            if extension.is_empty() {
                continue;
            }
            if resolve_syntax(state, Some(extension)).is_some() {
                return Some(extension.to_string());
            }
        }
    }

    None
}

/// Cut `content` at the given byte/line budget, on a line boundary (or a char
/// boundary when a single line exceeds the byte budget). Returns the slice and
/// whether anything was cut. Used to bound the preview image's input — the
/// HTML view renders content in full.
pub fn truncate_for_render(content: &str, max_bytes: usize, max_lines: usize) -> (&str, bool) {
    if content.len() <= max_bytes && bytecount::count(content.as_bytes(), b'\n') < max_lines {
        return (content, false);
    }

    let mut end = 0;
    for (count, line) in LinesWithEndings::from(content).enumerate() {
        if count >= max_lines || end + line.len() > max_bytes {
            break;
        }
        end += line.len();
    }

    if end == 0 {
        // The first line alone exceeds the byte budget: cut inside it.
        end = max_bytes.min(content.len());
        while !content.is_char_boundary(end) {
            end -= 1;
        }
    }

    (&content[..end], end < content.len())
}

fn filename_extension(filename: Option<&str>) -> Option<String> {
    let extension = normalized_token(filename?.rsplit_once('.')?.1)?;
    if extension.is_empty() {
        return None;
    }

    Some(extension.into_owned())
}

pub(crate) fn resolve_syntax<'a>(
    state: &'a AppState,
    extension: Option<&str>,
) -> Option<&'a SyntaxReference> {
    let extension = normalized_token(extension?)?;
    let &index = state.syntax_index_by_token.get(extension.as_ref())?;
    state.syntax_set.syntaxes().get(index)
}

pub(crate) fn normalized_token(token: &str) -> Option<Cow<'_, str>> {
    let token = token.trim().trim_start_matches('.');
    if token.is_empty() {
        return None;
    }

    if token.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Some(Cow::Owned(token.to_ascii_lowercase()))
    } else {
        Some(Cow::Borrowed(token))
    }
}

fn syntax_for_rendering<'a>(
    state: &'a AppState,
    extension: Option<&str>,
    content: &str,
) -> Option<&'a SyntaxReference> {
    if content.len() > state.highlight_max_bytes {
        return None;
    }

    resolve_syntax(state, extension)
}

#[cfg(test)]
fn stored_language_for_syntax(syntax: &SyntaxReference) -> String {
    syntax
        .file_extensions
        .first()
        .cloned()
        .unwrap_or_else(|| syntax.name.clone())
}

/// Lines per `content-visibility: auto` chunk. Small enough that rendering a
/// chunk as it scrolls into view is a few milliseconds, large enough that even
/// a 300k-line paste stays in the low thousands of containment boundaries.
const CHUNK_LINES: usize = 256;

/// Cap on a chunk's estimated intrinsic width. The estimate only positions the
/// horizontal scrollbar before a chunk first renders (the `auto` keyword keeps
/// the real size afterwards); an unclamped multi-MB single line would ask the
/// browser for a layout area beyond what it supports.
const CHUNK_MAX_ESTIMATED_COLS: usize = 2000;

/// Render every line of `content` through `render_line` (which appends the
/// line's inner HTML — escaped text or syntect spans — to the buffer), wrapped
/// in numbered/anchored per-line markup and grouped into lazy-render chunks.
fn render_lines_chunked(content: &str, mut render_line: impl FnMut(&str, &mut String)) -> String {
    let total_lines = if content.is_empty() {
        1
    } else {
        bytecount::count(content.as_bytes(), b'\n') + usize::from(!content.ends_with('\n'))
    };

    // One width estimate for the whole paste (its widest line), shared by all
    // chunks. Chunk-local estimates would make the grid's max-content width
    // fluctuate as chunks render in, toggling/resizing the horizontal
    // scrollbar while scrolling.
    let mut max_cols = 1usize;
    for line in LinesWithEndings::from(content) {
        max_cols = max_cols.max(visual_columns(trim_line_ending(line)));
    }
    let max_cols = max_cols.min(CHUNK_MAX_ESTIMATED_COLS);

    let mut html = String::with_capacity(content.len() + content.len() / 4 + total_lines * 56);
    html.push_str("<div class=\"code-grid\"");
    let digits = decimal_digits(total_lines);
    if digits > 4 {
        // The stylesheet's default gutter fits 4-digit line numbers; widen it
        // for everything in this view via the CSS variable.
        let _ = write!(html, " style=\"--ln:calc({digits}ch + 1.2em)\"");
    }
    html.push('>');

    let mut chunk = String::new();
    let mut chunk_lines = 0usize;
    let mut line_html = String::new();
    let mut line_number = 0usize;

    for line in LinesWithEndings::from(content) {
        line_number += 1;
        line_html.clear();
        render_line(line, &mut line_html);
        push_line_html(&mut chunk, line_number, &line_html);
        chunk_lines += 1;
        if chunk_lines == CHUNK_LINES {
            flush_chunk(&mut html, &mut chunk, chunk_lines, max_cols);
            chunk_lines = 0;
        }
    }

    if line_number == 0 {
        push_line_html(&mut chunk, 1, "");
        chunk_lines = 1;
    }
    if chunk_lines > 0 {
        flush_chunk(&mut html, &mut chunk, chunk_lines, max_cols);
    }

    html.push_str("</div>");
    html
}

fn flush_chunk(output: &mut String, chunk: &mut String, lines: usize, cols: usize) {
    // Lines are 1.5em tall (the app's line-height); `auto` self-corrects any
    // estimation error once the chunk has rendered.
    let height_em = (lines * 3).div_ceil(2);
    let _ = write!(
        output,
        "<div class=\"code-chunk\" style=\"contain-intrinsic-size:auto {cols}ch auto {height_em}em\">"
    );
    output.push_str(chunk);
    output.push_str("</div>");
    chunk.clear();
}

/// Estimated rendered width of a line in `ch` units (tabs at the default
/// tab-size of 8). Only used for the chunk intrinsic-size hint.
fn visual_columns(line: &str) -> usize {
    line.chars()
        .map(|c| if c == '\t' { 8 } else { 1 })
        .sum::<usize>()
        .max(1)
}

fn decimal_digits(mut n: usize) -> usize {
    let mut digits = 1;
    while n >= 10 {
        n /= 10;
        digits += 1;
    }
    digits
}

fn push_line_html(output: &mut String, line_number: usize, line_html: &str) {
    let mut buf = itoa::Buffer::new();
    let n = buf.format(line_number);

    // Unquoted attribute values are valid HTML5 for these (no spaces/quotes)
    // and this markup is repeated once per line — bytes here are page weight.
    output.push_str("<code id=L");
    output.push_str(n);
    output.push_str("><a href=#L");
    output.push_str(n);
    output.push('>');
    output.push_str(n);
    output.push_str("</a>");
    output.push_str(line_html);
    output.push_str("</code>");
}

fn push_escaped_html(output: &mut String, value: &str) {
    let bytes = value.as_bytes();
    let mut last = 0;
    for (i, &b) in bytes.iter().enumerate() {
        let replacement = match b {
            b'&' => "&amp;",
            b'<' => "&lt;",
            b'>' => "&gt;",
            b'"' => "&quot;",
            b'\'' => "&#39;",
            _ => continue,
        };
        output.push_str(&value[last..i]);
        output.push_str(replacement);
        last = i + 1;
    }
    output.push_str(&value[last..]);
}

pub(crate) fn trim_line_ending(line: &str) -> &str {
    line.strip_suffix("\r\n")
        .or_else(|| line.strip_suffix('\n'))
        .or_else(|| line.strip_suffix('\r'))
        .unwrap_or(line)
}

#[cfg(test)]
mod tests {
    use syntect::dumps::from_uncompressed_data;
    use syntect::parsing::SyntaxSet;

    use super::{
        filename_extension, is_safe_markdown_url, push_escaped_html, render_lines_chunked,
        render_markdown_with, stored_language_for_syntax, trim_line_ending, truncate_for_render,
    };

    /// Code-block stub standing in for the real (state-dependent) highlighter.
    fn passthrough_code(_lang: Option<&str>, code: &str) -> String {
        format!("<pre><code>{code}</code></pre>")
    }

    /// The chunked renderer with the plain (non-highlighted) line renderer, so
    /// markup shape can be tested without an `AppState`.
    fn render_plain(content: &str) -> String {
        render_lines_chunked(content, |line, out| {
            push_escaped_html(out, trim_line_ending(line));
        })
    }

    #[test]
    fn renders_numbered_anchored_escaped_lines() {
        let out = render_plain("a<b\nc\n");
        assert!(out.starts_with("<div class=\"code-grid\">"), "{out}");
        assert!(
            out.contains("<code id=L1><a href=#L1>1</a>a&lt;b</code>"),
            "{out}"
        );
        assert!(
            out.contains("<code id=L2><a href=#L2>2</a>c</code>"),
            "{out}"
        );
        assert!(out.ends_with("</div></div>"), "{out}");
    }

    #[test]
    fn chunks_lines_with_intrinsic_size_hints() {
        let out = render_plain(&"x\n".repeat(600));
        // 600 lines at 256 lines per chunk: 256 + 256 + 88.
        assert_eq!(out.matches("<div class=\"code-chunk\"").count(), 3);
        // Full chunk: 1 column wide, 256 * 1.5em tall.
        assert!(
            out.contains("style=\"contain-intrinsic-size:auto 1ch auto 384em\""),
            "{out}"
        );
        // Numbering continues across chunk boundaries.
        assert!(out.contains("<code id=L257>"), "{out}");
        assert!(out.contains("<code id=L600>"), "{out}");
    }

    #[test]
    fn widens_gutter_for_five_digit_line_counts() {
        assert!(!render_plain(&"x\n".repeat(100)).contains("--ln:"));
        assert!(render_plain(&"x\n".repeat(10_000)).contains("--ln:calc(5ch + 1.2em)"));
    }

    #[test]
    fn estimates_chunk_width_from_longest_line() {
        // Widest line is "\tx": a tab (8 columns) plus one character.
        let out = render_plain("ab\n\tx\n");
        assert!(out.contains("contain-intrinsic-size:auto 9ch"), "{out}");
        // A multi-thousand-column line is clamped to the estimate cap.
        let out = render_plain(&"y".repeat(10_000));
        assert!(out.contains("contain-intrinsic-size:auto 2000ch"), "{out}");
    }

    #[test]
    fn all_chunks_share_the_global_width_estimate() {
        // 301 lines -> 2 chunks; the wide line is in the second, but both
        // must carry its width so the grid width (and with it the horizontal
        // scrollbar) stays stable as chunks lazy-render.
        let content = format!("{}{}\n", "short\n".repeat(300), "w".repeat(50));
        let out = render_plain(&content);
        assert_eq!(out.matches("<div class=\"code-chunk\"").count(), 2);
        assert_eq!(out.matches("contain-intrinsic-size:auto 50ch").count(), 2);
    }

    #[test]
    fn renders_empty_content_as_single_empty_line() {
        let out = render_plain("");
        assert!(
            out.contains("<code id=L1><a href=#L1>1</a></code>"),
            "{out}"
        );
        assert_eq!(out.matches("<code ").count(), 1);
    }

    #[test]
    fn escapes_raw_block_html_in_markdown() {
        let out = render_markdown_with("<script>alert('xss')</script>", passthrough_code);
        assert!(!out.contains("<script>"), "raw <script> leaked: {out}");
        assert!(
            out.contains("&lt;script&gt;"),
            "expected escaped script: {out}"
        );
    }

    #[test]
    fn escapes_raw_inline_html_in_markdown() {
        let out = render_markdown_with("hi <img src=x onerror=alert(1)> bye", passthrough_code);
        assert!(!out.contains("<img"), "raw <img> tag leaked: {out}");
        assert!(out.contains("&lt;img"), "expected escaped img: {out}");
    }

    #[test]
    fn neutralizes_javascript_link_scheme() {
        let out = render_markdown_with("[click](javascript:alert(1))", passthrough_code);
        assert!(
            !out.contains("javascript:"),
            "javascript: scheme leaked: {out}"
        );
        assert!(
            out.contains("href=\"#\""),
            "expected neutralized href: {out}"
        );
    }

    #[test]
    fn neutralizes_data_image_scheme() {
        let out = render_markdown_with("![x](data:text/html,<b>no</b>)", passthrough_code);
        assert!(
            !out.contains("data:text/html"),
            "data: scheme leaked: {out}"
        );
        assert!(out.contains("src=\"#\""), "expected neutralized src: {out}");
    }

    #[test]
    fn does_not_emit_heading_attributes() {
        let out = render_markdown_with("# hi {onclick=alert(1)}", passthrough_code);
        assert!(
            !out.contains("<h1 onclick"),
            "heading attribute leaked: {out}"
        );
    }

    #[test]
    fn keeps_safe_links_and_relative_paths() {
        let out = render_markdown_with(
            "[a](https://example.com/x) [b](/rel) [c](#frag)",
            passthrough_code,
        );
        assert!(
            out.contains("https://example.com/x"),
            "https link dropped: {out}"
        );
        assert!(
            out.contains("href=\"/rel\""),
            "relative link dropped: {out}"
        );
        assert!(
            out.contains("href=\"#frag\""),
            "fragment link dropped: {out}"
        );
    }

    #[test]
    fn code_blocks_still_render_via_callback() {
        let out = render_markdown_with("```rust\nlet x = 1;\n```", |lang, code| {
            format!(
                "<pre data-lang=\"{}\">{}</pre>",
                lang.unwrap_or(""),
                code.len()
            )
        });
        assert!(
            out.contains("data-lang=\"rust\""),
            "code block lang lost: {out}"
        );
    }

    #[test]
    fn rejects_overlong_and_hidden_schemes() {
        assert!(!is_safe_markdown_url("javascript:alert(1)"));
        assert!(!is_safe_markdown_url("java\tscript:alert(1)"));
        assert!(!is_safe_markdown_url("vbscript:x"));
        assert!(is_safe_markdown_url("https://example.com"));
        assert!(is_safe_markdown_url("HTTPS://EXAMPLE.COM"));
        assert!(is_safe_markdown_url("mailto:a@b.c"));
        // No scheme at all: relative paths/queries/fragments are safe even
        // when longer than any scheme buffer.
        assert!(is_safe_markdown_url("averylongrelativefilename.txt"));
        assert!(is_safe_markdown_url("dir/file:with:colons"));
        assert!(is_safe_markdown_url("#fragment"));
    }

    #[test]
    fn truncates_at_line_boundary() {
        let content = "one\ntwo\nthree\n";
        let (slice, truncated) = truncate_for_render(content, 1024, 2);
        assert_eq!(slice, "one\ntwo\n");
        assert!(truncated);

        let (slice, truncated) = truncate_for_render(content, 1024, 100);
        assert_eq!(slice, content);
        assert!(!truncated);
    }

    #[test]
    fn truncates_at_byte_budget() {
        let content = "aaaa\nbbbb\ncccc\n";
        let (slice, truncated) = truncate_for_render(content, 12, 100);
        assert_eq!(slice, "aaaa\nbbbb\n");
        assert!(truncated);
    }

    #[test]
    fn truncates_huge_single_line_at_char_boundary() {
        // 4-byte scalar values; an 11-byte budget must not split one.
        let content = "𝄞𝄞𝄞𝄞".repeat(100);
        let (slice, truncated) = truncate_for_render(&content, 11, 100);
        assert_eq!(slice.len(), 8);
        assert!(truncated);
        assert!(content.starts_with(slice));
    }

    fn load_syntax_set() -> SyntaxSet {
        from_uncompressed_data(include_bytes!("../syntaxes.bin"))
            .expect("failed to load syntaxes.bin")
    }

    #[test]
    fn extracts_lowercased_extension() {
        assert_eq!(
            filename_extension(Some("src/main.RS")),
            Some("rs".to_string())
        );
    }

    #[test]
    fn ignores_missing_extension() {
        assert_eq!(filename_extension(Some("Dockerfile")), None);
        assert_eq!(filename_extension(Some("file.")), None);
        assert_eq!(filename_extension(None), None);
    }

    #[test]
    fn prefers_extension_for_stored_language() {
        let syntax_set = load_syntax_set();
        let syntax = syntax_set
            .find_syntax_by_name("Rust")
            .expect("Rust syntax must exist");

        assert_eq!(stored_language_for_syntax(syntax), "rs");
    }
}
