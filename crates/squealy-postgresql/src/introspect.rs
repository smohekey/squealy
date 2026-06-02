use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyAction,
    ForeignKeyModel, GeneratedColumnModel, GeneratedStorage, IdentityMode, IdentityModel,
    IndexDirection, IndexMethod, IndexModel, SchemaModel, SqlType, TableModel,
};
use tokio_postgres::Client;

use crate::PostgresError;

struct TableRef {
    schema: String,
    name: String,
}

pub(crate) async fn database(client: &Client) -> Result<DatabaseModel, PostgresError> {
    let table_refs = table_refs(client).await?;
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

        let table = table(client, &table_ref).await?;
        schemas
            .last_mut()
            .expect("schema just pushed")
            .tables
            .push(table);
    }

    Ok(DatabaseModel { schemas })
}

async fn table_refs(client: &Client) -> Result<Vec<TableRef>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT n.nspname, c.relname
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema')
  AND n.nspname NOT LIKE 'pg_toast%'
ORDER BY n.nspname, c.relname",
            &[],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| TableRef {
            schema: row.get(0),
            name: row.get(1),
        })
        .collect())
}

async fn table(client: &Client, table_ref: &TableRef) -> Result<TableModel, PostgresError> {
    let columns = columns(client, table_ref).await?;
    let (primary_key, uniques) = key_constraints(client, table_ref).await?;

    Ok(TableModel {
        name: table_ref.name.clone(),
        columns,
        primary_key,
        foreign_keys: foreign_keys(client, table_ref).await?,
        uniques,
        checks: checks(client, table_ref).await?,
        indexes: indexes(client, table_ref).await?,
    })
}

async fn columns(client: &Client, table_ref: &TableRef) -> Result<Vec<ColumnModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT
    a.attname,
    format_type(a.atttypid, a.atttypmod),
    a.attnotnull,
    a.attidentity::text,
    a.attgenerated::text,
    pg_get_expr(ad.adbin, ad.adrelid)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid
LEFT JOIN pg_attrdef ad ON ad.adrelid = c.oid AND ad.adnum = a.attnum
WHERE n.nspname = $1
  AND c.relname = $2
  AND c.relkind IN ('r', 'p')
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let db_type: String = row.get(1);
            let ty = sql_type(&db_type);
            let identity: String = row.get(3);
            let generated: String = row.get(4);
            let default: Option<String> = row.get(5);
            ColumnModel {
                name: row.get(0),
                ty: ty.clone(),
                nullable: !row.get::<_, bool>(2),
                default: (generated != "s")
                    .then(|| default.clone())
                    .flatten()
                    .map(|value| default_value(&ty, &value)),
                identity: identity_model(&identity),
                generated: generated_model(&generated, default),
            }
        })
        .collect())
}

async fn key_constraints(
    client: &Client,
    table_ref: &TableRef,
) -> Result<(Option<Constraint>, Vec<Constraint>), PostgresError> {
    let rows = client
        .query(
            "\
SELECT
    con.conname,
    con.contype::text,
    ARRAY(
        SELECT a.attname::text
        FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = key.attnum
        ORDER BY key.position
    ) AS columns
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND con.contype IN ('p', 'u')
ORDER BY con.contype, con.conname",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    let mut primary_key = None;
    let mut uniques = Vec::new();
    for row in rows {
        let constraint = Constraint {
            name: row.get(0),
            columns: row.get(2),
        };
        match row.get::<_, String>(1).as_str() {
            "p" => primary_key = Some(constraint),
            "u" => uniques.push(constraint),
            _ => {}
        }
    }

    Ok((primary_key, uniques))
}

async fn foreign_keys(
    client: &Client,
    table_ref: &TableRef,
) -> Result<Vec<ForeignKeyModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT
    con.conname,
    ARRAY(
        SELECT a.attname::text
        FROM unnest(con.conkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = con.conrelid AND a.attnum = key.attnum
        ORDER BY key.position
    ) AS columns,
    rn.nspname,
    rc.relname,
    ARRAY(
        SELECT a.attname::text
        FROM unnest(con.confkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = con.confrelid AND a.attnum = key.attnum
        ORDER BY key.position
    ) AS references_columns,
    con.confdeltype::text,
    con.confupdtype::text
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class rc ON rc.oid = con.confrelid
JOIN pg_namespace rn ON rn.oid = rc.relnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND con.contype = 'f'
ORDER BY con.conname",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let references_schema: String = row.get(2);
            ForeignKeyModel {
                name: row.get(0),
                columns: row.get(1),
                references_schema: Some(references_schema),
                references_table: row.get(3),
                references_columns: row.get(4),
                on_delete: action(row.get::<_, String>(5).as_str()),
                on_update: action(row.get::<_, String>(6).as_str()),
            }
        })
        .collect())
}

