//! Reads a live SQLite database into the neutral [`DatabaseModel`].
//!
//! SQLite has no `information_schema`; introspection reads `sqlite_master` and the table-valued PRAGMA
//! functions (`pragma_table_info` / `pragma_foreign_key_list` / `pragma_index_list` /
//! `pragma_index_xinfo`), which — unlike `PRAGMA` statements — accept a bound table-name parameter.
//!
//! SQLite is dynamically typed and does not round-trip several facts the schema-aware backends do, so
//! introspection is deliberately lossy in matching ways (the desired model is canonicalized the same
//! way before diffing — see the `canonical_*` hooks on [`Sqlite`](crate::Sqlite)):
//! - **No namespaces** — every table is read under the default (`None`) schema.
//! - **Type affinity, not type** — a column's declared type collapses to one of five affinities, so
//!   `Varchar`/`Uuid`/`Timestamp`/`Bool` etc. all read back as their affinity's representative type.
//! - **Unnamed constraints** — a `UNIQUE`/foreign-key/primary-key constraint does not round-trip its
//!   declared name (the backing auto-index is `sqlite_autoindex_…`; foreign keys are positional).
//! - **`CHECK` constraints and column collations** live only in the `CREATE TABLE` text (no PRAGMA
//!   reports either), so they are recovered by parsing the stored statement — table checks by
//!   [`table_checks`] and per-column `COLLATE` clauses by [`column_collations`]. A check has no
//!   round-tripping name, so the diff matches it by a name derived from its expression (see
//!   [`Sqlite::canonical_check_name`](crate::Sqlite)). A partial index's `WHERE` predicate, likewise only
//!   in the DDL text, is recovered the same way (see [`partial_predicate`]).
//! - **Column comments and generated columns are not read** — SQLite has no column comments, and
//!   generated columns are not yet modelled; these are left empty (a documented limitation), and the
//!   renderer rejects a model carrying a column comment so a published schema has none to miss.

use std::collections::BTreeMap;

use squealy::{
    ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyAction, ForeignKeyModel,
    IdentityMode, IdentityModel, IndexDirection, IndexModel, SchemaModel, SqlType, TableModel,
    ViewColumnModel, ViewModel, ViewQueryModel,
};
use tokio_rusqlite::rusqlite::{self, Row};

use crate::sql::sqlite_affinity;
use crate::{SqliteConnection, SqliteError};

/// Introspects every user table into a single unqualified [`SchemaModel`] (SQLite has no namespaces).
pub(crate) async fn database(connection: &SqliteConnection) -> Result<DatabaseModel, SqliteError> {
    let mut tables = Vec::new();
    for name in table_names(connection).await? {
        tables.push(table(connection, &name).await?);
    }
    let mut views = Vec::new();
    for name in view_names(connection).await? {
        views.push(view(connection, &name).await?);
    }
    // SQLite has no namespace object: an empty database has no schema to report. Emitting an empty
    // default schema would diff against a genuinely empty model (`schemas: []`) as a spurious
    // `DropSchema`, so only include the schema once there is at least one object in it. Both tables and
    // views can populate it; a view is recovered by name only (see [`view`]), which is enough for the
    // diff to drop a removed or renamed view and to detect name collisions.
    let schemas = if tables.is_empty() && views.is_empty() {
        Vec::new()
    } else {
        vec![SchemaModel {
            name: None,
            tables,
            views,
        }]
    };
    Ok(DatabaseModel { schemas })
}

/// User table names in a deterministic order, excluding SQLite's internal objects (`sqlite_*`, which
/// SQLite reserves and forbids for user tables) and this backend's own bookkeeping tables
/// (`__squealy_*`). `GLOB` treats `_` literally (unlike `LIKE`), so the prefixes match exactly.
async fn table_names(connection: &SqliteConnection) -> Result<Vec<String>, SqliteError> {
    query(
        connection,
        "SELECT name FROM sqlite_master \
         WHERE type = 'table' \
           AND name NOT GLOB 'sqlite_*' \
           AND name NOT GLOB '__squealy_*' \
         ORDER BY name",
        None,
        |row| row.get::<_, String>(0),
    )
    .await
}

/// User view names in a deterministic order, excluding SQLite's internal objects and this backend's
/// bookkeeping tables — the same prefix filter as [`table_names`].
async fn view_names(connection: &SqliteConnection) -> Result<Vec<String>, SqliteError> {
    query(
        connection,
        "SELECT name FROM sqlite_master \
         WHERE type = 'view' \
           AND name NOT GLOB 'sqlite_*' \
           AND name NOT GLOB '__squealy_*' \
         ORDER BY name",
        None,
        |row| row.get::<_, String>(0),
    )
    .await
}

