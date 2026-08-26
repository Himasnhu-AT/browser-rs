//! Options, the per-tag conversion rules, and the public entry points.
//!
//! Every rule here is a line-by-line port of `markdownify` 1.2's
//! `MarkdownConverter`, restricted to the option set the reference implementation actually uses
//! (it calls `markdownify(body)` with no keyword arguments, so
//! [`MarkdownOptions::default`] *is* the reference implementation's configuration).

use std::borrow::Cow;
use std::sync::LazyLock;

use browser_dom::{Document, NodeRef};
use regex::Regex;

use crate::error::{MarkdownError, Result};
use crate::escape::{
    chomp, lstrip_chars, map_lines, py_rstrip, py_strip, rstrip_chars, strip_chars,
    ASCII_STRIP_CHARS,
};
use crate::traversal::{heading_level, ParentTags, Walker};

// ---------------------------------------------------------------------------
// whitespace-collapsing regexes (markdownify's module-level patterns)
// ---------------------------------------------------------------------------

/// `re_whitespace` — runs of tabs and spaces.
static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\t ]+").expect("the static pattern is valid"));

/// `re_all_whitespace` — runs of any ASCII whitespace.
static RE_ALL_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[\t \r\n]+").expect("the static pattern is valid"));

/// `re_newline_whitespace` — a run of whitespace containing at least one newline.
static RE_NEWLINE_WHITESPACE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[\t \r\n]*[\r\n][\t \r\n]*").expect("the static pattern is valid")
});

/// markdownify's `re_whitespace.sub(' ', text)`.
#[must_use]
pub fn collapse_inline_whitespace(text: &str) -> String {
    RE_WHITESPACE.replace_all(text, " ").into_owned()
}

/// markdownify's `re_all_whitespace.sub(' ', text)`.
#[must_use]
pub fn collapse_all_whitespace(text: &str) -> String {
    RE_ALL_WHITESPACE.replace_all(text, " ").into_owned()
}

/// markdownify's `re_newline_whitespace.sub('\n', text)`.
#[must_use]
pub fn collapse_newline_whitespace(text: &str) -> String {
    RE_NEWLINE_WHITESPACE.replace_all(text, "\n").into_owned()
}

// ---------------------------------------------------------------------------
// options
// ---------------------------------------------------------------------------

/// How `<h1>`…`<h6>` are rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum HeadingStyle {
    /// `# Heading`.
    Atx,
    /// `## Heading ##`.
    AtxClosed,
    /// Setext (`=====` / `-----`) for `h1`/`h2`, ATX for `h3`+. markdownify's
    /// default, and therefore the reference implementation's.
    #[default]
    Underlined,
}

/// How `<br>` is rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum NewlineStyle {
    /// Two trailing spaces then a newline. The default.
    #[default]
    Spaces,
    /// A backslash then a newline.
    Backslash,
}

/// What `strip_document` does to the finished document.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum DocumentStrip {
    /// Drop leading separation newlines.
    Lstrip,
    /// Drop trailing separation newlines.
    Rstrip,
    /// Drop both. The default.
    #[default]
    Strip,
}

/// What `strip_pre` does to a `<pre>` body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum PreStrip {
    /// Drop every leading and trailing newline. The default.
    #[default]
    Strip,
    /// Drop at most one leading and one trailing newline.
    StripOne,
}

/// Every markdownify option, pre-filled with markdownify's defaults.
///
/// The reference implementation passes no options at all, so `MarkdownOptions::default()` is the
/// configuration the port has to match byte for byte.
///
/// The field list mirrors markdownify's `DefaultOptions` and is therefore
/// fixed, so the struct is **not** `#[non_exhaustive]` — downstream crates are
/// meant to write `MarkdownOptions { wrap: true, ..Default::default() }`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownOptions {
    /// Emit `<https://example.com>` when the link text equals the href.
    pub autolinks: bool,
    /// Bullet characters, cycled by `<ul>` nesting depth. Default `"*+-"`.
    pub bullets: String,
    /// Info string put on fenced `<pre>` blocks. Default empty.
    pub code_language: String,
    /// Only convert these tags; everything else passes its text through.
    /// Mutually exclusive with [`MarkdownOptions::strip`].
    pub convert: Option<Vec<String>>,
    /// Use the href as the link title when no `title` attribute is present.
    pub default_title: bool,
    /// Escape `*` as `\*` in text nodes. On by default.
    pub escape_asterisks: bool,
    /// Escape `_` as `\_` in text nodes. On by default.
    pub escape_underscores: bool,
    /// Escape the wider Markdown punctuation set. **Off** by default.
    pub escape_misc: bool,
    /// Heading rendering. Default [`HeadingStyle::Underlined`].
    pub heading_style: HeadingStyle,
    /// Parent tags in which `<img>` survives inside `_inline` context.
    pub keep_inline_images_in: Vec<String>,
    /// `<br>` rendering. Default [`NewlineStyle::Spaces`].
    pub newline_style: NewlineStyle,
    /// Do not convert these tags. Mutually exclusive with
    /// [`MarkdownOptions::convert`].
    pub strip: Option<Vec<String>>,
    /// Trimming applied to the whole document. Default
    /// `Some(DocumentStrip::Strip)`.
    pub strip_document: Option<DocumentStrip>,
    /// Trimming applied inside `<pre>`. Default `Some(PreStrip::Strip)`.
    pub strip_pre: Option<PreStrip>,
    /// The character doubled for `<strong>` and used singly for `<em>`.
    pub strong_em_symbol: char,
    /// Markup wrapped around `<sub>`. Empty by default (the tag vanishes).
    pub sub_symbol: String,
    /// Markup wrapped around `<sup>`. Empty by default.
    pub sup_symbol: String,
    /// Treat a table's first row as a header even when it holds `<td>` cells.
    pub table_infer_header: bool,
    /// Hard-wrap paragraph text. Off by default.
    pub wrap: bool,
    /// Column to wrap at when [`MarkdownOptions::wrap`] is on.
    pub wrap_width: Option<usize>,
}

