use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};

use html5gum::{DefaultEmitter, Reader, State, Token, Tokenizer};
use url::Url;

use crate::api::{Error, ErrorCode};
use crate::config::Limits;
use crate::context::RequestContext;

#[cfg(test)]
mod tests;

pub(crate) struct ExtractedPage {
    pub title: Option<String>,
    pub text: String,
    pub highlights: Vec<String>,
    pub truncated: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Namespace {
    Html,
    Svg,
    Math,
}

pub(crate) struct Node {
    pub tag: String,
    pub attrs: BTreeMap<String, String>,
    pub text: String,
    pub parent: usize,
    pub end: usize,
    namespace: Namespace,
}

impl Node {
    pub fn attr(&self, name: &str) -> &str {
        self.attrs.get(name).map(String::as_str).unwrap_or("")
    }

    pub fn class(&self, name: &str) -> bool {
        self.attr("class")
            .split_ascii_whitespace()
            .any(|c| c == name)
    }

    fn child_namespace(&self, tag: &str) -> Namespace {
        let integration = match self.namespace {
            Namespace::Svg => matches!(self.tag.as_str(), "foreignobject" | "desc" | "title"),
            Namespace::Math => {
                (matches!(self.tag.as_str(), "mi" | "mo" | "mn" | "ms" | "mtext")
                    && !matches!(tag, "mglyph" | "malignmark"))
                    || (self.tag == "annotation-xml"
                        && (tag == "svg"
                            || self.attr("encoding").eq_ignore_ascii_case("text/html")
                            || self
                                .attr("encoding")
                                .eq_ignore_ascii_case("application/xhtml+xml")))
            }
            Namespace::Html => true,
        };
        if integration {
            match tag {
                "svg" => Namespace::Svg,
                "math" => Namespace::Math,
                _ => Namespace::Html,
            }
        } else {
            self.namespace
        }
    }

    fn hidden(&self) -> bool {
        matches!(
            self.tag.as_str(),
            "template" | "script" | "style" | "noscript"
        ) || self.attrs.contains_key("hidden")
            || self.attr("aria-hidden").eq_ignore_ascii_case("true")
            || (self.tag == "input" && self.attr("type").eq_ignore_ascii_case("hidden"))
            || (self.tag == "dialog" && !self.attrs.contains_key("open"))
            || self.attr("style").split(';').any(|rule| {
                rule.split_once(':').is_some_and(|(key, value)| {
                    let value = value.split('!').next().unwrap_or("").trim();
                    (key.trim().eq_ignore_ascii_case("display")
                        && value.eq_ignore_ascii_case("none"))
                        || (key.trim().eq_ignore_ascii_case("visibility")
                            && (value.eq_ignore_ascii_case("hidden")
                                || value.eq_ignore_ascii_case("collapse")))
                })
            })
    }

    fn ignored(&self) -> bool {
        matches!(
            self.tag.as_str(),
            "head"
                | "title"
                | "script"
                | "style"
                | "noscript"
                | "template"
                | "nav"
                | "button"
                | "input"
                | "select"
                | "textarea"
                | "svg"
                | "math"
                | "canvas"
                | "iframe"
                | "object"
                | "embed"
                | "footer"
        ) || self.hidden()
            || matches!(
                self.attr("role"),
                "navigation" | "banner" | "contentinfo" | "search"
            )
            || self.attr("id").eq_ignore_ascii_case("cookie-banner")
            || self.attr("class").split_ascii_whitespace().any(|name| {
                matches!(
                    name,
                    "cookie-banner"
                        | "cookie-consent"
                        | "advertisement"
                        | "adsbygoogle"
                        | "navbar"
                        | "navigation"
                        | "breadcrumbs"
                        | "breadcrumb"
                        | "site-header"
                        | "site-footer"
                        | "social-share"
                )
            })
    }
}

pub(crate) struct Document {
    pub nodes: Vec<Node>,
}

fn too_large(message: &str) -> Error {
    Error::new(ErrorCode::TooLarge, message)
}

const READ_INTERVAL: usize = 4096;
const MAX_TOKEN_BYTES: usize = 65536;
const MAX_ATTRIBUTE_BYTES: usize = 16384;
const MAX_ATTRIBUTES: usize = 256;

struct CheckedReader<'a> {
    input: &'a [u8],
    ctx: &'a RequestContext,
    consumed: &'a Cell<usize>,
    unchecked: usize,
}

impl CheckedReader<'_> {
    fn charge(&mut self, bytes: usize) -> Result<(), Error> {
        if self.unchecked + bytes >= READ_INTERVAL {
            self.ctx.check()?;
            self.unchecked = 0;
        }
        let consumed = self.consumed.get().saturating_add(bytes);
        if consumed > MAX_TOKEN_BYTES {
            return Err(too_large("HTML token exceeds the byte limit"));
        }
        self.consumed.set(consumed);
        self.unchecked += bytes;
        Ok(())
    }
}

