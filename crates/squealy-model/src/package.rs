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
//! ├── model.kdl      the DatabaseModel
//! └── refactor.kdl   optional explicit refactor operations
//! ```

use std::io::{self, Read, Write};
use std::path::Path;

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use squealy::{
    CheckModel, ColumnModel, Constraint, ConstraintDeferrability, ConstraintEnforcement,
    ConstraintValidation, DatabaseModel, DefaultValue, ExprFragment, ForeignKeyAction,
    ForeignKeyMatch, ForeignKeyModel, GeneratedColumnModel, GeneratedStorage, IdentityMode,
    IdentityModel, IndexCollation, IndexDirection, IndexMethod, IndexModel, IndexNullsOrder,
    IndexOperatorClass, JoinItem, JoinKind, OrderDirection, OrderItem, OrderNulls, ProjectionItem,
    SchemaModel, SourceRef, SqlType, TableModel, ViewColumnModel, ViewModel, ViewQueryModel,
};

use crate::{CastColumn, RefactorLog, RefactorOperation, RenameColumn, RenameTable};

/// Current package format version, recorded in `manifest.kdl`.
pub const FORMAT_VERSION: i128 = 1;
pub const PACKAGE_FORMAT_VERSION_METADATA_KEY: &str = "package.format_version";
pub const PACKAGE_CONTENT_HASH_METADATA_KEY: &str = "package.content_hash";
pub const SQUEALY_MODEL_VERSION_METADATA_KEY: &str = "squealy_model.version";

const MODEL_ENTRY: &str = "model.kdl";
const MANIFEST_ENTRY: &str = "manifest.kdl";
const REFACTOR_ENTRY: &str = "refactor.kdl";

/// Maximum number of bytes read from any single package entry. Reading is bounded so a malicious
/// or corrupt archive (for example a zip bomb that declares a huge uncompressed size) cannot
/// exhaust memory. 64 MiB is far larger than any realistic schema document.
const MAX_ENTRY_BYTES: u64 = 64 * 1024 * 1024;