impl Default for MarkdownOptions {
    fn default() -> Self {
        Self {
            autolinks: true,
            bullets: "*+-".to_owned(),
            code_language: String::new(),
            convert: None,
            default_title: false,
            escape_asterisks: true,
            escape_underscores: true,
            escape_misc: false,
            heading_style: HeadingStyle::Underlined,
            keep_inline_images_in: Vec::new(),
            newline_style: NewlineStyle::Spaces,
            strip: None,
            strip_document: Some(DocumentStrip::Strip),
            strip_pre: Some(PreStrip::Strip),
            strong_em_symbol: '*',
            sub_symbol: String::new(),
            sup_symbol: String::new(),
            table_infer_header: false,
            wrap: false,
            wrap_width: Some(80),
        }
    }
}

impl MarkdownOptions {
    /// markdownify's `__init__` validation.
    ///
    /// # Errors
    /// [`MarkdownError::StripAndConvert`] when both `strip` and `convert` are set.
    pub fn validate(&self) -> Result<()> {
        if self.strip.is_some() && self.convert.is_some() {
            return Err(MarkdownError::StripAndConvert);
        }
        Ok(())
    }

    /// markdownify's `should_convert_tag`.
    #[must_use]
    pub fn should_convert_tag(&self, tag: &str) -> bool {
        if let Some(strip) = &self.strip {
            return !strip.iter().any(|name| name == tag);
        }
        if let Some(convert) = &self.convert {
            return convert.iter().any(|name| name == tag);
        }
        true
    }
}

// ---------------------------------------------------------------------------
// entry points
// ---------------------------------------------------------------------------

/// Convert `node` and everything under it to Markdown.
///
/// `node` may be a [`browser_dom::Document`] root (`doc.root()`) or any
/// element. In either case it plays the part of BeautifulSoup's `[document]`
/// wrapper: nothing above or beside it is visible to the converter, which is
/// exactly what the reference implementation gets by re-parsing `element.html_content`.
///
/// ```
/// use browser_dom::Document;
/// use browser_markdown::{to_markdown, MarkdownOptions};
///
/// let doc = Document::parse("<h1>Title</h1><p>Body</p>");
/// let md = to_markdown(doc.root(), &MarkdownOptions::default());
/// assert_eq!(md, "Title\n=====\n\nBody");
/// ```
#[must_use]
pub fn to_markdown(node: NodeRef<'_>, opts: &MarkdownOptions) -> String {
    let walker = Walker::new(opts, node);
    let text = walker.process_element(node, ParentTags::empty());
    if node.is_document() {
        // `convert__document_` already ran as this node's own converter.
        text
    } else if opts.should_convert_tag(crate::traversal::DOCUMENT_NAME) {
        // Python wraps the element in a fresh soup, whose own converter is
        // `convert__document_` — and `strip`/`convert` can switch that off too.
        strip_document(&text, opts)
    } else {
        text
    }
}

/// [`to_markdown`] with markdownify's defaults.
#[must_use]
pub fn to_markdown_default(node: NodeRef<'_>) -> String {
    to_markdown(node, &MarkdownOptions::default())
}

/// The parser flags markdownify is written against.
///
/// markdownify hands its input to `BeautifulSoup(html, 'html.parser')`, and
/// `html.parser` keeps **every** whitespace-only text node. Its whitespace
/// rules (`_can_ignore`, `should_remove_whitespace_inside/outside`, `chomp`)
/// exist precisely to decide which of those nodes matter, so a tree that has
/// already had them deleted silently loses the space in
/// `<em>a</em> <strong>b</strong>`.
///
/// [`browser_dom`]'s default `remove_blank_text` deletes every whitespace-only
/// text node outside `pre`/`textarea`, which is stricter than libxml2's
/// `areBlanks()` (that one keeps whitespace in mixed content). So the crate's
/// own string entry points turn it off — see the deviation table in the crate
/// README.
#[must_use]
fn markdownify_parse_options() -> browser_dom::ParseOptions {
    browser_dom::ParseOptions::new().remove_blank_text(false)
}

/// Parse an HTML string exactly the way markdownify does.
///
/// Two departures from [`Document::parse`], both forced by
/// `BeautifulSoup(html, "html.parser")`:
///
/// * whitespace-only text nodes survive — see [`markdownify_parse_options`];
/// * the input is **not** trimmed. `Selector(content)` runs `content.strip()`
///   before parsing and [`browser_dom::parse::prepare_str`] mirrors it, but
///   markdownify hands the string straight to the tokenizer, and
///   `convert__document_` then strips only newlines — so `markdownify("  x  ")`
///   is `"  x  "`, not `"x"`. Going through the byte entry point skips the trim
///   while keeping the NUL stripping and the empty-input fallback.
fn parse_like_markdownify(html: &str) -> Document {
    Document::parse_bytes(html.as_bytes(), &markdownify_parse_options())
}

/// The reference implementation's `Convertor._convert_to_markdown(body)`: parse an HTML string and
/// convert the whole document.
#[must_use]
pub fn html_to_markdown(html: &str) -> String {
    let doc = parse_like_markdownify(html);
    to_markdown(doc.root(), &MarkdownOptions::default())
}

/// A validated, reusable converter.
///
/// Cheaper than [`to_markdown`] when converting many nodes with the same
/// non-default options, and the only way to surface
/// [`MarkdownError::StripAndConvert`].
#[derive(Debug, Clone, Default)]
pub struct MarkdownConverter {
    opts: MarkdownOptions,
}

