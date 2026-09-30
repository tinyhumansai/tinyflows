//! Heuristics for the "did the authoring turn end on a question?" backstop.
//!
//! A builder turn that stops with no proposal, no error and no question leaves
//! the author with silence. A host that wants a guaranteed terminal state asks
//! [`text_looks_like_question`] whether the model's final text already asks
//! something answerable, and only synthesizes a fallback question when it does
//! not. The fallback itself is the host's: it depends on the host's transcript
//! and tool vocabulary, which this crate does not name.

/// Heuristic: does `text` already contain a clear, answerable question in its
/// final paragraph? Conservative by design (issue: builder convergence) — a
/// false negative (an actual question this misses) no longer discards the
/// model's text (a host that keeps it prepends its fallback instead), so the safe failure mode
/// stays "add a guaranteed question on top", never "under-detect and stay
/// silent".
///
/// Regression (#4887 follow-up): the original version only checked for a `?`
/// at the very end of the text / last line, which false-negatived on the
/// extremely common LLM pattern "What's X? You can find it at Y." — a real
/// question immediately followed by a trailing instructional sentence. The
/// backstop then clobbered a specific, answerable question with a generic
/// fallback. To catch that shape, this now also scans the LAST non-empty
/// paragraph for a `?` that isn't inside inline code or a fenced code block
/// (so a literal `?` in a code sample, e.g. `WHERE id = ?`, doesn't count).
///
/// Note: the trailing-noise strip below deliberately does NOT include the
/// backtick. Stripping a trailing backtick would peel off the CLOSING
/// delimiter of a code span whose last character is `?` (e.g. `` `id = ?` ``
/// at the very end of the text), exposing that `?` as if it were a bare
/// trailing question mark and defeating the code guard entirely.
#[must_use]
pub fn text_looks_like_question(text: &str) -> bool {
    let trimmed = text
        .trim()
        .trim_end_matches(['"', '\'', ')', ']', '*', '_', '.'])
        .trim_end();
    if trimmed.is_empty() {
        return false;
    }
    // Final-paragraph scan: a question can sit mid-paragraph, followed by a
    // further trailing sentence on the SAME line/paragraph ("...ID? You can
    // find it under Profile > Copy member ID."). Take the last non-blank
    // paragraph and accept it if it contains a `?` that isn't inside inline
    // code / a code fence.
    let Some(paragraph) = last_paragraph(trimmed) else {
        return false;
    };
    let paragraph_start = trimmed.len() - paragraph.len();
    let prefix = &trimmed[..paragraph_start];
    question_mark_outside_code_with_state(paragraph, code_span_state(prefix))
}

/// Returns the last non-blank paragraph of `text` — a maximal run of
/// consecutive non-blank lines, working backward from the end and skipping
/// any trailing blank lines first. `None` if `text` has no non-blank lines.
///
/// CodeRabbit review follow-up: this used to split on the literal `"\n\n"`
/// byte sequence, which mishandles two real shapes:
/// - **CRLF input** (`"question?\r\n\r\nstatus"`): the separator is
///   `"\r\n\r\n"`, not `"\n\n"`, so the whole text was treated as ONE
///   paragraph — an earlier question could then suppress the fallback for a
///   trailing non-question status paragraph.
/// - **Whitespace-only separator lines** (`"question?\n \nstatus"` — a blank
///   line that isn't perfectly empty): same failure, same reason.
///
/// Working line-by-line via [`str::lines`] (which normalizes CRLF) and
/// treating any all-whitespace line as blank fixes both.
fn last_paragraph(text: &str) -> Option<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    for line in text.split_inclusive('\n') {
        let content_end = start + line.trim_end_matches('\n').len();
        lines.push((start, content_end, line.trim().is_empty()));
        start += line.len();
    }

    let last_nonblank = lines.iter().rposition(|(_, _, blank)| !blank)?;
    let mut first = last_nonblank;
    while first > 0 && !lines[first - 1].2 {
        first -= 1;
    }
    Some(&text[lines[first].0..lines[last_nonblank].1])
}

