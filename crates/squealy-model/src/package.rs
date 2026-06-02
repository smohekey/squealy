//! The `.sqz` schema package: a KDL model inside a zip container.
//!
//! The package is a *derived* deploy artifact (the Rust crate remains the source of truth), so it can
//! be handed to an environment without a Rust toolchain. It is backend-neutral and serialized
//! deterministically so a given model always produces the same bytes.
//!
//! Layout:
//! ```text
//! package.sqz (zip)
//! ├── manifest.kdl   metadata
//! └── model.kdl      the DatabaseModel
//! ```

use std::fmt;
use std::io::{self, Read, Write};
use std::path::Path;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use squealy::{
    CheckModel, ColumnModel, Constraint, ConstraintDeferrability, DatabaseModel, DefaultValue,
    ForeignKeyAction, ForeignKeyMatch, ForeignKeyModel, GeneratedColumnModel, GeneratedStorage,
    IdentityMode, IdentityModel, IndexCollation, IndexDirection, IndexMethod, IndexModel,
    IndexNullsOrder, IndexOperatorClass, SchemaModel, SqlType, TableModel,
};

/// Current package format version, recorded in `manifest.kdl`.
pub const FORMAT_VERSION: i128 = 1;

const MODEL_ENTRY: &str = "model.kdl";
const MANIFEST_ENTRY: &str = "manifest.kdl";

/// An error produced while reading or writing a `.sqz` package.
#[derive(Debug)]
pub enum PackageError {
    Io(io::Error),
    Zip(String),
    /// The KDL failed to parse, or did not match the expected schema-model shape.
    Malformed(String),
}

impl fmt::Display for PackageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PackageError::Io(error) => write!(formatter, "package io error: {error}"),
            PackageError::Zip(error) => write!(formatter, "package archive error: {error}"),
            PackageError::Malformed(error) => write!(formatter, "malformed package: {error}"),
        }
    }
}

impl std::error::Error for PackageError {}

impl From<io::Error> for PackageError {
    fn from(error: io::Error) -> Self {
        PackageError::Io(error)
    }
}

impl From<zip::result::ZipError> for PackageError {
    fn from(error: zip::result::ZipError) -> Self {
        PackageError::Zip(error.to_string())
    }
}

fn malformed(message: impl Into<String>) -> PackageError {
    PackageError::Malformed(message.into())
}

// --- Public API ---------------------------------------------------------------------------------

/// Serializes a model to its canonical `model.kdl` text.
pub fn to_kdl(model: &DatabaseModel) -> String {
    let mut document = model_to_document(model);
    document.autoformat();
    document.to_string()
}

/// Parses a model from `model.kdl` text.
pub fn from_kdl(text: &str) -> Result<DatabaseModel, PackageError> {
    let document =
        KdlDocument::parse_v2(text).map_err(|error| malformed(format!("invalid KDL: {error}")))?;
    model_from_document(&document)
}

/// Writes a `.sqz` package (zip of `manifest.kdl` + `model.kdl`) to `path`.
pub fn write_package(model: &DatabaseModel, path: &Path) -> Result<(), PackageError> {
    let file = std::fs::File::create(path)?;
    write_package_to(model, file)
}

/// Reads a model back from a `.sqz` package at `path`.
pub fn read_package(path: &Path) -> Result<DatabaseModel, PackageError> {
    let file = std::fs::File::open(path)?;
    read_package_from(file)
}

/// Writes a package to any writer (used by [`write_package`]; handy for tests with a `Cursor`).
pub fn write_package_to<W: Write + io::Seek>(
    model: &DatabaseModel,
    writer: W,
) -> Result<(), PackageError> {
    let mut zip = zip::ZipWriter::new(writer);
    // Stored (no compression) keeps the dependency surface minimal; a fixed timestamp keeps the
    // archive byte-reproducible.
    let options = zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Stored)
        .last_modified_time(zip::DateTime::default());

    zip.start_file(MANIFEST_ENTRY, options)?;
    zip.write_all(manifest_kdl(model).as_bytes())?;

    zip.start_file(MODEL_ENTRY, options)?;
    zip.write_all(to_kdl(model).as_bytes())?;

    zip.finish()?;
    Ok(())
}

/// Reads a model from any package reader (used by [`read_package`]).
pub fn read_package_from<R: Read + io::Seek>(reader: R) -> Result<DatabaseModel, PackageError> {
    let mut archive = zip::ZipArchive::new(reader)?;
    let mut model_kdl = String::new();
    archive
        .by_name(MODEL_ENTRY)?
        .read_to_string(&mut model_kdl)?;
    from_kdl(&model_kdl)
}

fn manifest_kdl(model: &DatabaseModel) -> String {
    let mut document = KdlDocument::new();
    let mut manifest = KdlNode::new("manifest");
    let mut body = KdlDocument::new();

    let mut format_version = KdlNode::new("format-version");
    format_version.push(KdlEntry::new(KdlValue::Integer(FORMAT_VERSION)));
    body.nodes_mut().push(format_version);

    let mut squealy_version = KdlNode::new("squealy-version");
    squealy_version.push(KdlEntry::new(env!("CARGO_PKG_VERSION")));
    body.nodes_mut().push(squealy_version);

    let mut neutral = KdlNode::new("neutral");
    neutral.push(KdlEntry::new(KdlValue::Bool(true)));
    body.nodes_mut().push(neutral);

    // `model` is referenced so the manifest stays in step with the model it describes; richer
    // metadata (content hash, target backend) lands with later sprints.
    let _ = model;

    manifest.set_children(body);
    document.nodes_mut().push(manifest);
    document.autoformat();
    document.to_string()
}