impl MarkdownConverter {
    /// Build a converter.
    ///
    /// # Errors
    /// [`MarkdownError::StripAndConvert`] when `opts` sets both `strip` and
    /// `convert`, exactly as markdownify's constructor raises `ValueError`.
    pub fn new(opts: MarkdownOptions) -> Result<Self> {
        opts.validate()?;
        Ok(Self { opts })
    }

    /// The options in force.
    #[must_use]
    pub fn options(&self) -> &MarkdownOptions {
        &self.opts
    }

    /// Convert one node.
    #[must_use]
    pub fn convert(&self, node: NodeRef<'_>) -> String {
        to_markdown(node, &self.opts)
    }

    /// Parse `html` and convert the resulting document.
    ///
    /// The parse matches markdownify's own: whitespace-only text nodes are kept,
    /// comments are dropped, and CDATA is folded into text.
    #[must_use]
    pub fn convert_html(&self, html: &str) -> String {
        let doc = parse_like_markdownify(html);
        to_markdown(doc.root(), &self.opts)
    }
}

/// markdownify's `convert__document_`.
fn strip_document(text: &str, opts: &MarkdownOptions) -> String {
    match opts.strip_document {
        Some(DocumentStrip::Lstrip) => text.trim_start_matches('\n').to_owned(),
        Some(DocumentStrip::Rstrip) => text.trim_end_matches('\n').to_owned(),
        Some(DocumentStrip::Strip) => text.trim_matches('\n').to_owned(),
        None => text.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// pre stripping
// ---------------------------------------------------------------------------

/// markdownify's `strip_pre()`: `^[ \n]*\n` and `[ \n]*$`.
#[must_use]
pub fn strip_pre_all(text: &str) -> &str {
    let text = drop_leading_run_through_last_newline(text);
    rstrip_chars(text, &[' ', '\n'])
}

/// markdownify's `strip1_pre()`: `^ *\n` and `\n *$`.
#[must_use]
pub fn strip_pre_one(text: &str) -> &str {
    let mut out = text;
    let leading_spaces = out.len() - out.trim_start_matches(' ').len();
    if out[leading_spaces..].starts_with('\n') {
        out = &out[leading_spaces + 1..];
    }
    let trailing_spaces = out.len() - out.trim_end_matches(' ').len();
    let head = &out[..out.len() - trailing_spaces];
    if let Some(stripped) = head.strip_suffix('\n') {
        out = stripped;
    }
    out
}

/// The `^[ \n]*\n` half of `strip_pre`: greedy over spaces and newlines, then
/// backtracked so the match ends on the last newline of the run.
fn drop_leading_run_through_last_newline(text: &str) -> &str {
    let run = text.len() - text.trim_start_matches([' ', '\n']).len();
    match text[..run].rfind('\n') {
        Some(index) => &text[index + 1..],
        None => text,
    }
}

// ---------------------------------------------------------------------------
// the per-tag conversion rules
// ---------------------------------------------------------------------------

impl Walker<'_> {
    /// markdownify's `get_conv_fn` + the call to it.
    ///
    /// `None` means "no converter for this tag": the joined child text is used
    /// unchanged.
    #[must_use]
    pub(crate) fn convert_tag(
        &self,
        name: &str,
        el: NodeRef<'_>,
        text: &str,
        parent_tags: ParentTags,
    ) -> Option<String> {
        let opts = self.options();
        if !opts.should_convert_tag(name) {
            return None;
        }
        let converted = match name {
            crate::traversal::DOCUMENT_NAME => strip_document(text, opts),
            "a" => self.convert_a(el, text, parent_tags),
            "b" | "strong" => {
                let symbol = opts.strong_em_symbol.to_string();
                inline_conversion(&format!("{symbol}{symbol}"), text, parent_tags)
            }
            "em" | "i" => inline_conversion(&opts.strong_em_symbol.to_string(), text, parent_tags),
            "del" | "s" => inline_conversion("~~", text, parent_tags),
            "sub" => inline_conversion(&opts.sub_symbol, text, parent_tags),
            "sup" => inline_conversion(&opts.sup_symbol, text, parent_tags),
            "blockquote" => self.convert_blockquote(text, parent_tags),
            "br" => self.convert_br(text, parent_tags),
            "code" | "kbd" | "samp" => convert_code(text, parent_tags),
            "div" | "article" | "section" | "dl" => convert_div(text, parent_tags),
            "dd" => convert_dd(text, parent_tags),
            "dt" => convert_dt(text, parent_tags),
            "hr" => "\n\n---\n\n".to_owned(),
            "img" => self.convert_img(el, parent_tags),
            "video" => self.convert_video(el, text, parent_tags),
            "ul" | "ol" | "list" => self.convert_list(el, text, parent_tags),
            "li" => self.convert_li(el, text),
            "p" => self.convert_p(text, parent_tags),
            "pre" => self.convert_pre(text),
            "q" => format!("\"{text}\""),
            "script" | "style" => String::new(),
            "table" => format!("\n\n{}\n\n", py_strip(text)),
            "caption" => format!("{}\n\n", py_strip(text)),
            "figcaption" => format!("\n\n{}\n\n", py_strip(text)),
            "td" | "th" => convert_cell(el, text),
            "tr" => self.convert_tr(el, text),
            _ => {
                let level = heading_level(name)?;
                self.convert_heading(level, text, parent_tags)
            }
        };
        Some(converted)
    }

    /// markdownify's `convert_a`.
    fn convert_a(&self, el: NodeRef<'_>, text: &str, parent_tags: ParentTags) -> String {
        if parent_tags.contains(ParentTags::NOFORMAT) {
            return text.to_owned();
        }
        let (prefix, suffix, text) = chomp(text);
        if text.is_empty() {
            return String::new();
        }
        let opts = self.options();
        let href = el.attr("href");
        let mut title = el.attr("title").map(str::to_owned);

        if opts.autolinks
            && href.is_some_and(|href| text.replace(r"\_", "_") == href)
            && !title.as_ref().is_some_and(|t| !t.is_empty())
            && !opts.default_title
        {
            return format!("<{}>", href.unwrap_or_default());
        }
        if opts.default_title && !title.as_ref().is_some_and(|t| !t.is_empty()) {
            title = href.map(str::to_owned);
        }
        let title_part = match &title {
            Some(title) if !title.is_empty() => {
                format!(" \"{}\"", title.replace('"', "\\\""))
            }
            _ => String::new(),
        };
        match href {
            // A present-but-empty href is falsy in Python.
            Some(href) if !href.is_empty() => {
                format!("{prefix}[{text}]({href}{title_part}){suffix}")
            }
            _ => text.to_owned(),
        }
    }

    /// markdownify's `convert_blockquote`.
    fn convert_blockquote(&self, text: &str, parent_tags: ParentTags) -> String {
        let text = strip_chars(text, &ASCII_STRIP_CHARS);
        if parent_tags.contains(ParentTags::INLINE) {
            return format!(" {text} ");
        }
        if text.is_empty() {
            return "\n".to_owned();
        }
        let quoted = map_lines(text, |line| {
            if line.is_empty() {
                ">".to_owned()
            } else {
                format!("> {line}")
            }
        });
        format!("\n{quoted}\n\n")
    }

    /// markdownify's `convert_br`.
    fn convert_br(&self, text: &str, parent_tags: ParentTags) -> String {
        if parent_tags.contains(ParentTags::INLINE) {
            return if text.is_empty() {
                " ".to_owned()
            } else {
                format!("{text} ")
            };
        }
        match self.options().newline_style {
            NewlineStyle::Backslash => format!("\\\n{text}"),
            NewlineStyle::Spaces => format!("  \n{text}"),
        }
    }

    /// markdownify's `convert_hN`.
    fn convert_heading(&self, level: u32, text: &str, parent_tags: ParentTags) -> String {
        if parent_tags.contains(ParentTags::INLINE) {
            return text.to_owned();
        }
        let level = level.clamp(1, 6);
        let text = py_strip(text);
        let style = self.options().heading_style;
        if style == HeadingStyle::Underlined && level <= 2 {
            let pad = if level == 1 { '=' } else { '-' };
            return underline(text, pad);
        }
        let text = collapse_all_whitespace(text);
        let hashes = "#".repeat(level as usize);
        if style == HeadingStyle::AtxClosed {
            format!("\n\n{hashes} {text} {hashes}\n\n")
        } else {
            format!("\n\n{hashes} {text}\n\n")
        }
    }

    /// markdownify's `convert_img`.
    fn convert_img(&self, el: NodeRef<'_>, parent_tags: ParentTags) -> String {
        let alt = el.attr("alt").unwrap_or("");
        let src = el.attr("src").unwrap_or("");
        let title = el.attr("title").unwrap_or("");
        let title_part = if title.is_empty() {
            String::new()
        } else {
            format!(" \"{}\"", title.replace('"', "\\\""))
        };
        if parent_tags.contains(ParentTags::INLINE) && !self.parent_keeps_inline_images(el) {
            return alt.to_owned();
        }
        format!("![{alt}]({src}{title_part})")
    }

    /// markdownify's `convert_video`.
    fn convert_video(&self, el: NodeRef<'_>, text: &str, parent_tags: ParentTags) -> String {
        if parent_tags.contains(ParentTags::INLINE) && !self.parent_keeps_inline_images(el) {
            return text.to_owned();
        }
        let mut src = el.attr("src").unwrap_or("").to_owned();
        if src.is_empty() {
            if let Some(source) = el
                .descendant_elements()
                .find(|node| node.tag() == "source" && node.attr("src").is_some())
            {
                src = source.attr("src").unwrap_or("").to_owned();
            }
        }
        let poster = el.attr("poster").unwrap_or("");
        match (src.is_empty(), poster.is_empty()) {
            (false, false) => format!("[![{text}]({poster})]({src})"),
            (false, true) => format!("[{text}]({src})"),
            (true, false) => format!("![{text}]({poster})"),
            (true, true) => text.to_owned(),
        }
    }

    fn parent_keeps_inline_images(&self, el: NodeRef<'_>) -> bool {
        let Some(parent) = self.parent_of(el) else {
            return false;
        };
        let Some(name) = parent.local_name() else {
            return false;
        };
        self.options()
            .keep_inline_images_in
            .iter()
            .any(|allowed| allowed == name)
    }

    /// markdownify's `convert_list` (`<ul>`, `<ol>`).
    fn convert_list(&self, el: NodeRef<'_>, text: &str, parent_tags: ParentTags) -> String {
        let before_paragraph = self
            .next_block_content_sibling(el)
            .is_some_and(|sibling| !matches!(sibling.local_name(), Some("ul" | "ol")));
        if parent_tags.contains(ParentTags::LI) {
            return format!("\n{}", py_rstrip(text));
        }
        if before_paragraph {
            format!("\n\n{text}\n")
        } else {
            format!("\n\n{text}")
        }
    }

    /// markdownify's `convert_li`.
    fn convert_li(&self, el: NodeRef<'_>, text: &str) -> String {
        let text = py_strip(text);
        if text.is_empty() {
            return "\n".to_owned();
        }
        let bullet = self.li_bullet(el);
        let bullet_width = bullet.chars().count();
        let indent = " ".repeat(bullet_width);
        let indented = map_lines(text, |line| {
            if line.is_empty() {
                String::new()
            } else {
                format!("{indent}{line}")
            }
        });
        // The first line always starts with `bullet_width` ASCII spaces because
        // `text` was stripped, so byte and character offsets coincide.
        let body = indented.get(bullet_width..).unwrap_or("");
        format!("{bullet}{body}\n")
    }

    /// The bullet (or `N.`) an `<li>` is rendered with, plus its trailing space.
    fn li_bullet(&self, el: NodeRef<'_>) -> String {
        let parent = self.parent_of(el);
        if parent.is_some_and(|parent| parent.local_name() == Some("ol")) {
            // `if parent.get("start") and str(parent.get("start")).isnumeric():
            //      start = int(parent.get("start"))
            //  else: start = 1`
            let start = parent
                .and_then(|parent| parent.attr("start"))
                .filter(|value| !value.is_empty())
                .and_then(to_ascii_digits)
                .unwrap_or_else(|| "1".to_owned());
            let previous = el
                .prev_siblings()
                .filter(|sibling| sibling.local_name() == Some("li"))
                .count();
            return format!("{}. ", decimal_add(&start, previous));
        }
        // `depth = -1`, then +1 for every `<ul>` from this node up to the root.
        let mut depth: i64 = -1;
        let mut current = Some(el);
        while let Some(node) = current {
            if node.local_name() == Some("ul") {
                depth += 1;
            }
            current = self.parent_of(node);
        }
        let bullets: Vec<char> = self.options().bullets.chars().collect();
        if bullets.is_empty() {
            return " ".to_owned();
        }
        let len = i64::try_from(bullets.len()).unwrap_or(1);
        let index = usize::try_from(depth.rem_euclid(len)).unwrap_or(0);
        match bullets.get(index) {
            Some(bullet) => format!("{bullet} "),
            None => " ".to_owned(),
        }
    }

    /// markdownify's `convert_p`.
    fn convert_p(&self, text: &str, parent_tags: ParentTags) -> String {
        if parent_tags.contains(ParentTags::INLINE) {
            return format!(" {} ", strip_chars(text, &ASCII_STRIP_CHARS));
        }
        let text = strip_chars(text, &ASCII_STRIP_CHARS);
        let opts = self.options();
        let text: Cow<'_, str> = if opts.wrap {
            match opts.wrap_width {
                Some(width) => Cow::Owned(wrap_paragraph(text, width)),
                None => Cow::Borrowed(text),
            }
        } else {
            Cow::Borrowed(text)
        };
        if text.is_empty() {
            String::new()
        } else {
            format!("\n\n{text}\n\n")
        }
    }

    /// markdownify's `convert_pre`.
    fn convert_pre(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let opts = self.options();
        let text = match opts.strip_pre {
            Some(PreStrip::Strip) => strip_pre_all(text),
            Some(PreStrip::StripOne) => strip_pre_one(text),
            None => text,
        };
        format!("\n\n```{}\n{text}\n```\n\n", opts.code_language)
    }
}

