//! The template language: parsing, validation and rendering. Pure, no I/O.
//!
//! # Syntax
//!
//! Plain text with tags in double braces:
//!
//! * `{{name}}` inserts a variable (dotted names, see the catalogue).
//! * `{{#if name}}...{{else}}...{{/if}}` (`{{else}}` is optional) renders the
//!   first part when the value is true: a non-empty string, a non-empty list,
//!   `true` or a number other than zero.
//! * `{{#each list}}...{{/each}}` repeats its body for every item. Inside it,
//!   `{{this}}` is the item (a list of text), `{{this.field}}` a field of a
//!   record item and `{{@index}}` the position, counting from 0. `this` is
//!   always the innermost loop.
//! * `{{!-- comment --}}` is dropped.
//! * `\{{` writes a literal `{{`.
//!
//! A block tag (`#if`, `else`, `/if`, `#each`, `/each`, a comment) that stands
//! alone on its line removes the whole line, so templates can be laid out
//! readably without leaving blank lines behind.
//!
//! There are no expressions, no comparisons, no includes and nothing runs. The
//! output is plain text: nothing is HTML-escaped. **A value is inserted as it
//! is and is never scanned again for tags**, so an issue description that
//! contains `{{issue.title}}` stays that text.
//!
//! # Limits
//!
//! A template is at most [`MAX_TEMPLATE_CHARS`] characters and blocks nest at
//! most [`MAX_DEPTH`] deep. The rendered text is at most [`MAX_OUTPUT_CHARS`]
//! characters; when it would be longer, [`render`] shortens the values rather
//! than failing, largest first: it repeatedly cuts the longest text among all
//! variable values and list item fields to half (never below 200 characters)
//! and ends it with [`TRUNCATED_SUFFIX`], until the text fits. Only if the
//! template alone still produces too much (a loop over a loop) the output
//! itself is cut at the limit and ends with the same marker.
//!
//! # Validation
//!
//! [`parse`] checks everything a template can get wrong: unknown variables, a
//! list printed as text, `this` outside a loop, unbalanced blocks, nesting too
//! deep. Every error names the line and column.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde_json::Value;

use super::catalogue::{self, ItemKind, Kind};

pub const MAX_TEMPLATE_CHARS: usize = 20_000;
pub const MAX_DEPTH: usize = 4;
pub const MAX_OUTPUT_CHARS: usize = 200_000;
/// What a shortened text ends with.
pub const TRUNCATED_SUFFIX: &str = "\u{2026} [truncated]";
/// A text is never shortened below this many characters.
const MIN_KEPT_CHARS: usize = 200;
/// Node visits one render may make; a loop of loops of empty bodies must end.
const MAX_STEPS: usize = 2_000_000;
/// Shortening rounds before the output itself is cut.
const MAX_ROUNDS: usize = 4_000;

/// The values a template is rendered with, by catalogue name.
pub type Vars = BTreeMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateError {
    pub message: String,
    /// Both start at 1; the column counts characters.
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for TemplateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} (line {}, column {})",
            self.message, self.line, self.column
        )
    }
}

impl std::error::Error for TemplateError {}

fn error_at(src: &str, byte: usize, message: impl Into<String>) -> TemplateError {
    let before = &src[..byte.min(src.len())];
    let line = before.matches('\n').count() + 1;
    let column = before
        .rsplit('\n')
        .next()
        .map_or(0, |last| last.chars().count())
        + 1;
    TemplateError {
        message: message.into(),
        line,
        column,
    }
}

// ---- lexing ----

enum Tok {
    Text(String),
    /// A tag's trimmed inside and the byte offset of its `{{`.
    Tag(String, usize),
}

fn blank(text: &str) -> bool {
    text.chars().all(|c| matches!(c, ' ' | '\t' | '\r'))
}

