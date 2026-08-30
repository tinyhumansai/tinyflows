/// Whether `source` looks like it relies on n8n's code-node runtime globals
/// or return convention rather than tinyflows' stdin/stdout contract — a
/// lightweight lexer. String/comment contents are skipped, and `return` is
/// incompatible only outside a function body.
fn uses_n8n_code_globals(source: &str, check_top_level_return: bool) -> bool {
    #[derive(Clone, Copy)]
    enum PendingFunctionBody {
        Declaration(usize),
        Arrow,
    }

    #[derive(Clone, Copy)]
    struct ItemsBinding {
        depth: usize,
        expires_at_semicolon: bool,
    }

    let bytes = source.as_bytes();
    let mut index = 0;
    let mut brace_depth = 0usize;
    let mut paren_depth = 0usize;
    let mut function_depths = Vec::new();
    let mut pending_function_body = None;
    let mut pending_variable_declaration = false;
    let mut items_bindings = Vec::new();
    while index < bytes.len() {
        let starts_comment = bytes[index] == b'/'
            && matches!(bytes.get(index + 1), Some(b'/') | Some(b'*'));
        if matches!(pending_function_body, Some(PendingFunctionBody::Arrow))
            && !bytes[index].is_ascii_whitespace()
            && bytes[index] != b'{'
            && !starts_comment
        {
            pending_function_body = None;
        }
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len()
                    && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'/' if is_regex_start(bytes, index) => index = skip_regex(bytes, index),
            quote @ (b'\'' | b'"') => index = skip_quoted(bytes, index, quote),
            b'`' => {
                index += 1;
                while index < bytes.len() && bytes[index] != b'`' {
                    if bytes[index] == b'\\' {
                        index = (index + 2).min(bytes.len());
                    } else if bytes[index] == b'$' && bytes.get(index + 1) == Some(&b'{') {
                        let start = index + 2;
                        let end = template_expression_end(bytes, start);
                        if uses_n8n_code_globals(&source[start..end], check_top_level_return) {
                            return true;
                        }
                        index = (end + 1).min(bytes.len());
                    } else {
                        index += 1;
                    }
                }
                index = (index + 1).min(bytes.len());
            }
            b'=' if bytes.get(index + 1) == Some(&b'>') => {
                pending_function_body = Some(PendingFunctionBody::Arrow);
                index += 2;
            }
            b'(' => {
                paren_depth += 1;
                index += 1;
            }
            b')' => {
                paren_depth = paren_depth.saturating_sub(1);
                index += 1;
            }
            b'{' => {
                brace_depth += 1;
                let is_function_body = matches!(
                    pending_function_body,
                    Some(PendingFunctionBody::Arrow)
                ) || matches!(
                    pending_function_body,
                    Some(PendingFunctionBody::Declaration(depth)) if depth == paren_depth
                );
                if is_function_body {
                    function_depths.push(brace_depth);
                    pending_function_body = None;
                }
                index += 1;
            }
            b'}' => {
                if function_depths.last() == Some(&brace_depth) {
                    function_depths.pop();
                }
                items_bindings.retain(|binding: &ItemsBinding| binding.depth < brace_depth);
                brace_depth = brace_depth.saturating_sub(1);
                index += 1;
            }
            b';' => {
                items_bindings.retain(|binding| !binding.expires_at_semicolon);
                index += 1;
            }
            first if first.is_ascii_alphabetic() || matches!(first, b'_' | b'$') => {
                let start = index;
                index += 1;
                while bytes.get(index).is_some_and(|character| {
                    character.is_ascii_alphanumeric() || matches!(character, b'_' | b'$')
                }) {
                    index += 1;
                }
                let token = &source[start..index];
                if previous_significant(bytes, start) != Some(b'.')
                    && (token == "function"
                        || (!is_control_keyword(token) && method_body_follows(bytes, index)))
                {
                    pending_function_body = Some(PendingFunctionBody::Declaration(paren_depth));
                    pending_variable_declaration = false;
                } else if matches!(token, "const" | "let" | "var") {
                    pending_variable_declaration = true;
                } else if token == "items" {
                    let function_parameter = matches!(
                        pending_function_body,
                        Some(PendingFunctionBody::Declaration(depth)) if paren_depth > depth
                    );
                    let arrow_parameter = arrow_parameter_scope(bytes, index);
                    if pending_variable_declaration || function_parameter || arrow_parameter.is_some()
                    {
                        let block_scoped_parameter = function_parameter || arrow_parameter == Some(true);
                        items_bindings.push(ItemsBinding {
                            depth: brace_depth + usize::from(block_scoped_parameter),
                            expires_at_semicolon: arrow_parameter == Some(false),
                        });
                    } else if !items_bindings
                        .iter()
                        .any(|binding| binding.depth <= brace_depth)
                    {
                        return true;
                    }
                    pending_variable_declaration = false;
                } else if ["$json", "$input", "$node"].contains(&token)
                    || (check_top_level_return
                        && token == "return"
                        && function_depths.is_empty())
                {
                    return true;
                } else {
                    pending_variable_declaration = false;
                }
            }
            _ => index += 1,
        }
    }
    false
}