/// Introspects one view — its name and output column names. SQLite stores a view as its verbatim
/// `CREATE VIEW` text, which cannot be reconstructed into the structural [`ViewQueryModel`] body, so the
/// body (and its projection) stays empty: the "introspected, body unknown" marker the diff keys on to
/// re-apply the desired definition rather than compare bodies. `PRAGMA table_info` does report a view's
/// column *names*, but not a usable type for a computed output (`length(x)`, `a || b` come back with an
/// empty type), so every column is recorded as a single sentinel type ([`SqlType::Bytes`]) — the same
/// type a desired view column canonicalizes to (see `Sqlite::canonical_view_column_type`). The diff then
/// compares view columns by name: a column-set change (add/remove/rename) differs and forces a
/// destructive `DropView` + re-create, while an unchanged view (including computed columns) matches and
/// is re-applied non-destructively. The columns also let a removed or renamed view produce a `DropView`
/// (a view invisible to introspection would otherwise linger) and take part in the one-namespace check.
async fn view(connection: &SqliteConnection, name: &str) -> Result<ViewModel, SqliteError> {
    // A view whose body cannot be analyzed — it references a dropped table, or calls a function this
    // connection has not registered — makes `PRAGMA table_info` error when it reparses the view. Fall
    // back to a name-only view so introspection still succeeds and the diff can drop (or recreate) the
    // broken view, rather than failing the whole introspection and stranding a database that contains
    // one. The name comes from `sqlite_master` and always reads back; only the column probe can fail.
    let columns = columns(connection, name)
        .await
        .map(|rows| {
            rows.into_iter()
                .map(|column| ViewColumnModel {
                    name: column.name,
                    // SQLite cannot type a view output; use one sentinel so the diff compares by name.
                    ty: SqlType::Bytes,
                    nullable: false,
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(ViewModel {
        name: name.to_owned(),
        comment: None,
        columns,
        query: ViewQueryModel::default(),
    })
}

async fn table(connection: &SqliteConnection, name: &str) -> Result<TableModel, SqliteError> {
    let column_rows = columns(connection, name).await?;
    let create_sql = table_sql(connection, name).await?;

    let primary_key = primary_key(&column_rows);
    let autoincrement = primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.len() == 1 && declares_autoincrement(create_sql.as_deref()));
    // A column's `COLLATE` clause and the table's `CHECK` constraints live only in the `CREATE TABLE`
    // text (no PRAGMA reports either), so recover them by parsing the stored statement.
    let collations = create_sql
        .as_deref()
        .map(column_collations)
        .unwrap_or_default();

    let mut columns = Vec::with_capacity(column_rows.len());
    for column in &column_rows {
        let is_sole_pk_column = primary_key
            .as_ref()
            .is_some_and(|pk| pk.columns.len() == 1 && pk.columns[0] == column.name);
        // A single-column `INTEGER` primary key is the rowid alias, which is always `NOT NULL` even
        // though `PRAGMA table_info` reports `notnull = 0` for it. Every *other* primary-key column
        // (a `TEXT` key, a non-`INTEGER` type, or a composite key) genuinely allows NULLs in SQLite
        // unless declared `NOT NULL`, so its `notnull` flag is respected rather than forced.
        let is_rowid_alias =
            is_sole_pk_column && column.declared_type.eq_ignore_ascii_case("INTEGER");
        let is_sole_pk = autoincrement && is_sole_pk_column;
        // A `[u8; N]` column renders as `BLOB` plus a generated width `CHECK`; recover the width from
        // that check (which no PRAGMA reports) so a `FixedBytes(N)` column round-trips rather than
        // collapsing to `Bytes` (its BLOB affinity) and leaving a stale width check on a size change.
        let ty = match sql_type(&column.declared_type) {
            SqlType::Bytes => create_sql
                .as_deref()
                .and_then(|sql| fixed_bytes_width(sql, &column.name))
                .map_or(SqlType::Bytes, SqlType::FixedBytes),
            other => other,
        };
        columns.push(ColumnModel {
            name: column.name.clone(),
            comment: None,
            ty,
            collation: collations.get(&column.name).cloned(),
            nullable: column.notnull == 0 && !is_rowid_alias,
            default: column
                .default
                .as_deref()
                .map(|value| default_value(&column.declared_type, value)),
            // Only the single-column integer rowid primary key can carry `AUTOINCREMENT`.
            identity: is_sole_pk.then_some(IdentityModel {
                mode: IdentityMode::AutoIncrement,
            }),
            generated: None,
        });
    }

    let (uniques, indexes) = indexes(connection, name).await?;

    Ok(TableModel {
        name: name.to_owned(),
        comment: None,
        columns,
        primary_key,
        foreign_keys: foreign_keys(connection, name).await?,
        uniques,
        // SQLite exposes table `CHECK` constraints only in the `CREATE TABLE` text (no PRAGMA), so they
        // are recovered by parsing the stored statement (see [`table_checks`]).
        checks: create_sql.as_deref().map(table_checks).unwrap_or_default(),
        indexes,
    })
}

/// The raw `PRAGMA table_info` row shape.
struct ColumnRow {
    name: String,
    declared_type: String,
    notnull: i64,
    default: Option<String>,
    /// 0 for a non-key column, else the 1-based position of the column in the primary key.
    pk: i64,
}

async fn columns(
    connection: &SqliteConnection,
    table: &str,
) -> Result<Vec<ColumnRow>, SqliteError> {
    query(
        connection,
        "SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1) ORDER BY cid",
        Some(table.to_owned()),
        |row| {
            Ok(ColumnRow {
                name: row.get(0)?,
                declared_type: row.get(1)?,
                notnull: row.get(2)?,
                default: row.get(3)?,
                pk: row.get(4)?,
            })
        },
    )
    .await
}

/// Builds the primary key from the `pk` ordinals reported by `PRAGMA table_info` (0 = not a key
/// column, else the 1-based key position), ordered by that position. Rendered unnamed by SQLite, so the
/// name is left empty — the desired model is canonicalized to an empty primary-key name before diffing.
fn primary_key(columns: &[ColumnRow]) -> Option<Constraint> {
    let mut key_columns: Vec<&ColumnRow> = columns.iter().filter(|column| column.pk > 0).collect();
    if key_columns.is_empty() {
        return None;
    }
    key_columns.sort_by_key(|column| column.pk);
    Some(Constraint {
        name: String::new(),
        columns: key_columns
            .into_iter()
            .map(|column| column.name.clone())
            .collect(),
    })
}

/// The `CREATE TABLE` text SQLite stored verbatim, used to detect `AUTOINCREMENT` (which no PRAGMA
/// reports).
async fn table_sql(
    connection: &SqliteConnection,
    table: &str,
) -> Result<Option<String>, SqliteError> {
    Ok(query(
        connection,
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
        Some(table.to_owned()),
        |row| row.get::<_, Option<String>>(0),
    )
    .await?
    .into_iter()
    .next()
    .flatten())
}

/// Whether the stored `CREATE TABLE` text declares `AUTOINCREMENT` (no PRAGMA reports it, and
/// `sqlite_sequence` only gains a row after the first insert). SQLite stores the statement as written,
/// so the keyword is present iff an identity column was declared — but a plain substring scan would
/// misfire on a table named `autoincrements`, a quoted identifier, or a text default/comment containing
/// the word, so match it as a standalone unquoted keyword token (see [`contains_keyword`]).
fn declares_autoincrement(sql: Option<&str>) -> bool {
    sql.is_some_and(|sql| contains_keyword(sql, "AUTOINCREMENT"))
}

/// Whether `keyword` (ASCII, matched case-insensitively) appears in `sql` as a standalone token in
/// SQL code — see [`keyword_token_end`].
fn contains_keyword(sql: &str, keyword: &str) -> bool {
    keyword_token_end(sql, keyword).is_some()
}

/// If `bytes[index]` begins a token that is not SQL code — a string literal (`'…'`), a quoted identifier
/// (`"…"`, `` `…` ``, `[…]`) or a comment (`-- …`, `/* … */`) — returns the byte offset just past it (a
/// doubled quote inside a `'…'`/`"…"`/`` `…` `` literal is treated as an escape, not a close). Returns
/// `None` if `index` sits on ordinary code, so a caller advances by one. Both the keyword scanner and the
/// `CREATE TABLE`-body splitter use this so parentheses/commas/keywords inside quotes or comments are not
/// mistaken for code.
fn skip_noncode(bytes: &[u8], index: usize) -> Option<usize> {
    match bytes[index] {
        quote @ (b'\'' | b'"' | b'`') => {
            let mut cursor = index + 1;
            while cursor < bytes.len() {
                if bytes[cursor] == quote {
                    if bytes.get(cursor + 1) == Some(&quote) {
                        cursor += 2;
                        continue;
                    }
                    return Some(cursor + 1);
                }
                cursor += 1;
            }
            Some(cursor)
        }
        // Bracketed identifier `[ident]` (no escape form in SQLite).
        b'[' => {
            let mut cursor = index + 1;
            while cursor < bytes.len() && bytes[cursor] != b']' {
                cursor += 1;
            }
            Some((cursor + 1).min(bytes.len()))
        }
        // Line comment `-- …` to end of line.
        b'-' if bytes.get(index + 1) == Some(&b'-') => {
            let mut cursor = index + 2;
            while cursor < bytes.len() && bytes[cursor] != b'\n' {
                cursor += 1;
            }
            Some(cursor)
        }
        // Block comment `/* … */`.
        b'/' if bytes.get(index + 1) == Some(&b'*') => {
            let mut cursor = index + 2;
            while cursor < bytes.len()
                && !(bytes[cursor] == b'*' && bytes.get(cursor + 1) == Some(&b'/'))
            {
                cursor += 1;
            }
            Some((cursor + 2).min(bytes.len()))
        }
        _ => None,
    }
}

/// Whether `byte` can be part of an unquoted SQL identifier/keyword token. A non-ASCII byte (a
/// continuation or lead byte of a multi-byte UTF-8 identifier such as `é`) counts as a word byte, so a
/// Unicode identifier is scanned as one whole token — this keeps token boundaries on `char` boundaries,
/// avoiding a mid-`char` slice (and its panic) in the token helpers.
fn is_word_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_' || !byte.is_ascii()
}

/// The byte offset just past the first occurrence of `keyword` (ASCII, matched case-insensitively) as a
/// standalone token in SQL code — outside string literals, quoted identifiers and comments (see
/// [`skip_noncode`]), and not as a substring of a longer identifier. Returns `None` if the keyword does
/// not appear in code. This keeps a keyword distinct from an identifier that merely contains it (e.g.
/// `autoincrements`) or a `WHERE` inside a quoted name.
fn keyword_token_end(sql: &str, keyword: &str) -> Option<usize> {
    let bytes = sql.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if let Some(next) = skip_noncode(bytes, index) {
            index = next;
        } else if is_word_byte(bytes[index]) {
            let start = index;
            while index < bytes.len() && is_word_byte(bytes[index]) {
                index += 1;
            }
            if sql[start..index].eq_ignore_ascii_case(keyword) {
                return Some(index);
            }
        } else {
            index += 1;
        }
    }
    None
}

/// Splits the column-definition / table-constraint list inside the outermost parentheses of a
/// `CREATE TABLE` statement into its top-level (depth-1) comma-separated entries, each trimmed. A comma
/// or parenthesis inside a nested parenthesis, string literal, quoted identifier or comment is not a
/// split point (see [`skip_noncode`]), so a column's own inline `CHECK (…)`/`DEFAULT (…)` and a
/// multi-column constraint's column list stay within their entry. Returns an empty vec when the
/// statement has no parenthesized body (e.g. `CREATE TABLE t AS SELECT …`).
fn table_entries(create_sql: &str) -> Vec<&str> {
    let bytes = create_sql.as_bytes();
    let mut entries = Vec::new();
    let mut depth = 0usize;
    let mut index = 0;
    // The start of the current depth-1 entry, set once the outermost `(` is entered.
    let mut entry_start = None;
    while index < bytes.len() {
        if let Some(next) = skip_noncode(bytes, index) {
            index = next;
            continue;
        }
        match bytes[index] {
            b'(' => {
                depth += 1;
                index += 1;
                if depth == 1 {
                    entry_start = Some(index);
                }
            }
            b')' => {
                if depth == 1 {
                    if let Some(start) = entry_start.take() {
                        push_entry(&mut entries, &create_sql[start..index]);
                    }
                    return entries;
                }
                depth = depth.saturating_sub(1);
                index += 1;
            }
            b',' if depth == 1 => {
                if let Some(start) = entry_start {
                    push_entry(&mut entries, &create_sql[start..index]);
                }
                index += 1;
                entry_start = Some(index);
            }
            _ => index += 1,
        }
    }
    // A malformed (unterminated) statement: emit whatever the last open entry captured.
    if let Some(start) = entry_start {
        push_entry(&mut entries, &create_sql[start..]);
    }
    entries
}

fn push_entry<'a>(entries: &mut Vec<&'a str>, entry: &'a str) {
    let trimmed = entry.trim();
    if !trimmed.is_empty() {
        entries.push(trimmed);
    }
}