async fn checks(client: &Client, table_ref: &TableRef) -> Result<Vec<CheckModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT con.conname, pg_get_constraintdef(con.oid)
FROM pg_constraint con
JOIN pg_class c ON c.oid = con.conrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND con.contype = 'c'
ORDER BY con.conname",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| CheckModel {
            name: row.get(0),
            expression: check_expression(&row.get::<_, String>(1)),
        })
        .collect())
}

async fn indexes(client: &Client, table_ref: &TableRef) -> Result<Vec<IndexModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT
    idx.relname,
    i.indisunique,
    am.amname,
    ARRAY(
        SELECT a.attname::text
        FROM unnest(i.indkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = key.attnum
        ORDER BY key.position
    ) AS columns,
    ARRAY(
        SELECT CASE WHEN (option & 1) = 1 THEN 'DESC' ELSE 'ASC' END
        FROM unnest(i.indoption) WITH ORDINALITY AS opt(option, position)
        ORDER BY position
    ) AS directions,
    pg_get_expr(i.indpred, i.indrelid) AS predicate,
    pg_get_expr(i.indexprs, i.indrelid) AS expressions
FROM pg_index i
JOIN pg_class c ON c.oid = i.indrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class idx ON idx.oid = i.indexrelid
JOIN pg_am am ON am.oid = idx.relam
LEFT JOIN pg_constraint con ON con.conindid = i.indexrelid
WHERE n.nspname = $1
  AND c.relname = $2
  AND con.oid IS NULL
ORDER BY idx.relname",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| IndexModel {
            name: row.get(0),
            unique: row.get(1),
            method: Some(IndexMethod::from_sql(&row.get::<_, String>(2))),
            columns: row.get(3),
            expressions: row.get::<_, Option<String>>(6).into_iter().collect(),
            directions: row
                .get::<_, Vec<String>>(4)
                .into_iter()
                .map(|direction| index_direction(&direction))
                .collect(),
            predicate: row.get(5),
        })
        .collect())
}

fn sql_type(db_type: &str) -> SqlType {
    match db_type {
        "boolean" => SqlType::Bool,
        "smallint" => SqlType::I16,
        "integer" => SqlType::I32,
        "bigint" => SqlType::I64,
        "real" => SqlType::F32,
        "double precision" => SqlType::F64,
        "text" => SqlType::String,
        "date" => SqlType::Date,
        "time without time zone" => SqlType::Time { tz: false },
        "time with time zone" => SqlType::Time { tz: true },
        "timestamp without time zone" => SqlType::Timestamp { tz: false },
        "timestamp with time zone" => SqlType::Timestamp { tz: true },
        "uuid" => SqlType::Uuid,
        "json" => SqlType::Json,
        "jsonb" => SqlType::Jsonb,
        "bytea" => SqlType::Bytes,
        other => parametric_sql_type(other).unwrap_or_else(|| SqlType::Raw(other.to_owned())),
    }
}

fn identity_model(identity: &str) -> Option<IdentityModel> {
    let mode = match identity {
        "a" => IdentityMode::Always,
        "d" => IdentityMode::ByDefault,
        _ => return None,
    };
    Some(IdentityModel { mode })
}

fn generated_model(generated: &str, expression: Option<String>) -> Option<GeneratedColumnModel> {
    match generated {
        "s" => Some(GeneratedColumnModel {
            expression: expression.unwrap_or_default(),
            storage: GeneratedStorage::Stored,
        }),
        _ => None,
    }
}

fn parametric_sql_type(db_type: &str) -> Option<SqlType> {
    let open = db_type.find('(')?;
    let close = db_type.rfind(')')?;
    if close + 1 != db_type.len() {
        return None;
    }

    let kind = db_type[..open].trim();
    let args = &db_type[open + 1..close];
    match kind {
        "character varying" | "varchar" => args.trim().parse().ok().map(SqlType::Varchar),
        "character" | "char" => args.trim().parse().ok().map(SqlType::Char),
        "numeric" | "decimal" => {
            let parts = args.split(',').map(str::trim).collect::<Vec<_>>();
            let [precision, scale] = parts[..] else {
                return None;
            };
            Some(SqlType::Decimal {
                precision: precision.parse().ok()?,
                scale: scale.parse().ok()?,
            })
        }
        _ => None,
    }
}

fn default_value(ty: &SqlType, value: &str) -> DefaultValue {
    let trimmed = value.trim();
    match trimmed.to_ascii_lowercase().as_str() {
        "null" => return DefaultValue::Null,
        "true" => return DefaultValue::Bool(true),
        "false" => return DefaultValue::Bool(false),
        "current_timestamp" | "current_timestamp()" | "now()" => {
            return DefaultValue::CurrentTimestamp;
        }
        "current_date" | "current_date()" => return DefaultValue::CurrentDate,
        "current_time" | "current_time()" => return DefaultValue::CurrentTime,
        _ => {}
    }

    if let Some(text) = postgres_string_literal(trimmed)
        && matches!(
            ty,
            SqlType::String | SqlType::Varchar(_) | SqlType::Char(_) | SqlType::Text
        )
    {
        return DefaultValue::Text(text);
    }

    match ty {
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
        _ => DefaultValue::Raw(value.to_owned()),
    }
}

