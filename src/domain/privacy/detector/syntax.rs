//! Syntactic roles distinguish source references from opaque content.

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Word,
    Literal,
    Symbol(u8),
    Boundary,
}

#[derive(Clone, Copy)]
struct Token {
    start: usize,
    end: usize,
    kind: Kind,
}

/// Unquoted declarations and type positions name code, while the same spelling in a
/// configuration value remains credential evidence. Literal and comment tokens cannot open
/// a declaration or a typed scope.
pub(super) fn credential_reference_regions(text: &str) -> Vec<(usize, usize)> {
    let Some(tokens) = lex(text) else {
        return Vec::new();
    };
    let word = |i: usize| -> &str {
        tokens
            .get(i)
            .filter(|token| token.kind == Kind::Word)
            .map_or("", |token| &text[token.start..token.end])
    };
    let declaration_start = |i: usize| {
        if matches!(
            word(i),
            "const" | "let" | "var" | "val" | "final" | "static"
        ) {
            Some(i)
        } else if matches!(word(i), "mut" | "ref")
            && i > 0
            && matches!(word(i - 1), "let" | "var" | "const")
        {
            Some(i - 1)
        } else {
            None
        }
    };
    let mut scopes = Vec::new();
    // Only a closed scope can establish a type or executable role. An unfinished snippet
    // must not grant that role to credentials in the prose that follows it.
    let mut closed = vec![false; tokens.len()];
    let mut openings = Vec::new();
    for (i, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::Symbol(open @ (b'(' | b'{' | b'[')) => openings.push((i, open)),
            Kind::Symbol(close @ (b')' | b'}' | b']')) => {
                let expected = match close {
                    b')' => b'(',
                    b'}' => b'{',
                    _ => b'[',
                };
                let Some((at, open)) = openings.pop() else {
                    return Vec::new();
                };
                if open != expected {
                    return Vec::new();
                }
                closed[at] = true;
            }
            _ => {}
        }
    }
    let mut regions = Vec::new();
    let mut statement = 0;
    let mut function_body = false;
    let mut python_classes = std::collections::HashSet::new();
    for (i, token) in tokens.iter().enumerate() {
        match token.kind {
            Kind::Boundary | Kind::Symbol(b';' | b'}') => statement = i + 1,
            _ => {}
        }
        if matches!(token.kind, Kind::Symbol(b';' | b'}')) {
            function_body = false;
        }
        if i == statement
            && word(i) == "class"
            && !word(i + 1).is_empty()
            && tokens
                .get(i + 2)
                .is_some_and(|token| token.kind == Kind::Symbol(b':'))
            && tokens
                .get(i + 3)
                .is_some_and(|token| token.kind == Kind::Boundary)
            && tokens.get(i + 4).is_some_and(|body| {
                let prefix = text[..token.start].rsplit('\n').next().unwrap_or_default();
                let indent = &text[tokens[i + 3].end..body.start];
                body.kind == Kind::Word
                    && prefix.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
                    && indent.starts_with(prefix)
                    && indent.len() > prefix.len()
                    && indent.bytes().all(|byte| matches!(byte, b' ' | b'\t'))
            })
        {
            python_classes.insert(word(i + 1));
        }
        match token.kind {
            Kind::Symbol(open @ (b'(' | b'{' | b'[')) => {
                let typed = closed[i]
                    && (statement..i).any(|at| {
                        !word(at + 1).is_empty()
                            && match open {
                                b'(' => matches!(word(at), "fn" | "def" | "function"),
                                b'{' => {
                                    matches!(word(at), "struct" | "interface")
                                        || (word(at) == "type"
                                            && tokens.get(at + 2).is_some_and(|token| {
                                                token.kind == Kind::Symbol(b'=')
                                            }))
                                }
                                _ => false,
                            }
                    });
                let executable = closed[i]
                    && open == b'{'
                    && (function_body
                        || scopes.last().is_some_and(|&(_, _, executable)| executable)
                        || (statement..i)
                            .any(|at| word(at) == "match" && !word(at + 1).is_empty())
                        || (i > 0
                            && regions
                                .last()
                                .is_some_and(|&(_, end)| end == tokens[i - 1].end)));
                if open == b'{' {
                    function_body = false;
                }
                scopes.push((open, typed, executable));
                statement = i + 1;
            }
            Kind::Symbol(close @ (b')' | b'}' | b']')) => {
                let open = match close {
                    b')' => b'(',
                    b'}' => b'{',
                    _ => b'[',
                };
                let Some((found, typed, _)) = scopes.pop() else {
                    return Vec::new();
                };
                if found != open {
                    return Vec::new();
                }
                if open == b'(' && typed {
                    function_body = true;
                }
            }
            _ => {}
        }
        let next = match token.kind {
            Kind::Symbol(b'=') => {
                i + 1
                    + usize::from(
                        tokens
                            .get(i + 1)
                            .is_some_and(|token| token.kind == Kind::Symbol(b'>')),
                    )
            }
            Kind::Symbol(b':')
                if i > 0
                    && tokens[i - 1].kind != Kind::Symbol(b':')
                    && tokens
                        .get(i + 1)
                        .is_none_or(|token| token.kind != Kind::Symbol(b':')) =>
            {
                i + 1
            }
            _ => continue,
        };
        let Some(value) = tokens.get(next).filter(|token| token.kind == Kind::Word) else {
            continue;
        };
        let mut end = next;
        while let Some(separator) = tokens.get(end + 1) {
            let width = match separator.kind {
                Kind::Symbol(b'.') => 1,
                Kind::Symbol(b':')
                    if tokens
                        .get(end + 2)
                        .is_some_and(|token| token.kind == Kind::Symbol(b':')) =>
                {
                    2
                }
                _ => break,
            };
            let last = end + width + 1;
            if !tokens
                .get(last)
                .is_some_and(|token| token.kind == Kind::Word)
                || !(end..last).all(|at| tokens[at].end == tokens[at + 1].start)
            {
                break;
            }
            end = last;
        }
        // Shell declarations can name variables called `const` or `var` before assigning a
        // password. An adjacent keyword alone therefore cannot establish a source binding.
        let binding = i >= 2
            && tokens[i - 1].kind == Kind::Word
            && declaration_start(i - 2) == Some(statement);
        // The terminal word names the type; namespace segments can contain underscores.
        let annotation = token.kind == Kind::Symbol(b':')
            && reference_name(word(end))
            && scopes
                .last()
                .is_some_and(|(_, typed, executable)| *typed || (*executable && binding));
        let initializer = token.kind == Kind::Symbol(b'=')
            && next == i + 1
            && binding
            && (end > next || reference_name(word(next)));
        // Python bindings omit a declaration keyword. A preceding class declaration supplies
        // the name's role; spelling alone cannot exempt an unquoted configuration password.
        let python_reference = token.kind == Kind::Symbol(b'=')
            && i == statement + 1
            && tokens[statement].kind == Kind::Word
            && next == end
            && python_classes.contains(word(next))
            && tokens.get(end + 1).is_none_or(|token| {
                matches!(token.kind, Kind::Boundary | Kind::Symbol(b';'))
                    || (token.kind == Kind::Symbol(b'(') && closed[end + 1])
            });
        let qualified_call = end > next
            && scopes.last().is_some_and(|(_, _, executable)| *executable)
            && tokens.get(end + 1).is_some_and(|token| {
                token.kind == Kind::Symbol(b'{')
                    || (token.kind == Kind::Symbol(b'(') && tokens[end].end == token.start)
            });
        if annotation || initializer || qualified_call || python_reference {
            regions.push((value.start, tokens[end].end));
        }
    }
    regions
}