// --- Model -> KDL -------------------------------------------------------------------------------

fn model_to_document(model: &DatabaseModel) -> KdlDocument {
    let mut document = KdlDocument::new();
    let mut database = KdlNode::new("database");
    let mut schemas = KdlDocument::new();
    for schema in &model.schemas {
        schemas.nodes_mut().push(schema_to_node(schema));
    }
    database.set_children(schemas);
    document.nodes_mut().push(database);
    document
}

fn schema_to_node(schema: &SchemaModel) -> KdlNode {
    let mut node = KdlNode::new("schema");
    if let Some(name) = &schema.name {
        node.push(KdlEntry::new(name.clone()));
    }
    let mut tables = KdlDocument::new();
    for table in &schema.tables {
        tables.nodes_mut().push(table_to_node(table));
    }
    node.set_children(tables);
    node
}

fn table_to_node(table: &TableModel) -> KdlNode {
    let mut node = KdlNode::new("table");
    node.push(KdlEntry::new(table.name.clone()));
    if let Some(comment) = &table.comment {
        node.push(KdlEntry::new_prop("comment", comment.clone()));
    }

    let mut body = KdlDocument::new();
    for column in &table.columns {
        body.nodes_mut().push(column_to_node(column));
    }
    if let Some(primary_key) = &table.primary_key {
        body.nodes_mut()
            .push(constraint_to_node("primary-key", primary_key));
    }
    for unique in &table.uniques {
        body.nodes_mut().push(constraint_to_node("unique", unique));
    }
    for foreign_key in &table.foreign_keys {
        body.nodes_mut().push(foreign_key_to_node(foreign_key));
    }
    for index in &table.indexes {
        body.nodes_mut().push(index_to_node(index));
    }
    for check in &table.checks {
        body.nodes_mut().push(check_to_node(check));
    }
    node.set_children(body);
    node
}

fn column_to_node(column: &ColumnModel) -> KdlNode {
    let mut node = KdlNode::new("column");
    node.push(KdlEntry::new(column.name.clone()));
    if let Some(comment) = &column.comment {
        node.push(KdlEntry::new_prop("comment", comment.clone()));
    }

    write_sql_type(&mut node, &column.ty);
    if column.nullable {
        node.push(KdlEntry::new_prop("nullable", KdlValue::Bool(true)));
    }
    if let Some(identity) = &column.identity {
        node.push(KdlEntry::new_prop(
            "identity",
            identity_mode(&identity.mode),
        ));
    }
    if let Some(generated) = &column.generated {
        node.push(KdlEntry::new_prop(
            "generated",
            generated_storage(&generated.storage),
        ));
        if !generated.expression.is_empty() {
            node.push(KdlEntry::new_prop(
                "generated-expr",
                generated.expression.clone(),
            ));
        }
    }
    if let Some(default) = &column.default {
        let (kind, value) = default_parts(default);
        node.push(KdlEntry::new_prop("default", kind));
        if let Some(value) = value {
            node.push(KdlEntry::new_prop("default-value", value));
        }
    }
    node
}

fn constraint_to_node(kind: &str, constraint: &Constraint) -> KdlNode {
    let mut node = KdlNode::new(kind);
    for column in &constraint.columns {
        node.push(KdlEntry::new(column.clone()));
    }
    node.push(KdlEntry::new_prop("name", constraint.name.clone()));
    node
}

fn foreign_key_to_node(foreign_key: &ForeignKeyModel) -> KdlNode {
    let mut node = KdlNode::new("foreign-key");
    for column in &foreign_key.columns {
        node.push(KdlEntry::new(column.clone()));
    }
    node.push(KdlEntry::new_prop("name", foreign_key.name.clone()));
    if let Some(schema) = &foreign_key.references_schema {
        node.push(KdlEntry::new_prop("references-schema", schema.clone()));
    }
    node.push(KdlEntry::new_prop(
        "references-table",
        foreign_key.references_table.clone(),
    ));
    if let Some(match_type) = &foreign_key.match_type {
        node.push(KdlEntry::new_prop("match", foreign_key_match(match_type)));
    }
    if let Some(deferrability) = &foreign_key.deferrability {
        node.push(KdlEntry::new_prop(
            "deferrable",
            constraint_deferrability(deferrability),
        ));
    }
    if let Some(on_delete) = &foreign_key.on_delete {
        node.push(KdlEntry::new_prop(
            "on-delete",
            foreign_key_action(on_delete),
        ));
    }
    if let Some(on_update) = &foreign_key.on_update {
        node.push(KdlEntry::new_prop(
            "on-update",
            foreign_key_action(on_update),
        ));
    }

    // Referenced columns go in a child node as separate KDL values (paired by position with the
    // local columns above), so names containing whitespace survive the round-trip.
    let mut references = KdlNode::new("references");
    for column in &foreign_key.references_columns {
        references.push(KdlEntry::new(column.clone()));
    }
    let mut children = KdlDocument::new();
    children.nodes_mut().push(references);
    node.set_children(children);

    node
}