/// The table-level `CHECK` expressions of a stored `CREATE TABLE` statement, in declaration order. Only
/// entries that *are* a table check are returned: a leading `CONSTRAINT <name>` label is skipped, and
/// `PRIMARY KEY`/`UNIQUE`/`FOREIGN KEY` constraints and column definitions (whose own inline `CHECK` is
/// not at the entry's start) are not checks. The recovered name is left empty — the diff matches checks
/// by a name derived from the expression (see [`Sqlite::canonical_check_name`](crate::Sqlite)). The
/// generated `[u8; N]` width check lives inline in its column definition, not as a top-level entry, so
/// it is not returned here.
fn table_checks(create_sql: &str) -> Vec<squealy::CheckModel> {
    table_entries(create_sql)
        .into_iter()
        .filter_map(|entry| {
            table_check_expression(entry).map(|expression| squealy::CheckModel {
                name: String::new(),
                expression: squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
                    .read_check_expression_or_raw(&expression),
                validation: None,
                enforcement: None,
            })
        })
        .collect()
}

/// The `CHECK` expression of a table-constraint entry, or `None` if the entry is not a table-level check.
fn table_check_expression(entry: &str) -> Option<String> {
    let (keyword, rest) = leading_keyword(entry)?;
    let body = if keyword.eq_ignore_ascii_case("CONSTRAINT") {
        // `CONSTRAINT <name> CHECK (…)`: skip the name token, then require the `CHECK` keyword.
        let (_, after_name) = leading_token(rest)?;
        let (constraint_kind, after_kind) = leading_keyword(after_name)?;
        constraint_kind
            .eq_ignore_ascii_case("CHECK")
            .then_some(after_kind)?
    } else if keyword.eq_ignore_ascii_case("CHECK") {
        rest
    } else {
        // A column definition (leading token is a column name) or another constraint kind.
        return None;
    };
    parenthesized(body)
}

