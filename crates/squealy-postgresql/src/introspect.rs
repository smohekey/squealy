use squealy::{
    CheckModel, ColumnModel, Constraint, ConstraintDeferrability, ConstraintValidation,
    DatabaseModel, DefaultValue, EnumModel, ForeignKeyAction, ForeignKeyMatch, ForeignKeyModel,
    GeneratedColumnModel, GeneratedStorage, IdentityMode, IdentityModel, IndexCollation,
    IndexDirection, IndexMethod, IndexModel, IndexNullsOrder, IndexOperatorClass, SchemaModel,
    SequenceDataType, SequenceModel, SequenceOwnedBy, SourceRef, SqlType, TableModel, ViewBody,
    ViewColumnModel, ViewModel, ViewQueryModel,
};
use tokio_postgres::Client;

use crate::PostgresError;

struct TableRef {
    schema: String,
    name: String,
}

pub(crate) async fn database(client: &Client) -> Result<DatabaseModel, PostgresError> {
    let mut schemas = Vec::<SchemaModel>::new();

    for table_ref in table_refs(client).await? {
        let table = table(client, &table_ref).await?;
        schema_entry(&mut schemas, &table_ref.schema)
            .tables
            .push(table);
    }

    // Views are reconstructed from their `pg_get_viewdef` deparse (see `view`), which OMITS a schema
    // qualifier for any relation on the session `search_path` (the default includes `public`): a
    // public-schema view would deparse `FROM users` and reconstruct its source as `schema: None`, unequal
    // to a desired `Some("public")`, churning every plan. Empty the `search_path` around the view
    // introspection so every relation deparses fully qualified, then restore it (best-effort, even on
    // error, so a reused connection is not left altered). Only the view pass needs this — the table
    // constraint / index / generated-column deparse names `pg_catalog` builtins, which resolve regardless
    // of `search_path`, so those keep the default to avoid perturbing their round-trip.
    let original_search_path: String = client
        .query_one("SELECT current_setting('search_path')", &[])
        .await?
        .get(0);
    client
        .execute("SELECT set_config('search_path', '', false)", &[])
        .await?;
    let views = introspect_views(client).await;
    let _ = client
        .execute(
            "SELECT set_config('search_path', $1, false)",
            &[&original_search_path],
        )
        .await;
    for (schema, view) in views? {
        schema_entry(&mut schemas, &schema).views.push(view);
    }

    for (schema, enum_type) in introspect_enums(client).await? {
        schema_entry(&mut schemas, &schema).enums.push(enum_type);
    }

    for (schema, sequence) in introspect_sequences(client).await? {
        schema_entry(&mut schemas, &schema).sequences.push(sequence);
    }

    Ok(DatabaseModel { schemas })
}

/// Introspects every free-standing sequence (`CREATE SEQUENCE`), paired with its schema. A sequence
/// implicitly owned by an identity or serial column (an `auto`/`internal` `pg_depend` link to a column)
/// is excluded — it is created with its column, not as a standalone object. A sequence explicitly tied
/// to a column with `ALTER SEQUENCE ... OWNED BY` (a `deptype = 'a'` link) is kept, and its owning
/// column recovered.
async fn introspect_sequences(
    client: &Client,
) -> Result<Vec<(String, SequenceModel)>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT n.nspname,
       c.relname,
       format_type(s.seqtypid, NULL) AS data_type,
       s.seqstart,
       s.seqincrement,
       s.seqmin,
       s.seqmax,
       s.seqcache,
       s.seqcycle,
       owner.tablename,
       owner.colname
FROM pg_sequence s
JOIN pg_class c ON c.oid = s.seqrelid
JOIN pg_namespace n ON n.oid = c.relnamespace
LEFT JOIN LATERAL (
    SELECT rel.relname AS tablename, att.attname AS colname
    FROM pg_depend d
    JOIN pg_class rel ON rel.oid = d.refobjid
    JOIN pg_attribute att ON att.attrelid = d.refobjid AND att.attnum = d.refobjsubid
    WHERE d.classid = 'pg_class'::regclass
      AND d.objid = c.oid
      AND d.deptype = 'a'
      AND d.refobjsubid <> 0
    LIMIT 1
) owner ON TRUE
WHERE c.relkind = 'S'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', '__squealy')
  AND n.nspname NOT LIKE 'pg_toast%'
  AND NOT EXISTS (
      SELECT 1 FROM pg_depend d
      WHERE d.classid = 'pg_class'::regclass
        AND d.objid = c.oid
        AND d.deptype = 'i'
        AND d.refobjsubid <> 0
  )