fn index_to_node(index: &IndexModel) -> KdlNode {
    let mut node = KdlNode::new("index");
    for column in &index.columns {
        node.push(KdlEntry::new(column.clone()));
    }
    node.push(KdlEntry::new_prop("name", index.name.clone()));
    if index.unique {
        node.push(KdlEntry::new_prop("unique", KdlValue::Bool(true)));
    }
    if let Some(method) = &index.method {
        node.push(KdlEntry::new_prop("method", index_method(method)));
    }
    if let Some(predicate) = &index.predicate {
        node.push(KdlEntry::new_prop("predicate", predicate.clone()));
    }
    if !index.expressions.is_empty()
        || !index.include_columns.is_empty()
        || !index.directions.is_empty()
        || !index.nulls.is_empty()
        || !index.collations.is_empty()
        || !index.operator_classes.is_empty()
    {
        let mut children = KdlDocument::new();
        if !index.expressions.is_empty() {
            let mut expressions = KdlNode::new("expressions");
            for expression in &index.expressions {
                expressions.push(KdlEntry::new(expression.clone()));
            }
            children.nodes_mut().push(expressions);
        }
        if !index.include_columns.is_empty() {
            let mut include = KdlNode::new("include");
            for column in &index.include_columns {
                include.push(KdlEntry::new(column.clone()));
            }
            children.nodes_mut().push(include);
        }
        if !index.directions.is_empty() {
            let mut directions = KdlNode::new("directions");
            for direction in &index.directions {
                directions.push(KdlEntry::new(index_direction(direction)));
            }
            children.nodes_mut().push(directions);
        }
        if !index.nulls.is_empty() {
            let mut nulls = KdlNode::new("nulls");
            for order in &index.nulls {
                nulls.push(KdlEntry::new(index_nulls_order(order)));
            }
            children.nodes_mut().push(nulls);
        }
        for collation in &index.collations {
            children
                .nodes_mut()
                .push(index_collation_to_node(collation));
        }
        for operator_class in &index.operator_classes {
            children
                .nodes_mut()
                .push(index_operator_class_to_node(operator_class));
        }
        node.set_children(children);
    }
    node
}

fn index_collation_to_node(collation: &IndexCollation) -> KdlNode {
    let mut node = KdlNode::new("collation");
    node.push(KdlEntry::new(KdlValue::Integer(collation.position as i128)));
    node.push(KdlEntry::new(collation.name.clone()));
    node
}

fn index_operator_class_to_node(operator_class: &IndexOperatorClass) -> KdlNode {
    let mut node = KdlNode::new("operator-class");
    node.push(KdlEntry::new(KdlValue::Integer(
        operator_class.position as i128,
    )));
    node.push(KdlEntry::new(operator_class.name.clone()));
    node
}

fn check_to_node(check: &CheckModel) -> KdlNode {
    let mut node = KdlNode::new("check");
    node.push(KdlEntry::new_prop("name", check.name.clone()));
    node.push(KdlEntry::new_prop("expr", check.expression.clone()));
    node
}

fn write_sql_type(node: &mut KdlNode, ty: &SqlType) {
    let name = match ty {
        SqlType::I8 => "i8",
        SqlType::I16 => "i16",
        SqlType::I32 => "i32",
        SqlType::I64 => "i64",
        SqlType::I128 => "i128",
        SqlType::Isize => "isize",
        SqlType::U8 => "u8",
        SqlType::U16 => "u16",
        SqlType::U32 => "u32",
        SqlType::U64 => "u64",
        SqlType::U128 => "u128",
        SqlType::Usize => "usize",
        SqlType::F32 => "f32",
        SqlType::F64 => "f64",
        SqlType::String => "string",
        SqlType::Bool => "bool",
        SqlType::Varchar(_) => "varchar",
        SqlType::Char(_) => "char",
        SqlType::Text => "text",
        SqlType::Decimal { .. } => "decimal",
        SqlType::Date => "date",
        SqlType::Time { .. } => "time",
        SqlType::Timestamp { .. } => "timestamp",
        SqlType::Uuid => "uuid",
        SqlType::Json => "json",
        SqlType::Jsonb => "jsonb",
        SqlType::Bytes => "bytes",
        SqlType::Raw(_) => "raw",
    };
    node.push(KdlEntry::new_prop("type", name));

    // Structured extras (purely-default `tz=#false` is omitted).
    match ty {
        SqlType::Varchar(length) | SqlType::Char(length) => {
            node.push(KdlEntry::new_prop(
                "length",
                KdlValue::Integer(*length as i128),
            ));
        }
        SqlType::Decimal { precision, scale } => {
            node.push(KdlEntry::new_prop(
                "precision",
                KdlValue::Integer(*precision as i128),
            ));
            node.push(KdlEntry::new_prop(
                "scale",
                KdlValue::Integer(*scale as i128),
            ));
        }
        SqlType::Time { tz: true } | SqlType::Timestamp { tz: true } => {
            node.push(KdlEntry::new_prop("tz", KdlValue::Bool(true)));
        }
        SqlType::Raw(raw) => {
            node.push(KdlEntry::new_prop("raw", raw.clone()));
        }
        _ => {}
    }
}

fn default_parts(default: &DefaultValue) -> (&'static str, Option<String>) {
    match default {
        DefaultValue::Null => ("null", None),
        DefaultValue::Int(value) => ("int", Some(value.to_string())),
        DefaultValue::UInt(value) => ("uint", Some(value.to_string())),
        DefaultValue::Float(value) => ("float", Some(value.to_string())),
        DefaultValue::Text(value) => ("text", Some(value.clone())),
        DefaultValue::Bool(value) => ("bool", Some(value.to_string())),
        DefaultValue::CurrentTimestamp => ("current_timestamp", None),
        DefaultValue::CurrentDate => ("current_date", None),
        DefaultValue::CurrentTime => ("current_time", None),
        DefaultValue::Raw(value) => ("raw", Some(value.clone())),
    }
}

// --- KDL -> Model -------------------------------------------------------------------------------

fn model_from_document(document: &KdlDocument) -> Result<DatabaseModel, PackageError> {
    let database = document
        .nodes()
        .iter()
        .find(|node| node.name().value() == "database")
        .ok_or_else(|| malformed("missing `database` node"))?;

    let schemas = child_nodes(database, "schema")
        .map(schema_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DatabaseModel { schemas })
}