fn postgres_string_literal(value: &str) -> Option<String> {
    let mut chars = value.strip_prefix('\'')?.chars().peekable();
    let mut out = String::new();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                out.push('\'');
            } else {
                let rest = chars.collect::<String>();
                return (rest.is_empty() || rest.starts_with("::")).then_some(out);
            }
        } else {
            out.push(ch);
        }
    }
    None
}

fn action(action: &str) -> Option<ForeignKeyAction> {
    match action {
        "a" => None,
        "r" => Some(ForeignKeyAction::Restrict),
        "c" => Some(ForeignKeyAction::Cascade),
        "n" => Some(ForeignKeyAction::SetNull),
        "d" => Some(ForeignKeyAction::SetDefault),
        _ => None,
    }
}

fn index_direction(direction: &str) -> IndexDirection {
    match direction {
        "DESC" => IndexDirection::Desc,
        _ => IndexDirection::Asc,
    }
}

fn check_expression(definition: &str) -> String {
    definition
        .strip_prefix("CHECK (")
        .and_then(|body| body.strip_suffix(')'))
        .unwrap_or(definition)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_postgres_scalar_types_to_neutral_types() {
        assert_eq!(sql_type("boolean"), SqlType::Bool);
        assert_eq!(sql_type("smallint"), SqlType::I16);
        assert_eq!(sql_type("integer"), SqlType::I32);
        assert_eq!(sql_type("bigint"), SqlType::I64);
        assert_eq!(sql_type("real"), SqlType::F32);
        assert_eq!(sql_type("double precision"), SqlType::F64);
        assert_eq!(sql_type("text"), SqlType::String);
        assert_eq!(sql_type("date"), SqlType::Date);
        assert_eq!(
            sql_type("time without time zone"),
            SqlType::Time { tz: false }
        );
        assert_eq!(sql_type("time with time zone"), SqlType::Time { tz: true });
        assert_eq!(
            sql_type("timestamp without time zone"),
            SqlType::Timestamp { tz: false }
        );
        assert_eq!(
            sql_type("timestamp with time zone"),
            SqlType::Timestamp { tz: true }
        );
        assert_eq!(sql_type("uuid"), SqlType::Uuid);
        assert_eq!(sql_type("json"), SqlType::Json);
        assert_eq!(sql_type("jsonb"), SqlType::Jsonb);
        assert_eq!(sql_type("bytea"), SqlType::Bytes);
        assert_eq!(sql_type("citext"), SqlType::Raw("citext".to_owned()));
    }

    #[test]
    fn maps_postgres_parametric_types_to_neutral_types() {
        assert_eq!(sql_type("character varying(64)"), SqlType::Varchar(64));
        assert_eq!(sql_type("character(2)"), SqlType::Char(2));
        assert_eq!(
            sql_type("numeric(10,2)"),
            SqlType::Decimal {
                precision: 10,
                scale: 2
            }
        );
        assert_eq!(
            sql_type("numeric(10,2)[]"),
            SqlType::Raw("numeric(10,2)[]".to_owned())
        );
    }

    #[test]
    fn maps_foreign_key_actions() {
        assert_eq!(action("a"), None);
        assert_eq!(action("c"), Some(ForeignKeyAction::Cascade));
        assert_eq!(action("r"), Some(ForeignKeyAction::Restrict));
        assert_eq!(action("n"), Some(ForeignKeyAction::SetNull));
        assert_eq!(action("d"), Some(ForeignKeyAction::SetDefault));
    }

    #[test]
    fn maps_postgres_defaults_to_neutral_values() {
        assert_eq!(
            default_value(&SqlType::Char(2), "'MB'::bpchar"),
            DefaultValue::Text("MB".to_owned())
        );
        assert_eq!(
            default_value(&SqlType::String, "'can''t'::text"),
            DefaultValue::Text("can't".to_owned())
        );
        assert_eq!(default_value(&SqlType::I32, "42"), DefaultValue::Int(42));
        assert_eq!(
            default_value(&SqlType::Bool, "true"),
            DefaultValue::Bool(true)
        );
        assert_eq!(
            default_value(&SqlType::Timestamp { tz: true }, "now()"),
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
    fn strips_check_wrapper() {
        assert_eq!(check_expression("CHECK ((score > 0))"), "(score > 0)");
        assert_eq!(check_expression("score > 0"), "score > 0");
    }
}
