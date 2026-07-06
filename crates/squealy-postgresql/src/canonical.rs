//! Semantic canonicalization of partial-index predicates and `CHECK` expressions, so a desired model
//! does not churn against a live schema (git-bug `64d9d03`).
//!
//! A crate-rendered or user-authored expression (`("deleted_at" IS NULL)`, `status IN (1, 2, 3)`)
//! and the form PostgreSQL's `pg_get_expr` / `pg_get_constraintdef` deparse back
//! (`(deleted_at IS NULL)`, `(status = ANY (ARRAY[1, 2, 3]))`) describe the *same* expression but
//! differ in surface form: identifier quoting, boolean case, associative nesting
//! (`((a AND b) AND c)` vs `(a AND b AND c)`), synthesized literal type casts (`'x'::text`,
//! `'-5'::integer`), and operator rewrites (`IN` → `= ANY (ARRAY[..])`, `BETWEEN` → `>= AND <=`,
//! `LIKE` → `~~`). Comparing the strings directly therefore reports a never-settling
//! `AlterIndex` / `AlterCheck` after a clean publish.
//!
//! [`canonical_index_predicate`] **parses** a partial-index predicate into a
//! normalized AST and re-serialize it to a single canonical string, collapsing all of those
//! differences to equality when the *same* canonicalization is applied to both the desired and the
//! introspected model before diffing. The canonical string is internal — it need not match pg's
//! deparse, only be identical for equivalent expressions. Anything outside the grammar (subqueries,
//! `CASE`, timestamp literals pg reformats, escape / dollar-quoted strings, ...) fails to parse, and
//! the caller falls back to string equality — never a wrong match.

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

/// Canonicalizes a partial-index predicate to the form PostgreSQL's `pg_get_expr` deparse produces,
/// by parsing it and re-serializing a normalized AST (see [`normalize`]). Falls back to the input
/// unchanged when the grammar does not cover the expression, so the diff degrades to string equality
/// rather than mis-matching.
pub(crate) fn canonical_index_predicate(predicate: &str) -> String {
    normalize(predicate).unwrap_or_else(|| predicate.to_owned())
}

// ===========================================================================================
// A small PostgreSQL boolean-expression parser + normalizer.
//
// `pg_get_expr` / `pg_get_constraintdef` deparse an expression into a canonical surface form that
// differs from the crate-rendered / user-authored form in ways a pure string transform cannot
// reconcile: synthesized literal type casts (`'x'::text`, `'-5'::integer`), associative flattening
// (`((a AND b) AND c)` -> `(a AND b AND c)`), and operator rewrites (`IN (..)` -> `= ANY (ARRAY[..])`,
// `BETWEEN a AND b` -> `(x >= a) AND (x <= b)`, `LIKE` -> `~~`). Parsing both the desired and the
// introspected string into the same normalized AST and re-serializing collapses all of these to
// equality. Anything outside the grammar (subqueries, CASE, timestamp literals pg reformats, escape
// or dollar-quoted strings, ...) makes the parse fail, and the caller falls back to string equality.
// ===========================================================================================