/// Does `text` contain at least one *sentence-terminal* `?` that isn't
/// inside a backtick-delimited code span (inline code like `` `U...` `` or a
/// fenced block like `` ``` ``)? Follows the CommonMark code-span rule: a
/// *run* of one or more consecutive backticks opens a span, and that span is
/// closed only by the next run of the SAME length — a shorter or longer run
/// of backticks encountered while inside a span is just literal backtick
/// characters, not a delimiter.
///
/// CodeRabbit review follow-up: an earlier version tracked a running
/// per-character backtick COUNT and used its parity (even = outside code).
/// That misclassifies any multi-backtick span whose delimiter is more than
/// one backtick — e.g. ``` ``SELECT ? FROM t`` ``` opens with a 2-backtick
/// run (count 0→2, even → looks "outside" again immediately), so the `?`
/// inside a valid double-backtick span was wrongly treated as outside code.
/// Tracking delimiter run LENGTH (not raw backtick count) fixes this while
/// still handling the common single-backtick and triple-backtick-fence
/// cases, since those are just the run-length-1 and run-length-3 instances
/// of the same rule.
///
/// Codex review follow-up: a bare `?` outside code isn't necessarily a real
/// question — a status line like "Checked https://api.example/search?q=foo
/// and got 403." has one mid-token, in a URL query string. Counting that
/// would flip `text_looks_like_question` to `true` and skip
/// the host's fallback entirely, leaving the user with an
/// unanswerable status note — exactly the failure mode this backstop exists
/// to prevent. So each candidate `?` is additionally required to be
/// sentence-terminal via [`is_sentence_terminal_question_mark`].
#[must_use]
pub fn question_mark_outside_code(text: &str) -> bool {
    question_mark_outside_code_with_state(text, None)
}

fn code_span_state(text: &str) -> Option<usize> {
    let chars: Vec<char> = text.chars().collect();
    let mut open_run_len = None;
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            let start = i;
            while i < chars.len() && chars[i] == '`' {
                i += 1;
            }
            let run_len = i - start;
            open_run_len = match open_run_len {
                None => Some(run_len),
                Some(n) if n == run_len => None,
                Some(n) => Some(n),
            };
        } else {
            i += 1;
        }
    }
    open_run_len
}

fn question_mark_outside_code_with_state(text: &str, mut open_run_len: Option<usize>) -> bool {
    let chars: Vec<char> = text.chars().collect();
    // `Some(n)` while scanning is inside a code span opened by a run of `n`
    // backticks; that span closes only on the next run of exactly `n`.
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '`' {
            let start = i;
            while i < chars.len() && chars[i] == '`' {
                i += 1;
            }
            let run_len = i - start;
            open_run_len = match open_run_len {
                None => Some(run_len),
                Some(n) if n == run_len => None,
                Some(n) => Some(n), // mismatched run length: still inside the span
            };
            continue;
        }
        if chars[i] == '?'
            && open_run_len.is_none()
            && is_sentence_terminal_question_mark(&chars, i)
        {
            return true;
        }
        i += 1;
    }
    false
}

/// Is the `?` at `chars[index]` sentence-terminal — i.e. does it read as an
/// actual question mark rather than a character that merely happens to be a
/// `?` mid-token (a URL query string like `search?q=foo`, a shell glob,
/// etc.)? Skips over any immediately-following closing quote/bracket
/// punctuation (`"`, `'`, right single/double quotes, `)`, `]`) and requires
/// what remains to be whitespace or the end of the text — the shape a `?`
/// takes at the end of a real sentence or clause.
fn is_sentence_terminal_question_mark(chars: &[char], index: usize) -> bool {
    let mut i = index + 1;
    while let Some(&c) = chars.get(i) {
        if matches!(c, '"' | '\'' | '\u{2019}' | '\u{201D}' | ')' | ']') {
            i += 1;
            continue;
        }
        return c.is_whitespace();
    }
    true // '?' was the last character in the paragraph.
}

#[cfg(test)]
#[path = "trail_off_tests.rs"]
mod tests;