fn lex(src: &str) -> Result<Vec<Tok>, TemplateError> {
    let mut toks = Vec::new();
    let mut text = String::new();
    // Whether the pending text starts at the beginning of a line.
    let mut at_line_start = true;
    let mut i = 0;
    while i < src.len() {
        let Some(found) = src[i..].find("{{") else {
            text.push_str(&src[i..]);
            break;
        };
        let open = i + found;
        text.push_str(&src[i..open]);
        if text.ends_with('\\') {
            text.pop();
            text.push_str("{{");
            i = open + 2;
            continue;
        }
        let after = open + 2;
        let (inner, end, comment) = if src[after..].starts_with("!--") {
            match src[after + 3..].find("--}}") {
                Some(p) => (String::new(), after + 3 + p + 4, true),
                None => {
                    return Err(error_at(
                        src,
                        open,
                        "this comment is never closed; end it with `--}}`",
                    ));
                }
            }
        } else {
            match src[after..].find("}}") {
                Some(p) => (
                    src[after..after + p].trim().to_string(),
                    after + p + 2,
                    false,
                ),
                None => {
                    return Err(error_at(
                        src,
                        open,
                        "`{{` is never closed; end the tag with `}}` (write `\\{{` for a literal `{{`)",
                    ));
                }
            }
        };
        let block = comment || inner.starts_with('#') || inner.starts_with('/') || inner == "else";
        let rest = &src[end..];
        let line_rest = rest.split('\n').next().unwrap_or("");
        let last_line_start = text.rfind('\n').map(|p| p + 1);
        let prefix = match last_line_start {
            Some(p) => Some(&text[p..]),
            None if at_line_start => Some(text.as_str()),
            None => None,
        };
        let standalone = block && prefix.is_some_and(blank) && blank(line_rest);
        let mut next = end;
        if standalone {
            text.truncate(last_line_start.unwrap_or(0));
            next = match rest.find('\n') {
                Some(p) => end + p + 1,
                None => src.len(),
            };
            at_line_start = true;
        } else {
            at_line_start = false;
        }
        if !comment {
            if !text.is_empty() {
                toks.push(Tok::Text(std::mem::take(&mut text)));
            }
            toks.push(Tok::Tag(inner, open));
        }
        i = next;
    }
    if !text.is_empty() {
        toks.push(Tok::Text(text));
    }
    Ok(toks)
}

// ---- the parse tree ----

#[derive(Debug, Clone, PartialEq)]
enum Ref {
    Var(String),
    This,
    Field(String),
    Index,
}

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Text(String),
    Var(Ref),
    If {
        cond: Ref,
        then: Vec<Node>,
        otherwise: Vec<Node>,
    },
    Each {
        list: String,
        body: Vec<Node>,
    },
}

/// A validated template, ready to render.
#[derive(Debug, Clone)]
pub struct Template {
    nodes: Vec<Node>,
    used: BTreeSet<String>,
}

impl Template {
    /// The catalogue names the template refers to, sorted.
    pub fn variables_used(&self) -> Vec<String> {
        self.used.iter().cloned().collect()
    }
}

enum End {
    Eof,
    Else(usize),
    CloseIf(usize),
    CloseEach(usize),
}