#[derive(Clone, Debug, PartialEq, Eq)]
enum Lit {
    Int(i128),
    /// A non-integer numeric literal, kept as its canonical text (e.g. `-1.5`).
    Num(String),
    Str(String),
    Bool(bool),
    Null,
    /// A literal whose value PostgreSQL reformats in a way we cannot reproduce offline (e.g. a
    /// timestamp). Carried as its already-canonical `'value'::type` text so equal inputs still match.
    Opaque(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Node {
    And(Vec<Node>),
    Or(Vec<Node>),
    Not(Box<Node>),
    IsNull(Box<Node>, bool),       // negated = IS NOT NULL
    IsBool(Box<Node>, bool, bool), // (operand, value, negated) = `x IS [NOT] TRUE|FALSE`
    Compare(&'static str, Box<Node>, Box<Node>),
    In(Box<Node>, Vec<Node>),
    Like(Box<Node>, Box<Node>, bool, bool), // (operand, pattern, case_insensitive, negated)
    Arith(&'static str, Box<Node>, Box<Node>),
    Neg(Box<Node>),
    Cast(Box<Node>, String),
    Func(String, Vec<Node>),
    Col(String),
    Lit(Lit),
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Tok {
    Ident(String), // folded: unquoted lowercased, quoted preserved
    Num(String),   // unsigned numeric text
    Str(String),   // decoded string value
    Sym(&'static str),
    Kw(&'static str),
}

const KEYWORDS: &[&str] = &[
    "all", "and", "any", "array", "between", "false", "ilike", "in", "is", "like", "not", "null",
    "or", "true",
];

fn normalize(input: &str) -> Option<String> {
    let tokens = tokenize(input)?;
    if tokens.is_empty() {
        return None;
    }
    let mut parser = Parser {
        tokens: &tokens,
        pos: 0,
    };
    let node = parser.parse_or()?;
    if parser.pos != tokens.len() {
        return None;
    }
    Some(serialize(&node))
}

fn tokenize(input: &str) -> Option<Vec<Tok>> {
    let bytes = input.as_bytes();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match b {
            _ if b.is_ascii_whitespace() => i += 1,
            b'(' => {
                tokens.push(Tok::Sym("("));
                i += 1;
            }
            b')' => {
                tokens.push(Tok::Sym(")"));
                i += 1;
            }
            b'[' => {
                tokens.push(Tok::Sym("["));
                i += 1;
            }
            b']' => {
                tokens.push(Tok::Sym("]"));
                i += 1;
            }
            b',' => {
                tokens.push(Tok::Sym(","));
                i += 1;
            }
            b'+' => {
                tokens.push(Tok::Sym("+"));
                i += 1;
            }
            b'*' => {
                tokens.push(Tok::Sym("*"));
                i += 1;
            }
            b'/' => {
                tokens.push(Tok::Sym("/"));
                i += 1;
            }
            b'%' => {
                tokens.push(Tok::Sym("%"));
                i += 1;
            }
            b'-' => {
                tokens.push(Tok::Sym("-"));
                i += 1;
            }
            b':' if bytes.get(i + 1) == Some(&b':') => {
                tokens.push(Tok::Sym("::"));
                i += 2;
            }
            b'=' => {
                tokens.push(Tok::Sym("="));
                i += 1;
            }
            b'<' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Tok::Sym("<="));
                    i += 2;
                } else if bytes.get(i + 1) == Some(&b'>') {
                    tokens.push(Tok::Sym("<>"));
                    i += 2;
                } else {
                    tokens.push(Tok::Sym("<"));
                    i += 1;
                }
            }
            b'>' => {
                if bytes.get(i + 1) == Some(&b'=') {
                    tokens.push(Tok::Sym(">="));
                    i += 2;
                } else {
                    tokens.push(Tok::Sym(">"));
                    i += 1;
                }
            }
            b'!' if bytes.get(i + 1) == Some(&b'=') => {
                tokens.push(Tok::Sym("<>"));
                i += 2;
            }
            // `!~~` / `!~~*` are PostgreSQL's deparse of `NOT LIKE` / `NOT ILIKE`.
            b'!' if bytes.get(i + 1) == Some(&b'~') && bytes.get(i + 2) == Some(&b'~') => {
                if bytes.get(i + 3) == Some(&b'*') {
                    tokens.push(Tok::Sym("!~~*"));
                    i += 4;
                } else {
                    tokens.push(Tok::Sym("!~~"));
                    i += 3;
                }
            }
            b'~' => {
                if bytes.get(i + 1) == Some(&b'~') {
                    if bytes.get(i + 2) == Some(&b'*') {
                        tokens.push(Tok::Sym("~~*"));
                        i += 3;
                    } else {
                        tokens.push(Tok::Sym("~~"));
                        i += 2;
                    }
                } else {
                    return None;
                }
            }
            // Standard string literal -> decoded value. Escape / dollar-quoted strings are not
            // decoded here; bail to the string-equality fallback rather than risk a wrong value.
            b'\'' => {
                let end = quoted_string_end(bytes, i, false);
                if end > bytes.len() || bytes.get(end.wrapping_sub(1)) != Some(&b'\'') {
                    return None;
                }
                let inner = &input[i + 1..end - 1];
                tokens.push(Tok::Str(inner.replace("''", "'")));
                i = end;
            }
            b'E' | b'e' if bytes.get(i + 1) == Some(&b'\'') => return None,
            b'$' if dollar_quoted_end(bytes, i).is_some() => return None,
            // Quoted identifier (case preserved, never a keyword).
            b'"' => {
                let mut j = i + 1;
                while j < bytes.len() {
                    if bytes[j] == b'"' {
                        if bytes.get(j + 1) == Some(&b'"') {
                            j += 2;
                            continue;
                        }
                        j += 1;
                        break;
                    }
                    j += 1;
                }
                let raw = &input[i..j];
                tokens.push(Tok::Ident(raw[1..raw.len() - 1].replace("\"\"", "\"")));
                i = j;
            }
            _ if b.is_ascii_digit()
                || (b == b'.' && bytes.get(i + 1).is_some_and(u8::is_ascii_digit)) =>
            {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'.') {
                    i += 1;
                }
                if matches!(bytes.get(i), Some(b'e' | b'E')) {
                    i += 1;
                    if matches!(bytes.get(i), Some(b'+' | b'-')) {
                        i += 1;
                    }
                    while i < bytes.len() && bytes[i].is_ascii_digit() {
                        i += 1;
                    }
                }
                tokens.push(Tok::Num(input[start..i].to_owned()));
            }
            _ if b.is_ascii_alphabetic() || b == b'_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                let word = input[start..i].to_ascii_lowercase();
                match KEYWORDS.binary_search(&word.as_str()) {
                    Ok(idx) => tokens.push(Tok::Kw(KEYWORDS[idx])),
                    Err(_) => tokens.push(Tok::Ident(word)),
                }
            }
            _ => return None,
        }
    }
    Some(tokens)
}

struct Parser<'a> {
    tokens: &'a [Tok],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a Tok> {
        self.tokens.get(self.pos)
    }

    fn eat_sym(&mut self, sym: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Sym(s)) if *s == sym) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn eat_kw(&mut self, kw: &str) -> bool {
        if matches!(self.peek(), Some(Tok::Kw(k)) if *k == kw) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn parse_or(&mut self) -> Option<Node> {
        let mut operands = vec![self.parse_and()?];
        while self.eat_kw("or") {
            operands.push(self.parse_and()?);
        }
        Some(flatten(operands, true))
    }

    fn parse_and(&mut self) -> Option<Node> {
        let mut operands = vec![self.parse_not()?];
        while self.eat_kw("and") {
            operands.push(self.parse_not()?);
        }
        Some(flatten(operands, false))
    }

    fn parse_not(&mut self) -> Option<Node> {
        if self.eat_kw("not") {
            return Some(Node::Not(Box::new(self.parse_not()?)));
        }
        self.parse_predicate()
    }

    fn parse_predicate(&mut self) -> Option<Node> {
        let left = self.parse_additive()?;
        // IS [NOT] NULL | TRUE | FALSE
        if self.eat_kw("is") {
            let negated = self.eat_kw("not");
            if self.eat_kw("null") {
                return Some(Node::IsNull(Box::new(left), negated));
            }
            if self.eat_kw("true") {
                return Some(Node::IsBool(Box::new(left), true, negated));
            }
            if self.eat_kw("false") {
                return Some(Node::IsBool(Box::new(left), false, negated));
            }
            return None;
        }
        let negated = self.eat_kw("not");
        if self.eat_kw("in") {
            return self.parse_in(left, negated);
        }
        if self.eat_kw("between") {
            return self.parse_between(left, negated);
        }
        if self.eat_kw("like") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                false,
                negated,
            ));
        }
        if self.eat_kw("ilike") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                true,
                negated,
            ));
        }
        if negated {
            return None; // `NOT` only precedes IN / BETWEEN / LIKE here
        }
        if self.eat_sym("~~") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                false,
                false,
            ));
        }
        if self.eat_sym("~~*") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                true,
                false,
            ));
        }
        // `!~~` / `!~~*` are PostgreSQL's deparse of `NOT LIKE` / `NOT ILIKE`.
        if self.eat_sym("!~~") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                false,
                true,
            ));
        }
        if self.eat_sym("!~~*") {
            return Some(Node::Like(
                Box::new(left),
                Box::new(self.parse_additive()?),
                true,
                true,
            ));
        }
        for op in ["=", "<>", "<=", ">=", "<", ">"] {
            if self.eat_sym(op) {
                // `x = ANY (ARRAY[..])` / `x <> ALL (ARRAY[..])` are PostgreSQL's deparse of
                // `x IN (..)` / `x NOT IN (..)`.
                if op == "=" && matches!(self.peek(), Some(Tok::Kw("any"))) {
                    return self.parse_array_membership(left, "any");
                }
                if op == "<>" && matches!(self.peek(), Some(Tok::Kw("all"))) {
                    return self.parse_array_membership(left, "all");
                }
                return Some(Node::Compare(
                    op,
                    Box::new(left),
                    Box::new(self.parse_additive()?),
                ));
            }
        }
        Some(left)
    }

    fn parse_in(&mut self, left: Node, negated: bool) -> Option<Node> {
        if !self.eat_sym("(") {
            return None;
        }
        let items = self.parse_expr_list()?;
        if !self.eat_sym(")") {
            return None;
        }
        let node = Node::In(Box::new(left), items);
        Some(if negated {
            Node::Not(Box::new(node))
        } else {
            node
        })
    }

    /// Parses `<quantifier> (ARRAY[..])` after the comparison operator. `any` (from `= ANY`) is
    /// `IN`; `all` (from `<> ALL`) is `NOT IN`.
    fn parse_array_membership(&mut self, left: Node, quantifier: &'static str) -> Option<Node> {
        if !self.eat_kw(quantifier)
            || !self.eat_sym("(")
            || !self.eat_kw("array")
            || !self.eat_sym("[")
        {
            return None;
        }
        let items = self.parse_expr_list()?;
        if !self.eat_sym("]") || !self.eat_sym(")") {
            return None;
        }
        let node = Node::In(Box::new(left), items);
        Some(if quantifier == "all" {
            Node::Not(Box::new(node))
        } else {
            node
        })
    }

    fn parse_between(&mut self, left: Node, negated: bool) -> Option<Node> {
        let low = self.parse_additive()?;
        if !self.eat_kw("and") {
            return None;
        }
        let high = self.parse_additive()?;
        // Match pg's deparse: `BETWEEN a AND b` -> `(x >= a) AND (x <= b)`, and the negation
        // `NOT BETWEEN a AND b` -> `(x < a) OR (x > b)` (pg expands it, rather than negating).
        Some(if negated {
            Node::Or(vec![
                Node::Compare("<", Box::new(left.clone()), Box::new(low)),
                Node::Compare(">", Box::new(left), Box::new(high)),
            ])
        } else {
            Node::And(vec![
                Node::Compare(">=", Box::new(left.clone()), Box::new(low)),
                Node::Compare("<=", Box::new(left), Box::new(high)),
            ])
        })
    }

    fn parse_expr_list(&mut self) -> Option<Vec<Node>> {
        let mut items = vec![self.parse_additive()?];
        while self.eat_sym(",") {
            items.push(self.parse_additive()?);
        }
        Some(items)
    }

    fn parse_additive(&mut self) -> Option<Node> {
        let mut node = self.parse_multiplicative()?;
        loop {
            let op = if self.eat_sym("+") {
                "+"
            } else if self.eat_sym("-") {
                "-"
            } else {
                break;
            };
            node = Node::Arith(op, Box::new(node), Box::new(self.parse_multiplicative()?));
        }
        Some(node)
    }

    fn parse_multiplicative(&mut self) -> Option<Node> {
        let mut node = self.parse_unary()?;
        loop {
            let op = if self.eat_sym("*") {
                "*"
            } else if self.eat_sym("/") {
                "/"
            } else if self.eat_sym("%") {
                "%"
            } else {
                break;
            };
            node = Node::Arith(op, Box::new(node), Box::new(self.parse_unary()?));
        }
        Some(node)
    }

    fn parse_unary(&mut self) -> Option<Node> {
        if self.eat_sym("-") {
            let operand = self.parse_unary()?;
            return Some(negate(operand));
        }
        self.parse_cast()
    }

    fn parse_cast(&mut self) -> Option<Node> {
        let mut node = self.parse_primary()?;
        while self.eat_sym("::") {
            let ty = self.parse_type_name()?;
            node = apply_cast(node, &ty);
        }
        Some(node)
    }

    /// A (possibly multi-word) type name following `::`, e.g. `double precision`,
    /// `timestamp with time zone`, `character varying(8)`. Consumes consecutive identifier words and
    /// an optional parenthesized argument list.
    fn parse_type_name(&mut self) -> Option<String> {
        let mut parts = Vec::new();
        while let Some(Tok::Ident(word)) = self.peek() {
            parts.push(word.clone());
            self.pos += 1;
        }
        if parts.is_empty() {
            return None;
        }
        if self.eat_sym("(") {
            while !self.eat_sym(")") {
                self.peek()?;
                self.pos += 1;
            }
        }
        Some(parts.join(" "))
    }

    fn parse_primary(&mut self) -> Option<Node> {
        match self.peek()? {
            Tok::Sym("(") => {
                self.pos += 1;
                let node = self.parse_or()?;
                if !self.eat_sym(")") {
                    return None;
                }
                Some(node)
            }
            Tok::Num(text) => {
                let text = text.clone();
                self.pos += 1;
                Some(Node::Lit(number_literal(&text)))
            }
            Tok::Str(value) => {
                let value = value.clone();
                self.pos += 1;
                Some(Node::Lit(Lit::Str(value)))
            }
            Tok::Kw("null") => {
                self.pos += 1;
                Some(Node::Lit(Lit::Null))
            }
            Tok::Kw("true") => {
                self.pos += 1;
                Some(Node::Lit(Lit::Bool(true)))
            }
            Tok::Kw("false") => {
                self.pos += 1;
                Some(Node::Lit(Lit::Bool(false)))
            }
            Tok::Ident(name) => {
                let name = name.clone();
                self.pos += 1;
                if self.eat_sym("(") {
                    let args = if matches!(self.peek(), Some(Tok::Sym(")"))) {
                        Vec::new()
                    } else {
                        self.parse_expr_list()?
                    };
                    if !self.eat_sym(")") {
                        return None;
                    }
                    Some(Node::Func(name, args))
                } else {
                    Some(Node::Col(name))
                }
            }
            _ => None,
        }
    }
}

