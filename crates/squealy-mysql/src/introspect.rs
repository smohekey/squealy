use std::collections::BTreeMap;

use mysql_async::{params, prelude::Queryable};
use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyAction,
    ForeignKeyMatch, ForeignKeyModel, GeneratedColumnModel, GeneratedStorage, IdentityMode,
    IdentityModel, IndexDirection, IndexMethod, IndexModel, SchemaModel, SourceRef, SqlType,
    TableModel, ViewColumnModel, ViewModel, ViewQueryModel,
};

use crate::MysqlError;

struct TableRef {
    schema: String,
    name: String,
}

pub(crate) async fn database(conn: &mut mysql_async::Conn) -> Result<DatabaseModel, MysqlError> {
    let mut schemas = Vec::<SchemaModel>::new();

    for table_ref in table_refs(conn).await? {
        let table = table(conn, &table_ref).await?;
        schema_entry(&mut schemas, &table_ref.schema)
            .tables
            .push(table);
    }

    // Views are introspected by name and output columns only: a stored view definition cannot be
    // reconstructed into the structural `ViewQueryModel`, so the body stays empty and the diff matches
    // views by name and columns (it cannot detect a pure body change against a live database). Their
    // view-on-view dependencies are read separately (see `view_dependencies`) so the diff can still
    // order live drops correctly.
    for view_ref in view_refs(conn).await? {
        let view = view(conn, &view_ref).await?;
        schema_entry(&mut schemas, &view_ref.schema)
            .views
            .push(view);
    }

    Ok(DatabaseModel { schemas })
}

/// Finds the schema named `name`, creating and appending it (preserving discovery order) if absent.
fn schema_entry<'a>(schemas: &'a mut Vec<SchemaModel>, name: &str) -> &'a mut SchemaModel {
    if let Some(index) = schemas
        .iter()
        .position(|schema| schema.name.as_deref() == Some(name))
    {
        return &mut schemas[index];
    }
    schemas.push(SchemaModel {
        name: Some(name.to_owned()),
        tables: Vec::new(),
        views: Vec::new(),
    });
    schemas.last_mut().expect("schema just pushed")
}