struct Parser<'a> {
    src: &'a str,
    toks: std::vec::IntoIter<Tok>,
    used: BTreeSet<String>,
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.split('.').all(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Use {
    Print,
    Test,
}

impl Parser<'_> {
    fn err(&self, at: usize, message: impl Into<String>) -> TemplateError {
        error_at(self.src, at, message)
    }

    fn reference(
        &mut self,
        name: &str,
        at: usize,
        scope: &[&'static ItemKind],
        usage: Use,
    ) -> Result<Ref, TemplateError> {
        if name == "@index" {
            if scope.is_empty() {
                return Err(self.err(at, "`@index` only works inside `{{#each}}`"));
            }
            return Ok(Ref::Index);
        }
        if name == "this" || name.starts_with("this.") {
            let Some(item) = scope.last() else {
                return Err(self.err(at, format!("`{name}` only works inside `{{{{#each}}}}`")));
            };
            return match (name.strip_prefix("this."), item) {
                (None, ItemKind::Text) => Ok(Ref::This),
                (None, ItemKind::Record(fields)) => Err(self.err(
                    at,
                    format!(
                        "`this` is a record here; use one of its fields: {}",
                        field_names(fields)
                    ),
                )),
                (Some(_), ItemKind::Text) => Err(self.err(
                    at,
                    "the items of this list are text; use `{{this}}`, not a field",
                )),
                (Some(field), ItemKind::Record(fields)) => {
                    if fields.iter().any(|f| f.name == field) {
                        Ok(Ref::Field(field.to_string()))
                    } else {
                        Err(self.err(
                            at,
                            format!(
                                "unknown field `{field}` of the list items; use one of: {}",
                                field_names(fields)
                            ),
                        ))
                    }
                }
            };
        }
        if !valid_name(name) {
            return Err(self.err(
                at,
                if name.is_empty() {
                    "this tag needs a variable name".to_string()
                } else {
                    format!("`{name}` is not a valid variable name")
                },
            ));
        }
        let Some(variable) = catalogue::find(name) else {
            return Err(self.err(
                at,
                format!("unknown variable `{name}`; see the list of available variables"),
            ));
        };
        if matches!(variable.kind, Kind::List(_)) && usage == Use::Print {
            return Err(self.err(
                at,
                format!(
                    "`{name}` is a list; print its items with `{{{{#each {name}}}}}...{{{{/each}}}}` or test it with `{{{{#if {name}}}}}`"
                ),
            ));
        }
        self.used.insert(name.to_string());
        Ok(Ref::Var(name.to_string()))
    }

    fn list(&mut self, name: &str, at: usize) -> Result<&'static ItemKind, TemplateError> {
        if !valid_name(name) {
            return Err(self.err(
                at,
                if name.is_empty() {
                    "`{{#each}}` needs the name of a list".to_string()
                } else {
                    format!("`{name}` is not a valid list name")
                },
            ));
        }
        match catalogue::find(name).map(|v| &v.kind) {
            Some(Kind::List(item)) => {
                self.used.insert(name.to_string());
                Ok(item)
            }
            Some(Kind::Scalar(_)) => Err(self.err(
                at,
                format!("`{name}` is not a list, so `{{{{#each}}}}` cannot repeat it"),
            )),
            None => Err(self.err(
                at,
                format!("unknown variable `{name}`; see the list of available variables"),
            )),
        }
    }

    fn nodes(
        &mut self,
        depth: usize,
        scope: &mut Vec<&'static ItemKind>,
    ) -> Result<(Vec<Node>, End), TemplateError> {
        let mut out = Vec::new();
        while let Some(tok) = self.toks.next() {
            let (inner, at) = match tok {
                Tok::Text(text) => {
                    out.push(Node::Text(text));
                    continue;
                }
                Tok::Tag(inner, at) => (inner, at),
            };
            let (head, arg) = match inner.split_once(char::is_whitespace) {
                Some((head, arg)) => (head, arg.trim()),
                None => (inner.as_str(), ""),
            };
            match head {
                "else" if arg.is_empty() => return Ok((out, End::Else(at))),
                "/if" if arg.is_empty() => return Ok((out, End::CloseIf(at))),
                "/each" if arg.is_empty() => return Ok((out, End::CloseEach(at))),
                "#if" => {
                    let cond = self.reference(arg, at, scope, Use::Test)?;
                    if depth + 1 > MAX_DEPTH {
                        return Err(self.too_deep(at));
                    }
                    let (then, end) = self.nodes(depth + 1, scope)?;
                    let otherwise = match end {
                        End::CloseIf(_) => Vec::new(),
                        End::Else(_) => {
                            let (otherwise, end) = self.nodes(depth + 1, scope)?;
                            match end {
                                End::CloseIf(_) => otherwise,
                                End::Else(again) => {
                                    return Err(
                                        self.err(again, "an `{{#if}}` has only one `{{else}}`")
                                    );
                                }
                                End::CloseEach(close) => {
                                    return Err(self.err(
                                        close,
                                        "`{{/each}}` closes a block that was opened with `{{#if}}`",
                                    ));
                                }
                                End::Eof => return Err(self.unclosed("#if", at)),
                            }
                        }
                        End::CloseEach(close) => {
                            return Err(self.err(
                                close,
                                "`{{/each}}` closes a block that was opened with `{{#if}}`",
                            ));
                        }
                        End::Eof => return Err(self.unclosed("#if", at)),
                    };
                    out.push(Node::If {
                        cond,
                        then,
                        otherwise,
                    });
                }
                "#each" => {
                    let item = self.list(arg, at)?;
                    if depth + 1 > MAX_DEPTH {
                        return Err(self.too_deep(at));
                    }
                    scope.push(item);
                    let parsed = self.nodes(depth + 1, scope);
                    scope.pop();
                    let (body, end) = parsed?;
                    match end {
                        End::CloseEach(_) => out.push(Node::Each {
                            list: arg.to_string(),
                            body,
                        }),
                        End::Else(extra) => {
                            return Err(self.err(
                                extra,
                                "`{{else}}` only works inside `{{#if}}`, not directly in `{{#each}}`",
                            ));
                        }
                        End::CloseIf(close) => {
                            return Err(self.err(
                                close,
                                "`{{/if}}` closes a block that was opened with `{{#each}}`",
                            ));
                        }
                        End::Eof => return Err(self.unclosed("#each", at)),
                    }
                }
                _ if inner.starts_with('#') || inner.starts_with('/') || inner == "else" => {
                    return Err(self.err(
                        at,
                        format!(
                            "`{{{{{inner}}}}}` is not a tag; the blocks are `{{{{#if}}}}`, `{{{{else}}}}`, `{{{{/if}}}}`, `{{{{#each}}}}` and `{{{{/each}}}}`"
                        ),
                    ));
                }
                _ => {
                    if !arg.is_empty() {
                        return Err(self.err(
                            at,
                            format!("`{{{{{inner}}}}}` is not a tag; a variable stands alone in the braces"),
                        ));
                    }
                    let var = self.reference(&inner, at, scope, Use::Print)?;
                    out.push(Node::Var(var));
                }
            }
        }
        Ok((out, End::Eof))
    }

    fn too_deep(&self, at: usize) -> TemplateError {
        self.err(
            at,
            format!("blocks are nested more than {MAX_DEPTH} levels deep"),
        )
    }

    fn unclosed(&self, opener: &str, at: usize) -> TemplateError {
        let closer = if opener == "#if" { "/if" } else { "/each" };
        self.err(
            at,
            format!("`{{{{{opener}}}}}` is never closed; add `{{{{{closer}}}}}`"),
        )
    }
}