fn flatten(mut operands: Vec<Node>, is_or: bool) -> Node {
    if operands.len() == 1 {
        return operands.pop().unwrap();
    }
    let mut flat = Vec::new();
    for operand in operands {
        match operand {
            Node::Or(inner) if is_or => flat.extend(inner),
            Node::And(inner) if !is_or => flat.extend(inner),
            other => flat.push(other),
        }
    }
    if is_or {
        Node::Or(flat)
    } else {
        Node::And(flat)
    }
}

fn number_literal(text: &str) -> Lit {
    if text.contains(['.', 'e', 'E']) {
        Lit::Num(text.to_owned())
    } else if let Ok(value) = text.parse::<i128>() {
        Lit::Int(value)
    } else {
        Lit::Num(text.to_owned())
    }
}

fn negate(node: Node) -> Node {
    match node {
        Node::Lit(Lit::Int(value)) => Node::Lit(Lit::Int(-value)),
        Node::Lit(Lit::Num(text)) => {
            let negated = match text.strip_prefix('-') {
                Some(rest) => rest.to_owned(),
                None => format!("-{text}"),
            };
            Node::Lit(Lit::Num(negated))
        }
        other => Node::Neg(Box::new(other)),
    }
}

/// Applies a `::type` cast. A cast on a *literal* is the coercion PostgreSQL synthesizes in deparse
/// output, so it is folded into the literal's value (and dropped); a cast on anything else is kept.
fn apply_cast(node: Node, ty: &str) -> Node {
    match node {
        Node::Lit(lit) => match (type_category(ty), lit) {
            (TypeCategory::Int, Lit::Str(s) | Lit::Num(s)) => match s.parse::<i128>() {
                Ok(value) => Node::Lit(Lit::Int(value)),
                Err(_) => Node::Lit(Lit::Num(s)),
            },
            (TypeCategory::Int, Lit::Int(value)) => Node::Lit(Lit::Int(value)),
            (TypeCategory::Num, Lit::Str(s) | Lit::Num(s)) => Node::Lit(Lit::Num(s)),
            (TypeCategory::Num, Lit::Int(value)) => Node::Lit(Lit::Num(value.to_string())),
            (TypeCategory::Text, Lit::Str(s)) => Node::Lit(Lit::Str(s)),
            // A value pg reformats (timestamp/date/...): keep the already-canonical text so two
            // equal inputs still compare equal, but do not attempt to normalize the value.
            (TypeCategory::Other, Lit::Str(s)) => {
                Node::Lit(Lit::Opaque(format!("'{}'::{ty}", s.replace('\'', "''"))))
            }
            (_, other) => Node::Cast(Box::new(Node::Lit(other)), ty.to_owned()),
        },
        other => Node::Cast(Box::new(other), ty.to_owned()),
    }
}

