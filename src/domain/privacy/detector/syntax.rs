//! Syntactic identifier roles supply evidence for entropy-only discovery.

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