ORDER BY n.nspname, c.relname",
            &[],
        )
        .await?;

    let mut sequences = Vec::new();
    for row in rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let data_type_name: String = row.get(2);
        let data_type = match data_type_name.as_str() {
            "smallint" => SequenceDataType::SmallInt,
            "integer" => SequenceDataType::Integer,
            _ => SequenceDataType::BigInt,
        };
        let owned_by = match (
            row.get::<_, Option<String>>(9),
            row.get::<_, Option<String>>(10),
        ) {
            (Some(table), Some(column)) => Some(SequenceOwnedBy { table, column }),
            _ => None,
        };
        sequences.push((
            schema,
            SequenceModel {
                name,
                data_type,
                start: row.get(3),
                increment: row.get(4),
                min: row.get(5),
                max: row.get(6),
                cache: row.get(7),
                cycle: row.get(8),
                owned_by,
            },
        ));
    }
    Ok(sequences)
}

/// Introspects every user-defined enum type (`CREATE TYPE ... AS ENUM`), paired with its schema. Labels
/// are ordered by `enumsortorder`, the order PostgreSQL renders and compares them in.
async fn introspect_enums(client: &Client) -> Result<Vec<(String, EnumModel)>, PostgresError> {
    // LEFT JOIN so a label-less enum (`CREATE TYPE t AS ENUM ()`, which PostgreSQL permits) still returns
    // one row (with a NULL label) and is not silently omitted — otherwise it would churn a `CreateEnum`
    // every plan and fail because the type already exists.
    let rows = client
        .query(
            "\
SELECT n.nspname, t.typname, e.enumlabel
FROM pg_type t
JOIN pg_namespace n ON n.oid = t.typnamespace
LEFT JOIN pg_enum e ON e.enumtypid = t.oid
WHERE t.typtype = 'e'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', '__squealy')
  AND n.nspname NOT LIKE 'pg_toast%'
ORDER BY n.nspname, t.typname, e.enumsortorder",
            &[],
        )
        .await?;

    // Rows arrive grouped by (schema, typname) in label order; fold consecutive rows into one enum. A
    // label-less enum contributes a single row with a NULL label — start it with an empty label list.
    let mut enums: Vec<(String, EnumModel)> = Vec::new();
    for row in rows {
        let schema: String = row.get(0);
        let name: String = row.get(1);
        let label: Option<String> = row.get(2);
        match enums.last_mut() {
            Some((last_schema, last_enum)) if *last_schema == schema && last_enum.name == name => {
                if let Some(label) = label {
                    last_enum.labels.push(label);
                }
            }
            _ => enums.push((
                schema,
                EnumModel {
                    name,
                    labels: label.into_iter().collect(),
                },
            )),
        }
    }
    Ok(enums)
}

/// Introspects every view, pairing each with the name of the schema it belongs to. Called with an
/// emptied `search_path` (see [`database`]) so `pg_get_viewdef` fully qualifies the sources it deparses.
async fn introspect_views(client: &Client) -> Result<Vec<(String, ViewModel)>, PostgresError> {
    let mut views = Vec::new();
    for view_ref in view_refs(client).await? {
        let view = view(client, &view_ref).await?;
        views.push((view_ref.schema, view));
    }
    Ok(views)
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
        enums: Vec::new(),
        sequences: Vec::new(),
    });
    schemas.last_mut().expect("schema just pushed")
}

async fn view_refs(client: &Client) -> Result<Vec<TableRef>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT n.nspname, c.relname
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind = 'v'
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', '__squealy')
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

async fn view(client: &Client, view_ref: &TableRef) -> Result<ViewModel, PostgresError> {
    // Reconstruct the view's structural body from its `pg_get_viewdef` deparse so a squealy-published
    // view re-plans to empty (the diff's structural bar), rather than re-applying it every run. The
    // reverse parser inverts the deparse idioms it knows; a body it cannot yet lower falls back to the
    // body-unknown sentinel (only the view-on-view dependencies, which still order live drops) — graceful,
    // no regression versus the previous always-empty behaviour.
    let query = match view_definition(client, view_ref).await? {
        Some(sql) => {
            match squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                .read_view_query(&sql)
            {
                // A reconstructed body's dependencies are found by walking it, so the pg_rewrite lookup
                // is only needed for the fallback below.
                Ok(body) => body,
                Err(_) => empty_body(client, view_ref).await?,
            }
        }
        None => empty_body(client, view_ref).await?,
    };
    Ok(ViewModel {
        name: view_ref.name.clone(),
        comment: None,
        columns: view_columns(client, view_ref).await?,
        query,
    })
}

/// The body-unknown sentinel: an empty `SELECT` carrying only the view-on-view dependencies, so the diff
/// can still order live drops (drop a dependent before the view it selects from) even when the body could
/// not be reconstructed.
async fn empty_body(client: &Client, view_ref: &TableRef) -> Result<ViewBody, PostgresError> {
    Ok(ViewBody::Select(Box::new(ViewQueryModel {
        dependencies: view_dependencies(client, view_ref).await?,
        ..ViewQueryModel::default()
    })))
}

/// The view's `SELECT` body as PostgreSQL deparses it (`pg_get_viewdef(oid, pretty)`). `pretty = true`
/// gives the same idiom the reverse parser was tuned against (`::`-cast result-pins, `text` casts).
/// Returns `None` if the view no longer exists (dropped out of band between listing and this fetch).
async fn view_definition(
    client: &Client,
    view_ref: &TableRef,
) -> Result<Option<String>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT pg_get_viewdef(c.oid, true)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND c.relkind = 'v'",
            &[&view_ref.schema, &view_ref.name],
        )
        .await?;

    Ok(rows.first().map(|row| row.get(0)))
}