enum TypeCategory {
    Int,
    Num,
    Text,
    Other,
}

fn type_category(ty: &str) -> TypeCategory {
    match ty {
        "smallint" | "int2" | "integer" | "int" | "int4" | "bigint" | "int8" => TypeCategory::Int,
        "numeric" | "decimal" | "real" | "float4" | "float8" | "double precision" => {
            TypeCategory::Num
        }
        _ if ty.starts_with("text")
            || ty.starts_with("varchar")
            || ty.starts_with("char")
            || ty.starts_with("bpchar")
            || ty.starts_with("character") =>
        {
            TypeCategory::Text
        }
        _ => TypeCategory::Other,
    }
}

fn serialize(node: &Node) -> String {
    let mut out = String::new();
    write_node(node, &mut out);
    out
}

fn write_node(node: &Node, out: &mut String) {
    match node {
        Node::And(operands) => write_chain(operands, "and", out),
        Node::Or(operands) => write_chain(operands, "or", out),
        Node::Not(inner) => {
            out.push_str("(not ");
            write_node(inner, out);
            out.push(')');
        }
        Node::IsNull(inner, negated) => {
            out.push('(');
            write_node(inner, out);
            out.push_str(if *negated {
                " is not null)"
            } else {
                " is null)"
            });
        }
        Node::IsBool(inner, value, negated) => {
            out.push('(');
            write_node(inner, out);
            out.push_str(" is ");
            if *negated {
                out.push_str("not ");
            }
            out.push_str(if *value { "true)" } else { "false)" });
        }
        Node::Compare(op, left, right) | Node::Arith(op, left, right) => {
            out.push('(');
            write_node(left, out);
            out.push(' ');
            out.push_str(op);
            out.push(' ');
            write_node(right, out);
            out.push(')');
        }
        Node::In(operand, items) => {
            out.push('(');
            write_node(operand, out);
            out.push_str(" in (");
            write_list(items, out);
            out.push_str("))");
        }
        Node::Like(operand, pattern, case_insensitive, negated) => {
            out.push('(');
            write_node(operand, out);
            out.push(' ');
            if *negated {
                out.push_str("not ");
            }
            out.push_str(if *case_insensitive { "ilike " } else { "like " });
            write_node(pattern, out);
            out.push(')');
        }
        Node::Neg(inner) => {
            out.push_str("(- ");
            write_node(inner, out);
            out.push(')');
        }
        Node::Cast(inner, ty) => {
            out.push('(');
            write_node(inner, out);
            out.push_str(")::");
            out.push_str(ty);
        }
        Node::Func(name, args) => {
            out.push_str(name);
            out.push('(');
            write_list(args, out);
            out.push(')');
        }
        Node::Col(name) => write_ident(name, out),
        Node::Lit(lit) => write_lit(lit, out),
    }
}