fn schema_from_node(node: &KdlNode) -> Result<SchemaModel, PackageError> {
    let tables = child_nodes(node, "table")
        .map(table_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SchemaModel {
        name: first_arg(node).map(str::to_owned),
        tables,
    })
}

fn table_from_node(node: &KdlNode) -> Result<TableModel, PackageError> {
    let name = first_arg(node)
        .ok_or_else(|| malformed("`table` is missing its name"))?
        .to_owned();

    let columns = child_nodes(node, "column")
        .map(column_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let primary_key = child_nodes(node, "primary-key")
        .next()
        .map(constraint_from_node)
        .transpose()?;
    let uniques = child_nodes(node, "unique")
        .map(constraint_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let foreign_keys = child_nodes(node, "foreign-key")
        .map(foreign_key_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let indexes = child_nodes(node, "index")
        .map(index_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let checks = child_nodes(node, "check")
        .map(check_from_node)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(TableModel {
        name,
        comment: prop(node, "comment").map(str::to_owned),
        columns,
        primary_key,
        foreign_keys,
        uniques,
        checks,
        indexes,
    })
}

fn column_from_node(node: &KdlNode) -> Result<ColumnModel, PackageError> {
    let name = first_arg(node)
        .ok_or_else(|| malformed("`column` is missing its name"))?
        .to_owned();
    let ty = sql_type_from_node(node)?;
    let default = default_from_node(node)?;
    Ok(ColumnModel {
        name,
        comment: prop(node, "comment").map(str::to_owned),
        ty,
        nullable: prop_bool(node, "nullable"),
        default,
        identity: identity_from_node(node)?,
        generated: generated_from_node(node)?,
    })
}

fn constraint_from_node(node: &KdlNode) -> Result<Constraint, PackageError> {
    Ok(Constraint {
        name: required_prop(node, "name")?,
        columns: args(node),
    })
}

fn foreign_key_from_node(node: &KdlNode) -> Result<ForeignKeyModel, PackageError> {
    Ok(ForeignKeyModel {
        name: required_prop(node, "name")?,
        columns: args(node),
        references_schema: prop(node, "references-schema").map(str::to_owned),
        references_table: required_prop(node, "references-table")?,
        references_columns: child_nodes(node, "references")
            .next()
            .map(args)
            .unwrap_or_default(),
        match_type: prop(node, "match").map(ForeignKeyMatch::from_sql),
        deferrability: prop(node, "deferrable").map(ConstraintDeferrability::from_sql),
        on_delete: prop(node, "on-delete").map(ForeignKeyAction::from_sql),
        on_update: prop(node, "on-update").map(ForeignKeyAction::from_sql),
    })
}

fn index_from_node(node: &KdlNode) -> Result<IndexModel, PackageError> {
    Ok(IndexModel {
        name: required_prop(node, "name")?,
        columns: args(node),
        expressions: child_nodes(node, "expressions")
            .next()
            .map(args)
            .unwrap_or_default(),
        include_columns: child_nodes(node, "include")
            .next()
            .map(args)
            .unwrap_or_default(),
        unique: prop_bool(node, "unique"),
        method: prop(node, "method").map(IndexMethod::from_sql),
        directions: child_nodes(node, "directions")
            .next()
            .map(index_directions_from_node)
            .transpose()?
            .unwrap_or_default(),
        nulls: child_nodes(node, "nulls")
            .next()
            .map(index_nulls_from_node)
            .transpose()?
            .unwrap_or_default(),
        collations: child_nodes(node, "collation")
            .map(index_collation_from_node)
            .collect::<Result<Vec<_>, _>>()?,
        operator_classes: child_nodes(node, "operator-class")
            .map(index_operator_class_from_node)
            .collect::<Result<Vec<_>, _>>()?,
        predicate: prop(node, "predicate").map(str::to_owned),
    })
}

fn check_from_node(node: &KdlNode) -> Result<CheckModel, PackageError> {
    Ok(CheckModel {
        name: required_prop(node, "name")?,
        expression: required_prop(node, "expr")?,
    })
}

fn sql_type_from_node(node: &KdlNode) -> Result<SqlType, PackageError> {
    let name = prop(node, "type").ok_or_else(|| malformed("`column` is missing `type`"))?;
    Ok(match name {
        "i8" => SqlType::I8,
        "i16" => SqlType::I16,
        "i32" => SqlType::I32,
        "i64" => SqlType::I64,
        "i128" => SqlType::I128,
        "isize" => SqlType::Isize,
        "u8" => SqlType::U8,
        "u16" => SqlType::U16,
        "u32" => SqlType::U32,
        "u64" => SqlType::U64,
        "u128" => SqlType::U128,
        "usize" => SqlType::Usize,
        "f32" => SqlType::F32,
        "f64" => SqlType::F64,
        "string" => SqlType::String,
        "bool" => SqlType::Bool,
        "varchar" => SqlType::Varchar(required_u32(node, "length")?),
        "char" => SqlType::Char(required_u32(node, "length")?),
        "text" => SqlType::Text,
        "decimal" => SqlType::Decimal {
            precision: required_u32(node, "precision")?,
            scale: required_u32(node, "scale")?,
        },
        "date" => SqlType::Date,
        "time" => SqlType::Time {
            tz: prop_bool(node, "tz"),
        },
        "timestamp" => SqlType::Timestamp {
            tz: prop_bool(node, "tz"),
        },
        "uuid" => SqlType::Uuid,
        "json" => SqlType::Json,
        "jsonb" => SqlType::Jsonb,
        "bytes" => SqlType::Bytes,
        "raw" => SqlType::Raw(required_prop(node, "raw")?),
        other => return Err(malformed(format!("unknown column type `{other}`"))),
    })
}

/// Reads a required `u32` integer property (length/precision/scale).
fn required_u32(node: &KdlNode, key: &str) -> Result<u32, PackageError> {
    let value = node
        .entries()
        .iter()
        .find(|entry| entry.name().map(|name| name.value()) == Some(key))
        .and_then(|entry| entry.value().as_integer())
        .ok_or_else(|| {
            malformed(format!(
                "`{}` is missing integer `{key}`",
                node.name().value()
            ))
        })?;
    u32::try_from(value).map_err(|_| malformed(format!("`{key}` is out of range for u32")))
}

fn default_from_node(node: &KdlNode) -> Result<Option<DefaultValue>, PackageError> {
    let Some(kind) = prop(node, "default") else {
        return Ok(None);
    };
    let value = || required_prop(node, "default-value");
    let parsed = |label: &str| -> Result<String, PackageError> {
        value().map_err(|_| malformed(format!("`default` {label} is missing `default-value`")))
    };
    Ok(Some(match kind {
        "null" => DefaultValue::Null,
        "current_timestamp" => DefaultValue::CurrentTimestamp,
        "current_date" => DefaultValue::CurrentDate,
        "current_time" => DefaultValue::CurrentTime,
        "int" => DefaultValue::Int(
            parsed("int")?
                .parse()
                .map_err(|_| malformed("invalid integer default"))?,
        ),
        "uint" => DefaultValue::UInt(
            parsed("uint")?
                .parse()
                .map_err(|_| malformed("invalid unsigned default"))?,
        ),
        "float" => DefaultValue::Float(
            parsed("float")?
                .parse()
                .map_err(|_| malformed("invalid float default"))?,
        ),
        "bool" => DefaultValue::Bool(
            parsed("bool")?
                .parse()
                .map_err(|_| malformed("invalid bool default"))?,
        ),
        "text" => DefaultValue::Text(parsed("text")?),
        "raw" => DefaultValue::Raw(parsed("raw")?),
        other => return Err(malformed(format!("unknown default kind `{other}`"))),
    }))
}

// --- KDL node accessors -------------------------------------------------------------------------

fn child_nodes<'a>(node: &'a KdlNode, name: &'a str) -> impl Iterator<Item = &'a KdlNode> {
    node.children()
        .into_iter()
        .flat_map(KdlDocument::nodes)
        .filter(move |child| child.name().value() == name)
}

fn first_arg(node: &KdlNode) -> Option<&str> {
    node.entries()
        .iter()
        .find(|entry| entry.name().is_none())
        .and_then(|entry| entry.value().as_string())
}

fn args(node: &KdlNode) -> Vec<String> {
    node.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .filter_map(|entry| entry.value().as_string().map(str::to_owned))
        .collect()
}

fn prop<'a>(node: &'a KdlNode, key: &str) -> Option<&'a str> {
    node.entries()
        .iter()
        .find(|entry| entry.name().map(|name| name.value()) == Some(key))
        .and_then(|entry| entry.value().as_string())
}

fn prop_bool(node: &KdlNode, key: &str) -> bool {
    node.entries()
        .iter()
        .find(|entry| entry.name().map(|name| name.value()) == Some(key))
        .and_then(|entry| entry.value().as_bool())
        .unwrap_or(false)
}

fn required_prop(node: &KdlNode, key: &str) -> Result<String, PackageError> {
    prop(node, key)
        .map(str::to_owned)
        .ok_or_else(|| malformed(format!("`{}` is missing `{key}`", node.name().value())))
}

fn identity_mode(mode: &IdentityMode) -> &'static str {
    match mode {
        IdentityMode::Always => "always",
        IdentityMode::ByDefault => "by-default",
        IdentityMode::AutoIncrement => "auto-increment",
    }
}

fn generated_storage(storage: &GeneratedStorage) -> &'static str {
    match storage {
        GeneratedStorage::Virtual => "virtual",
        GeneratedStorage::Stored => "stored",
        GeneratedStorage::Unknown => "unknown",
    }
}