/// The other views this view depends on, read from its `ON SELECT` rewrite rule's dependencies.
/// Restricted to relations of kind `v` (other views); table dependencies are irrelevant to view
/// ordering because every table is created before any view.
async fn view_dependencies(
    client: &Client,
    view_ref: &TableRef,
) -> Result<Vec<SourceRef>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT DISTINCT dn.nspname, dc.relname
FROM pg_rewrite r
JOIN pg_depend d ON d.objid = r.oid AND d.deptype = 'n'
JOIN pg_class sc ON sc.oid = r.ev_class
JOIN pg_namespace sn ON sn.oid = sc.relnamespace
JOIN pg_class dc ON dc.oid = d.refobjid
JOIN pg_namespace dn ON dn.oid = dc.relnamespace
WHERE sn.nspname = $1
  AND sc.relname = $2
  AND d.refclassid = 'pg_class'::regclass
  AND dc.relkind = 'v'
  AND dc.oid <> sc.oid
ORDER BY dn.nspname, dc.relname",
            &[&view_ref.schema, &view_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let name: String = row.get(1);
            SourceRef {
                schema: Some(row.get(0)),
                // The alias is never rendered (an introspected view has no body to render); it only
                // needs to be present, so reuse the dependency name.
                alias: name.clone(),
                name,
            }
        })
        .collect())
}

async fn view_columns(
    client: &Client,
    view_ref: &TableRef,
) -> Result<Vec<ViewColumnModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT a.attname, format_type(a.atttypid, a.atttypmod), a.attnotnull, typ.typtype::text, typ.typname,
       tn.nspname, format('%I.%I', tn.nspname, typ.typname)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid
JOIN pg_type typ ON typ.oid = a.atttypid
JOIN pg_namespace tn ON tn.oid = typ.typnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND c.relkind = 'v'
  AND a.attnum > 0
  AND NOT a.attisdropped
ORDER BY a.attnum",
            &[&view_ref.schema, &view_ref.name],
        )
        .await?;

    Ok(rows
        .into_iter()
        .map(|row| {
            let db_type: String = row.get(1);
            let typtype: String = row.get(3);
            let type_name: String = row.get(4);
            let type_schema: String = row.get(5);
            let type_qualified: String = row.get(6);
            let ty = enum_or_sql_type(
                &typtype,
                &type_schema,
                &type_name,
                &type_qualified,
                &view_ref.schema,
                &db_type,
            );
            ViewColumnModel {
                name: row.get(0),
                ty,
                nullable: !row.get::<_, bool>(2),
            }
        })
        .collect())
}

async fn table_refs(client: &Client) -> Result<Vec<TableRef>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT n.nspname, c.relname
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE c.relkind IN ('r', 'p')
  AND n.nspname NOT IN ('pg_catalog', 'information_schema', '__squealy')
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
    let mut columns = columns(client, table_ref).await?;
    let (primary_key, uniques) = key_constraints(client, table_ref).await?;
    let mut checks = checks(client, table_ref).await?;

    // A fixed-width binary column (`[u8; N]`) is rendered as `bytea` + a generated
    // `CHECK (octet_length(col) = N)`. Fold that check back into `FixedBytes(N)` and drop it, so the
    // introspected model matches the declared one (idempotent publish).
    fold_fixed_bytes_checks(&mut columns, &mut checks);

    Ok(TableModel {
        name: table_ref.name.clone(),
        comment: table_comment(client, table_ref).await?,
        columns,
        primary_key,
        foreign_keys: foreign_keys(client, table_ref).await?,
        uniques,
        checks,
        indexes: indexes(client, table_ref).await?,
    })
}

/// Folds the *generated* fixed-width-binary length check into the column's `FixedBytes(N)` type and
/// removes it from the check list. The check is identified by matching its constraint name against the
/// deterministic `sqfb_<hash(column)>` name we generate (see [`crate::sql::fixed_bytes_check_name`]),
/// not by parsing the column name out of the expression — so it is robust to however PostgreSQL quotes
/// an exotic column identifier inside `octet_length(...)`. A user-authored `octet_length` check has a
/// different name and is left untouched, so it round-trips as `Bytes` + an explicit check.
fn fold_fixed_bytes_checks(columns: &mut [ColumnModel], checks: &mut Vec<CheckModel>) {
    checks.retain(|check| {
        if check.name.starts_with(crate::sql::FIXED_BYTES_CHECK_PREFIX)
            && let Some(width) = parse_octet_length_width(&check.expression)
            && let Some(column) = columns.iter_mut().find(|column| {
                column.ty == SqlType::Bytes
                    && crate::sql::fixed_bytes_check_name(&column.name) == check.name
            })
        {
            column.ty = SqlType::FixedBytes(width);
            return false;
        }
        true
    });
}

