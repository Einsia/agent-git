//! Changing a JSON file in its own text. A document parsed and written back comes out with its
//! keys sorted and its layout reflowed, which turns one added hook into a rewrite of a file the
//! person owns. Here only the values a change touches are written; every other byte is copied.

use serde::Serialize as _;
use serde_json::Value;
use std::ops::Range;

/// The key that tells, while a change runs, which item of the text an array element is. An array
/// can lose elements and gain new ones, so an element's index after the change says nothing about
/// where its text is. The key starts with NUL, and a file that already uses it is not edited.
const ORIGIN: &str = "\u{0}agit-origin";

/// `text`, a JSON document, after `change`, which must turn the document `text` holds into
/// `expected`. Values the change left equal are copied as written, items it kept keep their text
/// and the separators between them, and only what it added, removed or replaced is written, in the
/// layout around it. `None` when the text cannot be placed without a guess (an object with a key
/// twice, which a parse keeps only once; an element the change moved), or when the result would not
/// read back as `expected`; the caller then leaves the file alone.
pub(super) fn edit(
    text: &str,
    expected: &Value,
    change: impl FnOnce(&mut Value),
) -> Option<String> {
    let root = Parser { text, at: 0 }.document()?;
    let mut changed: Value = serde_json::from_str(text).ok()?;
    let mut origins = Vec::new();
    mark(&root, &mut changed, &mut origins)?;
    change(&mut changed);
    let render = Render {
        text,
        origins: &origins,
        style: Style::of(text, &root),
    };
    let edited = format!(
        "{}{}{}",
        &text[..root.span.start],
        render.node(&root, &changed)?,
        &text[root.span.end..]
    );
    (serde_json::from_str::<Value>(&edited).ok().as_ref() == Some(expected)).then_some(edited)
}

/// One value of the text: where it is, and for a container, where each of its items is.
struct Node {
    span: Range<usize>,
    items: Items,
}

enum Items {
    Scalar,
    Array(Vec<Node>),
    /// Each member's key as written, the key it decodes to, and its value.
    Object(Vec<(Range<usize>, String, Node)>),
}

impl Node {
    /// Where each item is; a member runs from its key to the end of its value.
    fn spans(&self) -> Vec<Range<usize>> {
        match &self.items {
            Items::Scalar => Vec::new(),
            Items::Array(items) => items.iter().map(|item| item.span.clone()).collect(),
            Items::Object(members) => members
                .iter()
                .map(|(key, _, value)| key.start..value.span.end)
                .collect(),
        }
    }
}

/// Reads the layout of text that `serde_json` has already accepted, so it checks only what the
/// layout depends on.
struct Parser<'t> {
    text: &'t str,
    at: usize,
}

impl Parser<'_> {
    fn document(mut self) -> Option<Node> {
        let root = self.value()?;
        self.space();
        (self.at == self.text.len()).then_some(root)
    }

    fn peek(&self) -> Option<u8> {
        self.text.as_bytes().get(self.at).copied()
    }

    fn space(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.at += 1;
        }
    }

    fn value(&mut self) -> Option<Node> {
        self.space();
        let start = self.at;
        let items = match self.peek()? {
            b'{' => {
                self.at += 1;
                let mut members: Vec<(Range<usize>, String, Node)> = Vec::new();
                if !self.empty(b'}') {
                    loop {
                        self.space();
                        let key = self.string()?;
                        let name: String = serde_json::from_str(&self.text[key.clone()]).ok()?;
                        if members.iter().any(|(_, other, _)| *other == name) {
                            return None;
                        }
                        self.space();
                        (self.peek()? == b':').then_some(())?;
                        self.at += 1;
                        members.push((key, name, self.value()?));
                        if self.close(b'}')? {
                            break;
                        }
                    }
                }
                Items::Object(members)
            }
            b'[' => {
                self.at += 1;
                let mut items = Vec::new();
                if !self.empty(b']') {
                    loop {
                        items.push(self.value()?);
                        if self.close(b']')? {
                            break;
                        }
                    }
                }
                Items::Array(items)
            }
            b'"' => {
                self.string()?;
                Items::Scalar
            }
            _ => {
                while !matches!(
                    self.peek(),
                    None | Some(b',' | b']' | b'}' | b' ' | b'\t' | b'\n' | b'\r')
                ) {
                    self.at += 1;
                }
                (self.at > start).then_some(())?;
                Items::Scalar
            }
        };
        Some(Node {
            span: start..self.at,
            items,
        })
    }

    /// Whether the container just opened closes at once, consuming the closing byte if so.
    fn empty(&mut self, closing: u8) -> bool {
        self.space();
        let empty = self.peek() == Some(closing);
        if empty {
            self.at += 1;
        }
        empty
    }

    /// After an item: `Some(true)` at the container's closing byte, `Some(false)` at a comma.
    fn close(&mut self, closing: u8) -> Option<bool> {
        self.space();
        let byte = self.peek()?;
        self.at += 1;
        match byte {
            b',' => Some(false),
            byte if byte == closing => Some(true),
            _ => None,
        }
    }

    fn string(&mut self) -> Option<Range<usize>> {
        let start = self.at;
        (self.peek()? == b'"').then_some(())?;
        self.at += 1;
        loop {
            match self.peek()? {
                b'"' => {
                    self.at += 1;
                    return Some(start..self.at);
                }
                b'\\' => self.at += 2,
                _ => self.at += 1,
            }
        }
    }
}