fn write_chain(operands: &[Node], op: &str, out: &mut String) {
    out.push('(');
    for (index, operand) in operands.iter().enumerate() {
        if index > 0 {
            out.push(' ');
            out.push_str(op);
            out.push(' ');
        }
        write_node(operand, out);
    }
    out.push(')');
}

fn write_list(items: &[Node], out: &mut String) {
    for (index, item) in items.iter().enumerate() {
        if index > 0 {
            out.push_str(", ");
        }
        write_node(item, out);
    }
}

fn write_ident(name: &str, out: &mut String) {
    if deparses_unquoted(name) {
        out.push_str(name);
    } else {
        out.push('"');
        out.push_str(&name.replace('"', "\"\""));
        out.push('"');
    }
}

fn write_lit(lit: &Lit, out: &mut String) {
    match lit {
        Lit::Int(value) => out.push_str(&value.to_string()),
        Lit::Num(text) => out.push_str(text),
        Lit::Str(value) => {
            out.push('\'');
            out.push_str(&value.replace('\'', "''"));
            out.push('\'');
        }
        Lit::Bool(value) => out.push_str(if *value { "true" } else { "false" }),
        Lit::Null => out.push_str("null"),
        Lit::Opaque(text) => out.push_str(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Asserts the crate-rendered / user-authored `desired` form and PostgreSQL's deparsed `actual`
    /// form canonicalize to the same string. Because the two inputs differ textually, equality also
    /// proves both parsed — had either fallen back to identity, the results would differ.
    #[track_caller]
    fn same(desired: &str, actual: &str) {
        assert_ne!(
            desired, actual,
            "inputs must differ so equality proves parsing"
        );
        assert_eq!(
            canonical_index_predicate(desired),
            canonical_index_predicate(actual),
            "desired {desired:?} and actual {actual:?} should canonicalize equal"
        );
    }

    #[test]
    fn keyword_tables_are_sorted_for_binary_search() {
        assert!(QUOTED_KEYWORDS.is_sorted());
        assert!(KEYWORDS.is_sorted());
    }

    #[test]
    fn identifier_quoting_and_boolean_case() {
        same("(\"deleted_at\" IS NULL)", "(deleted_at IS NULL)");
        same("(\"status\" = \"other\")", "(status = other)");
        same("(\"active\" = TRUE)", "(active = true)");
        // `between` is a col_name keyword pg keeps quoted; both sides quote it.
        same("(\"deleted_at\" IS NULL)", "(\"deleted_at\" is null)");
    }

    #[test]
    fn nary_associative_flattening() {
        same(
            "(((\"a\" = 1) AND (\"b\" = 2)) AND (\"c\" = 3))",
            "((a = 1) AND (b = 2) AND (c = 3))",
        );
        same(
            "((\"a\" = 1) OR ((\"b\" = 2) OR (\"c\" = 3)))",
            "((a = 1) OR (b = 2) OR (c = 3))",
        );
    }

    #[test]
    fn value_literal_casts_are_stripped() {
        same("(\"label\" = 'x')", "(label = 'x'::text)");
        same("(\"big\" = 5000000000)", "(big = '5000000000'::bigint)");
        same("(\"status\" = -5)", "(status = '-5'::integer)");
        same("(\"score\" > -1.5)", "(score > '-1.5'::numeric)");
        same("(\"price\" > 1.5)", "(price > (1.5)::double precision)");
    }

    #[test]
    fn operator_rewrites_in_between_like() {
        same("status IN (1, 2, 3)", "(status = ANY (ARRAY[1, 2, 3]))");
        same(
            "label IN ('a', 'b')",
            "(label = ANY (ARRAY['a'::text, 'b'::text]))",
        );
        same("qty BETWEEN 0 AND 100", "((qty >= 0) AND (qty <= 100))");
        same("label LIKE 'a%'", "(label ~~ 'a%'::text)");
        same("name ILIKE 'b%'", "(name ~~* 'b%'::text)");
    }

    #[test]
    fn negated_operator_rewrites() {
        // PostgreSQL deparses the negations with different operators than a leading `NOT`.
        same("status NOT IN (1, 2)", "(status <> ALL (ARRAY[1, 2]))");
        same("label NOT LIKE 'a%'", "(label !~~ 'a%'::text)");
        same("name NOT ILIKE 'b%'", "(name !~~* 'b%'::text)");
        same("qty NOT BETWEEN 0 AND 100", "((qty < 0) OR (qty > 100))");
    }

    #[test]
    fn check_expression_forms() {
        same("length(label) > 3", "(length(label) > 3)");
        same("qty + 1 > status * 2", "(((qty + 1) > (status * 2)))");
        same("(status = 1) IS TRUE", "(((status = 1) IS TRUE))");
        same(
            "score > 0 OR label IS NOT NULL",
            "((score > 0) OR (label IS NOT NULL))",
        );
    }

    #[test]
    fn string_literal_quotes_are_decoded_consistently() {
        same("\"label\" = 'o''brien'", "(label = 'o''brien'::text)");
    }

    #[test]
    fn unparseable_expressions_fall_back_to_identity() {
        // A subquery is outside the grammar: the parse fails and the input is returned unchanged, so
        // the diff degrades to string equality rather than mis-matching.
        let subquery = "(id IN (select x from t))";
        assert_eq!(canonical_index_predicate(subquery), subquery);
        // Escape strings are not decoded; bail rather than risk a wrong value.
        let escaped = "(a = E'x')";
        assert_eq!(canonical_index_predicate(escaped), escaped);
        // Dollar-quoted strings likewise.
        let dollar = "(a = $$x$$)";
        assert_eq!(canonical_index_predicate(dollar), dollar);
    }
}