fn arrow_parameter_scope(bytes: &[u8], mut index: usize) -> Option<bool> {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index..index + 2) != Some(b"=>") {
        let close = bytes[index..].iter().position(|byte| *byte == b')')? + index;
        index = close + 1;
        while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
            index += 1;
        }
        if bytes.get(index..index + 2) != Some(b"=>") {
            return None;
        }
    }
    index += 2;
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    Some(bytes.get(index) == Some(&b'{'))
}

fn is_control_keyword(token: &str) -> bool {
    matches!(token, "if" | "for" | "while" | "switch" | "catch" | "with")
}

fn method_body_follows(bytes: &[u8], mut index: usize) -> bool {
    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
        index += 1;
    }
    if bytes.get(index) != Some(&b'(') {
        return false;
    }
    let mut depth = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            quote @ (b'\'' | b'"' | b'`') => index = skip_quoted(bytes, index, quote),
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth = depth.saturating_sub(1);
                index += 1;
                if depth == 0 {
                    while bytes.get(index).is_some_and(u8::is_ascii_whitespace) {
                        index += 1;
                    }
                    return bytes.get(index) == Some(&b'{');
                }
            }
            _ => index += 1,
        }
    }
    false
}

fn is_regex_start(bytes: &[u8], index: usize) -> bool {
    if bytes[..index]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
        .is_none_or(|byte| {
            matches!(
                byte,
                b'=' | b'(' | b'[' | b'{' | b',' | b':' | b';' | b'!' | b'?'
                    | b'&' | b'|' | b'+' | b'-' | b'*' | b'%' | b'^' | b'~'
                    | b'<' | b'>'
            )
        })
    {
        return true;
    }

    previous_identifier(bytes, index).is_some_and(|token| {
        matches!(
            token,
            b"return" | b"throw" | b"case" | b"delete" | b"typeof" | b"void" | b"instanceof"
        )
    })
}

fn previous_significant(bytes: &[u8], index: usize) -> Option<u8> {
    bytes[..index]
        .iter()
        .rev()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn previous_identifier(bytes: &[u8], index: usize) -> Option<&[u8]> {
    let end = bytes[..index]
        .iter()
        .rposition(|byte| !byte.is_ascii_whitespace())?
        + 1;
    let start = bytes[..end]
        .iter()
        .rposition(|byte| !(byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'$')))
        .map_or(0, |position| position + 1);
    (start < end).then_some(&bytes[start..end])
}

fn skip_regex(bytes: &[u8], mut index: usize) -> usize {
    index += 1;
    let mut in_class = false;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => index = (index + 2).min(bytes.len()),
            b'[' => {
                in_class = true;
                index += 1;
            }
            b']' => {
                in_class = false;
                index += 1;
            }
            b'/' if !in_class => {
                index += 1;
                while bytes.get(index).is_some_and(u8::is_ascii_alphabetic) {
                    index += 1;
                }
                return index;
            }
            _ => index += 1,
        }
    }
    index
}

fn skip_quoted(bytes: &[u8], mut index: usize, quote: u8) -> usize {
    index += 1;
    while index < bytes.len() {
        if bytes[index] == b'\\' {
            index = (index + 2).min(bytes.len());
        } else if bytes[index] == quote {
            return index + 1;
        } else {
            index += 1;
        }
    }
    index
}

fn template_expression_end(bytes: &[u8], mut index: usize) -> usize {
    let mut depth = 1usize;
    while index < bytes.len() {
        match bytes[index] {
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                index += 2;
                while index < bytes.len() && bytes[index] != b'\n' {
                    index += 1;
                }
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                index += 2;
                while index + 1 < bytes.len()
                    && !(bytes[index] == b'*' && bytes[index + 1] == b'/')
                {
                    index += 1;
                }
                index = (index + 2).min(bytes.len());
            }
            b'/' if is_regex_start(bytes, index) => index = skip_regex(bytes, index),
            quote @ (b'\'' | b'"' | b'`') => index = skip_quoted(bytes, index, quote),
            b'{' => {
                depth += 1;
                index += 1;
            }
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return index;
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    bytes.len()
}
