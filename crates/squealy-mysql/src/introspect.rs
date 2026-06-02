use std::collections::BTreeMap;

use mysql_async::{params, prelude::Queryable};
use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyAction,
    ForeignKeyMatch, ForeignKeyModel, GeneratedColumnModel, GeneratedStorage, IdentityMode,
    IdentityModel, IndexDirection, IndexMethod, IndexModel, SchemaModel, SqlType, TableModel,
};

use crate::MysqlError;

struct TableRef {
    schema: String,
    name: String,
}

pub(crate) async fn database(conn: &mut mysql_async::Conn) -> Result<DatabaseModel, MysqlError> {
    let table_refs = table_refs(conn).await?;
    let mut schemas = Vec::<SchemaModel>::new();

    for table_ref in table_refs {
        if schemas
            .last()
            .is_none_or(|schema| schema.name.as_deref() != Some(table_ref.schema.as_str()))
        {
            schemas.push(SchemaModel {
                name: Some(table_ref.schema.clone()),
                tables: Vec::new(),
            });
        }

        let table = table(conn, &table_ref).await?;
        schemas
            .last_mut()
            .expect("schema just pushed")
            .tables
            .push(table);
    }

    Ok(DatabaseModel { schemas })
}

async fn table_refs(conn: &mut mysql_async::Conn) -> Result<Vec<TableRef>, MysqlError> {
    conn.query_map(
        "\
SELECT TABLE_SCHEMA, TABLE_NAME
FROM information_schema.TABLES
WHERE TABLE_TYPE = 'BASE TABLE'
  AND TABLE_SCHEMA NOT IN ('information_schema', 'mysql', 'performance_schema', 'sys')
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
        ): (
            String,
            String,
            String,
            String,
            Option<String>,
            String,
            Option<String>,
            String,
            Option<String>,
        )| {
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
            expression,
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
                String,
                Option<String>,
            )| { (name, non_unique, index_type, column, collation) },
        )
        .await
        .map_err(MysqlError::Introspect)?;

    let mut grouped = BTreeMap::<String, IndexModel>::new();
    for (name, non_unique, index_type, column, collation) in rows {
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

    Ok(grouped.into_values().collect())
}

fn sql_type(data_type: &str, column_type: &str) -> SqlType {
    let data_type = data_type.to_ascii_lowercase();
    let column_type = column_type.to_ascii_lowercase();
    let unsigned = column_type.contains(" unsigned");

    match data_type.as_str() {
        "tinyint" if column_type == "tinyint(1)" => SqlType::Bool,
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
        "varchar" => single_arg_type(&column_type, "varchar").map_or_else(
            || SqlType::Raw(column_type.to_uppercase()),
            SqlType::Varchar,
        ),
        "char" => single_arg_type(&column_type, "char")
            .map_or_else(|| SqlType::Raw(column_type.to_uppercase()), SqlType::Char),
        "text" => SqlType::Text,
        "decimal" => {
            decimal_type(&column_type).unwrap_or_else(|| SqlType::Raw(column_type.to_uppercase()))
        }
        "date" => SqlType::Date,
        "time" => SqlType::Time { tz: false },
        "datetime" => SqlType::Timestamp { tz: false },
        "timestamp" => SqlType::Timestamp { tz: true },
        "json" => SqlType::Json,
        "blob" => SqlType::Bytes,
        _ => SqlType::Raw(column_type.to_uppercase()),
    }
}

fn generated_model(extra: &str, expression: Option<String>) -> Option<GeneratedColumnModel> {
    let expression = expression.filter(|expression| !expression.is_empty())?;
    let storage = if extra.contains("stored generated") {
        GeneratedStorage::Stored
    } else if extra.contains("virtual generated") {
        GeneratedStorage::Virtual
    } else {
        GeneratedStorage::Unknown
    };
    Some(GeneratedColumnModel {
        expression,
        storage,
    })
}

fn single_arg_type(column_type: &str, kind: &str) -> Option<u32> {
    let args = type_args(column_type, kind)?;
    args.trim().parse().ok()
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

fn default_value(ty: &SqlType, value: &str) -> DefaultValue {
    let trimmed = value.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "null" => return DefaultValue::Null,
        "current_timestamp" | "current_timestamp()" => return DefaultValue::CurrentTimestamp,
        "current_date" | "current_date()" => return DefaultValue::CurrentDate,
        "current_time" | "current_time()" => return DefaultValue::CurrentTime,
        _ => {}
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
        assert_eq!(sql_type("time", "time"), SqlType::Time { tz: false });
        assert_eq!(
            sql_type("datetime", "datetime"),
            SqlType::Timestamp { tz: false }
        );
        assert_eq!(
            sql_type("timestamp", "timestamp"),
            SqlType::Timestamp { tz: true }
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
            default_value(&SqlType::Timestamp { tz: true }, "current_timestamp()"),
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
}