/// A single Unicode decimal digit — the character class `int()` accepts.
static RE_UNICODE_DIGIT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\p{Nd}$").expect("the static pattern is valid"));

/// The value of a Unicode decimal digit, or `None` if `c` is not one.
///
/// `int("٩٨")` is `98`, so a non-ASCII `<ol start>` has to be read the same
/// way. Rust's `char::to_digit` is ASCII-only and the numeric-value tables are
/// not in `std`, so the value is recovered from the block: every `Nd` run in
/// Unicode is a whole number of ten-digit blocks laid out `0..=9` in order, so
/// the offset from the run's first character, modulo ten, *is* the value.
fn unicode_decimal_value(c: char) -> Option<u32> {
    let mut buffer = [0u8; 4];
    let is_digit = |c: char| RE_UNICODE_DIGIT.is_match(c.encode_utf8(&mut [0u8; 4]));
    if !RE_UNICODE_DIGIT.is_match(c.encode_utf8(&mut buffer)) {
        return None;
    }
    let mut run_start = c as u32;
    while run_start > 0 {
        match char::from_u32(run_start - 1) {
            Some(previous) if is_digit(previous) => run_start -= 1,
            _ => break,
        }
    }
    Some((c as u32 - run_start) % 10)
}

/// `int(digits)` for a string `str.isnumeric()` accepts, as ASCII digits.
///
/// Returns `None` when `int()` itself would raise — that is, when some
/// character is numeric (`Nl`/`No`, e.g. `²` or `Ⅰ`) but has no decimal value.
fn to_ascii_digits(digits: &str) -> Option<String> {
    digits
        .chars()
        .map(|c| unicode_decimal_value(c).map(|value| char::from(b'0' + value as u8)))
        .collect()
}

