use squealy::{
    CheckModel, ColumnModel, Constraint, DatabaseModel, DefaultValue, ForeignKeyModel, IndexModel,
    SchemaModel, SqlType, TableModel,
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
    a.attidentity <> '',
    a.attgenerated <> '',
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
            let default: Option<String> = row.get(5);
            ColumnModel {
                name: row.get(0),
                ty: sql_type(&db_type),
                nullable: !row.get::<_, bool>(2),
                auto_increment: row.get(3),
                generated: row.get(4),
                default: default.map(DefaultValue::Raw),
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
    ARRAY(
        SELECT a.attname::text
        FROM unnest(i.indkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = key.attnum
        ORDER BY key.position
    ) AS columns
FROM pg_index i
JOIN pg_class c ON c.oid = i.indrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_class idx ON idx.oid = i.indexrelid
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
            columns: row.get(2),
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

fn action(action: &str) -> Option<String> {
    match action {
        "a" => None,
        "r" => Some("restrict".to_owned()),
        "c" => Some("cascade".to_owned()),
        "n" => Some("set null".to_owned()),
        "d" => Some("set default".to_owned()),
        _ => None,
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
        assert_eq!(action("c"), Some("cascade".to_owned()));
        assert_eq!(action("r"), Some("restrict".to_owned()));
        assert_eq!(action("n"), Some("set null".to_owned()));
        assert_eq!(action("d"), Some("set default".to_owned()));
    }

    #[test]
    fn strips_check_wrapper() {
        assert_eq!(check_expression("CHECK ((score > 0))"), "(score > 0)");
        assert_eq!(check_expression("score > 0"), "score > 0");
    }
}