/// Marks every object that is an array element of `value`, whose text `node` is, with the index of
/// its node in `origins`.
fn mark<'n>(node: &'n Node, value: &mut Value, origins: &mut Vec<&'n Node>) -> Option<()> {
    match (&node.items, value) {
        (Items::Object(members), Value::Object(map)) => {
            if map.contains_key(ORIGIN) {
                return None;
            }
            for (_, name, child) in members {
                mark(child, map.get_mut(name)?, origins)?;
            }
        }
        (Items::Array(items), Value::Array(elements)) => {
            for (item, element) in items.iter().zip(elements) {
                mark(item, element, origins)?;
                if let Value::Object(map) = element {
                    map.insert(ORIGIN.to_owned(), origins.len().into());
                    origins.push(item);
                }
            }
        }
        (Items::Scalar, _) => {}
        _ => return None,
    }
    Some(())
}

/// `value` without the marks [`mark`] added.
fn unmarked(value: &Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| *key != ORIGIN)
                .map(|(key, value)| (key.clone(), unmarked(value)))
                .collect(),
        ),
        Value::Array(elements) => Value::Array(elements.iter().map(unmarked).collect()),
        other => other.clone(),
    }
}

/// How the file lays itself out, for the values written into it.
struct Style {
    newline: &'static str,
    /// One level of indentation.
    unit: String,
    /// What separates a key from its value where an object has no member to copy it from.
    colon: String,
    /// Whether the document spreads over lines; an empty container then gets its first items on
    /// lines of their own.
    multiline: bool,
}

impl Style {
    fn of(text: &str, root: &Node) -> Self {
        let multiline = text[root.span.clone()].contains('\n');
        let unit = text
            .split('\n')
            .skip(1)
            .map(indent)
            .find(|indent| !indent.is_empty())
            .unwrap_or("  ");
        Self {
            newline: if text.contains("\r\n") { "\r\n" } else { "\n" },
            unit: unit.to_owned(),
            colon: first_colon(text, root)
                .unwrap_or(if multiline { ": " } else { ":" })
                .to_owned(),
            multiline,
        }
    }
}

fn first_colon<'t>(text: &'t str, node: &Node) -> Option<&'t str> {
    match &node.items {
        Items::Scalar => None,
        Items::Array(items) => items.iter().find_map(|item| first_colon(text, item)),
        Items::Object(members) => members
            .first()
            .map(|(key, _, value)| &text[key.end..value.span.start]),
    }
}

/// The spaces and tabs `line` starts with.
fn indent(line: &str) -> &str {
    &line[..line.len() - line.trim_start_matches([' ', '\t']).len()]
}

/// The indentation of the line `at` is on.
fn line_indent(text: &str, at: usize) -> &str {
    indent(&text[text[..at].rfind('\n').map_or(0, |newline| newline + 1)..at])
}

/// Where the items of one container go: the text before the first, between two and after the
/// last, the indentation of the lines an item starts on, and whether items sit on lines of their
/// own.
struct Layout {
    leading: String,
    separator: String,
    trailing: String,
    child: String,
    multiline: bool,
}

struct Render<'a> {
    text: &'a str,
    origins: &'a [&'a Node],
    style: Style,
}