fn identity_from_node(node: &KdlNode) -> Result<Option<IdentityModel>, PackageError> {
    let mode = if let Some(mode) = prop(node, "identity") {
        match mode {
            "always" => IdentityMode::Always,
            "by-default" => IdentityMode::ByDefault,
            "auto-increment" => IdentityMode::AutoIncrement,
            other => {
                return Err(malformed(format!(
                    "`{}` has unsupported identity mode `{other}`",
                    node.name().value()
                )));
            }
        }
    } else if prop_bool(node, "auto-increment") {
        IdentityMode::AutoIncrement
    } else {
        return Ok(None);
    };

    Ok(Some(IdentityModel { mode }))
}

fn generated_from_node(node: &KdlNode) -> Result<Option<GeneratedColumnModel>, PackageError> {
    let storage = if let Some(storage) = prop(node, "generated") {
        match storage {
            "virtual" => GeneratedStorage::Virtual,
            "stored" => GeneratedStorage::Stored,
            "unknown" => GeneratedStorage::Unknown,
            other => {
                return Err(malformed(format!(
                    "`{}` has unsupported generated storage `{other}`",
                    node.name().value()
                )));
            }
        }
    } else if prop_bool(node, "generated") {
        GeneratedStorage::Unknown
    } else {
        return Ok(None);
    };

    Ok(Some(GeneratedColumnModel {
        expression: prop(node, "generated-expr").unwrap_or_default().to_owned(),
        storage,
    }))
}

