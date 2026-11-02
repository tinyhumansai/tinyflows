//! Tolerant extraction of JSON values from model completion text.

use serde_json::Value;

/// Best-effort parse of an LLM completion as structured JSON.
///
/// Accepts a bare JSON object/array or one wrapped in a markdown code fence
/// (```json … ``` or ``` … ```). Returns `None` for anything that doesn't
/// parse to an object or array — scalars pass through the legacy `{text}`
/// shape instead, since `item.<field>` addressing is meaningless on them.
pub fn parse_llm_json(text: &str) -> Option<Value> {
    let trimmed = text.trim();
    let candidate = match trimmed.strip_prefix("```") {
        Some(rest) => {
            let rest = rest.strip_prefix("json").unwrap_or(rest);
            match rest.rsplit_once("```") {
                Some((inner, _)) => inner.trim(),
                None => trimmed,
            }
        }
        None => trimmed,
    };
    let parsed = serde_json::from_str::<Value>(candidate).ok()?;
    matches!(parsed, Value::Object(_) | Value::Array(_)).then_some(parsed)
}

/// Find and parse a fenced JSON block (```json … ``` or ``` … ```) anywhere
/// in `text`, not just when the whole text starts with it. Returns `None` when
/// no fenced block parses to an object or array.
pub fn extract_fenced_json_block(text: &str) -> Option<Value> {
    let text = text.trim();
    // Look for the first opening ``` fence
    let fence_start = text.find("```")?;
    let after_fence = text[fence_start + 3..].trim();
    // Skip optional "json" after the opening fence
    let content = after_fence
        .strip_prefix("json")
        .unwrap_or(after_fence)
        .trim();
    // Find the *last* closing ``` (preferring the outermost fence, which
    // matches how Markdown renderers treat nested fences — the last ``` is
    // the one that closes the block the LLM opened).
    let close = content.rfind("```")?;
    let inner = content[..close].trim();
    let parsed = serde_json::from_str::<Value>(inner).ok()?;
    matches!(parsed, Value::Object(_) | Value::Array(_)).then_some(parsed)
}

/// Find and parse the first balanced `{…}` or `[…]` span in `text`. Walks
/// through the text byte by byte tracking brace depth, skipping JSON string
/// literals and their escapes so braces inside values cannot close the span.
pub fn extract_balanced_json(text: &str) -> Option<Value> {
    let text = text.trim();
    let bytes = text.as_bytes();
    let len = bytes.len();
    for start in 0..len {
        let open_byte = bytes[start];
        let (open, close) = match open_byte {
            b'{' => (b'{', b'}'),
            b'[' => (b'[', b']'),
            _ => continue,
        };
        let mut depth = 0u32;
        let mut in_string = false;
        let mut escaped = false;
        for end in start..len {
            let b = bytes[end];
            if in_string {
                if escaped {
                    escaped = false;
                } else if b == b'\\' {
                    escaped = true;
                } else if b == b'"' {
                    in_string = false;
                }
                continue;
            }
            if b == b'"' {
                in_string = true;
                continue;
            }
            if b == open {
                depth = depth.checked_add(1)?;
            } else if b == close {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    // Found a balanced span — try to parse it.
                    let candidate = &text[start..=end];
                    if let Ok(parsed) = serde_json::from_str::<Value>(candidate)
                        && matches!(parsed, Value::Object(_) | Value::Array(_))
                    {
                        return Some(parsed);
                    }
                    // Span didn't parse; continue scanning from the position
                    // after this false-positive open byte.
                    break;
                }
            }
        }
    }
    None
}

/// Apply the shared ordered extraction chain for structured model output.
pub fn extract_structured_json(text: &str) -> Option<Value> {
    parse_llm_json(text)
        .or_else(|| extract_fenced_json_block(text))
        .or_else(|| extract_balanced_json(text))
}