fn field_names(fields: &[catalogue::Field]) -> String {
    fields.iter().map(|f| f.name).collect::<Vec<_>>().join(", ")
}

/// Parses and validates `src`.
pub fn parse(src: &str) -> Result<Template, TemplateError> {
    let chars = src.chars().count();
    if chars > MAX_TEMPLATE_CHARS {
        return Err(TemplateError {
            message: format!(
                "the template is {chars} characters long; the limit is {MAX_TEMPLATE_CHARS}"
            ),
            line: 1,
            column: 1,
        });
    }
    let toks = lex(src)?;
    let mut parser = Parser {
        src,
        toks: toks.into_iter(),
        used: BTreeSet::new(),
    };
    let (nodes, end) = parser.nodes(0, &mut Vec::new())?;
    match end {
        End::Eof => Ok(Template {
            nodes,
            used: parser.used,
        }),
        End::Else(at) => Err(parser.err(at, "`{{else}}` without an open `{{#if}}`")),
        End::CloseIf(at) => Err(parser.err(at, "`{{/if}}` without an open `{{#if}}`")),
        End::CloseEach(at) => Err(parser.err(at, "`{{/each}}` without an open `{{#each}}`")),
    }
}

// ---- rendering ----

struct Out {
    text: String,
    chars: usize,
    steps: usize,
    over: bool,
}

impl Out {
    fn push(&mut self, text: &str) -> bool {
        if self.over {
            return false;
        }
        let n = text.chars().count();
        if self.chars + n > MAX_OUTPUT_CHARS {
            self.over = true;
            return false;
        }
        self.text.push_str(text);
        self.chars += n;
        true
    }

    fn step(&mut self) -> bool {
        self.steps += 1;
        if self.steps > MAX_STEPS {
            self.over = true;
        }
        !self.over
    }
}

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(n) => n.as_f64().is_some_and(|n| n != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(map) => !map.is_empty(),
    }
}

struct Frame<'a> {
    item: &'a Value,
    index: usize,
}

/// `None` for an unknown name and for `@index`, which has no stored value.
fn lookup<'a>(reference: &Ref, vars: &'a Vars, frames: &[Frame<'a>]) -> Option<&'a Value> {
    match reference {
        Ref::Var(name) => vars.get(name),
        Ref::This => frames.last().map(|f| f.item),
        Ref::Field(field) => frames.last().and_then(|f| f.item.get(field)),
        Ref::Index => None,
    }
}

fn eval<'a>(nodes: &'a [Node], vars: &'a Vars, frames: &mut Vec<Frame<'a>>, out: &mut Out) -> bool {
    for node in nodes {
        if !out.step() {
            return false;
        }
        match node {
            Node::Text(text) => {
                if !out.push(text) {
                    return false;
                }
            }
            Node::Var(Ref::Index) => {
                let index = frames.last().map_or(0, |f| f.index);
                if !out.push(&index.to_string()) {
                    return false;
                }
            }
            Node::Var(reference) => {
                let shown = match lookup(reference, vars, frames) {
                    Some(Value::String(text)) => out.push(text),
                    Some(Value::Number(n)) => out.push(&n.to_string()),
                    Some(Value::Bool(flag)) => out.push(if *flag { "true" } else { "false" }),
                    _ => true,
                };
                if !shown {
                    return false;
                }
            }
            Node::If {
                cond,
                then,
                otherwise,
            } => {
                let yes = match cond {
                    Ref::Index => frames.last().is_some_and(|f| f.index > 0),
                    other => lookup(other, vars, frames).is_some_and(truthy),
                };
                if !eval(if yes { then } else { otherwise }, vars, frames, out) {
                    return false;
                }
            }
            Node::Each { list, body } => {
                let Some(Value::Array(items)) = vars.get(list) else {
                    continue;
                };
                for (index, item) in items.iter().enumerate() {
                    frames.push(Frame { item, index });
                    let go = eval(body, vars, frames, out);
                    frames.pop();
                    if !go {
                        return false;
                    }
                }
            }
        }
    }
    true
}