impl Render<'_> {
    /// The text of `node` changed into `target`.
    fn node(&self, node: &Node, target: &Value) -> Option<String> {
        let original: Value = serde_json::from_str(&self.text[node.span.clone()]).ok()?;
        if unmarked(target) == original {
            return Some(self.text[node.span.clone()].to_owned());
        }
        // Each piece with the index of the item it was, if it was one.
        let mut pieces: Vec<(Option<usize>, String)> = Vec::new();
        let layout = self.layout(node);
        match (&node.items, target) {
            (Items::Object(members), Value::Object(map)) => {
                for (index, (key, name, value)) in members.iter().enumerate() {
                    if let Some(changed) = map.get(name) {
                        let written = &self.text[key.start..value.span.start];
                        pieces.push((
                            Some(index),
                            format!("{written}{}", self.node(value, changed)?),
                        ));
                    }
                }
                let colon = members
                    .first()
                    .map_or(self.style.colon.as_str(), |(key, _, value)| {
                        &self.text[key.end..value.span.start]
                    });
                for (name, value) in map {
                    if name == ORIGIN || members.iter().any(|(_, other, _)| other == name) {
                        continue;
                    }
                    let key = Value::String(name.clone()).to_string();
                    let value = self.added(value, &layout.child, layout.multiline);
                    pieces.push((None, format!("{key}{colon}{value}")));
                }
            }
            (Items::Array(items), Value::Array(elements)) => {
                let mut next = 0;
                for element in elements {
                    let origin = match element.get(ORIGIN).and_then(Value::as_u64) {
                        Some(origin) => {
                            let item = *self.origins.get(usize::try_from(origin).ok()?)?;
                            Some(items.iter().position(|own| std::ptr::eq(own, item))?)
                        }
                        None if !element.is_object() => (next..items.len()).find(|&index| {
                            serde_json::from_str::<Value>(&self.text[items[index].span.clone()])
                                .is_ok_and(|item| item == unmarked(element))
                        }),
                        None => None,
                    };
                    match origin {
                        Some(index) if index < next => return None,
                        Some(index) => {
                            next = index + 1;
                            pieces.push((Some(index), self.node(&items[index], element)?));
                        }
                        None => pieces
                            .push((None, self.added(element, &layout.child, layout.multiline))),
                    }
                }
            }
            // A value replaced by one of another kind starts where the old one did.
            _ => {
                let indent = line_indent(self.text, node.span.start);
                return Some(self.added(target, indent, self.style.multiline));
            }
        }
        Some(self.compose(node, &layout, &pieces))
    }

    fn layout(&self, node: &Node) -> Layout {
        let spans = node.spans();
        let base = line_indent(self.text, node.span.start);
        let newline = self.style.newline;
        match (spans.first(), spans.last()) {
            (Some(first), Some(last)) => {
                let leading = &self.text[node.span.start + 1..first.start];
                Layout {
                    leading: leading.to_owned(),
                    separator: match spans.get(1) {
                        Some(second) => self.text[first.end..second.start].to_owned(),
                        None => format!(",{leading}"),
                    },
                    trailing: self.text[last.end..node.span.end - 1].to_owned(),
                    child: match leading.rfind('\n') {
                        Some(at) => leading[at + 1..].to_owned(),
                        None => format!("{base}{}", self.style.unit),
                    },
                    multiline: leading.contains('\n'),
                }
            }
            _ if self.style.multiline => {
                let child = format!("{base}{}", self.style.unit);
                Layout {
                    leading: format!("{newline}{child}"),
                    separator: format!(",{newline}{child}"),
                    trailing: format!("{newline}{base}"),
                    child,
                    multiline: true,
                }
            }
            _ => Layout {
                leading: String::new(),
                separator: ",".to_owned(),
                trailing: String::new(),
                child: base.to_owned(),
                multiline: false,
            },
        }
    }

    /// A value the change added, starting on a line indented by `indent`.
    fn added(&self, value: &Value, indent: &str, multiline: bool) -> String {
        let value = unmarked(value);
        if !multiline {
            return value.to_string();
        }
        let mut written = Vec::new();
        let formatter = serde_json::ser::PrettyFormatter::with_indent(self.style.unit.as_bytes());
        value
            .serialize(&mut serde_json::Serializer::with_formatter(
                &mut written,
                formatter,
            ))
            .expect("a JSON value serializes");
        String::from_utf8(written)
            .expect("serialized JSON is UTF-8")
            .replace('\n', &format!("{}{indent}", self.style.newline))
    }

    /// The container `node` holding `pieces`. Two items that were neighbours keep the text between
    /// them; any other pair gets the container's own separator.
    fn compose(&self, node: &Node, layout: &Layout, pieces: &[(Option<usize>, String)]) -> String {
        let spans = node.spans();
        let open = &self.text[node.span.start..node.span.start + 1];
        let close = &self.text[node.span.end - 1..node.span.end];
        if pieces.is_empty() {
            return format!("{open}{close}");
        }
        let mut out = format!("{open}{}", layout.leading);
        for (at, (origin, piece)) in pieces.iter().enumerate() {
            if at > 0 {
                match (pieces[at - 1].0, origin) {
                    (Some(before), Some(index)) if *index == before + 1 => {
                        out.push_str(&self.text[spans[before].end..spans[*index].start]);
                    }
                    _ => out.push_str(&layout.separator),
                }
            }
            out.push_str(piece);
        }
        out.push_str(&layout.trailing);
        out.push_str(close);
        out
    }
}