fn foreign_key_action(action: &ForeignKeyAction) -> &str {
    match action {
        ForeignKeyAction::NoAction => "no-action",
        ForeignKeyAction::Restrict => "restrict",
        ForeignKeyAction::Cascade => "cascade",
        ForeignKeyAction::SetNull => "set-null",
        ForeignKeyAction::SetDefault => "set-default",
        ForeignKeyAction::Raw(action) => action,
    }
}

fn foreign_key_match(match_type: &ForeignKeyMatch) -> &str {
    match match_type {
        ForeignKeyMatch::Simple => "simple",
        ForeignKeyMatch::Partial => "partial",
        ForeignKeyMatch::Full => "full",
        ForeignKeyMatch::Raw(match_type) => match_type,
    }
}

fn constraint_deferrability(deferrability: &ConstraintDeferrability) -> &str {
    match deferrability {
        ConstraintDeferrability::InitiallyImmediate => "initially-immediate",
        ConstraintDeferrability::InitiallyDeferred => "initially-deferred",
        ConstraintDeferrability::Raw(deferrability) => deferrability,
    }
}

fn index_method(method: &IndexMethod) -> &str {
    match method {
        IndexMethod::BTree => "btree",
        IndexMethod::Hash => "hash",
        IndexMethod::Gin => "gin",
        IndexMethod::Gist => "gist",
        IndexMethod::SpGist => "spgist",
        IndexMethod::Brin => "brin",
        IndexMethod::Raw(method) => method,
    }
}

fn index_direction(direction: &IndexDirection) -> &'static str {
    match direction {
        IndexDirection::Asc => "asc",
        IndexDirection::Desc => "desc",
    }
}

fn index_nulls_order(order: &IndexNullsOrder) -> &'static str {
    match order {
        IndexNullsOrder::First => "first",
        IndexNullsOrder::Last => "last",
    }
}

fn index_directions_from_node(node: &KdlNode) -> Result<Vec<IndexDirection>, PackageError> {
    args(node)
        .into_iter()
        .map(|direction| match direction.as_str() {
            "asc" => Ok(IndexDirection::Asc),
            "desc" => Ok(IndexDirection::Desc),
            other => Err(malformed(format!(
                "`directions` has unsupported index direction `{other}`"
            ))),
        })
        .collect()
}

fn index_nulls_from_node(node: &KdlNode) -> Result<Vec<IndexNullsOrder>, PackageError> {
    args(node)
        .into_iter()
        .map(|order| match order.as_str() {
            "first" => Ok(IndexNullsOrder::First),
            "last" => Ok(IndexNullsOrder::Last),
            other => Err(malformed(format!(
                "`nulls` has unsupported index null ordering `{other}`"
            ))),
        })
        .collect()
}

fn index_operator_class_from_node(node: &KdlNode) -> Result<IndexOperatorClass, PackageError> {
    let (position, name) = positioned_index_metadata_from_node(node, "operator-class")?;
    Ok(IndexOperatorClass { position, name })
}

fn index_collation_from_node(node: &KdlNode) -> Result<IndexCollation, PackageError> {
    let (position, name) = positioned_index_metadata_from_node(node, "collation")?;
    Ok(IndexCollation { position, name })
}

