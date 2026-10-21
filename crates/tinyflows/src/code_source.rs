//! A static check on a `code` node's `source` that a dry run cannot make.
//!
//! The sandbox stubs `code` nodes, so a script that reads a variable the
//! runtime never defines dry-runs clean and fails every real run. The
//! runtime hands the function body one
//! variable, `input`; builders keep writing `item`, the name the `=item`
//! expression language uses. This flags a source that reads `item` without
//! binding it anywhere.
//!
//! It is a token scan, not a parser: comments and string literals are blanked,
//! then every `item` word is classified as a binding (`const item`,
//! `item =>`, `(item) =>`, `function (item)`, `def f(item)`, `for item in`,
//! `lambda item:`, `item = …`) or a read. Any binding anywhere clears the
//! source (no scoping), so it misses some bugs rather than flagging working
//! code.

/// Whether `source` reads an `item` variable it never binds. `python` selects
/// Python comment and string syntax; otherwise JavaScript's.
pub(crate) fn reads_undefined_item(source: &str, python: bool) -> bool {
    let code = blank_comments_and_strings(source, python);
    let toks = tokens(&code);
    let mut read = false;
    for i in (0..toks.len()).filter(|&i| word(&toks, Some(i)) == Some("item")) {
        let prev = punct(&toks, i.checked_sub(1));
        // `x.item` is a property, `{ item: 1 }` an object key.
        if prev == Some('.')
            || (!python
                && punct(&toks, Some(i + 1)) == Some(':')
                && matches!(prev, Some('{' | ',')))
        {
            continue;
        }
        if binds(&toks, i) {
            return false;
        }
        read = true;
    }
    read
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Tok<'a> {
    Word(&'a str),
    Punct(char),
}

fn word<'a>(toks: &[Tok<'a>], i: Option<usize>) -> Option<&'a str> {
    match toks.get(i?) {
        Some(Tok::Word(w)) => Some(w),
        _ => None,
    }
}

fn punct(toks: &[Tok<'_>], i: Option<usize>) -> Option<char> {
    match toks.get(i?) {
        Some(Tok::Punct(c)) => Some(*c),
        _ => None,
    }
}

/// Whether the `item` word at `i` declares the name rather than reading it.
fn binds(toks: &[Tok<'_>], i: usize) -> bool {
    const DECLARE: [&str; 5] = ["var", "let", "const", "for", "lambda"];
    // Walk back over a `a, b,` list to whatever introduces it.
    let mut start = i;
    while start >= 2
        && punct(toks, Some(start - 1)) == Some(',')
        && word(toks, Some(start - 2)).is_some()
    {
        start -= 2;
    }
    let before = |n: usize| start.checked_sub(n);
    let declares = |n: usize| word(toks, before(n)).is_some_and(|w| DECLARE.contains(&w));
    let opener = punct(toks, before(1));
    // A parameter name opens its slot: `function (item)`, `catch (item)`,
    // `function f(a = 0, item)`, `def f(x: int, item)`. After `=` it is a read.
    let in_params = matches!(punct(toks, i.checked_sub(1)), Some('(' | ','))
        && enclosing_paren(toks, i).is_some_and(|p| {
            matches!(word(toks, p.checked_sub(1)), Some("function" | "catch"))
                || matches!(word(toks, p.checked_sub(2)), Some("function" | "def"))
        });
    if in_params
        || declares(1)
        // `const { item } = …`, `let [item] = …`.
        || (matches!(opener, Some('{' | '[')) && declares(2))
    {
        return true;
    }
    // Walk forward over a `, b, c` list to whatever follows it.
    let mut end = i + 1;
    while punct(toks, Some(end)) == Some(',') && word(toks, Some(end + 1)).is_some() {
        end += 2;
    }
    let after = |n: usize| punct(toks, Some(end + n));
    matches!(word(toks, Some(end)), Some("in" | "of"))
        // `item = …` / `item => …` (a default, an assignment or an arrow).
        || (after(0) == Some('=') && after(1) != Some('='))
        // `(item) => …` / `(item, i) => …`.
        || (after(0) == Some(')') && after(1) == Some('=') && after(2) == Some('>'))
}

/// Index of the unmatched `(` around token `i`; `None` when the innermost
/// unmatched bracket is a `[` or `{`, or there is none.
fn enclosing_paren(toks: &[Tok<'_>], i: usize) -> Option<usize> {
    let mut depth = 0usize;
    for j in (0..i).rev() {
        match punct(toks, Some(j)) {
            Some(')' | ']' | '}') => depth += 1,
            Some(c @ ('(' | '[' | '{')) if depth == 0 => return (c == '(').then_some(j),
            Some('(' | '[' | '{') => depth -= 1,
            _ => {}
        }
    }
    None
}

/// Splits into words (`[A-Za-z0-9_$]+`) and single punctuation characters.
fn tokens(code: &str) -> Vec<Tok<'_>> {
    let mut toks = Vec::new();
    let mut start = None;
    for (i, c) in code.char_indices() {
        if c.is_alphanumeric() || c == '_' || c == '$' {
            start.get_or_insert(i);
            continue;
        }
        if let Some(s) = start.take() {
            toks.push(Tok::Word(&code[s..i]));
        }
        if !c.is_whitespace() {
            toks.push(Tok::Punct(c));
        }
    }
    if let Some(s) = start {
        toks.push(Tok::Word(&code[s..]));
    }
    toks
}

/// Replaces comments and string literals with whitespace, so a word inside
/// one is never mistaken for code.
fn blank_comments_and_strings(source: &str, python: bool) -> String {
    let mut out = String::with_capacity(source.len());
    let mut chars = source.chars().peekable();
    while let Some(c) = chars.next() {
        let line_comment = if python {
            c == '#'
        } else {
            c == '/' && chars.peek() == Some(&'/')
        };
        if line_comment {
            chars.by_ref().find(|&c| c == '\n');
            out.push('\n');
        } else if !python && c == '/' && chars.peek() == Some(&'*') {
            chars.next();
            let mut prev = ' ';
            for c in chars.by_ref() {
                if prev == '*' && c == '/' {
                    break;
                }
                prev = c;
            }
            out.push(' ');
        } else if c == '"' || c == '\'' || (!python && c == '`') {
            while let Some(s) = chars.next() {
                if s == '\\' {
                    chars.next();
                } else if s == c {
                    break;
                }
            }
            out.push(' ');
        } else {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
#[path = "code_source_tests.rs"]
mod tests;