impl Reader for CheckedReader<'_> {
    type Error = Error;

    fn read_byte(&mut self) -> Result<Option<u8>, Error> {
        let Some(&byte) = self.input.first() else {
            self.ctx.check()?;
            return Ok(None);
        };
        self.charge(1)?;
        self.input = &self.input[1..];
        Ok(Some(byte))
    }

    fn try_read_string(&mut self, value: &[u8], case_sensitive: bool) -> Result<bool, Error> {
        self.ctx.check()?;
        let Some(candidate) = self.input.get(..value.len()) else {
            return Ok(false);
        };
        for (left, right) in value
            .chunks(READ_INTERVAL)
            .zip(candidate.chunks(READ_INTERVAL))
        {
            self.ctx.check()?;
            if !(left == right || !case_sensitive && left.eq_ignore_ascii_case(right)) {
                return Ok(false);
            }
        }
        for chunk in value.chunks(READ_INTERVAL) {
            self.charge(chunk.len())?;
        }
        self.input = &self.input[value.len()..];
        Ok(true)
    }

    fn read_until<'b>(
        &'b mut self,
        needle: &[u8],
        _: &'b mut [u8; 4],
    ) -> Result<Option<&'b [u8]>, Error> {
        if self.input.is_empty() {
            self.ctx.check()?;
            return Ok(None);
        }
        self.ctx.check()?;
        let chunk = &self.input[..self.input.len().min(READ_INTERVAL)];
        let length = chunk
            .iter()
            .position(|byte| needle.contains(byte))
            .map_or(chunk.len(), |i| i.max(1));
        self.charge(length)?;
        let (result, rest) = self.input.split_at(length);
        self.input = rest;
        Ok(Some(result))
    }
}