fn reference_name(value: &str) -> bool {
    // A leading acronym can qualify a source name; random mixed runs still need word shape.
    let capitals = value.bytes().take_while(u8::is_ascii_uppercase).count();
    super::camel_identifier(&value[capitals.saturating_sub(1)..])
}

/// Only bounded identifier roles are exempt; values and comments keep their entropy evidence.
pub(super) fn identifier_regions(text: &str) -> Vec<(usize, usize)> {
    let Some(tokens) = lex(text) else {
        return Vec::new();
    };
    let word = |i: usize| -> &str {
        tokens
            .get(i)
            .filter(|t| t.kind == Kind::Word)
            .map_or("", |t| &text[t.start..t.end])
    };
    let mut marked = vec![false; tokens.len()];
    let mut statement = 0;
    for i in 0..tokens.len() {
        let token = tokens[i];
        if token.kind == Kind::Boundary || token.kind == Kind::Symbol(b';') {
            mark_import(&tokens, text, statement, i, &mut marked);
            mark_call(&tokens, statement, i, &mut marked);
            statement = i + 1;
            continue;
        }
        if token.kind != Kind::Word {
            continue;
        }
        let previous = i.checked_sub(1).map_or("", &word);
        let following = tokens.get(i + 1).map(|t| t.kind);
        let declaration = match previous {
            "class" | "struct" | "interface" | "trait" | "enum" => {
                matches!(
                    following,
                    Some(Kind::Symbol(b'{' | b':' | b'(' | b'<' | b';'))
                ) || matches!(word(i + 1), "extends" | "implements")
            }
            "fn" | "def" | "function" => matches!(following, Some(Kind::Symbol(b'(' | b'<'))),
            "const" | "let" | "var" | "type" => {
                matches!(following, Some(Kind::Symbol(b'=' | b':' | b';')))
            }
            "namespace" | "package" => matches!(following, Some(Kind::Symbol(b'{' | b';'))),
            "extends" | "implements" => matches!(following, Some(Kind::Symbol(b'{' | b',' | b'<'))),
            _ => false,
        };
        let modified_binding = matches!(previous, "mut" | "ref")
            && i >= 2
            && matches!(word(i - 2), "let" | "var" | "const")
            && matches!(following, Some(Kind::Symbol(b'=' | b':' | b';')));
        if declaration || modified_binding {
            marked[i] = true;
        }
        if previous == "class" && following == Some(Kind::Symbol(b'(')) {
            mark_bases(&tokens, i + 2, &mut marked);
        }
    }
    mark_import(&tokens, text, statement, tokens.len(), &mut marked);
    mark_call(&tokens, statement, tokens.len(), &mut marked);

    // Qualified names inherit a proven role; punctuation alone cannot bless an opaque value.
    for i in (0..tokens.len()).rev() {
        if !marked[i] || tokens[i].kind != Kind::Word {
            continue;
        }
        if i >= 2 && tokens[i - 1].kind == Kind::Symbol(b'.') && tokens[i - 2].kind == Kind::Word {
            marked[i - 1] = true;
            marked[i - 2] = true;
        } else if i >= 3
            && tokens[i - 1].kind == Kind::Symbol(b':')
            && tokens[i - 2].kind == Kind::Symbol(b':')
            && tokens[i - 3].kind == Kind::Word
        {
            marked[i - 1] = true;
            marked[i - 2] = true;
            marked[i - 3] = true;
        }
    }
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for (token, include) in tokens.iter().zip(marked) {
        if include {
            if let Some(last) = regions.last_mut()
                && last.1 == token.start
            {
                last.1 = token.end;
            } else {
                regions.push((token.start, token.end));
            }
        }
    }
    regions
}