/// `int(decimal) + addend` with Python's arbitrary precision.
///
/// markdownify numbers an ordered list with `int(parent['start']) +
/// len(el.find_previous_siblings('li'))`, and a Python `int` has no width, so
/// `<ol start="99999999999999999999">` really does render
/// `99999999999999999999.`. `decimal` must be a non-empty ASCII digit string;
/// leading zeros are dropped the way `int()` drops them.
fn decimal_add(decimal: &str, addend: usize) -> String {
    // Little-endian digits, so the carry walks forwards.
    let mut digits: Vec<u8> = decimal
        .bytes()
        .rev()
        .map(|byte| byte.wrapping_sub(b'0'))
        .collect();
    let mut carry = addend;
    let mut position = 0;
    while carry > 0 {
        if position == digits.len() {
            digits.push(0);
        }
        let sum = usize::from(digits[position]) + carry % 10;
        digits[position] = (sum % 10) as u8;
        carry = carry / 10 + sum / 10;
        position += 1;
    }
    while digits.len() > 1 && digits.last() == Some(&0) {
        digits.pop();
    }
    digits
        .iter()
        .rev()
        .map(|digit| char::from(b'0' + digit))
        .collect()
}

/// markdownify's `abstract_inline_conversion`.
fn inline_conversion(markup_prefix: &str, text: &str, parent_tags: ParentTags) -> String {
    let markup_suffix = if markup_prefix.starts_with('<') && markup_prefix.ends_with('>') {
        format!("</{}", &markup_prefix[1..])
    } else {
        markup_prefix.to_owned()
    };
    if parent_tags.contains(ParentTags::NOFORMAT) {
        return text.to_owned();
    }
    let (prefix, suffix, text) = chomp(text);
    if text.is_empty() {
        return String::new();
    }
    format!("{prefix}{markup_prefix}{text}{markup_suffix}{suffix}")
}