fn positioned_index_metadata_from_node(
    node: &KdlNode,
    kind: &str,
) -> Result<(usize, String), PackageError> {
    let mut args = node
        .entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .map(KdlEntry::value);
    let position = args
        .next()
        .and_then(KdlValue::as_integer)
        .ok_or_else(|| malformed(format!("`{kind}` is missing integer position")))?;
    let position = usize::try_from(position)
        .map_err(|_| malformed(format!("`{kind}` position is out of range for usize")))?;
    let name = args
        .next()
        .and_then(KdlValue::as_string)
        .ok_or_else(|| malformed(format!("`{kind}` is missing name")))?
        .to_owned();
    Ok((position, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample_model() -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: Some("public".to_owned()),
                tables: vec![
                    TableModel {
                        name: "orgs".to_owned(),
                        comment: Some("Organizations in the catalog".to_owned()),
                        columns: vec![
                            ColumnModel {
                                name: "id".to_owned(),
                                comment: Some("Synthetic organization id".to_owned()),
                                ty: SqlType::I32,
                                nullable: false,
                                default: None,
                                identity: Some(IdentityModel {
                                    mode: IdentityMode::ByDefault,
                                }),
                                generated: None,
                            },
                            ColumnModel {
                                name: "slug".to_owned(),
                                comment: Some("Stable organization slug".to_owned()),
                                ty: SqlType::String,
                                nullable: false,
                                default: Some(DefaultValue::Text("acme".to_owned())),
                                identity: None,
                                generated: None,
                            },
                            ColumnModel {
                                name: "metadata".to_owned(),
                                comment: None,
                                ty: SqlType::Raw("jsonb".to_owned()),
                                nullable: true,
                                default: Some(DefaultValue::Raw("'{}'::jsonb".to_owned())),
                                identity: None,
                                generated: None,
                            },
                        ],
                        primary_key: Some(Constraint {
                            name: "pk_orgs".to_owned(),
                            columns: vec!["id".to_owned()],
                        }),
                        foreign_keys: vec![],
                        uniques: vec![Constraint {
                            name: "uq_orgs_slug".to_owned(),
                            columns: vec!["slug".to_owned()],
                        }],
                        checks: vec![CheckModel {
                            name: "ck_orgs_slug".to_owned(),
                            expression: "length(slug) > 0".to_owned(),
                        }],
                        indexes: vec![IndexModel {
                            name: "uq_orgs_slug_idx".to_owned(),
                            columns: vec!["slug".to_owned()],
                            expressions: Vec::new(),
                            include_columns: Vec::new(),
                            unique: true,
                            method: None,
                            directions: Vec::new(),
                            nulls: Vec::new(),
                            collations: Vec::new(),
                            operator_classes: Vec::new(),
                            predicate: None,
                        }],
                    },
                    TableModel {
                        name: "members".to_owned(),
                        comment: None,
                        columns: vec![ColumnModel {
                            name: "org_id".to_owned(),
                            comment: None,
                            ty: SqlType::I32,
                            nullable: false,
                            default: None,
                            identity: None,
                            generated: None,
                        }],
                        primary_key: None,
                        foreign_keys: vec![ForeignKeyModel {
                            name: "fk_members_org_id".to_owned(),
                            columns: vec!["org_id".to_owned()],
                            references_schema: Some("public".to_owned()),
                            references_table: "orgs".to_owned(),
                            references_columns: vec!["id".to_owned()],
                            match_type: None,
                            deferrability: None,
                            on_delete: Some(ForeignKeyAction::Cascade),
                            on_update: None,
                        }],
                        uniques: vec![],
                        checks: vec![],
                        indexes: vec![],
                    },
                ],
            }],
        }
    }

    #[test]
    fn kdl_round_trips() {
        let model = sample_model();
        let kdl = to_kdl(&model);
        assert!(kdl.contains("comment=\"Organizations in the catalog\""));
        assert!(kdl.contains("comment=\"Synthetic organization id\""));
        let parsed = from_kdl(&kdl).expect("model.kdl should parse");
        assert_eq!(parsed, model, "KDL round-trip diverged:\n{kdl}");
    }

    #[test]
    fn kdl_is_deterministic() {
        let model = sample_model();
        assert_eq!(to_kdl(&model), to_kdl(&model));
    }

    #[test]
    fn package_zip_round_trips() {
        let model = sample_model();
        let mut buffer = Vec::new();
        write_package_to(&model, Cursor::new(&mut buffer)).expect("write package");
        let parsed = read_package_from(Cursor::new(buffer)).expect("read package");
        assert_eq!(parsed, model);
    }

    #[test]
    fn kdl_round_trips_names_with_whitespace() {
        // Column names can contain whitespace (e.g. `#[column(name = "user id")]`); local and
        // referenced foreign-key columns must survive the round-trip as distinct values.
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "user id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        nullable: false,
                        default: None,
                        identity: None,
                        generated: None,
                    }],
                    primary_key: None,
                    foreign_keys: vec![ForeignKeyModel {
                        name: "fk_events_user_id".to_owned(),
                        columns: vec!["user id".to_owned()],
                        references_schema: None,
                        references_table: "users".to_owned(),
                        references_columns: vec!["account id".to_owned()],
                        match_type: None,
                        deferrability: None,
                        on_delete: None,
                        on_update: None,
                    }],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let parsed = from_kdl(&to_kdl(&model)).expect("parse");
        assert_eq!(parsed, model);
    }

    #[test]
    fn kdl_round_trips_structured_types() {
        // Every structured SqlType variant must survive the KDL encode/decode, including the
        // parametric ones (length/precision/scale) and the tz flag on time/timestamp.
        let types = [
            SqlType::Varchar(64),
            SqlType::Char(2),
            SqlType::Text,
            SqlType::Decimal {
                precision: 10,
                scale: 2,
            },
            SqlType::Date,
            SqlType::Time { tz: false },
            SqlType::Time { tz: true },
            SqlType::Timestamp { tz: false },
            SqlType::Timestamp { tz: true },
            SqlType::Uuid,
            SqlType::Json,
            SqlType::Jsonb,
            SqlType::Bytes,
            SqlType::Raw("citext".to_owned()),
        ];

        let columns = types
            .iter()
            .enumerate()
            .map(|(index, ty)| ColumnModel {
                name: format!("c{index}"),
                comment: None,
                ty: ty.clone(),
                nullable: false,
                default: None,
                identity: None,
                generated: None,
            })
            .collect();

        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "structured".to_owned(),
                    comment: None,
                    columns,
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(parsed, model, "structured-type round-trip diverged:\n{kdl}");
    }

    #[test]
    fn kdl_round_trips_identity_and_generated_columns() {
        let columns = vec![
            ColumnModel {
                name: "id_always".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: false,
                default: None,
                identity: Some(IdentityModel {
                    mode: IdentityMode::Always,
                }),
                generated: None,
            },
            ColumnModel {
                name: "id_by_default".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: false,
                default: None,
                identity: Some(IdentityModel {
                    mode: IdentityMode::ByDefault,
                }),
                generated: None,
            },
            ColumnModel {
                name: "id_auto_increment".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: false,
                default: None,
                identity: Some(IdentityModel {
                    mode: IdentityMode::AutoIncrement,
                }),
                generated: None,
            },
            ColumnModel {
                name: "virtual_generated".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: true,
                default: None,
                identity: None,
                generated: Some(GeneratedColumnModel {
                    expression: "length(slug)".to_owned(),
                    storage: GeneratedStorage::Virtual,
                }),
            },
            ColumnModel {
                name: "stored_generated".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: true,
                default: None,
                identity: None,
                generated: Some(GeneratedColumnModel {
                    expression: "char_length(`slug`)".to_owned(),
                    storage: GeneratedStorage::Stored,
                }),
            },
            ColumnModel {
                name: "unknown_generated".to_owned(),
                comment: None,
                ty: SqlType::I32,
                nullable: true,
                default: None,
                identity: None,
                generated: Some(GeneratedColumnModel {
                    expression: String::new(),
                    storage: GeneratedStorage::Unknown,
                }),
            },
        ];
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "derived_columns".to_owned(),
                    comment: None,
                    columns,
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "identity/generated round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_foreign_key_actions() {
        let actions = [
            ForeignKeyAction::NoAction,
            ForeignKeyAction::Restrict,
            ForeignKeyAction::Cascade,
            ForeignKeyAction::SetNull,
            ForeignKeyAction::SetDefault,
            ForeignKeyAction::Raw("match full".to_owned()),
        ];
        let foreign_keys = actions
            .iter()
            .enumerate()
            .map(|(index, action)| ForeignKeyModel {
                name: format!("fk_child_parent_{index}"),
                columns: vec![format!("parent_id_{index}")],
                references_schema: Some("public".to_owned()),
                references_table: "parents".to_owned(),
                references_columns: vec!["id".to_owned()],
                match_type: None,
                deferrability: None,
                on_delete: Some(action.clone()),
                on_update: Some(action.clone()),
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "children".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys,
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "foreign-key action round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_foreign_key_match_types() {
        let match_types = [
            ForeignKeyMatch::Simple,
            ForeignKeyMatch::Partial,
            ForeignKeyMatch::Full,
            ForeignKeyMatch::Raw("backend-specific".to_owned()),
        ];
        let foreign_keys = match_types
            .iter()
            .enumerate()
            .map(|(index, match_type)| ForeignKeyModel {
                name: format!("fk_child_parent_match_{index}"),
                columns: vec![format!("parent_id_{index}")],
                references_schema: Some("public".to_owned()),
                references_table: "parents".to_owned(),
                references_columns: vec!["id".to_owned()],
                match_type: Some(match_type.clone()),
                deferrability: None,
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "children".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys,
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        assert!(kdl.contains("match=full"));
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "foreign-key match round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_foreign_key_deferrability() {
        let values = [
            ConstraintDeferrability::InitiallyImmediate,
            ConstraintDeferrability::InitiallyDeferred,
            ConstraintDeferrability::Raw("backend-specific".to_owned()),
        ];
        let foreign_keys = values
            .iter()
            .enumerate()
            .map(|(index, deferrability)| ForeignKeyModel {
                name: format!("fk_child_parent_deferrable_{index}"),
                columns: vec![format!("parent_id_{index}")],
                references_schema: Some("public".to_owned()),
                references_table: "parents".to_owned(),
                references_columns: vec!["id".to_owned()],
                match_type: None,
                deferrability: Some(deferrability.clone()),
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "children".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys,
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        assert!(kdl.contains("deferrable=initially-deferred"));
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "foreign-key deferrability round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_index_methods() {
        let methods = [
            IndexMethod::BTree,
            IndexMethod::Hash,
            IndexMethod::Gin,
            IndexMethod::Gist,
            IndexMethod::SpGist,
            IndexMethod::Brin,
            IndexMethod::Raw("custom_method".to_owned()),
        ];
        let indexes = methods
            .iter()
            .enumerate()
            .map(|(index, method)| IndexModel {
                name: format!("idx_events_{index}"),
                columns: vec!["event_id".to_owned()],
                expressions: Vec::new(),
                include_columns: Vec::new(),
                unique: false,
                method: Some(method.clone()),
                directions: vec![IndexDirection::Desc],
                nulls: Vec::new(),
                collations: Vec::new(),
                operator_classes: Vec::new(),
                predicate: Some("event_id IS NOT NULL".to_owned()),
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes,
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(parsed, model, "index method round-trip diverged:\n{kdl}");
    }

    #[test]
    fn kdl_round_trips_index_expressions() {
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![IndexModel {
                        name: "idx_events_lower_name".to_owned(),
                        columns: Vec::new(),
                        expressions: vec!["lower(event_name)".to_owned()],
                        include_columns: Vec::new(),
                        unique: false,
                        method: Some(IndexMethod::BTree),
                        directions: vec![IndexDirection::Asc],
                        nulls: Vec::new(),
                        collations: vec![IndexCollation {
                            position: 0,
                            name: "C".to_owned(),
                        }],
                        operator_classes: vec![IndexOperatorClass {
                            position: 0,
                            name: "text_pattern_ops".to_owned(),
                        }],
                        predicate: None,
                    }],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "index expression round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_index_include_columns() {
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys: vec![],
                    uniques: vec![],
                    checks: vec![],
                    indexes: vec![IndexModel {
                        name: "idx_events_org_id".to_owned(),
                        columns: vec!["org_id".to_owned()],
                        expressions: Vec::new(),
                        include_columns: vec!["event_name".to_owned()],
                        unique: false,
                        method: Some(IndexMethod::BTree),
                        directions: vec![IndexDirection::Asc],
                        nulls: vec![IndexNullsOrder::Last],
                        collations: Vec::new(),
                        operator_classes: Vec::new(),
                        predicate: None,
                    }],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "index include column round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_reads_legacy_identity_and_generated_flags() {
        let kdl = r#"
database {
    schema {
        table "legacy_columns" {
            column "id" type="i32" auto-increment=#true
            column "computed" type="i32" generated=#true
        }
    }
}
"#;

        let parsed = from_kdl(kdl).expect("legacy model.kdl should parse");
        let columns = &parsed.schemas[0].tables[0].columns;
        assert_eq!(
            columns[0].identity,
            Some(IdentityModel {
                mode: IdentityMode::AutoIncrement
            })
        );
        assert_eq!(
            columns[1].generated,
            Some(GeneratedColumnModel {
                expression: String::new(),
                storage: GeneratedStorage::Unknown
            })
        );
    }
}