/// A call must occupy a complete statement; a parenthetical prose annotation is not evidence.
fn mark_call(tokens: &[Token], start: usize, end: usize, marked: &mut [bool]) {
    if start >= end || tokens[start].kind != Kind::Word {
        return;
    }
    let mut callee = start;
    loop {
        let next = callee + 1;
        if next >= end || tokens[callee].end != tokens[next].start {
            return;
        }
        let width = match tokens[next].kind {
            Kind::Symbol(b'(') => break,
            Kind::Symbol(b'.') => 1,
            Kind::Symbol(b':') if next + 1 < end && tokens[next + 1].kind == Kind::Symbol(b':') => {
                2
            }
            _ => return,
        };
        let name = next + width;
        if name >= end
            || tokens[name].kind != Kind::Word
            || (next..name).any(|i| tokens[i].end != tokens[i + 1].start)
        {
            return;
        }
        callee = name;
    }
    let mut closers = Vec::new();
    for (offset, token) in tokens[callee + 1..end].iter().enumerate() {
        match token.kind {
            Kind::Symbol(b'(') => closers.push(b')'),
            Kind::Symbol(b'[') => closers.push(b']'),
            Kind::Symbol(b'{') => closers.push(b'}'),
            Kind::Symbol(close @ (b')' | b']' | b'}')) => {
                if closers.pop() != Some(close) {
                    return;
                }
                if closers.is_empty() {
                    if callee + 2 + offset == end && simple_arguments(&tokens[callee + 2..end - 1])
                    {
                        marked[callee] = true;
                    }
                    return;
                }
            }
            Kind::Boundary => return,
            _ => {}
        }
    }
}