/// Maps each column's (unquoted) name to its `COLLATE` collation name, parsed from the `CREATE TABLE`
/// text — no PRAGMA reports a table column's collation. Only column-definition entries are scanned; a
/// `COLLATE` inside a table constraint is ignored. A `COLLATE` inside a column's own check expression is
/// not distinguished (the renderer never emits one there), matching the "recover exactly what the
/// renderer wrote" approach of [`fixed_bytes_width`] and [`partial_predicate`].
fn column_collations(create_sql: &str) -> BTreeMap<String, String> {
    let mut collations = BTreeMap::new();
    for entry in table_entries(create_sql) {
        // A table constraint (its leading token is a constraint keyword) is not a column definition.
        if leading_keyword(entry).is_some_and(|(keyword, _)| is_constraint_keyword(keyword)) {
            continue;
        }
        if let Some((name, rest)) = column_name(entry)
            && let Some(collation) = collate_clause(rest)
        {
            collations.insert(name, collation);
        }
    }
    collations
}

/// Whether `keyword` introduces a table-level constraint (rather than a column definition).
fn is_constraint_keyword(keyword: &str) -> bool {
    ["CONSTRAINT", "PRIMARY", "UNIQUE", "CHECK", "FOREIGN"]
        .iter()
        .any(|candidate| keyword.eq_ignore_ascii_case(candidate))
}

/// The leading bare-word token of `entry` (skipping leading whitespace) and the remainder after it, or
/// `None` if `entry` does not start with a word character (e.g. it starts with a quoted identifier).
fn leading_keyword(entry: &str) -> Option<(&str, &str)> {
    let trimmed = entry.trim_start();
    let bytes = trimmed.as_bytes();
    if !is_word_byte(*bytes.first()?) || bytes[0].is_ascii_digit() {
        return None;
    }
    let end = bytes
        .iter()
        .position(|&byte| !is_word_byte(byte))
        .unwrap_or(bytes.len());
    Some((&trimmed[..end], &trimmed[end..]))
}

/// The leading token of `entry` (skipping leading whitespace) — a quoted identifier, a bare word, or a
/// single punctuation byte — and the remainder after it. Used to step over a `CONSTRAINT <name>` label.
fn leading_token(entry: &str) -> Option<(&str, &str)> {
    let trimmed = entry.trim_start();
    let bytes = trimmed.as_bytes();
    let first = *bytes.first()?;
    let end = if let Some(quoted_end) = skip_noncode(bytes, 0) {
        quoted_end
    } else if is_word_byte(first) {
        bytes
            .iter()
            .position(|&byte| !is_word_byte(byte))
            .unwrap_or(bytes.len())
    } else {
        1
    };
    Some((&trimmed[..end], &trimmed[end..]))
}

/// The first token of a column-definition entry as its unquoted column name (matching how
/// `PRAGMA table_info` reports it), and the remainder after it. `None` if the entry does not begin with
/// an identifier.
fn column_name(entry: &str) -> Option<(String, &str)> {
    let (token, rest) = leading_token(entry)?;
    let name = unquote_ident(token)?;
    Some((name, rest))
}

/// The collation name of a column definition's `COLLATE` clause (the token after the `COLLATE` keyword),
/// unquoted, or `None` if the column has none.
fn collate_clause(column_rest: &str) -> Option<String> {
    let after_keyword = keyword_token_end(column_rest, "COLLATE")?;
    let (token, _) = leading_token(&column_rest[after_keyword..])?;
    unquote_ident(token)
}

/// Unquotes a SQL identifier token: strips a matching pair of `"…"`, `` `…` `` or `[…]` delimiters
/// (collapsing a doubled `"`/`` ` `` escape), or returns a bare identifier as-is. `None` if the token is
/// not an identifier (e.g. a punctuation byte).
fn unquote_ident(token: &str) -> Option<String> {
    let bytes = token.as_bytes();
    match bytes.first()? {
        quote @ (b'"' | b'`') => {
            let quote = *quote as char;
            let inner = token
                .strip_prefix(quote)
                .and_then(|rest| rest.strip_suffix(quote))
                .unwrap_or(token);
            Some(inner.replace(&format!("{quote}{quote}"), &quote.to_string()))
        }
        b'[' => Some(
            token
                .strip_prefix('[')
                .and_then(|rest| rest.strip_suffix(']'))
                .unwrap_or(token)
                .to_owned(),
        ),
        &first if is_word_byte(first) && !first.is_ascii_digit() => Some(token.to_owned()),
        _ => None,
    }
}

