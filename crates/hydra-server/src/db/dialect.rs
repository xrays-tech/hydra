//! SQL dialect helpers.
//!
//! The repo layer (Phase 2b) writes one portable SQL string per statement using
//! `?` placeholders and routes it through [`sql`]. PostgreSQL needs numbered
//! placeholders (`$1`, `$2`, …); MySQL accepts `?` unchanged.
//!
//! The rewriter is literal-aware: a `?` inside a string literal, quoted
//! identifier, or comment is data, not a bind marker, and must not be
//! renumbered. This is the property the Phase 2b statement audit relies on — a
//! missed literal silently shifts every later parameter.
//!
//! Only placeholder rewriting lives here. Timestamp/upsert fragments and
//! retryable-error classification are Phase 2b/2c concerns; the audit decided
//! select-under-lock + insert/update instead of backend-specific upsert syntax,
//! so no upsert helper is needed.

use std::borrow::Cow;

/// Rewrite portable `?` placeholders for the selected backend.
///
/// * PostgreSQL: each bare `?` becomes `$1`, `$2`, … in order.
/// * MySQL: input is returned unchanged.
///
/// A `?` inside `'…'`, `"…"`, `` `…` ``, a `--` line comment, or a `/* … */`
/// block comment is preserved verbatim.
#[cfg(feature = "db-postgres")]
pub fn sql(input: &str) -> Cow<'_, str> {
    rewrite_postgres(input)
}

/// Rewrite portable `?` placeholders for the selected backend.
///
/// MySQL accepts `?` natively, so this is a zero-copy pass-through.
#[cfg(not(feature = "db-postgres"))]
pub fn sql(input: &str) -> Cow<'_, str> {
    Cow::Borrowed(input)
}

#[cfg(any(feature = "db-postgres", test))]
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Normal,
    SingleQuote,
    DoubleQuote,
    Backtick,
    LineComment,
    BlockComment,
}