fn simple_arguments(tokens: &[Token]) -> bool {
    tokens.is_empty()
        || tokens
            .split(|token| token.kind == Kind::Symbol(b','))
            .all(|arg| {
                (arg.len() == 1 && arg[0].kind == Kind::Literal)
                    || qualified_name(arg)
                    || (!arg.is_empty()
                        && arg
                            .iter()
                            .all(|token| matches!(token.kind, Kind::Symbol(b'0'..=b'9')))
                        && arg.windows(2).all(|pair| pair[0].end == pair[1].start))
            })
}

fn mark_bases(tokens: &[Token], start: usize, marked: &mut [bool]) {
    let mut end = start;
    while let Some(token) = tokens.get(end) {
        match token.kind {
            Kind::Symbol(b')') => {
                if !tokens[start..end]
                    .split(|token| token.kind == Kind::Symbol(b','))
                    .all(qualified_name)
                {
                    return;
                }
                for i in start..end {
                    if matches!(tokens[i].kind, Kind::Word | Kind::Symbol(b'.' | b':')) {
                        marked[i] = true;
                    }
                }
                return;
            }
            Kind::Word | Kind::Symbol(b'.' | b':' | b',') => end += 1,
            _ => return,
        }
    }
}

fn mark_import(tokens: &[Token], text: &str, start: usize, end: usize, marked: &mut [bool]) {
    let Some(first) = tokens.get(start).filter(|_| start < end) else {
        return;
    };
    if first.kind != Kind::Word {
        return;
    }
    let head = &text[first.start..first.end];
    let mut list_start = start + 1;
    match head {
        "import" => {}
        "from" => {
            let Some(separator) = (list_start..end).find(|&i| {
                tokens[i].kind == Kind::Word && &text[tokens[i].start..tokens[i].end] == "import"
            }) else {
                return;
            };
            if !qualified_name(&tokens[list_start..separator]) {
                return;
            }
            list_start = separator + 1;
        }
        "use" | "using"
            if tokens
                .get(end)
                .is_some_and(|t| t.kind == Kind::Symbol(b';')) => {}
        _ => return,
    }
    if !import_list(&tokens[list_start..end], text) {
        return;
    }
    for i in start + 1..end {
        if matches!(tokens[i].kind, Kind::Word | Kind::Symbol(b'.' | b':')) {
            marked[i] = true;
        }
    }
}

fn qualified_name(tokens: &[Token]) -> bool {
    let mut expect_word = true;
    for token in tokens {
        match token.kind {
            Kind::Word if expect_word => expect_word = false,
            Kind::Symbol(b'.') if !expect_word => expect_word = true,
            _ => return false,
        }
    }
    !expect_word
}