async fn view_refs(conn: &mut mysql_async::Conn) -> Result<Vec<TableRef>, MysqlError> {
    conn.query_map(
        "\
SELECT TABLE_SCHEMA, TABLE_NAME
FROM information_schema.TABLES
WHERE TABLE_TYPE = 'VIEW'
  AND TABLE_SCHEMA NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys', '__squealy')
ORDER BY TABLE_SCHEMA, TABLE_NAME",
        |(schema, name)| TableRef { schema, name },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn view(conn: &mut mysql_async::Conn, view_ref: &TableRef) -> Result<ViewModel, MysqlError> {
    Ok(ViewModel {
        name: view_ref.name.clone(),
        comment: None,
        columns: view_columns(conn, view_ref).await?,
        // The body can't be reconstructed, but the view-on-view dependencies can — they let the diff
        // order live drops (drop a dependent before the view it selects from).
        query: ViewQueryModel {
            dependencies: view_dependencies(conn, view_ref).await?,
            ..ViewQueryModel::default()
        },
    })
}

/// The other views this view depends on, from `information_schema.VIEW_TABLE_USAGE` (MySQL 8.0.13+).
/// Joined against `VIEWS` so only view-on-view edges are kept (table dependencies are irrelevant to
/// view ordering — every table is created before any view). Note `VIEW_TABLE_USAGE` can resolve a
/// reference through to base tables, so deeply nested view-on-view edges may be incomplete.
async fn view_dependencies(
    conn: &mut mysql_async::Conn,
    view_ref: &TableRef,
) -> Result<Vec<SourceRef>, MysqlError> {
    conn.exec_map(
        "\
SELECT u.TABLE_SCHEMA, u.TABLE_NAME
FROM information_schema.VIEW_TABLE_USAGE u
JOIN information_schema.VIEWS v
  ON v.TABLE_SCHEMA = u.TABLE_SCHEMA AND v.TABLE_NAME = u.TABLE_NAME
WHERE u.VIEW_SCHEMA = :schema
  AND u.VIEW_NAME = :view
  AND NOT (u.TABLE_SCHEMA = :schema AND u.TABLE_NAME = :view)
ORDER BY u.TABLE_SCHEMA, u.TABLE_NAME",
        params! {
            "schema" => &view_ref.schema,
            "view" => &view_ref.name,
        },
        |(schema, name): (String, String)| SourceRef {
            schema: Some(schema),
            // The alias is never rendered (an introspected view has no body to render); it only needs
            // to be present, so reuse the dependency name.
            alias: name.clone(),
            name,
        },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn view_columns(
    conn: &mut mysql_async::Conn,
    view_ref: &TableRef,
) -> Result<Vec<ViewColumnModel>, MysqlError> {
    conn.exec_map(
        "\
SELECT COLUMN_NAME, DATA_TYPE, COLUMN_TYPE, IS_NULLABLE
FROM information_schema.COLUMNS
WHERE TABLE_SCHEMA = :schema
  AND TABLE_NAME = :view
ORDER BY ORDINAL_POSITION",
        params! {
            "schema" => &view_ref.schema,
            "view" => &view_ref.name,
        },
        |(name, data_type, column_type, is_nullable): (String, String, String, String)| {
            ViewColumnModel {
                name,
                ty: sql_type(&data_type, &column_type),
                nullable: is_nullable.eq_ignore_ascii_case("YES"),
            }
        },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn table_refs(conn: &mut mysql_async::Conn) -> Result<Vec<TableRef>, MysqlError> {
    conn.query_map(
        "\
SELECT TABLE_SCHEMA, TABLE_NAME
FROM information_schema.TABLES
WHERE TABLE_TYPE = 'BASE TABLE'
  AND TABLE_SCHEMA NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys', '__squealy')
ORDER BY TABLE_SCHEMA, TABLE_NAME",
        |(schema, name)| TableRef { schema, name },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn table(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<TableModel, MysqlError> {
    let columns = columns(conn, table_ref).await?;
    let (primary_key, uniques) = key_constraints(conn, table_ref).await?;

    Ok(TableModel {
        name: table_ref.name.clone(),
        comment: table_comment(conn, table_ref).await?,
        columns,
        primary_key,
        foreign_keys: foreign_keys(conn, table_ref).await?,
        uniques,
        checks: checks(conn, table_ref).await?,
        indexes: indexes(conn, table_ref).await?,
    })
}

/// Raw `information_schema.COLUMNS` row shape returned by the column query in [`columns`].
type ColumnRow = (
    String,         // COLUMN_NAME
    String,         // DATA_TYPE
    String,         // COLUMN_TYPE
    String,         // IS_NULLABLE
    Option<String>, // COLUMN_DEFAULT
    String,         // EXTRA
    Option<String>, // GENERATION_EXPRESSION
    String,         // COLUMN_COMMENT
    Option<String>, // collation (NULL when it matches the table default)
);

async fn columns(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<Vec<ColumnModel>, MysqlError> {
    conn.exec_map(
        "\
SELECT
    COLUMN_NAME,
    DATA_TYPE,
    COLUMN_TYPE,
    IS_NULLABLE,
    COLUMN_DEFAULT,
    EXTRA,
    GENERATION_EXPRESSION,
    COLUMN_COMMENT,
    NULLIF(COLLATION_NAME, TABLE_COLLATION)
FROM information_schema.COLUMNS c
JOIN information_schema.TABLES t
  ON t.TABLE_SCHEMA = c.TABLE_SCHEMA
 AND t.TABLE_NAME = c.TABLE_NAME
WHERE c.TABLE_SCHEMA = :schema
  AND c.TABLE_NAME = :table
ORDER BY c.ORDINAL_POSITION",
        params! {
            "schema" => &table_ref.schema,
            "table" => &table_ref.name,
        },
        |(
            name,
            data_type,
            column_type,
            is_nullable,
            default,
            extra,
            generation_expression,
            comment,
            collation,
        ): ColumnRow| {
            let extra = extra.to_ascii_lowercase();
            let ty = sql_type(&data_type, &column_type);
            ColumnModel {
                name,
                comment: non_empty(comment),
                ty: ty.clone(),
                collation,
                nullable: is_nullable.eq_ignore_ascii_case("YES"),
                default: default.map(|value| default_value(&ty, &value)),
                identity: extra.contains("auto_increment").then_some(IdentityModel {
                    mode: IdentityMode::AutoIncrement,
                }),
                generated: generated_model(&extra, generation_expression),
            }
        },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn table_comment(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<Option<String>, MysqlError> {
    conn.exec_first(
        "\
SELECT TABLE_COMMENT
FROM information_schema.TABLES
WHERE TABLE_SCHEMA = :schema
  AND TABLE_NAME = :table",
        params! {
            "schema" => &table_ref.schema,
            "table" => &table_ref.name,
        },
    )
    .await
    .map(|comment: Option<String>| comment.and_then(non_empty))
    .map_err(MysqlError::Introspect)
}

fn non_empty(value: String) -> Option<String> {
    (!value.is_empty()).then_some(value)
}

async fn key_constraints(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<(Option<Constraint>, Vec<Constraint>), MysqlError> {
    let rows = conn
        .exec_map(
            "\
SELECT
    tc.CONSTRAINT_NAME,
    tc.CONSTRAINT_TYPE,
    kcu.COLUMN_NAME
FROM information_schema.TABLE_CONSTRAINTS tc
JOIN information_schema.KEY_COLUMN_USAGE kcu
  ON kcu.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA
 AND kcu.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
 AND kcu.TABLE_SCHEMA = tc.TABLE_SCHEMA
 AND kcu.TABLE_NAME = tc.TABLE_NAME
WHERE tc.TABLE_SCHEMA = :schema
  AND tc.TABLE_NAME = :table
  AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE')
ORDER BY tc.CONSTRAINT_TYPE, tc.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            params! {
                "schema" => &table_ref.schema,
                "table" => &table_ref.name,
            },
            |(name, kind, column): (String, String, String)| (name, kind, column),
        )
        .await
        .map_err(MysqlError::Introspect)?;

    let mut primary_key = None::<Constraint>;
    let mut uniques = BTreeMap::<String, Vec<String>>::new();
    for (name, kind, column) in rows {
        if kind == "PRIMARY KEY" {
            primary_key
                .get_or_insert_with(|| Constraint {
                    name,
                    columns: Vec::new(),
                })
                .columns
                .push(column);
        } else {
            uniques.entry(name).or_default().push(column);
        }
    }

    Ok((
        primary_key,
        uniques
            .into_iter()
            .map(|(name, columns)| Constraint { name, columns })
            .collect(),
    ))
}

async fn foreign_keys(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<Vec<ForeignKeyModel>, MysqlError> {
    let rows = conn
        .exec_map(
            "\
SELECT
    kcu.CONSTRAINT_NAME,
    kcu.COLUMN_NAME,
    kcu.REFERENCED_TABLE_SCHEMA,
    kcu.REFERENCED_TABLE_NAME,
    kcu.REFERENCED_COLUMN_NAME,
    rc.MATCH_OPTION,
    rc.DELETE_RULE,
    rc.UPDATE_RULE
FROM information_schema.KEY_COLUMN_USAGE kcu
JOIN information_schema.REFERENTIAL_CONSTRAINTS rc
  ON rc.CONSTRAINT_SCHEMA = kcu.CONSTRAINT_SCHEMA
 AND rc.CONSTRAINT_NAME = kcu.CONSTRAINT_NAME
WHERE kcu.TABLE_SCHEMA = :schema
  AND kcu.TABLE_NAME = :table
  AND kcu.REFERENCED_TABLE_NAME IS NOT NULL
ORDER BY kcu.CONSTRAINT_NAME, kcu.ORDINAL_POSITION",
            params! {
                "schema" => &table_ref.schema,
                "table" => &table_ref.name,
            },
            |(
                name,
                column,
                references_schema,
                references_table,
                references_column,
                match_option,
                delete_rule,
                update_rule,
            ): (
                String,
                String,
                String,
                String,
                String,
                String,
                String,
                String,
            )| {
                (
                    name,
                    column,
                    references_schema,
                    references_table,
                    references_column,
                    match_option,
                    delete_rule,
                    update_rule,
                )
            },
        )
        .await
        .map_err(MysqlError::Introspect)?;

    let mut grouped = BTreeMap::<String, ForeignKeyModel>::new();
    for (
        name,
        column,
        references_schema,
        references_table,
        references_column,
        match_option,
        delete_rule,
        update_rule,
    ) in rows
    {
        let foreign_key = grouped
            .entry(name.clone())
            .or_insert_with(|| ForeignKeyModel {
                name,
                columns: Vec::new(),
                references_schema: Some(references_schema),
                references_table,
                references_columns: Vec::new(),
                match_type: match_type(&match_option),
                deferrability: None,
                validation: None,
                enforcement: None,
                on_delete: action(&delete_rule),
                on_update: action(&update_rule),
            });
        foreign_key.columns.push(column);
        foreign_key.references_columns.push(references_column);
    }

    Ok(grouped.into_values().collect())
}

fn match_type(value: &str) -> Option<ForeignKeyMatch> {
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "none" | "simple" => None,
        other => Some(ForeignKeyMatch::from_sql(other)),
    }
}

async fn checks(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<Vec<CheckModel>, MysqlError> {
    conn.exec_map(
        "\
SELECT cc.CONSTRAINT_NAME, cc.CHECK_CLAUSE
FROM information_schema.TABLE_CONSTRAINTS tc
JOIN information_schema.CHECK_CONSTRAINTS cc
  ON cc.CONSTRAINT_SCHEMA = tc.CONSTRAINT_SCHEMA
 AND cc.CONSTRAINT_NAME = tc.CONSTRAINT_NAME
WHERE tc.TABLE_SCHEMA = :schema
  AND tc.TABLE_NAME = :table
  AND tc.CONSTRAINT_TYPE = 'CHECK'
ORDER BY cc.CONSTRAINT_NAME",
        params! {
            "schema" => &table_ref.schema,
            "table" => &table_ref.name,
        },
        |(name, expression): (String, String)| CheckModel {
            name,
            expression: squealy_parse::Reader::new(squealy_parse::SqlDialect::Mysql)
                .read_check_expression_or_raw(&expression),
            validation: None,
            enforcement: None,
        },
    )
    .await
    .map_err(MysqlError::Introspect)
}

async fn indexes(
    conn: &mut mysql_async::Conn,
    table_ref: &TableRef,
) -> Result<Vec<IndexModel>, MysqlError> {
    // `COLUMN_NAME` is NULL for a functional/expression key part (MySQL 8.0.13+); it must be decoded as
    // `Option<String>`, as decoding it as a plain `String` fails row decoding on any table carrying an
    // expression index and aborts the whole introspection. squealy does not model expression indexes yet
    // (the renderer rejects them), so an index with any functional key part is skipped entirely — leaving
    // it alone rather than crashing or misrepresenting it. Full expression-index support (ordered key
    // parts + rendering) is deferred to the round-trip epic.
    let rows = conn
        .exec_map(
            "\
SELECT INDEX_NAME, NON_UNIQUE, INDEX_TYPE, COLUMN_NAME, COLLATION
FROM information_schema.STATISTICS s
LEFT JOIN information_schema.TABLE_CONSTRAINTS tc
  ON tc.TABLE_SCHEMA = s.TABLE_SCHEMA
 AND tc.TABLE_NAME = s.TABLE_NAME
 AND tc.CONSTRAINT_NAME = s.INDEX_NAME
 AND tc.CONSTRAINT_TYPE IN ('PRIMARY KEY', 'UNIQUE')
WHERE s.TABLE_SCHEMA = :schema
  AND s.TABLE_NAME = :table
  AND s.INDEX_NAME <> 'PRIMARY'
  AND tc.CONSTRAINT_NAME IS NULL
ORDER BY INDEX_NAME, SEQ_IN_INDEX",
            params! {
                "schema" => &table_ref.schema,
                "table" => &table_ref.name,
            },
            |(name, non_unique, index_type, column, collation): (
                String,
                u8,
                String,
                Option<String>,
                Option<String>,
            )| { (name, non_unique, index_type, column, collation) },
        )
        .await
        .map_err(MysqlError::Introspect)?;

    let mut grouped = BTreeMap::<String, IndexModel>::new();
    let mut has_expression = std::collections::BTreeSet::<String>::new();
    for (name, non_unique, index_type, column, collation) in rows {
        // A functional key part has a NULL `COLUMN_NAME`; mark the whole index for removal.
        let Some(column) = column else {
            has_expression.insert(name);
            continue;
        };
        let index = grouped.entry(name.clone()).or_insert_with(|| IndexModel {
            name,
            columns: Vec::new(),
            expressions: Vec::new(),
            include_columns: Vec::new(),
            unique: non_unique == 0,
            method: Some(IndexMethod::from_sql(&index_type)),
            directions: Vec::new(),
            nulls: Vec::new(),
            collations: Vec::new(),
            operator_classes: Vec::new(),
            predicate: None,
        });
        index.columns.push(column);
        index.directions.push(index_direction(collation.as_deref()));
    }
    grouped.retain(|name, _| !has_expression.contains(name));

    Ok(grouped.into_values().collect())
}

fn sql_type(data_type: &str, column_type: &str) -> SqlType {
    let data_type = data_type.to_ascii_lowercase();
    // A lowercased copy is used only for structural matching (the `match` arms, `unsigned`, the
    // `single_arg_type`/`decimal_type`/`temporal_precision` parsers); a `Raw` payload keeps the original
    // `column_type` (via `raw_column_type`) so `ENUM`/`SET` member labels are not case-folded.
    let lower = column_type.to_ascii_lowercase();
    let unsigned = lower.contains(" unsigned");
    let raw = || SqlType::Raw(raw_column_type(column_type));

    match data_type.as_str() {
        "tinyint" if lower == "tinyint(1)" => SqlType::Bool,
        "tinyint" if unsigned => SqlType::U8,
        "tinyint" => SqlType::I8,
        "smallint" if unsigned => SqlType::U16,
        "smallint" => SqlType::I16,
        "int" | "integer" if unsigned => SqlType::U32,
        "int" | "integer" => SqlType::I32,
        "bigint" if unsigned => SqlType::U64,
        "bigint" => SqlType::I64,
        "float" => SqlType::F32,
        "double" => SqlType::F64,
        "varchar" => single_arg_type(&lower, "varchar").map_or_else(raw, SqlType::Varchar),
        "char" => single_arg_type(&lower, "char").map_or_else(raw, SqlType::Char),
        "text" => SqlType::Text,
        "decimal" => decimal_type(&lower).unwrap_or_else(raw),
        "date" => SqlType::Date,
        "time" => SqlType::Time {
            tz: false,
            precision: temporal_precision(&lower, "time"),
        },
        "datetime" => SqlType::Timestamp {
            tz: false,
            precision: temporal_precision(&lower, "datetime"),
        },
        "timestamp" => SqlType::Timestamp {
            tz: true,
            precision: temporal_precision(&lower, "timestamp"),
        },
        "json" => SqlType::Json,
        "blob" => SqlType::Bytes,
        // Fixed-width binary `BINARY(N)` -> `[u8; N]` (the width is in the type, so it round-trips
        // directly; variable-length `varbinary` is left as a raw type).
        "binary" => single_arg_type(&lower, "binary").map_or_else(raw, SqlType::FixedBytes),
        _ => raw(),
    }
}

/// A MySQL `COLUMN_TYPE` normalized to a neutral `Raw` type name: keywords are upper-cased, but the
/// contents of single-quoted string literals are preserved exactly. `ENUM`/`SET` member labels live in
/// those literals, and upper-casing them would change the column's set of allowed values — the corruption
/// this guards against. A doubled `''` is an escaped quote that stays inside the literal.
fn raw_column_type(column_type: &str) -> String {
    let mut out = String::with_capacity(column_type.len());
    let mut in_literal = false;
    let mut chars = column_type.chars().peekable();
    while let Some(character) = chars.next() {
        match character {
            '\'' if in_literal && chars.peek() == Some(&'\'') => {
                out.push('\'');
                out.push(chars.next().expect("peeked a quote"));
            }
            '\'' => {
                in_literal = !in_literal;
                out.push('\'');
            }
            _ if in_literal => out.push(character),
            _ => out.extend(character.to_uppercase()),
        }
    }
    out
}

fn generated_model(extra: &str, expression: Option<String>) -> Option<GeneratedColumnModel> {
    let expression = expression.filter(|expression| !expression.trim().is_empty())?;
    let storage = if extra.contains("stored generated") {
        GeneratedStorage::Stored
    } else if extra.contains("virtual generated") {
        GeneratedStorage::Virtual
    } else {
        GeneratedStorage::Unknown
    };
    Some(GeneratedColumnModel {
        // Structure the `GENERATION_EXPRESSION` deparse into a comparable node, falling back to a
        // verbatim `Raw` for an expression the reverse parser cannot yet lower.
        expression: Some(
            squealy_parse::Reader::new(squealy_parse::SqlDialect::Mysql)
                .read_generated_expression_or_raw(&expression),
        ),
        storage,
    })
}

fn single_arg_type(column_type: &str, kind: &str) -> Option<u32> {
    let args = type_args(column_type, kind)?;
    args.trim().parse().ok()
}

/// The fractional-seconds precision of a `time`/`timestamp`/`datetime` column, read from its
/// `COLUMN_TYPE` (e.g. `timestamp(6)`). A bare form has no parenthesized argument and is fsp 0. Always
/// returns `Some` — a MySQL temporal column always has a definite precision — so an introspected column
/// carries an explicit precision the desired side is canonicalized to match (see
/// [`Sqlite`-style `canonical_sql_type`](crate::Mysql::canonical_sql_type)).
fn temporal_precision(column_type: &str, kind: &str) -> Option<u8> {
    Some(
        type_args(column_type, kind)
            .and_then(|args| args.trim().parse().ok())
            .unwrap_or(0),
    )
}

fn decimal_type(column_type: &str) -> Option<SqlType> {
    let args = type_args(column_type, "decimal")?;
    let parts = args.split(',').map(str::trim).collect::<Vec<_>>();
    let [precision, scale] = parts[..] else {
        return None;
    };
    Some(SqlType::Decimal {
        precision: precision.parse().ok()?,
        scale: scale.parse().ok()?,
    })
}

fn type_args<'a>(column_type: &'a str, kind: &str) -> Option<&'a str> {
    let open = column_type.find('(')?;
    let close = column_type.rfind(')')?;
    if close + 1 != column_type.len() || column_type[..open].trim() != kind {
        return None;
    }
    Some(&column_type[open + 1..close])
}

/// Whether a lowercased default expression is the current-time function `name`, in any of the forms
/// MySQL reports: bare (`name`), empty-call (`name()`), or with a fractional-seconds precision
/// (`name(6)`, as reported for a `TIMESTAMP(6) DEFAULT CURRENT_TIMESTAMP(6)` column).
fn is_current_function(value: &str, name: &str) -> bool {
    let Some(rest) = value.strip_prefix(name) else {
        return false;
    };
    rest.is_empty()
        || rest == "()"
        || (rest.starts_with('(')
            && rest.ends_with(')')
            && rest[1..rest.len() - 1].trim().parse::<u8>().is_ok())
}

fn default_value(ty: &SqlType, value: &str) -> DefaultValue {
    let trimmed = value.trim();
    let lower = trimmed.to_ascii_lowercase();
    if lower == "null" {
        return DefaultValue::Null;
    }
    if is_current_function(&lower, "current_timestamp") {
        return DefaultValue::CurrentTimestamp;
    }
    if is_current_function(&lower, "current_date") {
        return DefaultValue::CurrentDate;
    }
    if is_current_function(&lower, "current_time") {
        return DefaultValue::CurrentTime;
    }

    match ty {
        SqlType::Bool => match trimmed.to_ascii_lowercase().as_str() {
            "1" | "true" => DefaultValue::Bool(true),
            "0" | "false" => DefaultValue::Bool(false),
            _ => DefaultValue::Raw(value.to_owned()),
        },
        SqlType::I8
        | SqlType::I16
        | SqlType::I32
        | SqlType::I64
        | SqlType::I128
        | SqlType::Isize => trimmed
            .parse()
            .map(DefaultValue::Int)
            .unwrap_or_else(|_| DefaultValue::Raw(value.to_owned())),
        SqlType::U8
        | SqlType::U16
        | SqlType::U32
        | SqlType::U64
        | SqlType::U128
        | SqlType::Usize => trimmed
            .parse()
            .map(DefaultValue::UInt)
            .unwrap_or_else(|_| DefaultValue::Raw(value.to_owned())),
        SqlType::F32 | SqlType::F64 => trimmed
            .parse()
            .map(DefaultValue::Float)
            .unwrap_or_else(|_| DefaultValue::Raw(value.to_owned())),
        SqlType::String | SqlType::Varchar(_) | SqlType::Char(_) | SqlType::Text => {
            DefaultValue::Text(value.to_owned())
        }
        _ => DefaultValue::Raw(value.to_owned()),
    }
}

fn action(action: &str) -> Option<ForeignKeyAction> {
    match action {
        "CASCADE" => Some(ForeignKeyAction::Cascade),
        "RESTRICT" => Some(ForeignKeyAction::Restrict),
        "SET NULL" => Some(ForeignKeyAction::SetNull),
        "SET DEFAULT" => Some(ForeignKeyAction::SetDefault),
        "NO ACTION" => None,
        _ => None,
    }
}

fn index_direction(collation: Option<&str>) -> IndexDirection {
    match collation {
        Some("D") => IndexDirection::Desc,
        _ => IndexDirection::Asc,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_mysql_scalar_types_to_neutral_types() {
        assert_eq!(sql_type("tinyint", "tinyint(1)"), SqlType::Bool);
        assert_eq!(sql_type("tinyint", "tinyint"), SqlType::I8);
        assert_eq!(sql_type("tinyint", "tinyint unsigned"), SqlType::U8);
        assert_eq!(sql_type("smallint", "smallint"), SqlType::I16);
        assert_eq!(sql_type("smallint", "smallint unsigned"), SqlType::U16);
        assert_eq!(sql_type("int", "int"), SqlType::I32);
        assert_eq!(sql_type("int", "int unsigned"), SqlType::U32);
        assert_eq!(sql_type("bigint", "bigint"), SqlType::I64);
        assert_eq!(sql_type("bigint", "bigint unsigned"), SqlType::U64);
        assert_eq!(sql_type("float", "float"), SqlType::F32);
        assert_eq!(sql_type("double", "double"), SqlType::F64);
        assert_eq!(sql_type("text", "text"), SqlType::Text);
        assert_eq!(sql_type("date", "date"), SqlType::Date);
        // A bare temporal column is fsp 0; `time(n)`/`timestamp(n)` recover the precision.
        assert_eq!(
            sql_type("time", "time"),
            SqlType::Time {
                tz: false,
                precision: Some(0)
            }
        );
        assert_eq!(
            sql_type("datetime", "datetime(3)"),
            SqlType::Timestamp {
                tz: false,
                precision: Some(3)
            }
        );
        assert_eq!(
            sql_type("timestamp", "timestamp(6)"),
            SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }
        );
        assert_eq!(
            sql_type("timestamp", "timestamp"),
            SqlType::Timestamp {
                tz: true,
                precision: Some(0)
            }
        );
        assert_eq!(sql_type("json", "json"), SqlType::Json);
        assert_eq!(sql_type("blob", "blob"), SqlType::Bytes);
        assert_eq!(
            sql_type("geometry", "geometry"),
            SqlType::Raw("GEOMETRY".to_owned())
        );
    }

    #[test]
    fn maps_mysql_parametric_types_to_neutral_types() {
        assert_eq!(sql_type("varchar", "varchar(64)"), SqlType::Varchar(64));
        assert_eq!(sql_type("char", "char(2)"), SqlType::Char(2));
        assert_eq!(
            sql_type("decimal", "decimal(10,2)"),
            SqlType::Decimal {
                precision: 10,
                scale: 2
            }
        );
        assert_eq!(
            sql_type("decimal", "decimal(10,2) unsigned"),
            SqlType::Raw("DECIMAL(10,2) UNSIGNED".to_owned())
        );
        // Fixed-width binary `BINARY(N)` round-trips to `FixedBytes(N)`; variable `varbinary` does not.
        assert_eq!(sql_type("binary", "binary(32)"), SqlType::FixedBytes(32));
        assert_eq!(
            sql_type("varbinary", "varbinary(32)"),
            SqlType::Raw("VARBINARY(32)".to_owned())
        );
    }

    #[test]
    fn enum_and_set_labels_keep_their_case_in_raw() {
        // The type keyword is upper-cased, but the quoted member labels must NOT be — upper-casing them
        // would change the column's allowed values.
        assert_eq!(
            sql_type("enum", "enum('Active','InActive')"),
            SqlType::Raw("ENUM('Active','InActive')".to_owned())
        );
        assert_eq!(
            sql_type("set", "set('Read','Write')"),
            SqlType::Raw("SET('Read','Write')".to_owned())
        );
        // A doubled '' escape inside a label is preserved verbatim.
        assert_eq!(
            sql_type("enum", "enum('O''Brien')"),
            SqlType::Raw("ENUM('O''Brien')".to_owned())
        );
    }

    #[test]
    fn maps_foreign_key_actions() {
        assert_eq!(action("NO ACTION"), None);
        assert_eq!(action("CASCADE"), Some(ForeignKeyAction::Cascade));
        assert_eq!(action("RESTRICT"), Some(ForeignKeyAction::Restrict));
        assert_eq!(action("SET NULL"), Some(ForeignKeyAction::SetNull));
        assert_eq!(action("SET DEFAULT"), Some(ForeignKeyAction::SetDefault));
    }

    #[test]
    fn maps_mysql_defaults_to_neutral_values() {
        assert_eq!(
            default_value(&SqlType::Varchar(64), "MB"),
            DefaultValue::Text("MB".to_owned())
        );
        assert_eq!(default_value(&SqlType::Bool, "1"), DefaultValue::Bool(true));
        assert_eq!(default_value(&SqlType::U32, "42"), DefaultValue::UInt(42));
        assert_eq!(
            default_value(
                &SqlType::Timestamp {
                    tz: true,
                    precision: Some(6)
                },
                "current_timestamp()"
            ),
            DefaultValue::CurrentTimestamp
        );
        // A precise column reports its default with the fsp suffix (`CURRENT_TIMESTAMP(6)`); recognize
        // it so a `DEFAULT CURRENT_TIMESTAMP(6)` column re-plans empty rather than churning.
        assert_eq!(
            default_value(
                &SqlType::Timestamp {
                    tz: true,
                    precision: Some(6)
                },
                "current_timestamp(6)"
            ),
            DefaultValue::CurrentTimestamp
        );
        assert_eq!(
            default_value(
                &SqlType::Decimal {
                    precision: 10,
                    scale: 2
                },
                "42.00"
            ),
            DefaultValue::Raw("42.00".to_owned())
        );
    }

    #[test]
    fn generated_expression_lowers_to_structural_node() {
        // MySQL's `GENERATION_EXPRESSION` deparse (backtick-quoted) lowers to the same structural node an
        // authored one produces, so a published generated column re-plans to empty instead of churning
        // against the introspected string. The `extra` column reports the storage kind.
        let expected = Some(GeneratedColumnModel {
            expression: Some(squealy::ExprNode::Binary {
                op: squealy::ArithmeticOp::Add,
                left: Box::new(squealy::ExprNode::BareColumn {
                    column: "base".to_owned(),
                }),
                right: Box::new(squealy::ExprNode::Literal("1".to_owned())),
            }),
            storage: GeneratedStorage::Stored,
        });
        assert_eq!(
            generated_model("stored generated", Some("(`base` + 1)".to_owned())),
            expected
        );

        // The storage kind comes from `extra`; an unrecognized one falls back to `Unknown`.
        assert!(matches!(
            generated_model("virtual generated", Some("(`base` + 1)".to_owned())),
            Some(GeneratedColumnModel {
                storage: GeneratedStorage::Virtual,
                ..
            })
        ));

        // MySQL keys "is generated" on a non-empty generation expression, so a column with none (or a
        // blank one) is not a generated column.
        assert_eq!(generated_model("virtual generated", None), None);
        assert_eq!(
            generated_model("stored generated", Some("   ".to_owned())),
            None
        );
    }

    #[test]
    fn general_function_check_lowers_to_structural_node() {
        // A general (user/built-in) function outside the closed scalar set lowers to a structural
        // `Function` node instead of falling to `Raw`, so a published general-function check re-plans to
        // empty. MySQL's `CHECK_CLAUSE` backtick-quotes column references.
        let expected = squealy::ExprNode::Function {
            name: "json_valid".to_owned(),
            args: vec![squealy::ExprNode::BareColumn {
                column: "data".to_owned(),
            }],
        };
        assert_eq!(
            squealy_parse::Reader::new(squealy_parse::SqlDialect::Mysql)
                .read_check_expression_or_raw("json_valid(`data`)"),
            expected
        );
    }
}