/// Requires `rest` (after leading whitespace) to begin with `(`, and returns the text inside the
/// matching close parenthesis, trimmed. Nested parentheses, quotes and comments are respected.
fn parenthesized(rest: &str) -> Option<String> {
    let trimmed = rest.trim_start();
    let bytes = trimmed.as_bytes();
    if bytes.first()? != &b'(' {
        return None;
    }
    let mut depth = 0usize;
    let mut index = 0;
    while index < bytes.len() {
        if let Some(next) = skip_noncode(bytes, index) {
            index = next;
            continue;
        }
        match bytes[index] {
            b'(' => {
                depth += 1;
                index += 1;
            }
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(trimmed[1..index].trim().to_owned());
                }
                index += 1;
            }
            _ => index += 1,
        }
    }
    None
}

async fn foreign_keys(
    connection: &SqliteConnection,
    table: &str,
) -> Result<Vec<ForeignKeyModel>, SqliteError> {
    // `PRAGMA foreign_key_list` reports one row per referencing column, grouped by a per-table `id` and
    // ordered within a key by `seq`. Rows are read in `(id, seq)` order so multi-column keys assemble
    // their columns in declaration order.
    let rows = query(
        connection,
        "SELECT id, \"table\", \"from\", \"to\", on_update, on_delete \
         FROM pragma_foreign_key_list(?1) ORDER BY id, seq",
        Some(table.to_owned()),
        |row| {
            Ok(ForeignKeyRow {
                id: row.get(0)?,
                references_table: row.get(1)?,
                column: row.get(2)?,
                // `to` is NULL when the foreign key omits the parent column list (`REFERENCES parent`),
                // meaning it references the parent's primary key; resolved below.
                references_column: row.get(3)?,
                on_update: row.get(4)?,
                on_delete: row.get(5)?,
            })
        },
    )
    .await?;

    // Preserve first-seen `id` order while grouping (a `BTreeMap<i64, _>` keyed by `id` keeps it). Track
    // the referenced columns separately as they may be NULL (an omitted parent column list).
    let mut grouped = BTreeMap::<i64, (ForeignKeyModel, Vec<Option<String>>)>::new();
    for row in rows {
        let entry = grouped.entry(row.id).or_insert_with(|| {
            (
                ForeignKeyModel {
                    // Unnamed by SQLite; the name is derived from the columns during canonicalization.
                    name: String::new(),
                    columns: Vec::new(),
                    references_schema: None,
                    references_table: row.references_table,
                    references_columns: Vec::new(),
                    match_type: None,
                    deferrability: None,
                    validation: None,
                    enforcement: None,
                    on_delete: action(&row.on_delete),
                    on_update: action(&row.on_update),
                },
                Vec::new(),
            )
        });
        entry.0.columns.push(row.column);
        entry.1.push(row.references_column);
    }

    let mut foreign_keys = Vec::with_capacity(grouped.len());
    for (mut foreign_key, references_columns) in grouped.into_values() {
        foreign_key.references_columns = if references_columns.iter().any(Option::is_none) {
            // An omitted parent column list references the parent's primary key: resolve it (in key
            // order) so the introspected model matches a model that names the columns explicitly.
            let parent_columns = columns(connection, &foreign_key.references_table).await?;
            primary_key(&parent_columns).map_or_else(Vec::new, |pk| pk.columns)
        } else {
            references_columns.into_iter().flatten().collect()
        };
        foreign_keys.push(foreign_key);
    }
    Ok(foreign_keys)
}

struct ForeignKeyRow {
    id: i64,
    references_table: String,
    column: String,
    references_column: Option<String>,
    on_update: String,
    on_delete: String,
}

/// Reads secondary indexes and unique constraints from `PRAGMA index_list`, splitting them by origin:
/// `u` is a `UNIQUE` constraint (a table-level [`Constraint`]), `c` is a `CREATE INDEX` (an
/// [`IndexModel`]), and `pk` is the primary key's backing index (already covered by `PRAGMA
/// table_info`, so skipped). Returns `(uniques, indexes)`.
async fn indexes(
    connection: &SqliteConnection,
    table: &str,
) -> Result<(Vec<Constraint>, Vec<IndexModel>), SqliteError> {
    let index_rows = query(
        connection,
        "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1) ORDER BY name",
        Some(table.to_owned()),
        |row| {
            Ok(IndexListRow {
                name: row.get(0)?,
                unique: row.get::<_, i64>(1)? != 0,
                origin: row.get(2)?,
                partial: row.get::<_, i64>(3)? != 0,
            })
        },
    )
    .await?;

    let mut uniques = Vec::new();
    let mut indexes = Vec::new();
    for index in index_rows {
        let (columns, directions) = index_columns(connection, &index.name).await?;
        match index.origin.as_str() {
            // A `UNIQUE` constraint: only the column set is meaningful (its auto-index name and any
            // ordering are SQLite-internal), so it becomes an unnamed table-level constraint.
            "u" => uniques.push(Constraint {
                name: String::new(),
                columns,
            }),
            // The primary key's backing index — already reconstructed from `PRAGMA table_info`.
            "pk" => {}
            // A `CREATE INDEX`, whose name SQLite *does* round-trip.
            _ => {
                // A partial index's `WHERE` predicate is not reported by any PRAGMA; SQLite stores the
                // `CREATE INDEX` text verbatim, so recover it from there (only when the index is partial).
                let predicate = if index.partial {
                    partial_predicate(index_sql(connection, &index.name).await?.as_deref()).map(
                        |predicate| {
                            Box::new(
                                squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
                                    .read_index_predicate_or_raw(&predicate),
                            )
                        },
                    )
                } else {
                    None
                };
                indexes.push(IndexModel {
                    name: index.name,
                    columns,
                    expressions: Vec::new(),
                    include_columns: Vec::new(),
                    unique: index.unique,
                    // SQLite has a single index method; the model leaves it unset (matching the renderer).
                    method: None,
                    directions,
                    nulls: Vec::new(),
                    collations: Vec::new(),
                    operator_classes: Vec::new(),
                    predicate,
                });
            }
        }
    }
    Ok((uniques, indexes))
}

/// The `CREATE INDEX` text SQLite stored verbatim for a named index.
async fn index_sql(
    connection: &SqliteConnection,
    index: &str,
) -> Result<Option<String>, SqliteError> {
    Ok(query(
        connection,
        "SELECT sql FROM sqlite_master WHERE type = 'index' AND name = ?1",
        Some(index.to_owned()),
        |row| row.get::<_, Option<String>>(0),
    )
    .await?
    .into_iter()
    .next()
    .flatten())
}