fn import_list(tokens: &[Token], text: &str) -> bool {
    let mut groups = Vec::new();
    let mut expect_name = true;
    let mut i = 0;
    while i < tokens.len() {
        let token = tokens[i];
        match token.kind {
            Kind::Word if expect_name => expect_name = false,
            Kind::Word if &text[token.start..token.end] == "as" => {
                if !tokens.get(i + 1).is_some_and(|t| t.kind == Kind::Word) {
                    return false;
                }
                i += 1;
            }
            Kind::Word if &text[token.start..token.end] == "from" && groups.is_empty() => {
                return i + 2 == tokens.len() && tokens[i + 1].kind == Kind::Literal;
            }
            Kind::Symbol(b'.' | b',') if !expect_name => expect_name = true,
            Kind::Symbol(b':')
                if !expect_name
                    && tokens
                        .get(i + 1)
                        .is_some_and(|t| t.kind == Kind::Symbol(b':')) =>
            {
                i += 1;
                expect_name = true;
            }
            Kind::Symbol(open @ (b'{' | b'(')) if expect_name => {
                groups.push(open);
            }
            Kind::Symbol(close @ (b'}' | b')')) if !expect_name => {
                let expected = if close == b'}' { b'{' } else { b'(' };
                if groups.pop() != Some(expected) {
                    return false;
                }
                expect_name = false;
            }
            Kind::Symbol(b'*') if expect_name => expect_name = false,
            _ => return false,
        }
        i += 1;
    }
    !expect_name && groups.is_empty()
}

// Exemption discovery is optional; exceeding its budget leaves the carrier fully scannable.
const MAX_SYNTAX_TOKENS: usize = 32_768;

/// Shell words that make an assignment after them a variable's value, never a program
/// declaration.
const SHELL_WORDS: [&str; 6] = ["export", "local", "readonly", "declare", "typeset", "env"];

/// The statements of one text, each read once, for the assignments found in it in order.
///
/// A statement runs from just after the last statement separator before a position: `;`, `{`,
/// `}`, or a line end that an odd run of backslashes doesn't continue (see [`statement_prefix`]).
/// Before it is read, a statement's `\`-line-end continuations are removed, so a word continued
/// across a line counts as one word.
pub(super) struct Statements<'a> {
    text: &'a str,
    /// Where each separator is, in order.
    separators: Vec<usize>,
    /// The statement read last: assignments arrive in order, so each statement is read once.
    current: Option<Statement>,
}

struct Statement {
    /// Where the statement starts in the text.
    start: usize,
    /// The statement with its continuations removed.
    joined: String,
    /// Where each continuation removed from the statement begins in the text, and the bytes
    /// removed by then, so a position in the text maps into `joined`.
    removed: Vec<(usize, usize)>,
    /// The end of the first word of `joined` that is a shell word, if any.
    first_shell_word_end: Option<usize>,
}

impl<'a> Statements<'a> {
    pub(super) fn new(text: &'a str) -> Self {
        let bytes = text.as_bytes();
        let separators = (0..bytes.len())
            .filter(|&at| separates(bytes, at))
            .collect();
        Self {
            text,
            separators,
            current: None,
        }
    }

    /// The statement before `position`, with its continuations removed, and whether a shell
    /// word stands in it: [`statement_prefix`] of the text before `position`, read for every
    /// assignment of a statement at once.
    pub(super) fn before(&mut self, position: usize) -> (&str, bool) {
        let start = match self.separators.partition_point(|&at| at < position) {
            0 => 0,
            index => self.separators[index - 1] + 1,
        };
        if self
            .current
            .as_ref()
            .is_none_or(|statement| statement.start != start)
        {
            let end = self
                .separators
                .get(self.separators.partition_point(|&at| at < position))
                .copied()
                .unwrap_or(self.text.len());
            self.current = Some(Statement::read(self.text, start, end));
        }
        let statement = self.current.as_ref().expect("read above");
        let at = statement.joined_position(position);
        let line = &statement.joined[..at];
        // Only whole words count. A shell word cut off at `position` leaves the statement before
        // the name ending in something other than whitespace, which no declaration does, so the
        // assignment is not code either way.
        let shell = statement.first_shell_word_end.is_some_and(|end| end <= at);
        (line, shell)
    }
}