/// An error produced while reading or writing a `.sqz` package.
#[derive(Debug, thiserror::Error)]
pub enum PackageError {
    #[error("package io error: {0}")]
    Io(#[from] io::Error),
    #[error("package archive error: {0}")]
    Zip(String),
    /// The KDL failed to parse, or did not match the expected schema-model shape.
    #[error("malformed package: {0}")]
    Malformed(String),
    /// A package entry was larger than [`MAX_ENTRY_BYTES`], so it was refused before being read
    /// fully into memory.
    #[error("package entry `{entry}` exceeds the {limit}-byte read limit")]
    TooLarge { entry: &'static str, limit: u64 },
}

impl From<zip::result::ZipError> for PackageError {
    fn from(error: zip::result::ZipError) -> Self {
        PackageError::Zip(error.to_string())
    }
}

fn malformed(message: impl Into<String>) -> PackageError {
    PackageError::Malformed(message.into())
}

/// Reads a package `entry` into a string, refusing anything larger than [`MAX_ENTRY_BYTES`] so a
/// hostile archive cannot exhaust memory. Memory use is bounded regardless of the size the archive
/// header declares, because the reader itself is capped.
fn read_entry_to_string(entry: impl Read, name: &'static str) -> Result<String, PackageError> {
    read_entry_to_string_limited(entry, name, MAX_ENTRY_BYTES)
}

fn read_entry_to_string_limited(
    entry: impl Read,
    name: &'static str,
    limit: u64,
) -> Result<String, PackageError> {
    let mut text = String::new();
    let read = entry.take(limit + 1).read_to_string(&mut text)?;
    if read as u64 > limit {
        return Err(PackageError::TooLarge { entry: name, limit });
    }
    Ok(text)
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

/// Writes a `.sqz` package including an optional `refactor.kdl` log.
pub fn write_package_with_refactors(
    model: &DatabaseModel,
    refactors: &RefactorLog,
    path: &Path,
) -> Result<(), PackageError> {
    let file = std::fs::File::create(path)?;
    write_package_with_refactors_to(model, refactors, file)
}

/// Reads a model back from a `.sqz` package at `path`.
pub fn read_package(path: &Path) -> Result<DatabaseModel, PackageError> {
    let file = std::fs::File::open(path)?;
    read_package_from(file)
}

/// Reads the optional refactor log back from a `.sqz` package at `path`.
pub fn read_refactor_log(path: &Path) -> Result<RefactorLog, PackageError> {
    let file = std::fs::File::open(path)?;
    read_refactor_log_from_package(file)
}

/// Writes a package to any writer (used by [`write_package`]; handy for tests with a `Cursor`).
pub fn write_package_to<W: Write + io::Seek>(
    model: &DatabaseModel,
    writer: W,
) -> Result<(), PackageError> {
    write_package_with_refactors_to(model, &RefactorLog::default(), writer)
}

/// Writes a package to any writer, optionally including `refactor.kdl`.
pub fn write_package_with_refactors_to<W: Write + io::Seek>(
    model: &DatabaseModel,
    refactors: &RefactorLog,
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

    if !refactors.is_empty() {
        zip.start_file(REFACTOR_ENTRY, options)?;
        zip.write_all(refactor_to_kdl(refactors).as_bytes())?;
    }

    zip.finish()?;
    Ok(())
}

/// Reads a model from any package reader (used by [`read_package`]).
pub fn read_package_from<R: Read + io::Seek>(reader: R) -> Result<DatabaseModel, PackageError> {
    let mut archive = zip::ZipArchive::new(reader)?;
    let model_kdl = read_entry_to_string(archive.by_name(MODEL_ENTRY)?, MODEL_ENTRY)?;
    from_kdl(&model_kdl)
}

/// Reads the optional refactor log from any package reader.
///
/// Packages without `refactor.kdl` return an empty log, so older packages remain readable.
pub fn read_refactor_log_from_package<R: Read + io::Seek>(
    reader: R,
) -> Result<RefactorLog, PackageError> {
    let mut archive = zip::ZipArchive::new(reader)?;
    let Ok(entry) = archive.by_name(REFACTOR_ENTRY) else {
        return Ok(RefactorLog::default());
    };
    let refactor_kdl = read_entry_to_string(entry, REFACTOR_ENTRY)?;
    refactor_from_kdl(&refactor_kdl)
}

/// Serializes a refactor log to canonical `refactor.kdl` text.
pub fn refactor_to_kdl(refactors: &RefactorLog) -> String {
    let mut document = refactor_to_document(refactors);
    document.autoformat();
    document.to_string()
}

/// Returns backend metadata entries that describe a desired package.
pub fn package_metadata(model: &DatabaseModel, refactors: &RefactorLog) -> Vec<(String, String)> {
    vec![
        (
            PACKAGE_FORMAT_VERSION_METADATA_KEY.to_owned(),
            FORMAT_VERSION.to_string(),
        ),
        (
            PACKAGE_CONTENT_HASH_METADATA_KEY.to_owned(),
            package_content_hash(model, refactors),
        ),
        (
            SQUEALY_MODEL_VERSION_METADATA_KEY.to_owned(),
            env!("CARGO_PKG_VERSION").to_owned(),
        ),
    ]
}

/// Computes a deterministic fingerprint over canonical package content.
pub fn package_content_hash(model: &DatabaseModel, refactors: &RefactorLog) -> String {
    let mut hash = Fnv1a64::new();
    hash.write(b"manifest.kdl\0");
    hash.write(manifest_kdl(model).as_bytes());
    hash.write(b"\0model.kdl\0");
    hash.write(to_kdl(model).as_bytes());
    if !refactors.is_empty() {
        hash.write(b"\0refactor.kdl\0");
        hash.write(refactor_to_kdl(refactors).as_bytes());
    }
    format!("fnv1a64:{:016x}", hash.finish())
}

/// Parses a refactor log from `refactor.kdl` text.
pub fn refactor_from_kdl(text: &str) -> Result<RefactorLog, PackageError> {
    let document =
        KdlDocument::parse_v2(text).map_err(|error| malformed(format!("invalid KDL: {error}")))?;
    refactor_from_document(&document)
}

struct Fnv1a64 {
    value: u64,
}

impl Fnv1a64 {
    fn new() -> Self {
        Self {
            value: 0xcbf29ce484222325,
        }
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.value ^= u64::from(*byte);
            self.value = self.value.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(self) -> u64 {
        self.value
    }
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

// --- RefactorLog <-> KDL ------------------------------------------------------------------------

fn refactor_to_document(refactors: &RefactorLog) -> KdlDocument {
    let mut document = KdlDocument::new();
    let mut root = KdlNode::new("refactors");
    let mut body = KdlDocument::new();

    for operation in &refactors.operations {
        body.nodes_mut().push(refactor_operation_to_node(operation));
    }

    root.set_children(body);
    document.nodes_mut().push(root);
    document
}

fn refactor_operation_to_node(operation: &RefactorOperation) -> KdlNode {
    match operation {
        RefactorOperation::RenameTable(operation) => {
            let mut node = KdlNode::new("rename-table");
            node.push(KdlEntry::new_prop("id", operation.id.clone()));
            if let Some(schema) = &operation.schema {
                node.push(KdlEntry::new_prop("schema", schema.clone()));
            }
            node.push(KdlEntry::new_prop("from", operation.from.clone()));
            node.push(KdlEntry::new_prop("to", operation.to.clone()));
            node
        }
        RefactorOperation::RenameColumn(operation) => {
            let mut node = KdlNode::new("rename-column");
            node.push(KdlEntry::new_prop("id", operation.id.clone()));
            if let Some(schema) = &operation.schema {
                node.push(KdlEntry::new_prop("schema", schema.clone()));
            }
            node.push(KdlEntry::new_prop("table", operation.table.clone()));
            node.push(KdlEntry::new_prop("from", operation.from.clone()));
            node.push(KdlEntry::new_prop("to", operation.to.clone()));
            node
        }
        RefactorOperation::CastColumn(operation) => {
            let mut node = KdlNode::new("cast-column");
            node.push(KdlEntry::new_prop("id", operation.id.clone()));
            if let Some(schema) = &operation.schema {
                node.push(KdlEntry::new_prop("schema", schema.clone()));
            }
            node.push(KdlEntry::new_prop("table", operation.table.clone()));
            node.push(KdlEntry::new_prop("column", operation.column.clone()));
            node.push(KdlEntry::new_prop("using", operation.using.clone()));
            node
        }
    }
}

fn refactor_from_document(document: &KdlDocument) -> Result<RefactorLog, PackageError> {
    let root = document
        .nodes()
        .iter()
        .find(|node| node.name().value() == "refactors")
        .ok_or_else(|| malformed("missing `refactors` node"))?;

    let mut operations = Vec::new();
    for node in root.children().into_iter().flat_map(KdlDocument::nodes) {
        operations.push(match node.name().value() {
            "rename-table" => RefactorOperation::RenameTable(rename_table_from_node(node)?),
            "rename-column" => RefactorOperation::RenameColumn(rename_column_from_node(node)?),
            "cast-column" => RefactorOperation::CastColumn(cast_column_from_node(node)?),
            other => return Err(malformed(format!("unknown refactor operation `{other}`"))),
        });
    }

    Ok(RefactorLog { operations })
}

fn rename_table_from_node(node: &KdlNode) -> Result<RenameTable, PackageError> {
    Ok(RenameTable {
        id: required_non_empty_prop(node, "id")?,
        schema: prop(node, "schema").map(str::to_owned),
        from: required_non_empty_prop(node, "from")?,
        to: required_non_empty_prop(node, "to")?,
    })
}

fn rename_column_from_node(node: &KdlNode) -> Result<RenameColumn, PackageError> {
    Ok(RenameColumn {
        id: required_non_empty_prop(node, "id")?,
        schema: prop(node, "schema").map(str::to_owned),
        table: required_non_empty_prop(node, "table")?,
        from: required_non_empty_prop(node, "from")?,
        to: required_non_empty_prop(node, "to")?,
    })
}

fn cast_column_from_node(node: &KdlNode) -> Result<CastColumn, PackageError> {
    Ok(CastColumn {
        id: required_non_empty_prop(node, "id")?,
        schema: prop(node, "schema").map(str::to_owned),
        table: required_non_empty_prop(node, "table")?,
        column: required_non_empty_prop(node, "column")?,
        using: required_non_empty_prop(node, "using")?,
    })
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
    for view in &schema.views {
        tables.nodes_mut().push(view_to_node(view));
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

fn view_to_node(view: &ViewModel) -> KdlNode {
    let mut node = KdlNode::new("view");
    node.push(KdlEntry::new(view.name.clone()));
    if let Some(comment) = &view.comment {
        node.push(KdlEntry::new_prop("comment", comment.clone()));
    }
    let mut body = KdlDocument::new();
    for column in &view.columns {
        body.nodes_mut().push(view_column_to_node(column));
    }
    body.nodes_mut().push(view_query_to_node(&view.query));
    node.set_children(body);
    node
}

fn view_column_to_node(column: &ViewColumnModel) -> KdlNode {
    let mut node = KdlNode::new("column");
    node.push(KdlEntry::new(column.name.clone()));
    write_sql_type(&mut node, &column.ty);
    if column.nullable {
        node.push(KdlEntry::new_prop("nullable", KdlValue::Bool(true)));
    }
    node
}

fn view_query_to_node(query: &ViewQueryModel) -> KdlNode {
    let mut node = KdlNode::new("query");
    if query.distinct {
        node.push(KdlEntry::new_prop("distinct", KdlValue::Bool(true)));
    }
    if let Some(limit) = query.limit {
        node.push(KdlEntry::new_prop(
            "limit",
            KdlValue::Integer(limit as i128),
        ));
    }
    if let Some(offset) = query.offset {
        node.push(KdlEntry::new_prop(
            "offset",
            KdlValue::Integer(offset as i128),
        ));
    }

    let mut body = KdlDocument::new();
    for item in &query.projection {
        let mut projection = KdlNode::new("projection");
        projection.push(KdlEntry::new(item.output_name.clone()));
        projection.push(KdlEntry::new_prop("expr", item.expr.0.clone()));
        body.nodes_mut().push(projection);
    }
    if let Some(from) = &query.from {
        body.nodes_mut()
            .push(view_source_to_node("from", from, None));
    }
    for join in &query.joins {
        let kind = match join.kind {
            JoinKind::Inner => "inner",
            JoinKind::Left => "left",
        };
        let mut node = view_source_to_node("join", &join.source, Some(kind));
        node.push(KdlEntry::new_prop("on", join.on.0.clone()));
        body.nodes_mut().push(node);
    }
    if let Some(filter) = &query.filter {
        let mut node = KdlNode::new("filter");
        node.push(KdlEntry::new(filter.0.clone()));
        body.nodes_mut().push(node);
    }
    for key in &query.group_by {
        let mut node = KdlNode::new("group-by");
        node.push(KdlEntry::new(key.0.clone()));
        body.nodes_mut().push(node);
    }
    if let Some(having) = &query.having {
        let mut node = KdlNode::new("having");
        node.push(KdlEntry::new(having.0.clone()));
        body.nodes_mut().push(node);
    }
    for order in &query.order_by {
        let mut node = KdlNode::new("order-by");
        node.push(KdlEntry::new(order.expr.0.clone()));
        if let Some(direction) = order.direction {
            node.push(KdlEntry::new_prop(
                "direction",
                match direction {
                    OrderDirection::Asc => "asc",
                    OrderDirection::Desc => "desc",
                },
            ));
        }
        if let Some(nulls) = order.nulls {
            node.push(KdlEntry::new_prop(
                "nulls",
                match nulls {
                    OrderNulls::First => "first",
                    OrderNulls::Last => "last",
                },
            ));
        }
        body.nodes_mut().push(node);
    }
    node.set_children(body);
    node
}

fn view_source_to_node(kind: &str, source: &SourceRef, join_kind: Option<&str>) -> KdlNode {
    let mut node = KdlNode::new(kind);
    node.push(KdlEntry::new(source.name.clone()));
    if let Some(schema) = &source.schema {
        node.push(KdlEntry::new_prop("schema", schema.clone()));
    }
    node.push(KdlEntry::new_prop("alias", source.alias.clone()));
    if let Some(join_kind) = join_kind {
        node.push(KdlEntry::new_prop("kind", join_kind));
    }
    node
}

fn column_to_node(column: &ColumnModel) -> KdlNode {
    let mut node = KdlNode::new("column");
    node.push(KdlEntry::new(column.name.clone()));
    if let Some(comment) = &column.comment {
        node.push(KdlEntry::new_prop("comment", comment.clone()));
    }

    write_sql_type(&mut node, &column.ty);
    if let Some(collation) = &column.collation {
        node.push(KdlEntry::new_prop("collation", collation.clone()));
    }
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
    if let Some(validation) = &foreign_key.validation {
        node.push(KdlEntry::new_prop(
            "validation",
            constraint_validation(validation),
        ));
    }
    if let Some(enforcement) = &foreign_key.enforcement {
        node.push(KdlEntry::new_prop(
            "enforcement",
            constraint_enforcement(enforcement),
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
    if let Some(validation) = &check.validation {
        node.push(KdlEntry::new_prop(
            "validation",
            constraint_validation(validation),
        ));
    }
    if let Some(enforcement) = &check.enforcement {
        node.push(KdlEntry::new_prop(
            "enforcement",
            constraint_enforcement(enforcement),
        ));
    }
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
    let views = child_nodes(node, "view")
        .map(view_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SchemaModel {
        name: first_arg(node).map(str::to_owned),
        tables,
        views,
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

fn view_from_node(node: &KdlNode) -> Result<ViewModel, PackageError> {
    let name = first_arg(node)
        .ok_or_else(|| malformed("`view` is missing its name"))?
        .to_owned();
    let columns = child_nodes(node, "column")
        .map(view_column_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let query = child_nodes(node, "query")
        .next()
        .map(view_query_from_node)
        .transpose()?
        .unwrap_or_default();
    Ok(ViewModel {
        name,
        comment: prop(node, "comment").map(str::to_owned),
        columns,
        query,
    })
}

fn view_column_from_node(node: &KdlNode) -> Result<ViewColumnModel, PackageError> {
    Ok(ViewColumnModel {
        name: first_arg(node)
            .ok_or_else(|| malformed("view `column` is missing its name"))?
            .to_owned(),
        ty: sql_type_from_node(node)?,
        nullable: prop_bool(node, "nullable"),
    })
}

fn view_query_from_node(node: &KdlNode) -> Result<ViewQueryModel, PackageError> {
    let projection = child_nodes(node, "projection")
        .map(|item| {
            Ok::<_, PackageError>(ProjectionItem {
                output_name: first_arg(item)
                    .ok_or_else(|| malformed("`projection` is missing its output name"))?
                    .to_owned(),
                expr: ExprFragment(required_prop(item, "expr")?),
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    let from = child_nodes(node, "from")
        .next()
        .map(view_source_from_node)
        .transpose()?;
    let joins = child_nodes(node, "join")
        .map(view_join_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    let filter = child_nodes(node, "filter")
        .next()
        .and_then(first_arg)
        .map(|expr| ExprFragment(expr.to_owned()));
    let group_by = child_nodes(node, "group-by")
        .filter_map(first_arg)
        .map(|expr| ExprFragment(expr.to_owned()))
        .collect();
    let having = child_nodes(node, "having")
        .next()
        .and_then(first_arg)
        .map(|expr| ExprFragment(expr.to_owned()));
    let order_by = child_nodes(node, "order-by")
        .map(view_order_from_node)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ViewQueryModel {
        distinct: prop_bool(node, "distinct"),
        projection,
        from,
        joins,
        filter,
        group_by,
        having,
        order_by,
        limit: prop_usize(node, "limit")?,
        offset: prop_usize(node, "offset")?,
    })
}

fn view_source_from_node(node: &KdlNode) -> Result<SourceRef, PackageError> {
    Ok(SourceRef {
        schema: prop(node, "schema").map(str::to_owned),
        name: first_arg(node)
            .ok_or_else(|| malformed("view source is missing its table name"))?
            .to_owned(),
        alias: required_prop(node, "alias")?,
    })
}

fn view_join_from_node(node: &KdlNode) -> Result<JoinItem, PackageError> {
    let kind = match prop(node, "kind") {
        Some("left") => JoinKind::Left,
        _ => JoinKind::Inner,
    };
    Ok(JoinItem {
        kind,
        source: view_source_from_node(node)?,
        on: ExprFragment(required_prop(node, "on")?),
    })
}

fn view_order_from_node(node: &KdlNode) -> Result<OrderItem, PackageError> {
    Ok(OrderItem {
        expr: ExprFragment(
            first_arg(node)
                .ok_or_else(|| malformed("`order-by` is missing its expression"))?
                .to_owned(),
        ),
        direction: match prop(node, "direction") {
            Some("asc") => Some(OrderDirection::Asc),
            Some("desc") => Some(OrderDirection::Desc),
            _ => None,
        },
        nulls: match prop(node, "nulls") {
            Some("first") => Some(OrderNulls::First),
            Some("last") => Some(OrderNulls::Last),
            _ => None,
        },
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
        collation: prop(node, "collation").map(str::to_owned),
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
        validation: prop(node, "validation").map(ConstraintValidation::from_sql),
        enforcement: prop(node, "enforcement").map(ConstraintEnforcement::from_sql),
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
        validation: prop(node, "validation").map(ConstraintValidation::from_sql),
        enforcement: prop(node, "enforcement").map(ConstraintEnforcement::from_sql),
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

/// Reads an optional non-negative integer property as `usize` (view `limit`/`offset`).
fn prop_usize(node: &KdlNode, key: &str) -> Result<Option<usize>, PackageError> {
    let Some(value) = node
        .entries()
        .iter()
        .find(|entry| entry.name().map(|name| name.value()) == Some(key))
        .and_then(|entry| entry.value().as_integer())
    else {
        return Ok(None);
    };
    usize::try_from(value)
        .map(Some)
        .map_err(|_| malformed(format!("`{key}` is out of range for usize")))
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

fn required_non_empty_prop(node: &KdlNode, key: &str) -> Result<String, PackageError> {
    let value = required_prop(node, key)?;
    if value.is_empty() {
        Err(malformed(format!(
            "`{}` has empty `{key}`",
            node.name().value()
        )))
    } else {
        Ok(value)
    }
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

fn constraint_validation(validation: &ConstraintValidation) -> &str {
    match validation {
        ConstraintValidation::Validated => "validated",
        ConstraintValidation::NotValidated => "not-validated",
        ConstraintValidation::Raw(validation) => validation,
    }
}

fn constraint_enforcement(enforcement: &ConstraintEnforcement) -> &str {
    match enforcement {
        ConstraintEnforcement::Enforced => "enforced",
        ConstraintEnforcement::NotEnforced => "not-enforced",
        ConstraintEnforcement::Raw(enforcement) => enforcement,
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

    #[test]
    fn read_entry_rejects_oversized_input() {
        let oversized = vec![b'x'; 64];
        let error =
            read_entry_to_string_limited(Cursor::new(&oversized), "model.kdl", 16).unwrap_err();
        assert!(matches!(
            error,
            PackageError::TooLarge {
                entry: "model.kdl",
                limit: 16
            }
        ));
    }

    #[test]
    fn read_entry_accepts_input_at_the_limit() {
        let data = vec![b'x'; 16];
        let text = read_entry_to_string_limited(Cursor::new(&data), "model.kdl", 16).unwrap();
        assert_eq!(text.len(), 16);
    }

    fn sample_model() -> DatabaseModel {
        DatabaseModel {
            schemas: vec![SchemaModel {
                name: Some("public".to_owned()),
                views: Vec::new(),
                tables: vec![
                    TableModel {
                        name: "orgs".to_owned(),
                        comment: Some("Organizations in the catalog".to_owned()),
                        columns: vec![
                            ColumnModel {
                                name: "id".to_owned(),
                                comment: Some("Synthetic organization id".to_owned()),
                                ty: SqlType::I32,
                                collation: None,
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
                                collation: Some("C".to_owned()),
                                nullable: false,
                                default: Some(DefaultValue::Text("acme".to_owned())),
                                identity: None,
                                generated: None,
                            },
                            ColumnModel {
                                name: "metadata".to_owned(),
                                comment: None,
                                ty: SqlType::Raw("jsonb".to_owned()),
                                collation: None,
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
                            validation: None,
                            enforcement: None,
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
                            collation: None,
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
                            validation: None,
                            enforcement: None,
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
        assert!(kdl.contains("collation=C"));
        let parsed = from_kdl(&kdl).expect("model.kdl should parse");
        assert_eq!(parsed, model, "KDL round-trip diverged:\n{kdl}");
    }

    #[test]
    fn kdl_is_deterministic() {
        let model = sample_model();
        assert_eq!(to_kdl(&model), to_kdl(&model));
    }

    #[test]
    fn kdl_round_trips_views() {
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: Some("public".to_owned()),
                tables: Vec::new(),
                views: vec![ViewModel {
                    name: "active_members".to_owned(),
                    comment: Some("Members of active orgs".to_owned()),
                    columns: vec![
                        ViewColumnModel {
                            name: "id".to_owned(),
                            ty: SqlType::I32,
                            nullable: false,
                        },
                        ViewColumnModel {
                            name: "name".to_owned(),
                            ty: SqlType::String,
                            nullable: true,
                        },
                    ],
                    query: ViewQueryModel {
                        distinct: true,
                        projection: vec![
                            ProjectionItem {
                                output_name: "id".to_owned(),
                                expr: ExprFragment("q0_0.\"id\"".to_owned()),
                            },
                            ProjectionItem {
                                output_name: "name".to_owned(),
                                expr: ExprFragment("q0_1.\"name\"".to_owned()),
                            },
                        ],
                        from: Some(SourceRef {
                            schema: Some("public".to_owned()),
                            name: "memberships".to_owned(),
                            alias: "q0_0".to_owned(),
                        }),
                        joins: vec![JoinItem {
                            kind: JoinKind::Left,
                            source: SourceRef {
                                schema: Some("public".to_owned()),
                                name: "orgs".to_owned(),
                                alias: "q0_1".to_owned(),
                            },
                            on: ExprFragment("(q0_0.\"org_id\" = q0_1.\"id\")".to_owned()),
                        }],
                        filter: Some(ExprFragment("q0_1.\"active\"".to_owned())),
                        group_by: vec![ExprFragment("q0_0.\"id\"".to_owned())],
                        having: Some(ExprFragment("(COUNT(q0_0.\"id\") > 0)".to_owned())),
                        order_by: vec![OrderItem {
                            expr: ExprFragment("q0_0.\"id\"".to_owned()),
                            direction: Some(OrderDirection::Desc),
                            nulls: Some(OrderNulls::Last),
                        }],
                        limit: Some(10),
                        offset: Some(5),
                    },
                }],
            }],
        };

        let kdl = to_kdl(&model);
        assert!(
            kdl.contains("distinct"),
            "DISTINCT view flag not serialized:\n{kdl}"
        );
        let parsed = from_kdl(&kdl).expect("view model.kdl should parse");
        assert_eq!(parsed, model, "view KDL round-trip diverged:\n{kdl}");
    }

    #[test]
    fn package_content_hash_includes_refactor_log() {
        let model = sample_model();
        let empty = RefactorLog::default();
        let refactors = sample_refactor_log();

        assert_eq!(
            package_content_hash(&model, &refactors),
            package_content_hash(&model, &refactors)
        );
        assert_ne!(
            package_content_hash(&model, &empty),
            package_content_hash(&model, &refactors)
        );

        let metadata = package_metadata(&model, &refactors);
        assert!(
            metadata
                .iter()
                .any(|(key, value)| key == PACKAGE_FORMAT_VERSION_METADATA_KEY
                    && value == &FORMAT_VERSION.to_string())
        );
        assert!(
            metadata
                .iter()
                .any(|(key, value)| key == PACKAGE_CONTENT_HASH_METADATA_KEY
                    && value.starts_with("fnv1a64:"))
        );
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
    fn refactor_kdl_round_trips() {
        let refactors = sample_refactor_log();
        let kdl = refactor_to_kdl(&refactors);
        assert!(kdl.contains("rename-table"));
        assert!(kdl.contains("rename-column"));
        assert!(kdl.contains("id=\"2026-rename-users\""));
        let parsed = refactor_from_kdl(&kdl).expect("refactor.kdl should parse");
        assert_eq!(
            parsed, refactors,
            "refactor KDL round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn package_zip_round_trips_optional_refactor_log() {
        let model = sample_model();
        let refactors = sample_refactor_log();
        let mut buffer = Vec::new();

        write_package_with_refactors_to(&model, &refactors, Cursor::new(&mut buffer))
            .expect("write package with refactors");

        let parsed_model = read_package_from(Cursor::new(buffer.clone())).expect("read model");
        let parsed_refactors =
            read_refactor_log_from_package(Cursor::new(buffer)).expect("read refactors");

        assert_eq!(parsed_model, model);
        assert_eq!(parsed_refactors, refactors);
    }

    #[test]
    fn package_without_refactor_log_reads_empty_refactor_log() {
        let model = sample_model();
        let mut buffer = Vec::new();
        write_package_to(&model, Cursor::new(&mut buffer)).expect("write package");

        let refactors =
            read_refactor_log_from_package(Cursor::new(buffer)).expect("read refactors");

        assert!(refactors.is_empty());
    }

    fn sample_refactor_log() -> RefactorLog {
        RefactorLog {
            operations: vec![
                RefactorOperation::RenameTable(RenameTable {
                    id: "2026-rename-users".to_owned(),
                    schema: Some("public".to_owned()),
                    from: "app_users".to_owned(),
                    to: "users".to_owned(),
                }),
                RefactorOperation::RenameColumn(RenameColumn {
                    id: "2026-rename-user-name".to_owned(),
                    schema: Some("public".to_owned()),
                    table: "users".to_owned(),
                    from: "display_name".to_owned(),
                    to: "name".to_owned(),
                }),
                RefactorOperation::CastColumn(CastColumn {
                    id: "2026-cast-user-score".to_owned(),
                    schema: Some("public".to_owned()),
                    table: "users".to_owned(),
                    column: "score".to_owned(),
                    using: "score::numeric".to_owned(),
                }),
            ],
        }
    }

    #[test]
    fn kdl_round_trips_names_with_whitespace() {
        // Column names can contain whitespace (e.g. `#[column(name = "user id")]`); local and
        // referenced foreign-key columns must survive the round-trip as distinct values.
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "events".to_owned(),
                    comment: None,
                    columns: vec![ColumnModel {
                        name: "user id".to_owned(),
                        comment: None,
                        ty: SqlType::I32,
                        collation: None,
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
                        validation: None,
                        enforcement: None,
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
                collation: None,
                nullable: false,
                default: None,
                identity: None,
                generated: None,
            })
            .collect();

        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
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
                collation: None,
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
                collation: None,
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
                collation: None,
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
                collation: None,
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
                collation: None,
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
                collation: None,
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
                views: Vec::new(),
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
                validation: None,
                enforcement: None,
                on_delete: Some(action.clone()),
                on_update: Some(action.clone()),
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
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
                validation: None,
                enforcement: None,
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
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
                validation: None,
                enforcement: None,
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
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
    fn kdl_round_trips_constraint_validation() {
        let values = [
            ConstraintValidation::Validated,
            ConstraintValidation::NotValidated,
            ConstraintValidation::Raw("backend-specific".to_owned()),
        ];
        let foreign_keys = values
            .iter()
            .enumerate()
            .map(|(index, validation)| ForeignKeyModel {
                name: format!("fk_child_parent_validation_{index}"),
                columns: vec![format!("parent_id_{index}")],
                references_schema: Some("public".to_owned()),
                references_table: "parents".to_owned(),
                references_columns: vec!["id".to_owned()],
                match_type: None,
                deferrability: None,
                validation: Some(validation.clone()),
                enforcement: None,
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "children".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys,
                    uniques: vec![],
                    checks: vec![CheckModel {
                        name: "ck_children_parent_id".to_owned(),
                        expression: "parent_id_0 > 0".to_owned(),
                        validation: Some(ConstraintValidation::NotValidated),
                        enforcement: None,
                    }],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        assert!(kdl.contains("validation=not-validated"));
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "constraint validation round-trip diverged:\n{kdl}"
        );
    }

    #[test]
    fn kdl_round_trips_constraint_enforcement() {
        let values = [
            ConstraintEnforcement::Enforced,
            ConstraintEnforcement::NotEnforced,
            ConstraintEnforcement::Raw("backend-specific".to_owned()),
        ];
        let foreign_keys = values
            .iter()
            .enumerate()
            .map(|(index, enforcement)| ForeignKeyModel {
                name: format!("fk_child_parent_enforcement_{index}"),
                columns: vec![format!("parent_id_{index}")],
                references_schema: Some("public".to_owned()),
                references_table: "parents".to_owned(),
                references_columns: vec!["id".to_owned()],
                match_type: None,
                deferrability: None,
                validation: None,
                enforcement: Some(enforcement.clone()),
                on_delete: None,
                on_update: None,
            })
            .collect();
        let model = DatabaseModel {
            schemas: vec![SchemaModel {
                name: None,
                views: Vec::new(),
                tables: vec![TableModel {
                    name: "children".to_owned(),
                    comment: None,
                    columns: vec![],
                    primary_key: None,
                    foreign_keys,
                    uniques: vec![],
                    checks: vec![CheckModel {
                        name: "ck_children_parent_id".to_owned(),
                        expression: "parent_id_0 > 0".to_owned(),
                        validation: None,
                        enforcement: Some(ConstraintEnforcement::NotEnforced),
                    }],
                    indexes: vec![],
                }],
            }],
        };

        let kdl = to_kdl(&model);
        assert!(kdl.contains("enforcement=not-enforced"));
        let parsed = from_kdl(&kdl).expect("parse");
        assert_eq!(
            parsed, model,
            "constraint enforcement round-trip diverged:\n{kdl}"
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
                views: Vec::new(),
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
                views: Vec::new(),
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
                views: Vec::new(),
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