/// Recovers the width `N` of a `FixedBytes(N)` column from the generated length `CHECK` in its stored
/// `CREATE TABLE` statement (`length(CAST("col" AS BLOB)) = N`), or `None` if the column carries no such
/// check (a plain `Bytes`/`BLOB` column). This matches the exact form the renderer emits in
/// `write_column`; a differently-spelled equivalent check on an external database is not recovered (the
/// column then reads back as `Bytes`, a documented limitation).
fn fixed_bytes_width(create_sql: &str, column: &str) -> Option<u32> {
    // The `CAST` argument is the column name quoted the same way the renderer quotes identifiers
    // (double-quoted, internal quotes doubled).
    let quoted = format!("\"{}\"", column.replace('"', "\"\""));
    let prefix = format!("length(CAST({quoted} AS BLOB)) = ");
    let rest = &create_sql[create_sql.find(&prefix)? + prefix.len()..];
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// Extracts a partial index's predicate — the text after the `WHERE` — from its stored `CREATE INDEX`
/// statement. SQLite stores the statement as written and a `CREATE INDEX` has exactly one `WHERE`
/// (SQLite forbids subqueries in a partial-index predicate), so the predicate round-trips verbatim
/// (matching the renderer's `WHERE <predicate>` output). The `WHERE` token is located outside quoted
/// identifiers, so an index/table/column name containing the word does not misfire.
fn partial_predicate(index_sql: Option<&str>) -> Option<String> {
    let sql = index_sql?;
    let start = keyword_token_end(sql, "WHERE")?;
    let predicate = sql[start..].trim();
    (!predicate.is_empty()).then(|| predicate.to_owned())
}

struct IndexListRow {
    name: String,
    unique: bool,
    origin: String,
    partial: bool,
}

/// The key columns of an index and their sort directions, via `PRAGMA index_xinfo`. Only true key
/// terms are kept (`key = 1` drops the trailing rowid SQLite appends), in `seqno` order. Trailing
/// ascending directions are dropped: `ASC` is the default sort order, so the renderer omits it for a
/// column the model leaves at the default, and a model that specifies only a non-default prefix (e.g.
/// `[Desc]` for two columns) must compare equal to the read-back `[Desc, Asc]`. `canonicalize_index`
/// trims the desired side the same way.
async fn index_columns(
    connection: &SqliteConnection,
    index: &str,
) -> Result<(Vec<String>, Vec<IndexDirection>), SqliteError> {
    let rows = query(
        connection,
        "SELECT name, desc FROM pragma_index_xinfo(?1) WHERE key = 1 ORDER BY seqno",
        Some(index.to_owned()),
        |row| {
            Ok(IndexColumnRow {
                name: row.get(0)?,
                descending: row.get::<_, i64>(1)? != 0,
            })
        },
    )
    .await?;

    let mut columns = Vec::with_capacity(rows.len());
    let mut directions = Vec::with_capacity(rows.len());
    for row in rows {
        // An expression term has a NULL column name; represent it as an empty column so the term is not
        // silently dropped (expression indexes are otherwise not supported by this backend).
        columns.push(row.name.unwrap_or_default());
        directions.push(if row.descending {
            IndexDirection::Desc
        } else {
            IndexDirection::Asc
        });
    }
    while directions.last() == Some(&IndexDirection::Asc) {
        directions.pop();
    }
    Ok((columns, directions))
}

struct IndexColumnRow {
    name: Option<String>,
    descending: bool,
}

/// Maps a live column's declared type to the neutral [`SqlType`] its SQLite affinity represents. SQLite
/// stores the type verbatim but is dynamically typed, so introspection can only recover the affinity —
/// the same representative the desired model is canonicalized to (see
/// [`Sqlite::canonical_sql_type`](crate::Sqlite)).
fn sql_type(declared: &str) -> SqlType {
    representative_type(affinity_of(declared))
}

/// SQLite's five type affinities, assigned from a declared type by the rules in the SQLite docs
/// ("Determination Of Column Affinity").
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Affinity {
    Integer,
    Text,
    Blob,
    Real,
    Numeric,
}

/// Computes the SQLite affinity of a declared type string, following SQLite's substring rules in order.
pub(crate) fn affinity_of(declared: &str) -> Affinity {
    let declared = declared.to_ascii_uppercase();
    if declared.contains("INT") {
        Affinity::Integer
    } else if declared.contains("CHAR") || declared.contains("CLOB") || declared.contains("TEXT") {
        Affinity::Text
    } else if declared.is_empty() || declared.contains("BLOB") {
        Affinity::Blob
    } else if declared.contains("REAL") || declared.contains("FLOA") || declared.contains("DOUB") {
        Affinity::Real
    } else {
        Affinity::Numeric
    }
}

/// The neutral type that represents each affinity. This is the canonical form both introspection and
/// [`canonical_sql_type`](crate::Sqlite::canonical_sql_type) collapse to, so a desired column and the
/// column read back from the database compare equal despite SQLite discarding the logical type.
pub(crate) fn representative_type(affinity: Affinity) -> SqlType {
    match affinity {
        Affinity::Integer => SqlType::I64,
        Affinity::Text => SqlType::Text,
        Affinity::Blob => SqlType::Bytes,
        Affinity::Real => SqlType::F64,
        // NUMERIC has no lossless neutral type (a `Decimal`'s precision/scale is not stored), so it is
        // kept as an opaque `Raw("NUMERIC")` — the same form `canonical_sql_type` produces.
        Affinity::Numeric => SqlType::Raw("NUMERIC".to_owned()),
    }
}

/// The affinity of a neutral [`SqlType`], derived from the affinity name the renderer would emit for it,
/// so [`canonical_sql_type`](crate::Sqlite::canonical_sql_type) collapses a desired type exactly as
/// introspection collapses the type read back.
pub(crate) fn affinity_of_sql_type(ty: &SqlType) -> Affinity {
    affinity_of(sqlite_affinity(ty))
}

/// Maps a `PRAGMA foreign_key_list` action string to a neutral [`ForeignKeyAction`]. SQLite reports
/// `NO ACTION` for the default (unspecified) rule; the model represents that as `None`.
fn action(action: &str) -> Option<ForeignKeyAction> {
    match action.trim().to_ascii_uppercase().as_str() {
        "" | "NO ACTION" => None,
        other => Some(ForeignKeyAction::from_sql(other)),
    }
}

/// Maps a live column default (the `dflt_value` text SQLite stores) to a neutral [`DefaultValue`],
/// following the affinity of the column so a numeric/text/boolean literal is recovered structurally.
fn default_value(declared_type: &str, value: &str) -> DefaultValue {
    let trimmed = value.trim();
    match trimmed.to_ascii_uppercase().as_str() {
        "NULL" => return DefaultValue::Null,
        "CURRENT_TIMESTAMP" => return DefaultValue::CurrentTimestamp,
        "CURRENT_DATE" => return DefaultValue::CurrentDate,
        "CURRENT_TIME" => return DefaultValue::CurrentTime,
        _ => {}
    }
    // A text default is stored single-quoted; unwrap and unescape it back to the neutral text value.
    if let Some(text) = unquote(trimmed) {
        return DefaultValue::Text(text);
    }
    match affinity_of(declared_type) {
        Affinity::Integer => trimmed
            .parse::<i128>()
            .map(DefaultValue::Int)
            .unwrap_or_else(|_| DefaultValue::Raw(value.to_owned())),
        Affinity::Real => trimmed
            .parse::<f64>()
            .map(DefaultValue::Float)
            .unwrap_or_else(|_| DefaultValue::Raw(value.to_owned())),
        _ => DefaultValue::Raw(value.to_owned()),
    }
}

/// Unwraps a SQLite single-quoted string literal (`'a''b'` → `a'b`), or `None` if `value` is not one.
fn unquote(value: &str) -> Option<String> {
    let inner = value.strip_prefix('\'')?.strip_suffix('\'')?;
    Some(inner.replace("''", "'"))
}

/// Runs a read-only introspection query, mapping each row with `map`. `arg` binds the single `?1`
/// parameter (a table or index name) for the PRAGMA table-valued functions.
async fn query<T, F>(
    connection: &SqliteConnection,
    sql: &'static str,
    arg: Option<String>,
    map: F,
) -> Result<Vec<T>, SqliteError>
where
    T: Send + 'static,
    F: FnMut(&Row<'_>) -> rusqlite::Result<T> + Send + 'static,
{
    connection
        .conn
        .call(move |conn| {
            let mut statement = conn.prepare(sql)?;
            let rows = statement
                .query_map(rusqlite::params_from_iter(arg), map)?
                .collect::<rusqlite::Result<Vec<T>>>()?;
            Ok(rows)
        })
        .await
        .map_err(SqliteError::Introspect)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The structural check node a SQLite-dialect read of `sql` produces (matching how
    /// [`table_checks`] lowers a recovered check body).
    fn check_expr(sql: &str) -> squealy::ExprNode {
        squealy_parse::Reader::new(squealy_parse::SqlDialect::Sqlite)
            .read_check_expression_or_raw(sql)
    }

    #[test]
    fn affinity_follows_sqlite_rules() {
        assert!(matches!(affinity_of("INTEGER"), Affinity::Integer));
        assert!(matches!(affinity_of("BIGINT"), Affinity::Integer));
        assert!(matches!(affinity_of("VARCHAR(64)"), Affinity::Text));
        assert!(matches!(affinity_of("TEXT"), Affinity::Text));
        assert!(matches!(affinity_of("BLOB"), Affinity::Blob));
        assert!(matches!(affinity_of(""), Affinity::Blob));
        assert!(matches!(affinity_of("REAL"), Affinity::Real));
        assert!(matches!(affinity_of("DOUBLE"), Affinity::Real));
        assert!(matches!(affinity_of("NUMERIC"), Affinity::Numeric));
        assert!(matches!(affinity_of("DECIMAL(10,2)"), Affinity::Numeric));
    }

    #[test]
    fn representative_types_match_affinities() {
        assert_eq!(sql_type("INTEGER"), SqlType::I64);
        assert_eq!(sql_type("TEXT"), SqlType::Text);
        assert_eq!(sql_type("REAL"), SqlType::F64);
        assert_eq!(sql_type("BLOB"), SqlType::Bytes);
        assert_eq!(sql_type("NUMERIC"), SqlType::Raw("NUMERIC".to_owned()));
    }

    #[test]
    fn defaults_recover_structurally() {
        assert_eq!(default_value("INTEGER", "42"), DefaultValue::Int(42));
        assert_eq!(default_value("REAL", "1.5"), DefaultValue::Float(1.5));
        assert_eq!(
            default_value("TEXT", "'draft'"),
            DefaultValue::Text("draft".to_owned())
        );
        assert_eq!(
            default_value("TEXT", "'a''b'"),
            DefaultValue::Text("a'b".to_owned())
        );
        assert_eq!(
            default_value("TIMESTAMP", "CURRENT_TIMESTAMP"),
            DefaultValue::CurrentTimestamp
        );
    }

    #[test]
    fn partial_predicate_extracts_where_clause() {
        assert_eq!(
            partial_predicate(Some(
                "CREATE INDEX \"i\" ON \"t\" (\"a\") WHERE \"deleted_at\" IS NULL"
            )),
            Some("\"deleted_at\" IS NULL".to_owned())
        );
        // A non-partial index has no `WHERE`.
        assert_eq!(
            partial_predicate(Some("CREATE INDEX \"i\" ON \"t\" (\"a\")")),
            None
        );
        assert_eq!(partial_predicate(None), None);
        // A quoted identifier containing ` WHERE ` must not be mistaken for the predicate delimiter.
        assert_eq!(
            partial_predicate(Some(
                "CREATE INDEX \"idx WHERE trap\" ON \"t\" (\"a\") WHERE \"a\" IS NOT NULL"
            )),
            Some("\"a\" IS NOT NULL".to_owned())
        );
    }

    #[test]
    fn fixed_bytes_width_recovers_generated_check() {
        let sql = "CREATE TABLE \"t\" (\"hash\" BLOB NOT NULL CHECK (length(CAST(\"hash\" AS BLOB)) = 16))";
        assert_eq!(fixed_bytes_width(sql, "hash"), Some(16));
        // A plain BLOB column has no width check.
        assert_eq!(
            fixed_bytes_width("CREATE TABLE \"t\" (\"data\" BLOB)", "data"),
            None
        );
    }

    #[test]
    fn contains_keyword_matches_only_standalone_unquoted_tokens() {
        assert!(contains_keyword(
            "CREATE TABLE \"t\" (\"id\" INTEGER PRIMARY KEY AUTOINCREMENT)",
            "AUTOINCREMENT"
        ));
        // An identifier that merely contains the word is not a match.
        assert!(!contains_keyword(
            "CREATE TABLE autoincrements (id INTEGER)",
            "AUTOINCREMENT"
        ));
        // A quoted identifier is not code.
        assert!(!contains_keyword(
            "CREATE TABLE \"t\" (\"autoincrement\" INTEGER)",
            "AUTOINCREMENT"
        ));
        // A string-literal default is not code.
        assert!(!contains_keyword(
            "CREATE TABLE \"t\" (\"note\" TEXT DEFAULT 'has AUTOINCREMENT')",
            "AUTOINCREMENT"
        ));
        // A comment is not code.
        assert!(!contains_keyword(
            "CREATE TABLE \"t\" (\"id\" INTEGER) -- AUTOINCREMENT here",
            "AUTOINCREMENT"
        ));
    }

    #[test]
    fn foreign_key_actions_map_no_action_to_none() {
        assert_eq!(action("NO ACTION"), None);
        assert_eq!(action(""), None);
        assert_eq!(action("CASCADE"), Some(ForeignKeyAction::Cascade));
        assert_eq!(action("SET NULL"), Some(ForeignKeyAction::SetNull));
    }

    #[test]
    fn table_entries_split_on_top_level_commas_only() {
        // Commas inside a column's inline check, a multi-column constraint's list, a string default, a
        // quoted identifier and a comment are not split points.
        let sql = "CREATE TABLE \"t\" (\n  \"a\" INTEGER CHECK (\"a\" IN (1, 2)),\n  \"b, c\" TEXT \
                   DEFAULT 'x, y',\n  UNIQUE (\"a\", \"b, c\") -- trailing, comment\n)";
        let entries = table_entries(sql);
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0], "\"a\" INTEGER CHECK (\"a\" IN (1, 2))");
        assert_eq!(entries[1], "\"b, c\" TEXT DEFAULT 'x, y'");
        assert_eq!(entries[2], "UNIQUE (\"a\", \"b, c\") -- trailing, comment");
        // A statement with no parenthesized body yields nothing.
        assert!(table_entries("CREATE TABLE t AS SELECT 1").is_empty());
    }

    #[test]
    fn table_checks_recovers_table_level_checks_only() {
        // Bare and CONSTRAINT-named table checks are recovered; a column's inline check (the generated
        // `[u8; N]` width check) and other constraint kinds are not.
        let sql = "CREATE TABLE \"t\" (\n  \"hash\" BLOB CHECK (length(CAST(\"hash\" AS BLOB)) = 16),\n \
                   \"n\" INTEGER,\n  PRIMARY KEY (\"n\"),\n  CHECK (\"n\" >= 0),\n  CONSTRAINT \"c2\" \
                   CHECK (\"n\" < 100)\n)";
        let checks = table_checks(sql);
        let expressions: Vec<squealy::ExprNode> =
            checks.iter().map(|c| c.expression.clone()).collect();
        assert_eq!(
            expressions,
            vec![check_expr("\"n\" >= 0"), check_expr("\"n\" < 100")]
        );
        // The recovered name is empty (matched by a derived name, not the DDL name).
        assert!(checks.iter().all(|c| c.name.is_empty()));
    }

    #[test]
    fn column_collations_recovers_collate_clauses() {
        let sql = "CREATE TABLE \"t\" (\n  \"name\" TEXT COLLATE NOCASE NOT NULL,\n  \"code\" TEXT \
                   COLLATE \"RTRIM\",\n  \"weird\" TEXT COLLATE \"a-b c\",\n  \"plain\" TEXT,\n  \
                   CHECK (\"plain\" <> '' COLLATE NOCASE)\n)";
        let collations = column_collations(sql);
        assert_eq!(collations.get("name"), Some(&"NOCASE".to_owned()));
        // A quoted collation name is unquoted.
        assert_eq!(collations.get("code"), Some(&"RTRIM".to_owned()));
        // A collation name that needs identifier quoting (the renderer emits it quoted) round-trips.
        assert_eq!(collations.get("weird"), Some(&"a-b c".to_owned()));
        // A column with no COLLATE, and a COLLATE inside a table constraint, contribute nothing.
        assert_eq!(collations.get("plain"), None);
        assert_eq!(collations.len(), 3);
    }

    #[test]
    fn parsers_handle_non_ascii_unquoted_identifiers() {
        // A valid SQLite table can start a column with an unquoted non-ASCII identifier. The token
        // helpers must scan it on `char` boundaries rather than slice at byte 1 (which would panic).
        let sql = "CREATE TABLE t (é TEXT COLLATE NOCASE, CHECK (é <> ''))";
        assert_eq!(column_collations(sql).get("é"), Some(&"NOCASE".to_owned()));
        assert_eq!(
            table_checks(sql)
                .iter()
                .map(|c| c.expression.clone())
                .collect::<Vec<_>>(),
            vec![check_expr("é <> ''")]
        );
        // A non-ASCII collation name round-trips too.
        assert_eq!(
            column_collations("CREATE TABLE t (a TEXT COLLATE crédit)").get("a"),
            Some(&"crédit".to_owned())
        );
    }

    #[test]
    fn parenthesized_extracts_balanced_body() {
        assert_eq!(
            parenthesized(" (a AND (b OR c))"),
            Some("a AND (b OR c)".to_owned())
        );
        // Not starting with a parenthesis.
        assert_eq!(parenthesized(" INTEGER"), None);
    }
}
