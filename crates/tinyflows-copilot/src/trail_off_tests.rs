use super::*;

#[test]
fn text_looks_like_question_detects_trailing_question_mark() {
    assert!(text_looks_like_question(
        "Which Slack channel should I post to?"
    ));
    assert!(text_looks_like_question("Which channel?\n"));
    // Trailing markdown/punctuation noise after the '?' shouldn't defeat it.
    assert!(text_looks_like_question("Which channel should I use?\""));
    // A trailing blank line after the question is still detected (the last
    // NON-BLANK line is what's checked).
    assert!(text_looks_like_question(
        "Which channel should I post to?\n\n"
    ));
}

/// Regression (#4887 follow-up): a question immediately followed by a
/// trailing pleasantry/instruction in the SAME paragraph ("...to? Let me
/// know!") used to be an accepted false negative. That false negative let the
/// trail-off backstop clobber real, specific questions with a generic
/// fallback — this is now DETECTED via the final-paragraph scan in
/// `text_looks_like_question`.
///
/// Note: a question mark separated from the trailing sentence by a full
/// blank-line paragraph break (`"...to?\n\nLet me know!"`) is a DIFFERENT
/// shape — the `?` there sits in an earlier paragraph, not the last one — and
/// remains an intentional false negative: the final-paragraph scan only
/// looks at the LAST non-blank paragraph, by design (see the function doc
/// and `text_looks_like_question_ignores_question_mark_in_earlier_paragraph`
/// below, which pins that scope decision).
#[test]
fn text_looks_like_question_detects_same_paragraph_trailing_pleasantry() {
    assert!(text_looks_like_question(
        "Which channel should I post to? Let me know!"
    ));
}

/// Pins the intentional cross-paragraph false negative documented above: a
/// `?` that sits in an EARLIER paragraph than the last one is deliberately
/// NOT detected — the final-paragraph scan only looks at the last non-blank
/// paragraph, by design. This is harmless because the trail-off backstop's
/// fallback is non-destructive (PREPEND, not REPLACE): even when this false
/// negative fires, the model's original question is preserved below the
/// fallback rather than discarded.
#[test]
fn text_looks_like_question_ignores_question_mark_in_earlier_paragraph() {
    assert!(!text_looks_like_question(
        "Which channel should I post to?\n\nLet me know!"
    ));
}

/// The exact shape a live tester hit (#4887 regression): a clear, specific
/// question mid-sentence, immediately followed by a trailing instructional
/// sentence on the SAME paragraph/line. The old last-line-only check missed
/// this entirely; the final-paragraph scan must catch it.
#[test]
fn text_looks_like_question_detects_mid_sentence_question_with_trailing_instruction() {
    assert!(text_looks_like_question(
        "Alan — what's your **Slack user ID** (the `U...` code) so I can DM you the daily \
         update? You can find it in Slack under Profile > Copy member ID."
    ));
}

/// A `?` that only appears inside inline code or a fenced code block must
/// NOT be treated as a question — the guard on `question_mark_outside_code`
/// has to hold, or a code sample like `WHERE id = ?` would false-positive.
#[test]
fn text_looks_like_question_ignores_question_mark_inside_code() {
    assert!(!text_looks_like_question(
        "Run the query below to check the row.\n\n`SELECT * FROM t WHERE id = ?`"
    ));
    assert!(!text_looks_like_question(
        "Here's the query:\n\n```sql\nSELECT * FROM t WHERE id = ?\n```"
    ));
    assert!(!text_looks_like_question(
        "Here's the query:\n\n```sql\n\nSELECT * FROM t WHERE id = ?\n```"
    ));
    assert!(!text_looks_like_question(
        "```sql\nSELECT * FROM t WHERE id = ?"
    ));
}

/// Codex review follow-up: a `?` mid-token that isn't a real question mark —
/// e.g. a URL query string in a status update — must NOT flip
/// `text_looks_like_question` to `true`. Counting it would make `flows_build`
/// skip `combine_trail_off_fallback` entirely, leaving the user with an
/// unanswerable status note and no guaranteed question — exactly the failure
/// mode this backstop exists to prevent.
#[test]
fn text_looks_like_question_ignores_question_mark_in_url_query_string() {
    assert!(!text_looks_like_question(
        "Checked https://api.example/search?q=foo and got 403."
    ));
    assert!(!text_looks_like_question(
        "Ran the search with filter?status=open but the API rejected it."
    ));
}

/// CodeRabbit review follow-up: paragraph boundaries must be recognized for
/// CRLF line endings and whitespace-only blank lines, not just a literal
/// `"\n\n"` byte sequence — otherwise an earlier question survives into what
/// should be treated as a separate, later, non-question status paragraph,
/// and the fallback gets wrongly suppressed for that trailing paragraph.
#[test]
fn text_looks_like_question_treats_crlf_and_whitespace_lines_as_paragraph_breaks() {
    // CRLF paragraph break: the earlier "?" must not leak into the final
    // paragraph, which is a plain status line with no question of its own.
    assert!(!text_looks_like_question(
        "Which channel should I post to?\r\n\r\nPosted the update just now."
    ));
    // Whitespace-only blank line (not perfectly empty) must also count as a
    // paragraph break.
    assert!(!text_looks_like_question(
        "Which channel should I post to?\n   \nPosted the update just now."
    ));
}

/// CodeRabbit review follow-up: a multi-backtick Markdown code span (e.g.
/// double backtick, used so the span can itself contain a literal single
/// backtick) must still be recognized as code — a naive backtick-count
/// parity check misclassifies it because two backticks flip parity back to
/// "even" immediately. The span must only close on a run of the SAME length
/// that opened it.
#[test]
fn text_looks_like_question_ignores_question_mark_inside_double_backtick_span() {
    assert!(!text_looks_like_question(
        "Run the query below to check the row.\n\n``SELECT * FROM t WHERE id = ?``"
    ));
    // A single backtick embedded inside a double-backtick span (the classic
    // reason to use a longer delimiter) must not be mistaken for the span's
    // closing delimiter.
    assert!(!text_looks_like_question(
        "Use ``SELECT `id` FROM t WHERE id = ?`` before retrying."
    ));
}

#[test]
fn text_looks_like_question_rejects_status_dumps_and_silence() {
    assert!(!text_looks_like_question(
        "## Done so far\n- Checked connections\n- Verified contracts"
    ));
    assert!(!text_looks_like_question(""));
    assert!(!text_looks_like_question("   "));
    assert!(!text_looks_like_question("I'll continue working on this."));
}
