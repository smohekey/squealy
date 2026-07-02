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
//! - **`CHECK` constraints, column comments, collations and generated columns are not read** — SQLite
//!   exposes checks only inside the `CREATE TABLE` text (no PRAGMA), and has no column comments; these
//!   are left empty (a documented limitation, revisited when the incremental plan lands).

use std::collections::BTreeMap;

use squealy::{
    ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyAction, ForeignKeyModel,
    IdentityMode, IdentityModel, IndexDirection, IndexModel, SchemaModel, SqlType, TableModel,
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
    Ok(DatabaseModel {
        schemas: vec![SchemaModel {
            name: None,
            tables,
            // Views are not introspected in this slice (SQLite view rendering is itself deferred).
            views: Vec::new(),
        }],
    })
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

async fn table(connection: &SqliteConnection, name: &str) -> Result<TableModel, SqliteError> {
    let column_rows = columns(connection, name).await?;
    let create_sql = table_sql(connection, name).await?;

    let primary_key = primary_key(&column_rows);
    let autoincrement = primary_key
        .as_ref()
        .is_some_and(|pk| pk.columns.len() == 1 && declares_autoincrement(create_sql.as_deref()));

    let mut columns = Vec::with_capacity(column_rows.len());
    for column in &column_rows {
        let is_sole_pk = autoincrement
            && primary_key
                .as_ref()
                .is_some_and(|pk| pk.columns[0] == column.name);
        columns.push(ColumnModel {
            name: column.name.clone(),
            comment: None,
            ty: sql_type(&column.declared_type),
            collation: None,
            // A column in the primary key is never nullable, even though `PRAGMA table_info` reports
            // `notnull = 0` for the `INTEGER PRIMARY KEY` rowid alias.
            nullable: column.notnull == 0 && column.pk == 0,
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
        // SQLite exposes `CHECK` constraints only in the `CREATE TABLE` text; reading them needs a SQL
        // parser, so they are left empty for now (revisited with the incremental plan).
        checks: Vec::new(),
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

/// Whether the stored `CREATE TABLE` text declares `AUTOINCREMENT`. SQLite stores the statement as
/// written, so the keyword is present iff this backend rendered an identity column; a case-insensitive
/// substring match is enough (the keyword is a reserved word that cannot appear inside an identifier or
/// a bare literal in the rendered DDL).
fn declares_autoincrement(sql: Option<&str>) -> bool {
    sql.is_some_and(|sql| sql.to_ascii_uppercase().contains("AUTOINCREMENT"))
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
                references_column: row.get(3)?,
                on_update: row.get(4)?,
                on_delete: row.get(5)?,
            })
        },
    )
    .await?;

    // Preserve first-seen `id` order while grouping (a `BTreeMap<i64, _>` keyed by `id` keeps it).
    let mut grouped = BTreeMap::<i64, ForeignKeyModel>::new();
    for row in rows {
        let foreign_key = grouped.entry(row.id).or_insert_with(|| ForeignKeyModel {
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
        });
        foreign_key.columns.push(row.column);
        foreign_key.references_columns.push(row.references_column);
    }
    Ok(grouped.into_values().collect())
}

struct ForeignKeyRow {
    id: i64,
    references_table: String,
    column: String,
    references_column: String,
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
            _ => indexes.push(IndexModel {
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
                // A partial index's `WHERE` predicate lives only in the `CREATE INDEX` text (no PRAGMA
                // reports it); reading it needs a SQL parser, so it is left unset for now.
                predicate: index.partial.then(String::new),
            }),
        }
    }
    Ok((uniques, indexes))
}

struct IndexListRow {
    name: String,
    unique: bool,
    origin: String,
    partial: bool,
}

/// The key columns of an index and their sort directions, via `PRAGMA index_xinfo`. Only true key
/// terms are kept (`key = 1` drops the trailing rowid SQLite appends), in `seqno` order. Directions are
/// reported only when at least one column is `DESC`; an all-ascending index (the common case, and what
/// the renderer produces) reports empty directions to match a crate-declared index.
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
    if directions.iter().all(|d| *d == IndexDirection::Asc) {
        directions.clear();
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
    fn foreign_key_actions_map_no_action_to_none() {
        assert_eq!(action("NO ACTION"), None);
        assert_eq!(action(""), None);
        assert_eq!(action("CASCADE"), Some(ForeignKeyAction::Cascade));
        assert_eq!(action("SET NULL"), Some(ForeignKeyAction::SetNull));
    }
}
