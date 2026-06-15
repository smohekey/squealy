//! Canonicalize a crate-rendered partial-index predicate into the form PostgreSQL's `pg_get_expr`
//! deparse produces, so a desired model does not churn against a live schema (git-bug `3a5766f`).
//!
//! The desired predicate always originates from squealy's `render_ddl_predicate`, so its shape is
//! known: identifiers are double-quoted, boolean literals are `TRUE`/`FALSE`, and the structure is
//! fully parenthesized. PostgreSQL's deparse agrees on operators, `IS NULL`/`IS NOT NULL`, and
//! parenthesization, and differs only in two surface details:
//!
//!   * it leaves an identifier unquoted when it is a "safe" lowercase identifier that PostgreSQL's
//!     `quote_identifier` would not quote (not a keyword, or only an `unreserved` keyword), whereas
//!     squealy always quotes; and
//!   * it lowercases boolean literals (`true` / `false`).
//!
//! This transform applies exactly those two changes and copies everything else verbatim, so a
//! literal-free predicate (`("deleted_at" IS NULL)` → `(deleted_at IS NULL)`), a column-to-column
//! comparison, a boolean comparison, and a small integer comparison all round-trip cleanly.
//!
//! It deliberately does **not** reproduce the structural normalizations PostgreSQL applies when it
//! understands the expression, so two kinds of predicate can still churn (both tracked separately,
//! for a semantic / round-trip-to-pg approach):
//!
//!   * value-literal type casts synthesized during parse (`'x'::text`, `(1.5)::double precision`,
//!     `'5000000000'::bigint`), which depend on the literal value and column type; and
//!   * flattening of associative chains of three or more operands — PostgreSQL deparses
//!     `((a AND b) AND c)` as `(a AND b AND c)`, while the renderer nests left-associatively.
//!
//! Single comparisons / null checks and two-operand `AND`/`OR` chains match structurally, so the
//! common soft-delete predicate (`("deleted_at" IS NULL)`) and its small combinations are covered.

/// Keywords PostgreSQL's `quote_identifier` quotes when used as an identifier: every keyword whose
/// category is not `unreserved` (`U`) — i.e. reserved (`R`), type/function-name (`T`), and col_name
/// (`C`). An identifier equal to one of these is quoted in deparse output even when it is otherwise
/// lowercase-safe (e.g. `between`, a col_name keyword, deparses as `"between"`), so it must stay
/// quoted here. Sourced from `SELECT word FROM pg_get_keywords() WHERE catcode <> 'U'` on
/// PostgreSQL 17. Kept sorted for binary search (asserted by a unit test).
static QUOTED_KEYWORDS: &[&str] = &[
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
    "between",
    "bigint",
    "binary",
    "bit",
    "boolean",
    "both",
    "case",
    "cast",
    "char",
    "character",
    "check",
    "coalesce",
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
    "dec",
    "decimal",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "exists",
    "extract",
    "false",
    "fetch",
    "float",
    "for",
    "foreign",
    "freeze",
    "from",
    "full",
    "grant",
    "greatest",
    "group",
    "grouping",
    "having",
    "ilike",
    "in",
    "initially",
    "inner",
    "inout",
    "int",
    "integer",
    "intersect",
    "interval",
    "into",
    "is",
    "isnull",
    "join",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "lateral",
    "leading",
    "least",
    "left",
    "like",
    "limit",
    "localtime",
    "localtimestamp",
    "merge_action",
    "national",
    "natural",
    "nchar",
    "none",
    "normalize",
    "not",
    "notnull",
    "null",
    "nullif",
    "numeric",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "out",
    "outer",
    "overlaps",
    "overlay",
    "placing",
    "position",
    "precision",
    "primary",
    "real",
    "references",
    "returning",
    "right",
    "row",
    "select",
    "session_user",
    "setof",
    "similar",
    "smallint",
    "some",
    "substring",
    "symmetric",
    "system_user",
    "table",
    "tablesample",
    "then",
    "time",
    "timestamp",
    "to",
    "trailing",
    "treat",
    "trim",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "values",
    "varchar",
    "variadic",
    "verbose",
    "when",
    "where",
    "window",
    "with",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// True when PostgreSQL would deparse `ident` without quotes: a non-empty identifier matching
/// `[a-z_][a-z0-9_]*` that is not a keyword PostgreSQL quotes (see [`QUOTED_KEYWORDS`]).
/// `quote_identifier` treats only `[a-z0-9_]` as safe, so an identifier containing `$`, uppercase,
/// or non-ASCII characters is quoted in deparse output (verified on pg 17, e.g. `foo$bar` deparses
/// as `"foo$bar"`) and is therefore kept quoted here.
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
    QUOTED_KEYWORDS.binary_search(&ident).is_err()
}

/// If `bytes[start]` (a `$`) begins a dollar-quoted string `$tag$...$tag$` (the tag follows the
/// unquoted-identifier rules and may be empty), returns the index just past the closing tag;
/// otherwise `None`. Such literals only appear in package- or programmatically-authored predicates
/// (the renderer never emits them) and are copied verbatim so their bodies are never read as
/// identifiers.
fn dollar_quoted_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut tag_end = start + 1;
    if matches!(bytes.get(tag_end), Some(c) if c.is_ascii_alphabetic() || *c == b'_') {
        tag_end += 1;
        while matches!(bytes.get(tag_end), Some(c) if c.is_ascii_alphanumeric() || *c == b'_') {
            tag_end += 1;
        }
    }
    if bytes.get(tag_end) != Some(&b'$') {
        return None;
    }
    let tag = &bytes[start..=tag_end];
    let mut i = tag_end + 1;
    while i + tag.len() <= bytes.len() {
        if &bytes[i..i + tag.len()] == tag {
            return Some(i + tag.len());
        }
        i += 1;
    }
    None
}