/// Extracts `N` from a generated `octet_length(<col>) = N` check expression, in either form it can be
/// read back as:
///  - **structural** — `octet_length` lowers to a general [`ExprNode::Function`](squealy::ExprNode::Function)
///    and the whole check to `Compare(Eq, Function, Literal(N))`;
///  - **`Raw`** — a legacy package's verbatim deparse string (or a form the grammar could not lower).
///
/// The column identifier inside the call is not inspected — only the width matters.
fn parse_octet_length_width(expression: &squealy::ExprNode) -> Option<u32> {
    match expression {
        squealy::ExprNode::Compare {
            op: squealy::CompareOp::Equals,
            left,
            right,
        } => {
            let squealy::ExprNode::Function { name, .. } = left.as_ref() else {
                return None;
            };
            let squealy::ExprNode::Literal(width) = right.as_ref() else {
                return None;
            };
            (name == "octet_length").then(|| width.trim().parse().ok())?
        }
        squealy::ExprNode::Raw(text) => {
            if !text.contains("octet_length(") {
                return None;
            }
            let (_, rhs) = text.rsplit_once('=')?;
            rhs.trim().trim_end_matches(')').trim().parse().ok()
        }
        _ => None,
    }
}

async fn columns(client: &Client, table_ref: &TableRef) -> Result<Vec<ColumnModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT
    a.attname,
    format_type(a.atttypid, a.atttypmod),
    CASE
        WHEN a.attcollation <> typ.typcollation THEN coll.collname
        ELSE NULL
    END,
    a.attnotnull,
    a.attidentity::text,
    a.attgenerated::text,
    pg_get_expr(ad.adbin, ad.adrelid),
    col_description(c.oid, a.attnum),
    typ.typtype::text,
    typ.typname,
    tn.nspname,
    format('%I.%I', tn.nspname, typ.typname)
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
JOIN pg_attribute a ON a.attrelid = c.oid
JOIN pg_type typ ON typ.oid = a.atttypid
JOIN pg_namespace tn ON tn.oid = typ.typnamespace
LEFT JOIN pg_collation coll ON coll.oid = a.attcollation
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
            let typtype: String = row.get(8);
            let type_schema: String = row.get(10);
            let type_name: String = row.get(9);
            let type_qualified: String = row.get(11);
            let ty = enum_or_sql_type(
                &typtype,
                &type_schema,
                &type_name,
                &type_qualified,
                &table_ref.schema,
                &db_type,
            );
            // Parse the default as an enum label whenever the catalog says the column IS an enum — even
            // a cross-schema one (which is represented as `Raw`, so `default_value` alone would miss it).
            let default_ty = if typtype == "e" {
                SqlType::Enum(type_name.clone())
            } else {
                ty.clone()
            };
            let identity: String = row.get(4);
            let generated: String = row.get(5);
            let default: Option<String> = row.get(6);
            ColumnModel {
                name: row.get(0),
                comment: row.get(7),
                ty: ty.clone(),
                collation: row.get(2),
                nullable: !row.get::<_, bool>(3),
                default: (generated != "s")
                    .then(|| default.clone())
                    .flatten()
                    .map(|value| default_value(&default_ty, &value)),
                identity: identity_model(&identity),
                generated: generated_model(&generated, default),
                // PostgreSQL has no `ON UPDATE` column auto-update attribute.
                on_update: None,
            }
        })
        .collect())
}

async fn table_comment(
    client: &Client,
    table_ref: &TableRef,
) -> Result<Option<String>, PostgresError> {
    let row = client
        .query_opt(
            "\
SELECT obj_description(c.oid, 'pg_class')
FROM pg_class c
JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = $1
  AND c.relname = $2
  AND c.relkind IN ('r', 'p')",
            &[&table_ref.schema, &table_ref.name],
        )
        .await?;

    Ok(row.and_then(|row| row.get(0)))
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
            prefix_lengths: Vec::new(),
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
    con.confmatchtype::text,
    con.condeferrable,
    con.condeferred,
    con.convalidated,
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
                match_type: match_type(row.get::<_, String>(5).as_str()),
                deferrability: deferrability(row.get(6), row.get(7)),
                validation: validation(row.get(8)),
                enforcement: None,
                on_delete: action(row.get::<_, String>(9).as_str()),
                on_update: action(row.get::<_, String>(10).as_str()),
            }
        })
        .collect())
}

fn match_type(value: &str) -> Option<ForeignKeyMatch> {
    match value {
        "s" => None,
        "f" => Some(ForeignKeyMatch::Full),
        "p" => Some(ForeignKeyMatch::Partial),
        other => Some(ForeignKeyMatch::Raw(other.to_owned())),
    }
}

fn deferrability(deferrable: bool, deferred: bool) -> Option<ConstraintDeferrability> {
    if !deferrable {
        None
    } else if deferred {
        Some(ConstraintDeferrability::InitiallyDeferred)
    } else {
        Some(ConstraintDeferrability::InitiallyImmediate)
    }
}