/// Whether the byte at `at` ends a statement: `;`, `{` or `}`, or a line end that isn't
/// continued, that is not preceded (before a `\r\n`'s `\r`, for its `\n`) by an odd run of
/// backslashes.
fn separates(bytes: &[u8], at: usize) -> bool {
    match bytes[at] {
        b';' | b'{' | b'}' => true,
        b'\n' | b'\r' => {
            let start = at - usize::from(bytes[at] == b'\n' && at > 0 && bytes[at - 1] == b'\r');
            let slashes = bytes[..start]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            slashes.is_multiple_of(2)
        }
        _ => false,
    }
}

impl Statement {
    fn read(text: &str, start: usize, end: usize) -> Self {
        let region = &text[start..end];
        let first = region.replace("\\\r\n", "");
        let joined = first.replace("\\\n", "");
        // Positions in `region` of what each pass removed, mapped back through the first pass.
        let mut removed = Vec::new();
        let mut taken = 0;
        let mut first_removed = Vec::new();
        for (at, _) in region.match_indices("\\\r\n") {
            first_removed.push(at);
        }
        // Every first-pass removal before a point took three bytes out of `region`. The second
        // pass meets its matches in increasing order, so the count of removals before them only
        // moves forward; recounting from the start for each makes a statement that alternates
        // both continuations quadratic.
        let mut before = 0;
        let mut to_region = |at_first: usize| {
            while before < first_removed.len() && first_removed[before] - 3 * before <= at_first {
                before += 1;
            }
            at_first + 3 * before
        };
        let mut all: Vec<(usize, usize)> = first_removed.iter().map(|&at| (at, 3)).collect();
        for (at, _) in first.match_indices("\\\n") {
            all.push((to_region(at), 2));
        }
        all.sort_unstable();
        for (at, length) in all {
            taken += length;
            removed.push((start + at, taken));
        }
        let mut words = Vec::new();
        let mut word_start = None;
        for (at, character) in joined.char_indices() {
            match (character.is_whitespace(), word_start) {
                (true, Some(begin)) => {
                    words.push((begin, at));
                    word_start = None;
                }
                (false, None) => word_start = Some(at),
                _ => {}
            }
        }
        if let Some(begin) = word_start {
            words.push((begin, joined.len()));
        }
        let first_shell_word_end = words
            .iter()
            .find(|&&(word, end)| SHELL_WORDS.contains(&&joined[word..end]))
            .map(|&(_, end)| end);
        Self {
            start,
            joined,
            removed,
            first_shell_word_end,
        }
    }

    /// Where `position` of the text falls in [`Self::joined`]: no continuation straddles an
    /// assignment's name, so every removal either ends before it or begins after it.
    fn joined_position(&self, position: usize) -> usize {
        let removed = self
            .removed
            .partition_point(|&(at, _)| at < position)
            .checked_sub(1)
            .map_or(0, |index| self.removed[index].1);
        position - self.start - removed
    }
}

/// A continued physical line retains the declaration authority of its logical statement. The
/// detector reads statements through [`Statements`]; this per-position form is the reference its
/// test compares against.
#[cfg(test)]
pub(super) fn statement_prefix(text: &str) -> &str {
    let bytes = text.as_bytes();
    let mut end = bytes.len();
    while let Some(at) = bytes[..end]
        .iter()
        .rposition(|byte| matches!(byte, b'\n' | b'\r' | b';' | b'{' | b'}'))
    {
        if matches!(bytes[at], b'\n' | b'\r') {
            let start = at - usize::from(bytes[at] == b'\n' && at > 0 && bytes[at - 1] == b'\r');
            let slashes = bytes[..start]
                .iter()
                .rev()
                .take_while(|&&byte| byte == b'\\')
                .count();
            if !slashes.is_multiple_of(2) {
                end = start - slashes;
                continue;
            }
        }
        return &text[at + 1..];
    }
    text
}

