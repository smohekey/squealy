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
/// `[a-z_][a-z0-9_]*` that is not a keyword PostgreSQL quotes (see [`QUOTED_KEYWORDS`]). Anything
/// with uppercase or special characters stays quoted too.
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