/// PostgreSQL `?` → `$n` rewriter, literal/comment aware.
///
/// Exposed to unit tests in every feature mode; production only calls it when
/// `db-postgres` is enabled.
#[cfg(any(feature = "db-postgres", test))]
fn rewrite_postgres(input: &str) -> Cow<'_, str> {
    let chars: Vec<(usize, char)> = input.char_indices().collect();
    let mut out = String::with_capacity(input.len());
    let mut state = State::Normal;
    let mut next_param = 0usize;
    let mut changed = false;
    let mut i = 0usize;

    while i < chars.len() {
        let c = chars[i].1;
        match state {
            State::Normal => match c {
                '\'' => {
                    out.push(c);
                    state = State::SingleQuote;
                    i += 1;
                }
                '"' => {
                    out.push(c);
                    state = State::DoubleQuote;
                    i += 1;
                }
                '`' => {
                    out.push(c);
                    state = State::Backtick;
                    i += 1;
                }
                '-' if matches!(chars.get(i + 1).map(|(_, c)| c), Some('-')) => {
                    out.push('-');
                    out.push('-');
                    state = State::LineComment;
                    i += 2;
                }
                '/' if matches!(chars.get(i + 1).map(|(_, c)| c), Some('*')) => {
                    out.push('/');
                    out.push('*');
                    state = State::BlockComment;
                    i += 2;
                }
                '?' => {
                    next_param += 1;
                    out.push('$');
                    out.push_str(&next_param.to_string());
                    changed = true;
                    i += 1;
                }
                _ => {
                    out.push(c);
                    i += 1;
                }
            },
            State::SingleQuote => {
                out.push(c);
                match c {
                    '\'' if matches!(chars.get(i + 1).map(|(_, c)| c), Some('\'')) => {
                        // Doubled quote escape: stay in the literal.
                        out.push('\'');
                        i += 2;
                    }
                    '\'' => {
                        state = State::Normal;
                        i += 1;
                    }
                    '\\' if i + 1 < chars.len() => {
                        // Backslash escape (conservative: MySQL/legacy literals).
                        out.push(chars[i + 1].1);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            State::DoubleQuote => {
                out.push(c);
                match c {
                    '"' if matches!(chars.get(i + 1).map(|(_, c)| c), Some('"')) => {
                        out.push('"');
                        i += 2;
                    }
                    '"' => {
                        state = State::Normal;
                        i += 1;
                    }
                    '\\' if i + 1 < chars.len() => {
                        out.push(chars[i + 1].1);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            State::Backtick => {
                out.push(c);
                match c {
                    '`' if matches!(chars.get(i + 1).map(|(_, c)| c), Some('`')) => {
                        out.push('`');
                        i += 2;
                    }
                    '`' => {
                        state = State::Normal;
                        i += 1;
                    }
                    '\\' if i + 1 < chars.len() => {
                        out.push(chars[i + 1].1);
                        i += 2;
                    }
                    _ => i += 1,
                }
            }
            State::LineComment => {
                out.push(c);
                if c == '\n' {
                    state = State::Normal;
                }
                i += 1;
            }
            State::BlockComment => {
                out.push(c);
                if c == '*' && matches!(chars.get(i + 1).map(|(_, c)| c), Some('/')) {
                    out.push('/');
                    state = State::Normal;
                    i += 2;
                } else {
                    i += 1;
                }
            }
        }
    }

    if changed {
        Cow::Owned(out)
    } else {
        Cow::Borrowed(input)
    }
}

#[cfg(test)]
mod tests {
    use super::rewrite_postgres;

    #[test]
    fn no_placeholders_is_borrowed() {
        let input = "SELECT id FROM provider";
        let out = rewrite_postgres(input);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, input);
    }

    #[test]
    fn multiple_placeholders_are_numbered_in_order() {
        assert_eq!(
            rewrite_postgres("INSERT INTO t (a, b, c) VALUES (?, ?, ?)"),
            "INSERT INTO t (a, b, c) VALUES ($1, $2, $3)"
        );
    }

    #[test]
    fn single_quoted_question_mark_is_untouched() {
        assert_eq!(
            rewrite_postgres("SELECT '?' AS literal, ? AS param"),
            "SELECT '?' AS literal, $1 AS param"
        );
    }

    #[test]
    fn doubled_single_quote_escape_is_handled() {
        assert_eq!(
            rewrite_postgres("SELECT 'it''s ?' AS s, ?"),
            "SELECT 'it''s ?' AS s, $1"
        );
    }

    #[test]
    fn backslash_escaped_quote_is_handled() {
        assert_eq!(
            rewrite_postgres("SELECT 'it\\'s ?' AS s, ?"),
            "SELECT 'it\\'s ?' AS s, $1"
        );
    }

    #[test]
    fn double_quoted_question_mark_is_untouched() {
        assert_eq!(
            rewrite_postgres("SELECT \"?\" FROM t WHERE x = ?"),
            "SELECT \"?\" FROM t WHERE x = $1"
        );
    }

    #[test]
    fn backtick_identifier_question_mark_is_untouched() {
        assert_eq!(
            rewrite_postgres("SELECT `?` FROM t WHERE x = ?"),
            "SELECT `?` FROM t WHERE x = $1"
        );
    }

    #[test]
    fn line_comment_question_mark_is_untouched() {
        assert_eq!(
            rewrite_postgres("SELECT ? -- why is ? here\nFROM t"),
            "SELECT $1 -- why is ? here\nFROM t"
        );
    }

    #[test]
    fn block_comment_question_mark_is_untouched() {
        assert_eq!(
            rewrite_postgres("SELECT ? /* ? is data */ FROM t WHERE x = ?"),
            "SELECT $1 /* ? is data */ FROM t WHERE x = $2"
        );
    }

    #[test]
    fn mixed_literals_comments_and_placeholders() {
        let input = "\
            UPDATE t SET a = ?, b = 'literal with ? and '' quote', \
            c = \"quoted ?\" /* ? block */ \
            -- trailing ? comment\n\
            WHERE id = ? AND d = ?";
        let expected = "\
            UPDATE t SET a = $1, b = 'literal with ? and '' quote', \
            c = \"quoted ?\" /* ? block */ \
            -- trailing ? comment\n\
            WHERE id = $2 AND d = $3";
        assert_eq!(rewrite_postgres(input), expected);
    }

    #[test]
    fn representative_multiline_query() {
        let input = "\
            INSERT INTO user_api_key\n\
                (id, user_id, name, key_hash, enabled, expires_at)\n\
            VALUES (?, ?, ?, ?, ?, ?)";
        let expected = "\
            INSERT INTO user_api_key\n\
                (id, user_id, name, key_hash, enabled, expires_at)\n\
            VALUES ($1, $2, $3, $4, $5, $6)";
        assert_eq!(rewrite_postgres(input), expected);
    }

    #[cfg(feature = "db-postgres")]
    #[test]
    fn dispatch_rewrites_for_postgres() {
        assert_eq!(super::sql("SELECT ?"), "SELECT $1");
    }

    #[cfg(not(feature = "db-postgres"))]
    #[test]
    fn dispatch_passes_through_for_mysql() {
        let input = "SELECT ?, '?'";
        let out = super::sql(input);
        assert!(matches!(out, std::borrow::Cow::Borrowed(_)));
        assert_eq!(out, input);
    }
}