fn lex(text: &str) -> Option<Vec<Token>> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if tokens.len() >= MAX_SYNTAX_TOKENS {
            return None;
        }
        let start = i;
        let b = bytes[i];
        if b == b'\\' {
            let continuation = if bytes[i..].starts_with(b"\\\r\n") {
                3
            } else if bytes[i..].starts_with(b"\\\n") {
                2
            } else {
                0
            };
            if continuation > 0 {
                let preceding = bytes[..i]
                    .iter()
                    .rev()
                    .take_while(|&&byte| byte == b'\\')
                    .count();
                if preceding.is_multiple_of(2) {
                    let mut after = i + continuation;
                    loop {
                        if bytes[after..].starts_with(b"\\\r\n") {
                            after += 3;
                        } else if bytes[after..].starts_with(b"\\\n") {
                            after += 2;
                        } else {
                            break;
                        }
                    }
                    let word_byte = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_';
                    // Splicing joins adjacent word fragments. Treating them as separate tokens
                    // could manufacture a declaration keyword from part of a variable name.
                    if i > 0
                        && word_byte(bytes[i - 1])
                        && bytes.get(after).is_some_and(|&byte| word_byte(byte))
                    {
                        return None;
                    }
                    i = after;
                    continue;
                }
            }
        }
        let kind = if b == b'\n' || b == b'\r' {
            i += 1;
            Kind::Boundary
        } else if b.is_ascii_whitespace() {
            i += 1;
            continue;
        } else if b == b'#' || bytes[i..].starts_with(b"//") {
            while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                i += 1;
            }
            Kind::Boundary
        } else if bytes[i..].starts_with(b"/*") {
            i += 2;
            let mut depth = 1;
            while i < bytes.len() && depth > 0 {
                if bytes[i..].starts_with(b"/*") {
                    depth += 1;
                    i += 2;
                } else if bytes[i..].starts_with(b"*/") {
                    depth -= 1;
                    i += 2;
                } else {
                    i += 1;
                }
            }
            Kind::Boundary
        } else if b == b'/' {
            // Slash literals and division are ambiguous without a language grammar.
            // Withholding evidence for the rest of the line cannot hide a secret.
            while i < bytes.len() && !matches!(bytes[i], b'\n' | b'\r') {
                i += 1;
            }
            Kind::Literal
        } else if matches!(b, b'\"' | b'\'' | b'`') {
            let triple = bytes.get(i + 1) == Some(&b) && bytes.get(i + 2) == Some(&b);
            // An apostrophe followed by an unclosed identifier may be a lifetime or label.
            // Guessing that it opens a string can expose syntax inside a later literal.
            if b == b'\'' && !triple {
                let mut name_end = i + 1;
                for ch in text[name_end..].chars() {
                    if ch != '_' && !ch.is_alphanumeric() {
                        break;
                    }
                    name_end += ch.len_utf8();
                }
                if name_end > i + 1 && bytes.get(name_end) != Some(&b'\'') {
                    // A plain quoted phrase cannot contain lifetime or label punctuation.
                    let phrase_end = text[name_end..].chars().find(|ch| {
                        *ch != '_' && !ch.is_alphanumeric() && !matches!(ch, ' ' | '\t')
                    });
                    if phrase_end != Some('\'') {
                        return None;
                    }
                }
            }
            let width = if triple { 3 } else { 1 };
            i += width;
            while i < bytes.len() {
                if bytes[i] == b'\\' {
                    i = (i + 2).min(bytes.len());
                } else if bytes[i] == b
                    && (!triple || (bytes.get(i + 1) == Some(&b) && bytes.get(i + 2) == Some(&b)))
                {
                    i += width;
                    break;
                } else {
                    i += 1;
                }
            }
            Kind::Literal
        } else if b.is_ascii_alphabetic() || b == b'_' {
            i += 1;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            // Custom raw delimiters need a language grammar; partial recognition is unsafe.
            let mut delimiter = i;
            while bytes.get(delimiter) == Some(&b'#') {
                delimiter += 1;
            }
            if (delimiter > i && bytes.get(delimiter) == Some(&b'"'))
                || (bytes.get(i) == Some(&b'"') && bytes[i - 1] == b'R')
            {
                return None;
            }
            Kind::Word
        } else {
            i += text[i..].chars().next().unwrap().len_utf8();
            Kind::Symbol(b)
        };
        tokens.push(Token {
            start,
            end: i,
            kind,
        });
    }
    Some(tokens)
}