/// markdownify's `convert_code` (shared by `code`, `kbd` and `samp`).
fn convert_code(text: &str, parent_tags: ParentTags) -> String {
    if parent_tags.contains(ParentTags::NOFORMAT) {
        return text.to_owned();
    }
    let (prefix, suffix, text) = chomp(text);
    if text.is_empty() {
        return String::new();
    }
    let max_backticks = longest_backtick_run(text);
    let delimiter = "`".repeat(max_backticks + 1);
    if max_backticks > 0 {
        format!("{prefix}{delimiter} {text} {delimiter}{suffix}")
    } else {
        format!("{prefix}{delimiter}{text}{delimiter}{suffix}")
    }
}

/// markdownify's `re_backtick_runs` reduced to a scan.
fn longest_backtick_run(text: &str) -> usize {
    let mut best = 0;
    let mut current = 0;
    for c in text.chars() {
        if c == '`' {
            current += 1;
            best = best.max(current);
        } else {
            current = 0;
        }
    }
    best
}

/// markdownify's `convert_div` (shared by `article`, `section` and `dl`).
fn convert_div(text: &str, parent_tags: ParentTags) -> String {
    if parent_tags.contains(ParentTags::INLINE) {
        return format!(" {} ", py_strip(text));
    }
    let text = py_strip(text);
    if text.is_empty() {
        String::new()
    } else {
        format!("\n\n{text}\n\n")
    }
}

/// markdownify's `convert_dd`.
fn convert_dd(text: &str, parent_tags: ParentTags) -> String {
    let text = py_strip(text);
    if parent_tags.contains(ParentTags::INLINE) {
        return format!(" {text} ");
    }
    if text.is_empty() {
        return "\n".to_owned();
    }
    let indented = map_lines(text, |line| {
        if line.is_empty() {
            String::new()
        } else {
            format!("    {line}")
        }
    });
    // Overwrite the first of the four indent spaces with the definition marker.
    let body = indented.get(1..).unwrap_or("");
    format!(":{body}\n")
}

/// markdownify's `convert_dt`.
fn convert_dt(text: &str, parent_tags: ParentTags) -> String {
    let text = collapse_all_whitespace(py_strip(text));
    if parent_tags.contains(ParentTags::INLINE) {
        return format!(" {text} ");
    }
    if text.is_empty() {
        return "\n".to_owned();
    }
    format!("\n\n{text}\n")
}

/// markdownify's `underline()`.
fn underline(text: &str, pad_char: char) -> String {
    let text = py_rstrip(text);
    if text.is_empty() {
        return String::new();
    }
    let rule: String = std::iter::repeat_n(pad_char, text.chars().count()).collect();
    format!("\n\n{text}\n{rule}\n\n")
}

/// markdownify's `convert_td` / `convert_th`, which are identical.
fn convert_cell(el: NodeRef<'_>, text: &str) -> String {
    let colspan = crate::tables::colspan(el);
    let body = py_strip(text).replace('\n', " ");
    format!(" {body}{}", " |".repeat(colspan))
}

/// markdownify's `convert_p` wrap loop: wrap each `<br>`-separated line, then
/// put the line's original trailing whitespace back.
fn wrap_paragraph(text: &str, width: usize) -> String {
    let mut out = String::with_capacity(text.len());
    for (index, line) in text.split('\n').enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let line = lstrip_chars(line, &ASCII_STRIP_CHARS);
        let trimmed = py_rstrip(line);
        let trailing = &line[trimmed.len()..];
        out.push_str(&wrap_line(trimmed, width));
        out.push_str(trailing);
    }
    out
}

/// A port of `textwrap.fill(break_long_words=False, break_on_hyphens=False)`.
///
/// `textwrap` splits on `(\s+)` keeping the separators, then greedily packs
/// chunks; `drop_whitespace` discards a whitespace chunk at either end of a
/// line, and `break_long_words=False` gives an over-long word a line of its
/// own. Keeping the whitespace chunks verbatim is what makes
/// `"Chomping  **spaced**"` keep both of its spaces.
///
/// Not ported: `expand_tabs`. It runs before `replace_whitespace` in
/// `textwrap._munge_whitespace`, but by the time `convert_p` runs with `wrap`
/// on, `process_text` has already collapsed every `[\t \r\n]` run to a single
/// space — so no tab can reach here. A vertical tab or form feed *can*, because
/// markdownify's collapse class does not include them, and those are handled by
/// `replace_whitespace` below.
fn wrap_line(line: &str, width: usize) -> String {
    // `textwrap` rejects a width below 1; markdownify never passes one.
    let width = width.max(1);
    // `replace_whitespace=True`: `unicode_whitespace_trans` maps each of
    // `\t\n\x0b\x0c\r` to a plain space.
    let line = &replace_whitespace(line);
    let mut chunks = split_keeping_whitespace(line);
    chunks.reverse(); // so the greedy loop can `pop()` like Python does
    let mut lines: Vec<String> = Vec::new();

    while !chunks.is_empty() {
        let mut current: Vec<&str> = Vec::new();
        let mut current_len = 0usize;

        // drop_whitespace: no leading whitespace on a continuation line
        if !lines.is_empty() && chunks.last().is_some_and(|c| py_strip(c).is_empty()) {
            chunks.pop();
        }
        while let Some(chunk) = chunks.last() {
            let len = chunk.chars().count();
            if current_len + len > width {
                break;
            }
            current_len += len;
            if let Some(chunk) = chunks.pop() {
                current.push(chunk);
            }
        }
        // _handle_long_word with break_long_words=False: a word wider than the
        // column gets a line to itself rather than being cut.
        if current.is_empty() {
            if let Some(chunk) = chunks.pop() {
                current.push(chunk);
            }
        }
        // drop_whitespace: no trailing whitespace either
        if current.last().is_some_and(|c| py_strip(c).is_empty()) {
            current.pop();
        }
        if !current.is_empty() {
            lines.push(current.concat());
        }
    }
    lines.join("\n")
}