fn validation(validated: bool) -> Option<ConstraintValidation> {
    (!validated).then_some(ConstraintValidation::NotValidated)
}

async fn checks(client: &Client, table_ref: &TableRef) -> Result<Vec<CheckModel>, PostgresError> {
    let rows = client
        .query(
            "\
SELECT con.conname, pg_get_constraintdef(con.oid), con.convalidated
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
            validation: validation(row.get(2)),
            enforcement: None,
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
        WHERE key.position <= i.indnkeyatts
        ORDER BY key.position
    ) AS columns,
    ARRAY(
        SELECT a.attname::text
        FROM unnest(i.indkey) WITH ORDINALITY AS key(attnum, position)
        JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = key.attnum
        WHERE key.position > i.indnkeyatts
        ORDER BY key.position
    ) AS include_columns,
    ARRAY(
        SELECT CASE WHEN (option & 1) = 1 THEN 'DESC' ELSE 'ASC' END
        FROM unnest(i.indoption) WITH ORDINALITY AS opt(option, position)
	        ORDER BY position
	    ) AS directions,
    ARRAY(
        SELECT CASE WHEN (option & 2) = 2 THEN 'FIRST' ELSE 'LAST' END
        FROM unnest(i.indoption) WITH ORDINALITY AS opt(option, position)
        ORDER BY position
    ) AS nulls,
    ARRAY(
        SELECT (indcoll.position - 1)::int
        FROM unnest(i.indcollation) WITH ORDINALITY AS indcoll(collation_oid, position)
        JOIN pg_collation coll ON coll.oid = indcoll.collation_oid
        WHERE indcoll.position <= i.indnkeyatts
          AND coll.collname <> 'default'
        ORDER BY indcoll.position
    ) AS collation_positions,
    ARRAY(
        SELECT coll.collname::text
        FROM unnest(i.indcollation) WITH ORDINALITY AS indcoll(collation_oid, position)
        JOIN pg_collation coll ON coll.oid = indcoll.collation_oid
        WHERE indcoll.position <= i.indnkeyatts
          AND coll.collname <> 'default'
        ORDER BY indcoll.position
    ) AS collations,
    ARRAY(
        SELECT (cls.position - 1)::int
        FROM unnest(i.indclass) WITH ORDINALITY AS cls(opcoid, position)
        JOIN pg_opclass opc ON opc.oid = cls.opcoid
        WHERE cls.position <= i.indnkeyatts
          AND NOT opc.opcdefault
        ORDER BY cls.position
    ) AS operator_class_positions,
    ARRAY(
        SELECT opc.opcname::text
        FROM unnest(i.indclass) WITH ORDINALITY AS cls(opcoid, position)
        JOIN pg_opclass opc ON opc.oid = cls.opcoid
        WHERE cls.position <= i.indnkeyatts
          AND NOT opc.opcdefault
        ORDER BY cls.position
    ) AS operator_classes,
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
        .map(|row| {
            let directions = row
                .get::<_, Vec<String>>(5)
                .into_iter()
                .map(|direction| index_direction(&direction))
                .collect::<Vec<_>>();
            let nulls = index_nulls(&directions, row.get(6));
            let collations = index_collations(row.get(7), row.get(8));
            let operator_classes = index_operator_classes(row.get(9), row.get(10));
            IndexModel {
                name: row.get(0),
                unique: row.get(1),
                method: Some(IndexMethod::from_sql(&row.get::<_, String>(2))),
                columns: row.get(3),
                // `pg_get_expr(indexprs, …)` returns every expression key of a multi-expression index as one
                // comma-separated string, so split it into one structural term per expression.
                expressions: row
                    .get::<_, Option<String>>(12)
                    .map(|expressions| {
                        squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                            .read_index_expressions_or_raw(&expressions)
                    })
                    .unwrap_or_default(),
                include_columns: row.get(4),
                directions,
                nulls,
                collations,
                operator_classes,
                // PostgreSQL has no column-prefix indexes (`col(n)`); it uses expression indexes instead.
                prefix_lengths: Vec::new(),
                predicate: row.get::<_, Option<String>>(11).map(|predicate| {
                    Box::new(
                        squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                            .read_index_predicate_or_raw(&predicate),
                    )
                }),
            }
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
        "uuid" => SqlType::Uuid,
        "json" => SqlType::Json,
        "jsonb" => SqlType::Jsonb,
        "bytea" => SqlType::Bytes,
        // `time`/`timestamp` (with the optional `(n)` precision and `with/without time zone` suffix that
        // `format_type` emits) are recovered structurally, then the remaining parametric types.
        other => temporal_sql_type(other)
            .or_else(|| parametric_sql_type(other))
            .unwrap_or_else(|| SqlType::Raw(other.to_owned())),
    }
}

/// Recovers a `time`/`timestamp` type from a `format_type` string — `timestamp with time zone`,
/// `timestamp(3) without time zone`, `time(6)`, etc. The `(n)` fractional-seconds precision sits between
/// the base name and the `with/without time zone` suffix, so it is not a trailing argument (unlike
/// `varchar(n)`). A form with no explicit `(n)` uses PostgreSQL's microsecond default, which round-trips
/// against a desired column canonicalized to `Some(6)`.
fn temporal_sql_type(db_type: &str) -> Option<SqlType> {
    let (rest, tz) = if let Some(rest) = db_type.strip_suffix(" without time zone") {
        (rest.trim_end(), false)
    } else if let Some(rest) = db_type.strip_suffix(" with time zone") {
        (rest.trim_end(), true)
    } else {
        (db_type, false)
    };
    let (base, precision) = match rest.find('(') {
        Some(open) => {
            let close = rest.rfind(')')?;
            // The `(n)` must terminate `rest`; anything trailing (e.g. the `[]` of a temporal *array*
            // like `timestamp(3) without time zone[]`) means this is not a scalar temporal type, so fall
            // through to `Raw` rather than mis-reading it as one.
            if close + 1 != rest.len() {
                return None;
            }
            let precision = rest[open + 1..close].trim().parse().ok()?;
            (rest[..open].trim(), Some(precision))
        }
        // A bare `timestamp`/`time` is PostgreSQL's microsecond default (`format_type` omits `(6)`).
        None => (rest, Some(6)),
    };
    match base {
        "timestamp" => Some(SqlType::Timestamp { tz, precision }),
        "time" => Some(SqlType::Time { tz, precision }),
        _ => None,
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
            // Structure the `pg_get_expr` deparse into a comparable node, falling back to a verbatim
            // `Raw` for an expression the reverse parser cannot yet lower. A blank deparse (there should
            // be none for a real stored generated column) yields no expression.
            expression: expression
                .filter(|expression| !expression.trim().is_empty())
                .map(|expression| {
                    squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
                        .read_generated_expression_or_raw(&expression)
                }),
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

    // An enum column's default is stored as `'label'::<type>` (e.g. `'sad'::publish_enums.mood`); recover
    // the bare label as text so a desired `DefaultValue::Text("sad")` compares equal instead of churning.
    // `postgres_string_literal` parses the leading quoted literal and tolerates the trailing `::<type>`
    // cast — including a label that itself contains `::` (`'a::b'::mood`), which lives inside the quotes.
    if let SqlType::Enum(_) = ty
        && let Some(text) = postgres_string_literal(trimmed)
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

/// Resolves a column's [`SqlType`] from the catalog. An enum in the **same** schema as its relation
/// becomes [`SqlType::Enum`] (rendered qualified with the relation's schema). A **cross-schema** enum
/// becomes a schema-qualified [`SqlType::Raw`] built from the catalog (`type_schema.type_name`) — NOT the
/// `format_type` string, which drops the qualifier when the enum's schema is on `search_path` (e.g.
/// `public`) and would then churn against a desired qualified `Raw`. Non-enum types use `sql_type`.
fn enum_or_sql_type(
    typtype: &str,
    type_schema: &str,
    type_name: &str,
    type_qualified: &str,
    relation_schema: &str,
    db_type: &str,
) -> SqlType {
    if typtype == "e" {
        if type_schema == relation_schema {
            SqlType::Enum(type_name.to_owned())
        } else {
            // `type_qualified` is PostgreSQL's `format('%I.%I', schema, name)` — quoted only where an
            // identifier requires it (`"Types"."Mood Type"`, but bare `types.mood`), so the round-trips
            // as a renderable, correctly-quoted `Raw`.
            SqlType::Raw(type_qualified.to_owned())
        }
    } else {
        sql_type(db_type)
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

fn index_nulls(directions: &[IndexDirection], nulls: Vec<String>) -> Vec<IndexNullsOrder> {
    let nulls = nulls
        .into_iter()
        .map(|order| match order.as_str() {
            "FIRST" => IndexNullsOrder::First,
            _ => IndexNullsOrder::Last,
        })
        .collect::<Vec<_>>();

    let all_default = nulls.iter().enumerate().all(|(position, order)| {
        let direction = directions.get(position).unwrap_or(&IndexDirection::Asc);
        matches!(
            (direction, order),
            (IndexDirection::Asc, IndexNullsOrder::Last)
                | (IndexDirection::Desc, IndexNullsOrder::First)
        )
    });
    if all_default { Vec::new() } else { nulls }
}

fn index_operator_classes(positions: Vec<i32>, names: Vec<String>) -> Vec<IndexOperatorClass> {
    positions
        .into_iter()
        .zip(names)
        .filter_map(|(position, name)| {
            usize::try_from(position)
                .ok()
                .map(|position| IndexOperatorClass { position, name })
        })
        .collect()
}

fn index_collations(positions: Vec<i32>, names: Vec<String>) -> Vec<IndexCollation> {
    positions
        .into_iter()
        .zip(names)
        .filter_map(|(position, name)| {
            usize::try_from(position)
                .ok()
                .map(|position| IndexCollation { position, name })
        })
        .collect()
}

fn check_expression(definition: &str) -> squealy::ExprNode {
    let inner = definition
        .strip_prefix("CHECK (")
        .and_then(|body| body.strip_suffix(')'))
        .unwrap_or(definition);
    squealy_parse::Reader::new(squealy_parse::SqlDialect::Postgres)
        .read_check_expression_or_raw(inner)
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
        // A bare `numeric` (no typmod) introspects faithfully as `Raw("numeric")` — it may hold arbitrary-
        // precision decimals, so introspection (which feeds `squealy extract`) must not narrow it to an
        // integer. Diff convergence with a published `I128`/`U64`/`U128` (or explicit `Raw("numeric")`)
        // column happens in `canonical_pg_sql_type`, not here. A `numeric(p,s)` recovers as `Decimal`.
        assert_eq!(sql_type("numeric"), SqlType::Raw("numeric".to_owned()));
        assert_eq!(sql_type("real"), SqlType::F32);
        assert_eq!(sql_type("double precision"), SqlType::F64);
        assert_eq!(sql_type("text"), SqlType::String);
        assert_eq!(sql_type("date"), SqlType::Date);
        // A bare temporal form uses PostgreSQL's microsecond default (`Some(6)`); an explicit `(n)`
        // (which `format_type` emits between the base and the `with/without time zone` suffix) is read.
        assert_eq!(
            sql_type("time without time zone"),
            SqlType::Time {
                tz: false,
                precision: Some(6)
            }
        );
        assert_eq!(
            sql_type("time with time zone"),
            SqlType::Time {
                tz: true,
                precision: Some(6)
            }
        );
        assert_eq!(
            sql_type("timestamp without time zone"),
            SqlType::Timestamp {
                tz: false,
                precision: Some(6)
            }
        );
        assert_eq!(
            sql_type("timestamp with time zone"),
            SqlType::Timestamp {
                tz: true,
                precision: Some(6)
            }
        );
        assert_eq!(
            sql_type("timestamp(3) with time zone"),
            SqlType::Timestamp {
                tz: true,
                precision: Some(3)
            }
        );
        assert_eq!(
            sql_type("time(0) without time zone"),
            SqlType::Time {
                tz: false,
                precision: Some(0)
            }
        );
        // A temporal *array* is not a scalar temporal type: it must not be mis-parsed (trailing `[]`
        // after the `(n)`), falling through to `Raw` like `numeric(10,2)[]` does.
        assert_eq!(
            sql_type("timestamp(3) without time zone[]"),
            SqlType::Raw("timestamp(3) without time zone[]".to_owned())
        );
        assert_eq!(sql_type("time(0)[]"), SqlType::Raw("time(0)[]".to_owned()));
        assert_eq!(sql_type("uuid"), SqlType::Uuid);
        assert_eq!(sql_type("json"), SqlType::Json);
        assert_eq!(sql_type("jsonb"), SqlType::Jsonb);
        assert_eq!(sql_type("bytea"), SqlType::Bytes);
        assert_eq!(sql_type("citext"), SqlType::Raw("citext".to_owned()));
    }

    #[test]
    fn parses_generated_octet_length_width() {
        // Live introspection lowers `octet_length(key) = 32` structurally (a general `Function` compared
        // to an integer literal); the width comes from the literal. Feed the actual deparse string
        // through `check_expression` to exercise the real path end to end.
        assert_eq!(
            parse_octet_length_width(&check_expression("CHECK ((octet_length(key) = 32))")),
            Some(32)
        );
        assert_eq!(
            parse_octet_length_width(&squealy::ExprNode::Compare {
                op: squealy::CompareOp::Equals,
                left: Box::new(squealy::ExprNode::Function {
                    name: "octet_length".to_owned(),
                    args: vec![squealy::ExprNode::BareColumn {
                        column: "key".to_owned(),
                    }],
                }),
                right: Box::new(squealy::ExprNode::Literal("12".to_owned())),
            }),
            Some(12)
        );
        // A user function or a non-equality comparison is not a width marker.
        assert_eq!(
            parse_octet_length_width(&check_expression("CHECK ((length(key) = 32))")),
            None
        );
        assert_eq!(
            parse_octet_length_width(&check_expression("CHECK ((octet_length(key) > 0))")),
            None
        );

        // A legacy package (or a form the grammar could not lower) still reads back as `Raw`; the width
        // is parsed from the text, so quoting of the identifier is irrelevant — including exotic names
        // containing `)` or `=` inside the quotes.
        let raw = |text: &str| squealy::ExprNode::Raw(text.to_owned());
        assert_eq!(
            parse_octet_length_width(&raw("(octet_length(key) = 32)")),
            Some(32)
        );
        assert_eq!(
            parse_octet_length_width(&raw("(octet_length(\"we)ird=name\") = 16)")),
            Some(16)
        );
        assert_eq!(parse_octet_length_width(&raw("length(key) = 32")), None);
    }

    #[test]
    fn folds_octet_length_check_into_fixed_bytes() {
        fn bytea_column(name: &str) -> ColumnModel {
            ColumnModel {
                name: name.to_owned(),
                comment: None,
                ty: SqlType::Bytes,
                collation: None,
                nullable: false,
                default: None,
                identity: None,
                generated: None,
                on_update: None,
            }
        }
        fn check(name: &str, expression: &str) -> CheckModel {
            CheckModel {
                name: name.to_owned(),
                // Route through the reader, so the fold sees exactly what live introspection produces
                // (a structural `octet_length(...)` comparison, not a hand-built `Raw`).
                expression: check_expression(expression),
                validation: None,
                enforcement: None,
            }
        }

        let mut columns = vec![bytea_column("key"), bytea_column("blob")];
        let mut checks = vec![
            // The generated check carries the deterministic `sqfb_<hash(column)>` name.
            check(
                &crate::sql::fixed_bytes_check_name("key"),
                "(octet_length(key) = 32)",
            ),
            check("secrets_blob_check", "(octet_length(blob) > 0)"),
        ];
        fold_fixed_bytes_checks(&mut columns, &mut checks);

        // The generated check folds into `FixedBytes(32)` and is removed; the unrelated bytea column
        // and its check are untouched.
        assert_eq!(columns[0].ty, SqlType::FixedBytes(32));
        assert_eq!(columns[1].ty, SqlType::Bytes);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "secrets_blob_check");
    }

    #[test]
    fn does_not_fold_user_authored_octet_length_checks() {
        // A user who modeled `Bytes` + their own `octet_length` check (before `FixedBytes` existed)
        // must keep that exact shape: the check is not `sqfb_`-named, so it is left as-is.
        let mut columns = vec![ColumnModel {
            name: "key".to_owned(),
            comment: None,
            ty: SqlType::Bytes,
            collation: None,
            nullable: false,
            default: None,
            identity: None,
            generated: None,
            on_update: None,
        }];
        let mut checks = vec![CheckModel {
            name: "my_key_len".to_owned(),
            expression: squealy::ExprNode::Raw("(octet_length(key) = 32)".to_owned()),
            validation: None,
            enforcement: None,
        }];
        fold_fixed_bytes_checks(&mut columns, &mut checks);

        assert_eq!(columns[0].ty, SqlType::Bytes);
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].name, "my_key_len");
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
            default_value(
                &SqlType::Timestamp {
                    tz: true,
                    precision: Some(6)
                },
                "now()"
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
    fn strips_check_wrapper() {
        // The `CHECK (...)` wrapper is stripped and the body lowers to the same structural expression
        // whether or not the wrapper (and pg's extra inner parens) is present.
        let expected = squealy::ExprNode::Compare {
            op: squealy::CompareOp::GreaterThan,
            left: Box::new(squealy::ExprNode::BareColumn {
                column: "score".to_owned(),
            }),
            right: Box::new(squealy::ExprNode::Literal("0".to_owned())),
        };
        assert_eq!(check_expression("CHECK ((score > 0))"), expected);
        assert_eq!(check_expression("score > 0"), expected);
    }

    #[test]
    fn general_function_check_lowers_to_structural_node() {
        // A general (user/built-in) function outside the closed scalar set lowers to a structural
        // `Function` node instead of falling to `Raw`. pg's `pg_get_constraintdef` synthesizes a `::text`
        // cast on the string literal, so the authored `jsonb_typeof(data) = 'object'` and its deparse
        // `jsonb_typeof(data) = 'object'::text` must converge to the same node — the churn a structural
        // general-function node kills (and the reason PostgreSQL's `canonical.rs` string normalizer is
        // no longer needed).
        let expected = squealy::ExprNode::Compare {
            op: squealy::CompareOp::Equals,
            left: Box::new(squealy::ExprNode::Function {
                name: "jsonb_typeof".to_owned(),
                args: vec![squealy::ExprNode::BareColumn {
                    column: "data".to_owned(),
                }],
            }),
            right: Box::new(squealy::ExprNode::Literal("'object'".to_owned())),
        };
        // The pg deparse form (extra parens + synthesized literal cast).
        assert_eq!(
            check_expression("CHECK ((jsonb_typeof(data) = 'object'::text))"),
            expected
        );
        // The authored / neutral form.
        assert_eq!(check_expression("jsonb_typeof(data) = 'object'"), expected);
    }

    #[test]
    fn generated_expression_lowers_to_structural_node() {
        // pg's `pg_get_expr` deparse of a stored generated column lowers to the same structural node an
        // authored one produces, so a published generated column re-plans to empty instead of churning
        // against the introspected string.
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
            generated_model("s", Some("(base + 1)".to_owned())),
            expected
        );

        // Only a stored generation kind (`s`) is a generated column.
        assert_eq!(generated_model("", Some("(base + 1)".to_owned())), None);

        // A blank deparse yields a generated column with no expression (the renderer rejects it).
        assert!(matches!(
            generated_model("s", Some("   ".to_owned())),
            Some(GeneratedColumnModel {
                expression: None,
                ..
            })
        ));
    }
}