/// Returns the index just past the closing `'` of the string literal whose opening quote is at
/// `quote`. A doubled `''` is an embedded quote in both modes; `escape_aware` (an `E'...'` escape
/// string) additionally treats `\x` as a two-character escape, so `\'` does not end the string.
fn quoted_string_end(bytes: &[u8], quote: usize, escape_aware: bool) -> usize {
    let mut i = quote + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' if escape_aware => i = (i + 2).min(bytes.len()),
            b'\'' if bytes.get(i + 1) == Some(&b'\'') => i += 2,
            b'\'' => return i + 1,
            _ => i += 1,
        }
    }
    i
}

/// Rewrites a squealy-rendered partial-index predicate into PostgreSQL's `pg_get_expr` form. See the
/// module docs for the scope and known limitations. String literals — standard `'...'`, escape
/// `E'...'`, and dollar-quoted `$tag$...$tag$` — are copied verbatim, so a body that happens to
/// contain a `"…"` span or a boolean-looking word is never rewritten.
pub(crate) fn canonical_index_predicate(predicate: &str) -> String {
    let bytes = predicate.as_bytes();
    let mut out = String::with_capacity(predicate.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            // Standard string literal: verbatim through the closing quote (`''` escape).
            b'\'' => {
                let end = quoted_string_end(bytes, i, false);
                out.push_str(&predicate[i..end]);
                i = end;
            }
            // Escape string `E'...'` / `e'...'`: verbatim (also honors `\` escapes). A bare `E`/`e`
            // not introducing a string falls through to the keyword/identifier handling below.
            b'E' | b'e' if bytes.get(i + 1) == Some(&b'\'') => {
                let end = quoted_string_end(bytes, i + 1, true);
                out.push_str(&predicate[i..end]);
                i = end;
            }
            // Dollar-quoted string literal: verbatim, or a lone `$` if it does not open one.
            b'$' => {
                if let Some(end) = dollar_quoted_end(bytes, i) {
                    out.push_str(&predicate[i..end]);
                    i = end;
                } else {
                    out.push('$');
                    i += 1;
                }
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
    fn quoted_keywords_are_sorted_for_binary_search() {
        assert!(QUOTED_KEYWORDS.is_sorted());
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
    fn keeps_quoted_keywords_and_mixed_case_identifiers_quoted() {
        // `order` is a reserved keyword and `between` a col_name keyword: PostgreSQL quotes both
        // categories in deparse output. `MixedCol` is not lowercase-safe.
        assert_eq!(
            canonical_index_predicate("(\"order\" IS NULL)"),
            "(\"order\" IS NULL)"
        );
        assert_eq!(
            canonical_index_predicate("(\"between\" IS NULL)"),
            "(\"between\" IS NULL)"
        );
        assert_eq!(
            canonical_index_predicate("(\"MixedCol\" IS NULL)"),
            "(\"MixedCol\" IS NULL)"
        );
    }

    #[test]
    fn unquotes_unreserved_keyword_identifiers() {
        // `name` and `value` are unreserved keywords, which PostgreSQL deparses unquoted.
        assert_eq!(
            canonical_index_predicate("(\"name\" IS NULL)"),
            "(name IS NULL)"
        );
        assert_eq!(
            canonical_index_predicate("(\"value\" IS NULL)"),
            "(value IS NULL)"
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
    fn keeps_dollar_and_non_safe_char_identifiers_quoted() {
        // PostgreSQL's `quote_identifier` only treats `[a-z0-9_]` as safe, so `$` stays quoted —
        // matching pg's deparse `("foo$bar" IS NULL)`. Unquoting it would itself cause churn.
        assert_eq!(
            canonical_index_predicate("(\"foo$bar\" IS NULL)"),
            "(\"foo$bar\" IS NULL)"
        );
    }

    #[test]
    fn does_not_rewrite_inside_dollar_quoted_strings() {
        // A `"…"` span inside a dollar-quoted string is part of the string value, not an identifier;
        // unquoting it would change which rows the index covers.
        assert_eq!(
            canonical_index_predicate("(\"label\" = $$\"active\"$$)"),
            "(label = $$\"active\"$$)"
        );
        // Tagged dollar-quote with an inner `TRUE` word and a nested `$$`.
        assert_eq!(
            canonical_index_predicate("(\"label\" = $tag$ TRUE \"x\" $tag$)"),
            "(label = $tag$ TRUE \"x\" $tag$)"
        );
    }

    #[test]
    fn does_not_rewrite_inside_escape_strings() {
        // `E'...'` honors backslash escapes, so `\'` does not end the string; the `"z"` span and the
        // `FALSE` word inside must be copied verbatim.
        assert_eq!(
            canonical_index_predicate("(\"a\" = E'x\\'\"z\" FALSE')"),
            "(a = E'x\\'\"z\" FALSE')"
        );
        assert_eq!(
            canonical_index_predicate("(\"a\" = E'it''s')"),
            "(a = E'it''s')"
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