/// `textwrap`'s `replace_whitespace` step.
fn replace_whitespace(line: &str) -> String {
    line.chars()
        .map(|c| match c {
            '\t' | '\n' | '\u{b}' | '\u{c}' | '\r' => ' ',
            other => other,
        })
        .collect()
}

/// `textwrap`'s `wordsep_simple_re`: split on runs of whitespace, keeping the
/// separators as chunks of their own.
fn split_keeping_whitespace(line: &str) -> Vec<&str> {
    let mut chunks = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let boundary = match rest.starts_with(char::is_whitespace) {
            true => rest.find(|c: char| !c.is_whitespace()),
            false => rest.find(char::is_whitespace),
        };
        let end = boundary.unwrap_or(rest.len());
        chunks.push(&rest[..end]);
        rest = &rest[end..];
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use browser_dom::Document;

    fn md(html: &str) -> String {
        html_to_markdown(html)
    }

    #[test]
    fn setext_headings() {
        assert_eq!(md("<h1>Title</h1>"), "Title\n=====");
        assert_eq!(md("<h2>Sub</h2>"), "Sub\n---");
        assert_eq!(md("<h3>Deep</h3>"), "### Deep");
        assert_eq!(md("<h9>Deeper</h9>"), "###### Deeper");
    }

    #[test]
    fn escapes_asterisks_and_underscores() {
        assert_eq!(md("<p>a*b_c</p>"), r"a\*b\_c");
    }

    #[test]
    fn bullet_cycling_by_depth() {
        assert_eq!(
            md("<ul><li>a<ul><li>b<ul><li>c</li></ul></li></ul></li></ul>"),
            "* a\n  + b\n    - c"
        );
    }

    #[test]
    fn ordered_list_start_attribute() {
        assert_eq!(
            md("<ol start=\"3\"><li>a</li><li>b</li></ol>"),
            "3. a\n4. b"
        );
    }

    /// The HTML5 tokenizer drops one U+000A straight after `<pre>`/`<textarea>`;
    /// libxml2 does not. The reference implementation never sees the difference because
    /// `strip_pre = Strip` removes leading newlines anyway, but the non-default
    /// `strip_pre` settings do, so the deviation is pinned rather than implied.
    #[test]
    fn pre_drops_one_leading_newline_at_the_tokenizer() {
        // The default: identical either way.
        assert_eq!(md("<pre>\n\nkeep\n</pre>"), "```\nkeep\n```");
        // With the stripping turned off, the eaten newline becomes visible.
        // Python's answer for this input is "```\n\n\nkeep\n\n```".
        let opts = MarkdownOptions {
            strip_pre: None,
            ..MarkdownOptions::default()
        };
        let doc = parse_like_markdownify("<pre>\n\nkeep\n</pre>");
        assert_eq!(to_markdown(doc.root(), &opts), "```\n\nkeep\n\n```");
    }

    #[test]
    fn ordered_list_start_is_arbitrary_precision() {
        // `int()` has no width in Python, so neither may this.
        assert_eq!(
            md("<ol start=\"99999999999999999999\"><li>a</li><li>b</li></ol>"),
            "99999999999999999999. a\n100000000000000000000. b"
        );
        // `int()` drops leading zeros.
        assert_eq!(md("<ol start=\"007\"><li>a</li></ol>"), "7. a");
        // `str.isnumeric()` is false for a signed or spaced value: start = 1.
        assert_eq!(md("<ol start=\"-3\"><li>a</li></ol>"), "1. a");
    }

    #[test]
    fn ordered_list_start_reads_unicode_decimal_digits() {
        // `int("\u0669\u0668")` is 98, so Arabic-Indic digits number the list.
        assert_eq!(md("<ol start=\"\u{669}\u{668}\"><li>a</li></ol>"), "98. a");
        assert_eq!(
            md("<ol start=\"\u{ff11}\u{ff12}\"><li>a</li></ol>"),
            "12. a"
        );
        // `\u00b2` is numeric but has no *decimal* value: `int()` raises in
        // Python, and we fall back to 1 rather than panicking.
        assert_eq!(md("<ol start=\"\u{b2}\"><li>a</li></ol>"), "1. a");
    }

    #[test]
    fn unicode_decimal_values_come_from_the_block() {
        assert_eq!(unicode_decimal_value('7'), Some(7));
        assert_eq!(unicode_decimal_value('\u{669}'), Some(9));
        assert_eq!(unicode_decimal_value('\u{ff10}'), Some(0));
        // The two Unicode runs where two Nd blocks are adjacent.
        assert_eq!(unicode_decimal_value('\u{116d0}'), Some(0));
        assert_eq!(unicode_decimal_value('\u{1D7CE}'), Some(0));
        assert_eq!(unicode_decimal_value('\u{1D7D7}'), Some(9));
        assert_eq!(unicode_decimal_value('\u{1D7D8}'), Some(0));
        assert_eq!(unicode_decimal_value('\u{b2}'), None);
        assert_eq!(unicode_decimal_value('x'), None);
    }

    #[test]
    fn decimal_add_carries_across_any_width() {
        assert_eq!(decimal_add("0", 0), "0");
        assert_eq!(decimal_add("9", 1), "10");
        assert_eq!(decimal_add("00042", 8), "50");
        assert_eq!(decimal_add("999", 12), "1011");
        assert_eq!(
            decimal_add("340282366920938463463374607431768211455", 1),
            "340282366920938463463374607431768211456"
        );
    }

    #[test]
    fn code_span_backtick_runs() {
        assert_eq!(md("<code>a `b` c</code>"), "`` a `b` c ``");
        assert_eq!(md("<code>plain</code>"), "`plain`");
    }

    #[test]
    fn autolink_shortcut() {
        assert_eq!(
            md("<a href=\"http://x.com\">http://x.com</a>"),
            "<http://x.com>"
        );
        assert_eq!(
            md("<a href=\"http://x.com\" title=\"T\">go</a>"),
            "[go](http://x.com \"T\")"
        );
        assert_eq!(md("<a>go</a>"), "go");
    }

    #[test]
    fn chomp_hoists_spaces_outside_markup() {
        assert_eq!(md("<p>a<b> foo</b>b</p>"), "a **foo**b");
    }

    #[test]
    fn pre_and_code_are_not_escaped() {
        assert_eq!(md("<pre>a_b\n  c</pre>"), "```\na_b\n  c\n```");
    }

    #[test]
    fn document_strip() {
        let doc = Document::parse("<p>x</p>");
        let opts = MarkdownOptions {
            strip_document: None,
            ..MarkdownOptions::default()
        };
        assert_eq!(to_markdown(doc.root(), &opts), "\n\nx\n\n");
    }

    #[test]
    fn converter_rejects_strip_and_convert() {
        let opts = MarkdownOptions {
            strip: Some(vec!["a".to_owned()]),
            convert: Some(vec!["b".to_owned()]),
            ..MarkdownOptions::default()
        };
        assert_eq!(
            MarkdownConverter::new(opts).unwrap_err(),
            MarkdownError::StripAndConvert
        );
    }

    #[test]
    fn strip_option_passes_text_through() {
        let opts = MarkdownOptions {
            strip: Some(vec!["b".to_owned()]),
            ..MarkdownOptions::default()
        };
        let doc = Document::parse("<p>a<b>bold</b></p>");
        assert_eq!(to_markdown(doc.root(), &opts), "abold");
    }

    #[test]
    fn pre_stripping_helpers() {
        assert_eq!(strip_pre_all("  \n  code\n\n"), "  code");
        assert_eq!(strip_pre_one("  \n  code\n  "), "  code");
    }

    #[test]
    fn li_without_a_list_parent_uses_the_last_bullet() {
        // markdownify's depth counter starts at -1 and `-1 % 3 == 2` in Python.
        assert_eq!(md("<div><li>orphan</li></div>"), "- orphan");
    }

    #[test]
    fn bullets_wrap_around_past_three_levels() {
        let html = "<ul><li>1<ul><li>2<ul><li>3<ul><li>4</li></ul></li></ul></li></ul></li></ul>";
        assert_eq!(md(html), "* 1\n  + 2\n    - 3\n      * 4");
    }

    #[test]
    fn video_source_and_poster_combinations() {
        assert_eq!(
            md("<video src=\"v.mp4\" poster=\"p.png\">alt</video>"),
            "[![alt](p.png)](v.mp4)"
        );
        assert_eq!(md("<video src=\"v.mp4\">alt</video>"), "[alt](v.mp4)");
        assert_eq!(md("<video poster=\"p.png\">alt</video>"), "![alt](p.png)");
        assert_eq!(md("<video>alt</video>"), "alt");
        assert_eq!(
            md("<video><source src=\"s.mp4\">alt</video>"),
            "[alt](s.mp4)"
        );
    }

    #[test]
    fn sub_and_sup_vanish_by_default() {
        assert_eq!(md("<p>H<sub>2</sub>O and x<sup>2</sup></p>"), "H2O and x2");
    }

    #[test]
    fn inline_context_flattens_blocks_inside_headings_and_cells() {
        assert_eq!(md("<h1><div>inner</div></h1>"), "inner\n=====");
        assert_eq!(
            md("<table><tr><td><p>a</p><blockquote>b</blockquote></td></tr></table>"),
            "|  |\n| --- |\n| a  b |"
        );
    }

    #[test]
    fn definition_lists() {
        assert_eq!(
            md("<dl><dt>Term</dt><dd>Meaning</dd></dl>"),
            "Term\n:   Meaning"
        );
    }

    #[test]
    fn blockquotes_nest_and_keep_blank_lines() {
        assert_eq!(md("<blockquote>quote</blockquote>"), "> quote");
        assert_eq!(
            md("<blockquote><p>one</p><p>two</p></blockquote>"),
            "> one\n>\n> two"
        );
        assert_eq!(md("<blockquote></blockquote>"), "");
    }

    #[test]
    fn noformat_swallows_inline_markup() {
        assert_eq!(md("<p><code>a <b>b</b> c</code></p>"), "`a b c`");
        assert_eq!(
            md("<pre><code>x <a href=\"/y\">y</a></code></pre>"),
            "```\nx y\n```"
        );
    }

    #[test]
    fn hr_and_q_and_captions() {
        assert_eq!(md("<p>a</p><hr><p>b</p>"), "a\n\n---\n\nb");
        assert_eq!(md("<p>said <q>hi</q></p>"), "said \"hi\"");
        assert_eq!(md("<figure><figcaption>cap</figcaption></figure>"), "cap");
    }

    #[test]
    fn subelement_conversion_ignores_outer_context() {
        let doc = Document::parse("<ul><li>outer<ul><li>inner</li></ul></li></ul>");
        let inner = doc
            .elements()
            .filter(|e| e.tag() == "ul")
            .nth(1)
            .expect("nested ul");
        // Converted on its own the nested list is a top-level list again.
        assert_eq!(to_markdown_default(inner), "* inner");
    }
}
