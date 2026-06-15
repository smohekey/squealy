//! Canonicalize a crate-rendered partial-index predicate into the form PostgreSQL's `pg_get_expr`
//! deparse produces, so a desired model does not churn against a live schema (git-bug `3a5766f`).
//!
//! The desired predicate always originates from squealy's `render_ddl_predicate`, so its shape is
//! known: identifiers are double-quoted, boolean literals are `TRUE`/`FALSE`, and the structure is
//! fully parenthesized. PostgreSQL's deparse agrees on operators, `IS NULL`/`IS NOT NULL`, and
//! parenthesization, and differs only in two surface details:
//!
//!   * it leaves an identifier unquoted when it is a "safe" lowercase identifier that is not a
//!     reserved (or type/function-name) keyword, whereas squealy always quotes; and
//!   * it lowercases boolean literals (`true` / `false`).
//!
//! This transform applies exactly those two changes and copies everything else verbatim, so a
//! literal-free predicate (`("deleted_at" IS NULL)` → `(deleted_at IS NULL)`), a column-to-column
//! comparison, a boolean comparison, and a small integer comparison all round-trip cleanly.
//!
//! It deliberately does **not** synthesize the value-literal type casts PostgreSQL adds during
//! parse (`'x'::text`, `(1.5)::double precision`, `'5000000000'::bigint`) — those depend on the
//! literal value and the column type, which an offline string transform cannot reproduce — so a
//! predicate comparing a column to a value literal can still churn. That is tracked separately.

/// Reserved (`R`) and type/function-name (`T`) keywords. PostgreSQL's `quote_identifier` quotes an
/// identifier matching one of these even when it is otherwise lowercase-safe; `col_name` (`C`) and
/// `unreserved` (`U`) keywords are deparsed unquoted, so they are not listed. Sourced from
/// `SELECT word FROM pg_get_keywords() WHERE catcode IN ('R','T')` on PostgreSQL 17. Kept sorted for
/// binary search (asserted by a unit test).
static RESERVED_KEYWORDS: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "authorization",
    "binary",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "collation",
    "column",
    "concurrently",
    "constraint",
    "create",
    "cross",
    "current_catalog",
    "current_date",
    "current_role",
    "current_schema",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "group",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "intersect",
    "into",
    "is",
    "isnull",
    "join",
    "lateral",
    "leading",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "natural",
    "not",
    "notnull",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "outer",
    "overlaps",
    "placing",
    "primary",
    "references",
    "returning",
    "right",
    "select",
    "session_user",
    "similar",
    "some",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
];

/// True when PostgreSQL would deparse `ident` without quotes: a non-empty identifier matching
/// `[a-z_][a-z0-9_]*` that is not a reserved/type-function keyword. Anything with uppercase or
/// special characters, or any reserved keyword, stays quoted.
fn deparses_unquoted(ident: &str) -> bool {
    let mut chars = ident.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_lowercase() || first == '_') {
        return false;
    }
    if !chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
        return false;
    }
    RESERVED_KEYWORDS.binary_search(&ident).is_err()
}

/// Rewrites a squealy-rendered partial-index predicate into PostgreSQL's `pg_get_expr` form. See the
/// module docs for the scope and the known value-literal-cast limitation.
pub(crate) fn canonical_index_predicate(predicate: &str) -> String {
    let bytes = predicate.as_bytes();
    let mut out = String::with_capacity(predicate.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // String literal: copy verbatim through the closing quote, honoring `''` escapes.
            b'\'' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'\'' {
                        if bytes.get(i + 1) == Some(&b'\'') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                out.push_str(&predicate[start..i]);
            }
            // Quoted identifier: unquote when PostgreSQL would, else re-emit the span verbatim.
            b'"' => {
                let start = i;
                i += 1;
                while i < bytes.len() {
                    if bytes[i] == b'"' {
                        if bytes.get(i + 1) == Some(&b'"') {
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    i += 1;
                }
                let raw = &predicate[start..i];
                let content = raw[1..raw.len() - 1].replace("\"\"", "\"");
                if deparses_unquoted(&content) {
                    out.push_str(&content);
                } else {
                    out.push_str(raw);
                }
            }
            // A bare word: a keyword (IS, NULL, NOT, AND, OR, ...) or a boolean literal. Only the
            // boolean literals are rewritten (lowercased); keywords pass through unchanged.
            b if b.is_ascii_alphabetic() => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                match &predicate[start..i] {
                    "TRUE" => out.push_str("true"),
                    "FALSE" => out.push_str("false"),
                    word => out.push_str(word),
                }
            }
            // Operators, parentheses, whitespace, digits: copy one UTF-8 character verbatim.
            _ => {
                let ch = predicate[i..].chars().next().expect("valid char boundary");
                let len = ch.len_utf8();
                out.push_str(&predicate[i..i + len]);
                i += len;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserved_keywords_are_sorted_for_binary_search() {
        assert!(RESERVED_KEYWORDS.is_sorted());
    }

    #[test]
    fn unquotes_safe_lowercase_identifiers() {
        assert_eq!(
            canonical_index_predicate("(\"deleted_at\" IS NULL)"),
            "(deleted_at IS NULL)"
        );
        assert_eq!(
            canonical_index_predicate("(\"status\" = \"other\")"),
            "(status = other)"
        );
    }

    #[test]
    fn keeps_reserved_and_mixed_case_identifiers_quoted() {
        // `order` is a reserved keyword; `MixedCol` is not lowercase-safe.
        assert_eq!(
            canonical_index_predicate("(\"order\" IS NULL)"),
            "(\"order\" IS NULL)"
        );
        assert_eq!(
            canonical_index_predicate("(\"MixedCol\" IS NULL)"),
            "(\"MixedCol\" IS NULL)"
        );
    }

    #[test]
    fn lowercases_boolean_literals() {
        assert_eq!(
            canonical_index_predicate("(\"active\" = TRUE)"),
            "(active = true)"
        );
        assert_eq!(
            canonical_index_predicate("(\"active\" = FALSE)"),
            "(active = false)"
        );
    }

    #[test]
    fn boolean_and_keyword_words_inside_string_literals_are_untouched() {
        // A string literal that happens to contain TRUE / an identifier-looking word must be copied
        // byte-for-byte, including the doubled-quote escape.
        assert_eq!(
            canonical_index_predicate("(\"label\" = 'TRUE order')"),
            "(label = 'TRUE order')"
        );
        assert_eq!(
            canonical_index_predicate("(\"label\" = 'o''brien')"),
            "(label = 'o''brien')"
        );
    }

    #[test]
    fn combines_null_check_and_comparison() {
        assert_eq!(
            canonical_index_predicate("((\"deleted_at\" IS NULL) AND (\"status\" = 1))"),
            "((deleted_at IS NULL) AND (status = 1))"
        );
        assert_eq!(
            canonical_index_predicate("(NOT (\"deleted_at\" IS NULL))"),
            "(NOT (deleted_at IS NULL))"
        );
    }
}