/// One pass: the text (at most the limit) and whether it ran out of room.
fn render_once(template: &Template, vars: &Vars) -> (String, bool) {
    let mut out = Out {
        text: String::new(),
        chars: 0,
        steps: 0,
        over: false,
    };
    eval(&template.nodes, vars, &mut Vec::new(), &mut out);
    (out.text, out.over)
}

/// Cuts `text` to `max` characters including [`TRUNCATED_SUFFIX`]; `None`
/// when it already fits.
pub fn cut_text(text: &str, max: usize) -> Option<String> {
    if text.chars().count() <= max {
        return None;
    }
    let keep = max.saturating_sub(TRUNCATED_SUFFIX.chars().count());
    let mut cut: String = text.chars().take(keep).collect();
    cut.push_str(TRUNCATED_SUFFIX);
    Some(cut)
}

enum Spot {
    Top(String),
    Item(String, usize),
    Field(String, usize, String),
}

/// Halves the longest text that can still be shortened; `false` when nothing can.
fn shrink_largest(vars: &mut Vars) -> bool {
    let mut candidates: Vec<(usize, Spot)> = Vec::new();
    for (name, value) in vars.iter() {
        match value {
            Value::String(text) => candidates.push((text.len(), Spot::Top(name.clone()))),
            Value::Array(items) => {
                for (index, item) in items.iter().enumerate() {
                    match item {
                        Value::String(text) => {
                            candidates.push((text.len(), Spot::Item(name.clone(), index)));
                        }
                        Value::Object(fields) => {
                            for (field, value) in fields {
                                if let Value::String(text) = value {
                                    candidates.push((
                                        text.len(),
                                        Spot::Field(name.clone(), index, field.clone()),
                                    ));
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
    // Longest first; the stable sort keeps catalogue order among equals.
    candidates.sort_by_key(|(len, _)| std::cmp::Reverse(*len));
    for (_, spot) in candidates {
        let target = match &spot {
            Spot::Top(name) => vars.get_mut(name),
            Spot::Item(name, index) => vars
                .get_mut(name)
                .and_then(|v| v.as_array_mut())
                .and_then(|items| items.get_mut(*index)),
            Spot::Field(name, index, field) => vars
                .get_mut(name)
                .and_then(|v| v.as_array_mut())
                .and_then(|items| items.get_mut(*index))
                .and_then(|item| item.get_mut(field.as_str())),
        };
        let Some(Value::String(text)) = target else {
            continue;
        };
        let whole: &str = text.as_str();
        let body = whole.strip_suffix(TRUNCATED_SUFFIX).unwrap_or(whole);
        let chars = body.chars().count();
        if chars <= MIN_KEPT_CHARS {
            continue;
        }
        let keep = (chars / 2).max(MIN_KEPT_CHARS);
        let mut cut: String = body.chars().take(keep).collect();
        cut.push_str(TRUNCATED_SUFFIX);
        *text = cut;
        return true;
    }
    false
}

/// The text and whether anything was shortened to make it fit.
pub struct Output {
    pub text: String,
    pub truncated: bool,
}

/// Renders `template` with `vars`, shortening values as described in the
/// module docs when the result would be longer than [`MAX_OUTPUT_CHARS`].
pub fn render(template: &Template, vars: &Vars) -> Output {
    let (text, over) = render_once(template, vars);
    if !over {
        return Output {
            text,
            truncated: false,
        };
    }
    let mut vars = vars.clone();
    for _ in 0..MAX_ROUNDS {
        if !shrink_largest(&mut vars) {
            break;
        }
        let (text, over) = render_once(template, &vars);
        if !over {
            return Output {
                text,
                truncated: true,
            };
        }
    }
    let (text, _) = render_once(template, &vars);
    let text = cut_text(&text, MAX_OUTPUT_CHARS.saturating_sub(1)).unwrap_or(text);
    Output {
        text,
        truncated: true,
    }
}