pub(crate) fn challenge_screen(
    doc: &Document,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<bool, Error> {
    let mut index = 1;
    while index < doc.nodes.len() {
        ctx.check()?;
        let node = &doc.nodes[index];
        if node.hidden() || matches!(node.tag.as_str(), "head" | "svg" | "math") {
            index = node.end;
            continue;
        }
        if matches!(
            node.attr("id"),
            "challenge-form" | "challenge-running" | "challenge-stage"
        ) || node.class("anomaly-modal")
            || (node.tag == "form"
                && safe_url(base, node.attr("action"), limits)
                    .is_some_and(|url| url.path().eq_ignore_ascii_case("/anomaly.js")))
        {
            return Ok(true);
        }
        index += 1;
    }
    Ok(false)
}

fn close_to(
    nodes: &mut [Node],
    stack: &mut Vec<usize>,
    open: &mut HashMap<String, Vec<usize>>,
    position: usize,
    end: usize,
    ctx: &RequestContext,
) -> Result<(), Error> {
    while stack.len() > position {
        ctx.check()?;
        let index = stack.pop().unwrap();
        nodes[index].end = end;
        let tag = &nodes[index].tag;
        let positions = open.get_mut(tag).unwrap();
        positions.pop();
        if positions.is_empty() {
            open.remove(tag);
        }
    }
    Ok(())
}

fn implied_close(tag: &str, open: &HashMap<String, Vec<usize>>) -> Option<usize> {
    let (names, boundaries): (&[&str], &[&str]) = match tag {
        "li" => (&["li"], &["ul", "ol"]),
        "dt" | "dd" => (&["dt", "dd"], &["dl"]),
        "td" | "th" => (&["td", "th"], &["tr", "table"]),
        "tr" => (&["tr"], &["table"]),
        "thead" | "tbody" | "tfoot" => (&["thead", "tbody", "tfoot"], &["table"]),
        "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => (&["h1", "h2", "h3", "h4", "h5", "h6"], &[]),
        "form" => (&["form"], &[]),
        _ => (&[], &[]),
    };
    let last = |name: &&str| {
        open.get(*name)
            .and_then(|positions| positions.last())
            .copied()
    };
    let boundary = boundaries.iter().filter_map(last).max().unwrap_or(0);
    let candidate = names
        .iter()
        .filter_map(last)
        .max()
        .filter(|&position| position > boundary);
    let paragraph = block(tag).then(|| last(&"p")).flatten();
    candidate.into_iter().chain(paragraph).min()
}

pub(crate) fn parse(html: &str, ctx: &RequestContext, limits: &Limits) -> Result<Document, Error> {
    ctx.check()?;
    if html.len() > limits.max_decoded_bytes {
        return Err(too_large("Decoded HTML exceeds the byte limit"));
    }
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    let consumed = Cell::new(0);
    let reader = CheckedReader {
        input: html.as_bytes(),
        ctx,
        consumed: &consumed,
        unchecked: 0,
    };
    let mut tokenizer = Tokenizer::new_with_emitter(reader, emitter);
    let mut nodes = vec![Node {
        tag: String::new(),
        attrs: BTreeMap::new(),
        text: String::new(),
        parent: 0,
        end: 1,
        namespace: Namespace::Html,
    }];
    let mut stack = vec![0];
    let mut open: HashMap<String, Vec<usize>> = HashMap::new();
    let mut count = 0;
    while let Some(token) = tokenizer.next() {
        ctx.check()?;
        if count >= limits.max_html_tokens {
            return Err(too_large("HTML exceeds the token limit"));
        }
        count += 1;
        let token = token?;
        if !matches!(token, Token::Error(_)) {
            consumed.set(0);
        }
        match token {
            Token::StartTag(start) => {
                if start.name.len() > 256 || start.attributes.len() > MAX_ATTRIBUTES {
                    return Err(too_large(
                        "HTML tag exceeds the name or attribute count limit",
                    ));
                }
                let tag = String::from_utf8_lossy(&start.name).into_owned();
                if nodes[*stack.last().unwrap()].tag == "head"
                    && !matches!(
                        tag.as_str(),
                        "html"
                            | "head"
                            | "base"
                            | "basefont"
                            | "bgsound"
                            | "link"
                            | "meta"
                            | "noframes"
                            | "script"
                            | "style"
                            | "template"
                            | "title"
                    )
                    && let Some(position) = open
                        .get("head")
                        .and_then(|positions| positions.last())
                        .copied()
                {
                    let end = nodes.len();
                    close_to(&mut nodes, &mut stack, &mut open, position, end, ctx)?;
                }
                if let Some(position) = implied_close(&tag, &open) {
                    let end = nodes.len();
                    close_to(&mut nodes, &mut stack, &mut open, position, end, ctx)?;
                }
                if stack.len() > limits.max_html_depth {
                    return Err(too_large("HTML exceeds the nesting depth limit"));
                }
                let mut attrs = BTreeMap::new();
                let mut bytes = start.name.len();
                for (key, value) in start.attributes {
                    ctx.check()?;
                    bytes = bytes
                        .saturating_add(key.len())
                        .saturating_add(value.value.len());
                    if key.len() > 256
                        || value.value.len() > MAX_ATTRIBUTE_BYTES
                        || bytes > MAX_TOKEN_BYTES
                    {
                        return Err(too_large("HTML attributes exceed the byte limit"));
                    }
                    let key = String::from_utf8_lossy(&key).into_owned();
                    if matches!(
                        key.as_str(),
                        "id" | "class"
                            | "role"
                            | "hidden"
                            | "aria-hidden"
                            | "aria-label"
                            | "href"
                            | "type"
                            | "style"
                            | "action"
                            | "open"
                            | "encoding"
                            | "name"
                            | "autocomplete"
                    ) {
                        attrs.insert(key, String::from_utf8_lossy(&value.value).into_owned());
                    }
                }
                let index = nodes.len();
                let parent = *stack.last().unwrap();
                let namespace = nodes[parent].child_namespace(&tag);
                let foreign = namespace != Namespace::Html;
                if foreign && start.self_closing {
                    tokenizer.set_state(State::Data);
                }
                let void = !foreign
                    && matches!(
                        tag.as_str(),
                        "area"
                            | "base"
                            | "br"
                            | "col"
                            | "embed"
                            | "hr"
                            | "img"
                            | "input"
                            | "link"
                            | "meta"
                            | "param"
                            | "source"
                            | "track"
                            | "wbr"
                    );
                nodes.push(Node {
                    tag,
                    attrs,
                    text: String::new(),
                    parent,
                    end: index + 1,
                    namespace,
                });
                if !(void || foreign && start.self_closing) {
                    open.entry(nodes[index].tag.clone())
                        .or_default()
                        .push(stack.len());
                    stack.push(index);
                }
            }
            Token::EndTag(end) => {
                if end.name.len() > 256 {
                    return Err(too_large("HTML tag name exceeds the byte limit"));
                }
                let name = String::from_utf8_lossy(&end.name);
                if let Some(position) = open
                    .get(name.as_ref())
                    .and_then(|positions| positions.last())
                    .copied()
                {
                    let end = nodes.len();
                    close_to(&mut nodes, &mut stack, &mut open, position, end, ctx)?;
                }
            }
            Token::String(text) => {
                if text.value.len() > MAX_TOKEN_BYTES {
                    return Err(too_large("HTML text token exceeds the byte limit"));
                }
                let mut body_text = false;
                if nodes[*stack.last().unwrap()].tag == "head" {
                    for chunk in text.value.chunks(READ_INTERVAL) {
                        ctx.check()?;
                        if chunk
                            .iter()
                            .any(|byte| !matches!(byte, b' ' | b'\t' | b'\n' | b'\r' | 12))
                        {
                            body_text = true;
                            break;
                        }
                    }
                }
                if body_text
                    && let Some(position) = open
                        .get("head")
                        .and_then(|positions| positions.last())
                        .copied()
                {
                    let end = nodes.len();
                    close_to(&mut nodes, &mut stack, &mut open, position, end, ctx)?;
                }
                let index = nodes.len();
                nodes.push(Node {
                    tag: String::new(),
                    attrs: BTreeMap::new(),
                    text: String::from_utf8_lossy(&text.value).into_owned(),
                    parent: *stack.last().unwrap(),
                    end: index + 1,
                    namespace: nodes[*stack.last().unwrap()].namespace,
                });
            }
            _ => {}
        }
    }
    for index in stack {
        ctx.check()?;
        nodes[index].end = nodes.len();
    }
    ctx.check()?;
    Ok(Document { nodes })
}

fn block(tag: &str) -> bool {
    matches!(
        tag,
        "address"
            | "article"
            | "aside"
            | "blockquote"
            | "div"
            | "dl"
            | "dt"
            | "dd"
            | "fieldset"
            | "figure"
            | "figcaption"
            | "footer"
            | "form"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "header"
            | "hr"
            | "li"
            | "main"
            | "nav"
            | "ol"
            | "p"
            | "pre"
            | "section"
            | "table"
            | "tr"
            | "ul"
    )
}

pub(crate) fn safe_url(base: &Url, href: &str, limits: &Limits) -> Option<Url> {
    if href.len() > limits.max_url_bytes || base.as_str().len() > limits.max_url_bytes {
        return None;
    }
    if href.chars().any(|ch| ch.is_control() || ch == '\\') {
        return None;
    }
    let href = href.trim();
    if let Some((scheme, rest)) = href.split_once(':')
        && !scheme.contains(['/', '?', '#'])
        && (!(scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https"))
            || !rest.starts_with("//"))
    {
        return None;
    }
    let url = base.join(href).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.as_str().len() <= limits.max_url_bytes)
        .then_some(url)
}

fn document_base(
    doc: &Document,
    base: &Url,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<Url, Error> {
    let mut index = 1;
    while index < doc.nodes.len() {
        ctx.check()?;
        let node = &doc.nodes[index];
        if matches!(
            node.tag.as_str(),
            "template" | "script" | "style" | "noscript" | "svg" | "math"
        ) {
            index = node.end;
            continue;
        }
        if node.tag == "base"
            && node.attrs.contains_key("href")
            && let Some(url) = safe_url(base, node.attr("href"), limits)
        {
            return Ok(url);
        }
        index += 1;
    }
    Ok(base.clone())
}

pub(crate) fn plain_text(
    doc: &Document,
    root: usize,
    cap: usize,
    clean: bool,
    ctx: &RequestContext,
) -> Result<String, Error> {
    Ok(bounded_plain_text(doc, root, cap, clean, ctx)?.0)
}

pub(crate) fn bounded_plain_text(
    doc: &Document,
    root: usize,
    cap: usize,
    clean: bool,
    ctx: &RequestContext,
) -> Result<(String, bool), Error> {
    let out = plain_output(doc, root, cap, clean, false, false, ctx)?;
    Ok((out.text.trim().to_owned(), out.truncated))
}

fn plain_output(
    doc: &Document,
    root: usize,
    cap: usize,
    clean: bool,
    escape: bool,
    exclude_code: bool,
    ctx: &RequestContext,
) -> Result<Output, Error> {
    let mut out = Output::new(cap);
    let mut index = root;
    let mut blocks = Vec::new();
    while index < doc.nodes[root].end && !out.truncated {
        ctx.check()?;
        while blocks.last().is_some_and(|&end| end <= index) {
            ctx.check()?;
            blocks.pop();
            out.space();
        }
        let node = &doc.nodes[index];
        if (clean && node.ignored())
            || (!clean && node.hidden())
            || (exclude_code && matches!(node.tag.as_str(), "pre" | "code"))
        {
            index = node.end;
            continue;
        }
        if block(&node.tag) || matches!(node.tag.as_str(), "td" | "th") {
            out.space();
            blocks.push(node.end);
        } else if node.tag == "br" {
            out.space();
        }
        out.normal(&node.text, escape, ctx)?;
        index += 1;
    }
    Ok(out)
}

struct Output {
    text: String,
    cap: usize,
    used: usize,
    reserved: usize,
    truncated: bool,
    pending_space: bool,
    marker_only: bool,
    indent: usize,
}

impl Output {
    fn new(cap: usize) -> Self {
        Self {
            text: String::new(),
            cap,
            used: 0,
            reserved: 0,
            truncated: false,
            pending_space: false,
            marker_only: false,
            indent: 0,
        }
    }

    fn room(&self) -> usize {
        self.cap.saturating_sub(self.used + self.reserved)
    }

    fn push(&mut self, text: &str) -> bool {
        let line_start = self.text.is_empty() || self.text.ends_with('\n');
        let padding = if self.indent == 0 {
            0
        } else {
            text.split_inclusive('\n')
                .enumerate()
                .filter(|(i, line)| !line.starts_with('\n') && (*i > 0 || line_start))
                .count()
                * self.indent
        };
        let count = text.chars().count() + padding;
        if count > self.room() {
            self.truncated = true;
            return false;
        }
        if self.indent == 0 {
            self.text.push_str(text);
        } else {
            for line in text.split_inclusive('\n') {
                if !line.starts_with('\n') && (self.text.is_empty() || self.text.ends_with('\n')) {
                    for _ in 0..self.indent {
                        self.text.push(' ');
                    }
                }
                self.text.push_str(line);
            }
        }
        self.used += count;
        if !text.is_empty() {
            self.marker_only = false;
        }
        true
    }

    fn push_checked(&mut self, mut text: &str, ctx: &RequestContext) -> Result<(), Error> {
        while !text.is_empty() {
            ctx.check()?;
            let mut end = text.len().min(READ_INTERVAL);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            if !self.push(&text[..end]) {
                break;
            }
            text = &text[end..];
        }
        Ok(())
    }

    fn space(&mut self) {
        self.pending_space = !self.text.is_empty() && !self.text.ends_with(char::is_whitespace);
    }

    fn flush_space(&mut self) {
        if self.pending_space {
            self.pending_space = false;
            self.push(" ");
        }
    }

    fn normal(&mut self, text: &str, escape: bool, ctx: &RequestContext) -> Result<(), Error> {
        for (i, ch) in text.chars().enumerate() {
            if i % 1024 == 0 {
                ctx.check()?;
            }
            if ch.is_whitespace() {
                self.space();
                continue;
            }
            self.flush_space();
            if self.truncated {
                break;
            }
            let escaped = if escape {
                match ch {
                    '<' => Some("&lt;"),
                    '>' => Some("&gt;"),
                    '&' => Some("&amp;"),
                    '\\' => Some("\\\\"),
                    '`' => Some("\\`"),
                    '*' => Some("\\*"),
                    '_' => Some("\\_"),
                    '{' => Some("\\{"),
                    '}' => Some("\\}"),
                    '[' => Some("\\["),
                    ']' => Some("\\]"),
                    '(' => Some("\\("),
                    ')' => Some("\\)"),
                    '#' => Some("\\#"),
                    '!' => Some("\\!"),
                    '|' => Some("\\|"),
                    '~' => Some("\\~"),
                    '-' => Some("\\-"),
                    '+' => Some("\\+"),
                    '.' => Some("\\."),
                    _ => None,
                }
            } else {
                None
            };
            let mut buf = [0; 4];
            if !self.push(escaped.unwrap_or_else(|| ch.encode_utf8(&mut buf))) {
                break;
            }
        }
        Ok(())
    }

    fn line(&mut self) {
        self.pending_space = false;
        if !self.marker_only && !self.text.is_empty() && !self.text.ends_with('\n') {
            self.push("\n");
        }
    }

    fn paragraph(&mut self) {
        self.line();
        if !self.marker_only && !self.text.is_empty() && !self.text.ends_with("\n\n") {
            self.push("\n");
        }
    }
}

fn raw_text(
    doc: &Document,
    root: usize,
    cap: usize,
    ctx: &RequestContext,
) -> Result<String, Error> {
    let mut out = Output::new(cap);
    let mut index = root + 1;
    while index < doc.nodes[root].end {
        ctx.check()?;
        let node = &doc.nodes[index];
        if node.ignored() {
            index = node.end;
            continue;
        }
        if node.tag == "br" {
            out.push("\n");
        }
        for (i, ch) in node.text.chars().enumerate() {
            if i % 1024 == 0 {
                ctx.check()?;
            }
            let mut buf = [0; 4];
            if !out.push(ch.encode_utf8(&mut buf)) {
                return Ok(out.text);
            }
        }
        index += 1;
    }
    Ok(out.text)
}

fn render_code(
    doc: &Document,
    root: usize,
    out: &mut Output,
    ctx: &RequestContext,
) -> Result<Option<usize>, Error> {
    let raw = raw_text(doc, root, out.room().saturating_add(1), ctx)?;
    let mut run = 0usize;
    let mut longest = 0usize;
    let mut fenced = doc.nodes[root].tag == "pre";
    for (i, ch) in raw.chars().enumerate() {
        if i % 1024 == 0 {
            ctx.check()?;
        }
        run = if ch == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
        fenced |= matches!(ch, '\n' | '\r');
    }
    let fence_len = longest.saturating_add(1).max(if fenced { 3 } else { 1 });
    if fenced {
        out.marker_only = false;
        out.line();
    } else {
        out.flush_space();
    }
    let overhead = fence_len
        .saturating_mul(2)
        .saturating_add(2)
        .saturating_add(if fenced { out.indent * 2 } else { 0 });
    if overhead > out.room() {
        out.normal(&raw, true, ctx)?;
        return Ok(None);
    }
    let fence = "`".repeat(fence_len);
    let suffix = if fenced {
        format!("\n{fence}")
    } else {
        format!(" {fence}")
    };
    let reserved = suffix.len() + if fenced { out.indent } else { 0 };
    out.reserved += reserved;
    out.push_checked(&fence, ctx)?;
    out.push(if fenced { "\n" } else { " " });
    for (i, ch) in raw.chars().enumerate() {
        if i % 1024 == 0 {
            ctx.check()?;
        }
        let mut buf = [0; 4];
        if !out.push(ch.encode_utf8(&mut buf)) {
            break;
        }
    }
    out.reserved -= reserved;
    out.push_checked(
        if fenced && out.text.ends_with('\n') {
            &fence
        } else {
            &suffix
        },
        ctx,
    )?;
    let completed = (fenced && !out.truncated).then_some(out.text.len());
    if fenced {
        out.line();
    }
    Ok(completed)
}

fn render(
    doc: &Document,
    root: usize,
    base: &Url,
    cap: usize,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<(String, bool), Error> {
    let mut out = Output::new(cap);
    let mut frames: Vec<(usize, String, bool)> = Vec::new();
    let mut index = root;
    let mut boundary = None;
    let mut completed = 0;
    let mut incomplete = root;
    let mut list_depth = vec![0usize; doc.nodes.len()];
    let mut item_depth = vec![0usize; doc.nodes.len()];
    for (i, node) in doc.nodes.iter().enumerate().skip(1) {
        ctx.check()?;
        list_depth[i] =
            list_depth[node.parent] + usize::from(matches!(node.tag.as_str(), "ul" | "ol"));
        item_depth[i] = if node.tag == "li" {
            list_depth[i]
        } else {
            item_depth[node.parent]
        };
    }
    while index <= doc.nodes[root].end {
        ctx.check()?;
        while frames
            .last()
            .is_some_and(|frame| doc.nodes[frame.0].end <= index)
        {
            let (start, suffix, newline) = frames.pop().unwrap();
            ctx.check()?;
            out.reserved -= suffix.chars().count();
            out.push(&suffix);
            if doc.nodes[start].tag == "table"
                || item_depth[start] > 0 && matches!(doc.nodes[start].tag.as_str(), "ul" | "ol")
            {
                out.paragraph();
            } else if newline {
                out.line();
            }
        }
        if !out.truncated && boundary.is_some_and(|end| end <= index) {
            completed = out.text.len();
            boundary = None;
        }
        if out.truncated || index == doc.nodes[root].end {
            break;
        }
        let node = &doc.nodes[index];
        if node.ignored() {
            index = node.end;
            continue;
        }
        if boundary.is_none()
            && !matches!(
                node.tag.as_str(),
                "html"
                    | "body"
                    | "main"
                    | "article"
                    | "section"
                    | "div"
                    | "aside"
                    | "header"
                    | "form"
                    | "fieldset"
                    | "figure"
            )
            && (!node.tag.is_empty() || !node.text.trim().is_empty())
        {
            boundary = Some(node.end);
            incomplete = index;
        }
        out.indent = item_depth[index] * 4;
        let is_block = block(&node.tag);
        if is_block {
            if item_depth[index] > 0 && !matches!(node.tag.as_str(), "li" | "ul" | "ol") {
                out.paragraph();
            } else {
                out.line();
            }
        }
        if matches!(node.tag.as_str(), "pre" | "code") {
            let checkpoint = render_code(doc, index, &mut out, ctx)?;
            if out.reserved == 0
                && let Some(checkpoint) = checkpoint
            {
                completed = checkpoint;
                if boundary == Some(node.end) {
                    boundary = None;
                }
            }
            index = node.end;
            continue;
        }
        match node.tag.as_str() {
            "h1" => {
                out.push("# ");
            }
            "h2" => {
                out.push("## ");
            }
            "h3" => {
                out.push("### ");
            }
            "h4" => {
                out.push("#### ");
            }
            "h5" => {
                out.push("##### ");
            }
            "h6" => {
                out.push("###### ");
            }
            "li" => {
                out.marker_only = false;
                out.line();
                out.indent = list_depth[index].saturating_sub(1) * 4;
                let ordered = doc.nodes[node.parent].tag == "ol";
                out.push(if ordered { "1. " } else { "- " });
                out.marker_only = true;
            }
            "tr" => {
                out.marker_only = false;
                out.line();
                out.push("- ");
                out.marker_only = true;
            }
            "td" | "th" => {
                out.flush_space();
            }
            "br" => {
                out.line();
            }
            "hr" => {
                out.push("\n---\n");
            }
            _ => {}
        }
        if node.tag == "a"
            && !node.attr("href").is_empty()
            && let Some(url) = safe_url(base, node.attr("href"), limits)
        {
            let mut suffix = String::from("](");
            for (i, ch) in url.as_str().chars().enumerate() {
                if i % 1024 == 0 {
                    ctx.check()?;
                }
                match ch {
                    '(' => suffix.push_str("%28"),
                    ')' => suffix.push_str("%29"),
                    '<' => suffix.push_str("%3C"),
                    '>' => suffix.push_str("%3E"),
                    '\\' => suffix.push_str("%5C"),
                    '&' => suffix.push_str("&amp;"),
                    _ => suffix.push(ch),
                }
            }
            suffix.push(')');
            out.flush_space();
            let cost = suffix.chars().count() + 1;
            if cost < out.room() {
                let label = plain_output(doc, index, out.room() - cost, true, true, false, ctx)?;
                if !label.text.is_empty() {
                    out.push("[");
                    out.push_checked(label.text.trim(), ctx)?;
                    out.push(&suffix);
                    out.truncated |= label.truncated;
                    index = node.end;
                    continue;
                }
            }
        }
        let mut suffix = String::new();
        if matches!(node.tag.as_str(), "td" | "th") {
            suffix.push_str(" | ");
            if suffix.len() <= out.room() {
                out.reserved += suffix.len();
            } else {
                suffix.clear();
            }
        }
        out.normal(&node.text, true, ctx)?;
        if !node.tag.is_empty() {
            frames.push((index, suffix, is_block));
        }
        index += 1;
    }
    if out.truncated {
        if completed > 0 {
            out.text.truncate(completed);
            return Ok((out.text.trim().to_owned(), true));
        }
        let fallback = plain_output(doc, incomplete, cap, true, true, false, ctx)?;
        return Ok((fallback.text.trim().to_owned(), true));
    }
    Ok((out.text.trim().to_owned(), false))
}

fn highlights(
    source: &str,
    query: Option<&str>,
    cap: usize,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<Vec<String>, Error> {
    let mut normalized_query = String::new();
    for (i, ch) in query
        .unwrap_or("")
        .chars()
        .take(limits.max_query_chars)
        .enumerate()
    {
        if i % 1024 == 0 {
            ctx.check()?;
        }
        normalized_query.extend(ch.to_lowercase());
    }
    let query = normalized_query;
    let terms: Vec<_> = query.split_whitespace().take(32).collect();
    let mut candidates: Vec<(usize, usize, String)> = Vec::new();
    let mut start = 0;
    let mut chars = 0;
    for (offset, ch) in source
        .char_indices()
        .chain(std::iter::once((source.len(), '\n')))
    {
        if chars % 256 == 0 {
            ctx.check()?;
        }
        if offset == source.len() || chars >= 260 && ch.is_whitespace() || chars == 300 {
            let excerpt = source[start..offset].trim();
            if !excerpt.is_empty() {
                let lower = excerpt.to_lowercase();
                let score = terms.iter().filter(|term| lower.contains(**term)).count();
                candidates.push((score, start, excerpt.to_owned()));
                candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
                candidates.truncate(3);
            }
            start = offset;
            chars = 0;
        }
        chars += 1;
    }
    candidates.sort_by_key(|item| item.1);
    let mut remaining = cap;
    let mut result = Vec::new();
    for (_, _, excerpt) in candidates {
        ctx.check()?;
        let excerpt: String = excerpt.chars().take(remaining.min(300)).collect();
        remaining -= excerpt.chars().count();
        if !excerpt.is_empty() {
            result.push(excerpt);
        }
    }
    Ok(result)
}

fn login_label(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    text.split(['|', '·'])
        .flat_map(|part| part.split(" - "))
        .any(|part| {
            let part = part.trim().trim_end_matches(['.', '!', ':', '…']);
            ["sign in", "sign-in", "log in", "login"]
                .iter()
                .any(|label| {
                    part == *label
                        || part.starts_with(&format!("{label} to "))
                        || part.ends_with(&format!(" {label}"))
                })
        })
}

fn blocked_label(text: &str) -> bool {
    let text = text.trim().to_lowercase();
    text.split(['|', '·'])
        .flat_map(|part| part.split(" - "))
        .any(|part| {
            matches!(
                part.trim().trim_end_matches(['.', '!', ':', '…']),
                "just a moment"
                    | "attention required"
                    | "verify you are human"
                    | "access denied"
                    | "403 forbidden"
                    | "403 access denied"
                    | "access denied (403)"
                    | "checking your browser"
                    | "complete the security check"
                    | "captcha"
            )
        })
}

fn blocked_screen(doc: &Document, root: usize, ctx: &RequestContext) -> Result<bool, Error> {
    let mut index = root;
    while index < doc.nodes[root].end {
        ctx.check()?;
        let node = &doc.nodes[index];
        if node.ignored()
            || matches!(
                node.tag.as_str(),
                "article" | "aside" | "pre" | "code" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            )
        {
            index = node.end;
            continue;
        }
        if matches!(node.attr("id"), "cf-error-details" | "cf-error-overview")
            || ["g-recaptcha", "h-captcha", "cf-turnstile"]
                .iter()
                .any(|class| node.class(class))
        {
            return Ok(true);
        }
        let text = if node.tag == "p" {
            plain_output(doc, index, 512, true, false, true, ctx)?.text
        } else {
            node.text.clone()
        };
        let text = text.trim().to_lowercase();
        if [
            "you don't have permission to access this page",
            "you do not have permission to access this page",
            "you are not authorized to access this page",
            "you are not authorised to access this page",
            "your request has been blocked",
            "this request has been blocked",
            "sorry, you have been blocked",
            "you have been blocked",
            "access to this page has been denied",
            "enable javascript and cookies to continue",
            "please enable javascript and cookies to continue",
            "please verify you are a human",
            "verifying you are human",
            "checking your browser before accessing",
            "please wait while we verify",
            "please wait while we check your browser",
            "please complete the security check",
            "checking if the site connection is secure",
            "performing security verification",
        ]
        .iter()
        .any(|message| text.trim_end_matches(['.', '!', ':', '…']) == *message)
        {
            return Ok(true);
        }
        index = if node.tag == "p" { node.end } else { index + 1 };
    }
    Ok(false)
}

pub(crate) fn extract(
    html: &str,
    base: &Url,
    query: Option<&str>,
    ctx: &RequestContext,
    limits: &Limits,
) -> Result<ExtractedPage, Error> {
    ctx.check()?;
    if html.len() > limits.max_decoded_bytes {
        return Err(too_large("Decoded HTML exceeds the byte limit"));
    }
    let doc = parse(html, ctx, limits)?;
    let base = document_base(&doc, base, ctx, limits)?;
    if challenge_screen(&doc, &base, ctx, limits)? {
        return Err(Error::new(
            ErrorCode::UpstreamBlocked,
            "Page contains an access challenge",
        ));
    }
    let mut visible = vec![0usize; doc.nodes.len()];
    let mut hidden = vec![false; doc.nodes.len()];
    let mut concealed = vec![false; doc.nodes.len()];
    let mut forms = vec![0usize; doc.nodes.len()];
    let mut secondary = vec![false; doc.nodes.len()];
    let mut primary_visible = vec![0usize; doc.nodes.len()];
    let mut body = 0;
    let mut title_index = None;
    let mut h1 = None;
    let mut heading_end = 0;
    for (index, node) in doc.nodes.iter().enumerate().skip(1) {
        ctx.check()?;
        hidden[index] = hidden[node.parent] || node.ignored();
        concealed[index] = concealed[node.parent] || node.hidden();
        secondary[index] = secondary[node.parent]
            || matches!(node.tag.as_str(), "aside" | "header" | "footer" | "nav")
            || matches!(
                node.attr("role"),
                "complementary" | "navigation" | "banner" | "contentinfo"
            );
        forms[index] = if node.tag == "form" {
            index
        } else {
            forms[node.parent]
        };
        if node.tag == "body" {
            body = index;
        }
        if node.tag == "title"
            && !concealed[index]
            && node.namespace == Namespace::Html
            && (doc.nodes[node.parent].tag == "head" || body == 0)
            && title_index.is_none()
        {
            title_index = Some(index);
        }
        if node.tag == "h1" && !hidden[index] && h1.is_none() {
            h1 = Some(index);
        }
        if !hidden[index] {
            for (i, ch) in node.text.chars().enumerate() {
                if i % 1024 == 0 {
                    ctx.check()?;
                }
                if !ch.is_whitespace() {
                    visible[index] += 1;
                }
            }
        }
        if matches!(node.tag.as_str(), "h1" | "h2" | "h3" | "h4" | "h5" | "h6") {
            heading_end = node.end;
        }
        if !secondary[index] && forms[index] == 0 && index >= heading_end {
            primary_visible[index] = visible[index];
        }
    }
    for index in (1..doc.nodes.len()).rev() {
        ctx.check()?;
        let parent = doc.nodes[index].parent;
        visible[parent] = visible[parent].saturating_add(visible[index]);
        primary_visible[parent] = primary_visible[parent].saturating_add(primary_visible[index]);
    }
    let mut root = body;
    let mut best = 0;
    let mut content_roots: [Option<usize>; 2] = [None; 2];
    for (index, node) in doc.nodes.iter().enumerate() {
        ctx.check()?;
        let priority = if node.tag == "main" || node.attr("role") == "main" {
            2
        } else if node.tag == "article" {
            1
        } else {
            0
        };
        if !hidden[index] && priority > 0 && primary_visible[index] > 0 {
            let slot = &mut content_roots[2 - priority];
            if slot.is_none_or(|old| primary_visible[index] > primary_visible[old]) {
                *slot = Some(index);
            }
        }
        if !hidden[index] && priority > 0 && visible[index] >= 200 {
            let score = visible[index].saturating_mul(priority);
            if score > best {
                root = index;
                best = score;
            }
        }
    }
    for (index, node) in doc
        .nodes
        .iter()
        .enumerate()
        .take(doc.nodes[root].end)
        .skip(root)
    {
        ctx.check()?;
        if node.tag == "h1" && !hidden[index] {
            h1 = Some(index);
            break;
        }
    }
    let (heading, heading_truncated) = h1
        .map(|index| bounded_plain_text(&doc, index, 512, false, ctx))
        .transpose()?
        .unwrap_or_default();
    let title_source = title_index
        .map(|index| bounded_plain_text(&doc, index, 512, false, ctx))
        .transpose()?
        .filter(|(title, _)| !title.is_empty())
        .or_else(|| (!heading.is_empty()).then(|| (heading.clone(), heading_truncated)));
    let (page_title, mut title_truncated) = match title_source {
        Some((title, truncated)) => (Some(title), truncated),
        None => (None, false),
    };
    if visible[root] < 4000
        && (blocked_label(&heading) || page_title.as_deref().is_some_and(blocked_label))
        && blocked_screen(&doc, root, ctx)?
    {
        return Err(Error::new(
            ErrorCode::UpstreamBlocked,
            "Page is a challenge or access-denied screen",
        ));
    }
    let mut login_context = page_title.as_deref().is_some_and(login_label) || login_label(&heading);
    let mut password = false;
    let mut email_form = false;
    for (index, node) in doc
        .nodes
        .iter()
        .enumerate()
        .take(doc.nodes[root].end)
        .skip(root)
    {
        ctx.check()?;
        if hidden[node.parent]
            || concealed[index]
            || secondary[index]
            || content_roots
                .iter()
                .flatten()
                .any(|&content| index < content || index >= doc.nodes[content].end)
        {
            continue;
        }
        if matches!(node.tag.as_str(), "h1" | "h2") && !hidden[index] {
            login_context |= login_label(&plain_text(&doc, index, 512, true, ctx)?);
        }
        if node.tag == "form" {
            login_context |= login_label(node.attr("aria-label"))
                || login_label(node.attr("id"))
                || login_label(&plain_text(&doc, index, 512, false, ctx)?);
        }
        if node.tag == "input" {
            password |= node.attr("type").eq_ignore_ascii_case("password");
            email_form |= forms[index] != 0
                && (node.attr("type").eq_ignore_ascii_case("email")
                    || matches!(node.attr("autocomplete"), "email" | "username")
                    || node.attr("name").eq_ignore_ascii_case("email"));
        }
    }
    if visible[root] < 4000 && login_context && (password || email_form) {
        return Err(Error::new(
            ErrorCode::UnsupportedContent,
            "Page is a login screen",
        ));
    }
    if visible[root] == 0 {
        return Err(Error::new(
            ErrorCode::ExtractionFailed,
            "Page has no readable content",
        ));
    }
    let source = plain_text(&doc, root, limits.max_text_chars, true, ctx)?;
    let highlights = highlights(
        &source,
        query,
        (limits.max_text_chars / 4).min(900),
        ctx,
        limits,
    )?;
    let title = page_title
        .map(|title| {
            let cap = (limits.max_text_chars / 8).min(512);
            title_truncated |= title.chars().count() > cap;
            title.chars().take(cap).collect::<String>()
        })
        .filter(|s| !s.is_empty());
    let used = title.as_ref().map_or(0, |s| s.chars().count())
        + highlights.iter().map(|s| s.chars().count()).sum::<usize>();
    let (text, truncated) = render(
        &doc,
        root,
        &base,
        limits.max_text_chars.saturating_sub(used),
        ctx,
        limits,
    )?;
    if text.is_empty() {
        return Err(Error::new(
            ErrorCode::ExtractionFailed,
            "Page has no readable content within the output limit",
        ));
    }
    ctx.check()?;
    Ok(ExtractedPage {
        title,
        text,
        highlights,
        truncated: truncated || title_truncated,
    })
}