#[cfg(test)]
mod tests {
    use super::identifier_regions;

    /// A closed annotation scope does not prove that its value names a type. Classification
    /// must reject opaque values independently of whether the field name suggests a credential.
    /// Qualified paths retain their full range while their terminal type name supplies the shape.
    #[test]
    fn annotation_roles_require_reference_shaped_values() {
        for (text, name) in [
            (
                "interface Auth { value: PasswordFieldComponent; }",
                "PasswordFieldComponent",
            ),
            ("fn new(value: HubCredential)", "HubCredential"),
            (
                "struct Auth { value: secret_store::HubCredential, }",
                "secret_store::HubCredential",
            ),
            (
                "fn new(value: secret_store::credential_types::HubCredential)",
                "secret_store::credential_types::HubCredential",
            ),
        ] {
            let start = text.find(name).unwrap();
            assert_eq!(
                super::credential_reference_regions(text),
                vec![(start, start + name.len())],
                "{text}"
            );
            let opaque = text.replace(
                name.rsplit("::").next().unwrap(),
                "c7Zq4xR2mNv8LpT3wYb6Ks1Hd9Fg5Jt0",
            );
            assert!(
                super::credential_reference_regions(&opaque).is_empty(),
                "{opaque}"
            );
        }
    }

    fn includes(text: &str, needle: &str) -> bool {
        let start = text.find(needle).unwrap();
        identifier_regions(text)
            .iter()
            .any(|&(s, e)| s <= start && start + needle.len() <= e)
    }

    /// Structural roles span languages without treating string contents as executable syntax.
    #[test]
    fn roles_preserve_identifiers_and_leave_values_visible() {
        let name = "ExtraordinaryTransportCoordinator";
        for source in [
            format!("from transport.server import {name} # adapter"),
            format!("import {{ {name} }} from \"transport\";"),
            format!("use transport::{{{name}}};"),
            format!("class {name}:"),
            format!("class LocalServer(transport.{name}): pass"),
            format!("class LocalServer extends {name} {{}}"),
            format!("struct {name} {{}}"),
            format!("const {name} = \"value\";"),
            format!("let mut {name} = value;"),
            format!("transport.{name}()"),
            format!("transport::{name}()"),
        ] {
            assert!(includes(&source, name), "{source}");
        }
        for source in [
            name.to_owned(),
            format!("value = {name}"),
            format!("password = \"{name}\""),
            format!("# import {name}"),
            format!("/* class {name} {{}} */"),
            format!("const value = \"import {name}\";"),
            format!("call(\"{name}\")"),
            format!("call({name})"),
            format!("import valid; password = {name}"),
            format!("import valid # {name}"),
            format!("import arbitrary prose {name}"),
            format!("import {{ {name}"),
            format!("import {{ {name})"),
            format!("from arbitrary prose import {name}"),
            format!("class {name} secret prose"),
            format!("class Safe(secret {name}):"),
            format!("const {name} secret prose"),
            format!("/{name}()/"),
            format!("const pattern = /class {name} {{}}/;"),
            format!("/* outer /* inner */ class {name} {{}} */"),
            format!("let raw = r#\"\nclass {name} {{}}\n\"#;"),
            format!("auto raw = R\"tag(\"; class {name} {{}})tag\";"),
        ] {
            assert!(!includes(&source, name), "{source}");
        }
    }

    /// Syntax-budget exhaustion withholds exemptions instead of withholding secret scanning.
    #[test]
    fn syntax_budget_fails_closed() {
        let source = format!(
            "{}\nimport ExtraordinaryTransportCoordinator",
            ";".repeat(super::MAX_SYNTAX_TOKENS)
        );
        assert!(identifier_regions(&source).is_empty());
    }
}
