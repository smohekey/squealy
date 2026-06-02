use std::borrow::Cow;
use std::io::{self, Write};

use squealy::{
    ArithmeticOp, AssignmentNode, AssignmentValueVisitor, AssignmentVisitor, BindSink, BindValue,
    Column, ColumnDefault, ColumnRef, CompareOp, Expr, ExprAst, ExprKind, ExprVisitor, Index,
    InsertAssignments, InsertRow, InsertRowVisitor, InsertRows, InsertableTable, Order,
    OrderDirection, Predicate, PredicateAst, PredicateAstVisitor, PredicateKind, PredicateNodes,
    PredicateVisitor, Projectable, ProjectionShape, ProjectionVisitor, QueryBuilder, SchemaTable,
    SelectAst, SelectSink, Selected, SourceAlias, SqlType, Table, TableProjection,
    UpdateAssignments, UpdateableTable,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Renderer {
    next_param: usize,
    next_runtime_param: usize,
}

impl Renderer {
    fn write_placeholder(&mut self, writer: &mut impl Write) -> io::Result<()> {
        self.next_param += 1;
        write!(writer, "${}", self.next_param)
    }

    fn next_runtime_param(&mut self) -> usize {
        let index = self.next_runtime_param;
        self.next_runtime_param += 1;
        index
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct PreparedSql {
    sql: String,
    params: Vec<SqlParam>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) enum SqlParam {
    Static(BindValue),
    Runtime(usize),
}

impl PreparedSql {
    pub(crate) fn into_parts(self) -> (String, Vec<SqlParam>) {
        (self.sql, self.params)
    }

    fn clear(&mut self) {
        self.sql.clear();
        self.params.clear();
    }

    fn push_param(&mut self, param: BindValue) {
        self.params.push(SqlParam::Static(param));
    }

    fn push_runtime_param(&mut self, index: usize) {
        self.params.push(SqlParam::Runtime(index));
    }
}

impl Write for PreparedSql {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let text = std::str::from_utf8(buf).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("SQL renderer wrote non-UTF-8 bytes: {error}"),
            )
        })?;
        self.sql.push_str(text);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

trait SqlWriter: Write {
    fn push_bind(&mut self, value: &BindValue);

    fn push_runtime_bind(&mut self, index: usize);
}

impl SqlWriter for PreparedSql {
    fn push_bind(&mut self, value: &BindValue) {
        self.push_param(value.clone());
    }

    fn push_runtime_bind(&mut self, index: usize) {
        self.push_runtime_param(index);
    }
}

struct SqlOnly<'writer, Writer>(&'writer mut Writer);

impl<Writer> Write for SqlOnly<'_, Writer>
where
    Writer: Write,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.flush()
    }
}

impl<Writer> SqlWriter for SqlOnly<'_, Writer>
where
    Writer: Write,
{
    fn push_bind(&mut self, _value: &BindValue) {}

    fn push_runtime_bind(&mut self, _index: usize) {}
}

struct ParamSinkWriter<'sink, Sink>
where
    Sink: BindSink,
{
    sink: &'sink mut Sink,
    error: Option<Sink::Error>,
}

impl<Sink> ParamSinkWriter<'_, Sink>
where
    Sink: BindSink,
{
    fn finish(self) -> Result<(), Sink::Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<Sink> Write for ParamSinkWriter<'_, Sink>
where
    Sink: BindSink,
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<Sink> SqlWriter for ParamSinkWriter<'_, Sink>
where
    Sink: BindSink,
{
    fn push_bind(&mut self, value: &BindValue) {
        if self.error.is_none() {
            self.error = self.sink.push_bind_value(value.clone()).err();
        }
    }

    fn push_runtime_bind(&mut self, _index: usize) {}
}

struct PostgresSelectSink<'writer, Writer> {
    writer: &'writer mut Writer,
    renderer: Renderer,
    columns: usize,
    sources: usize,
    filters: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl<'writer, Writer> PostgresSelectSink<'writer, Writer>
where
    Writer: SqlWriter,
{
    fn new(writer: &'writer mut Writer) -> io::Result<Self> {
        writer.write_all(b"SELECT ")?;
        Ok(Self {
            writer,
            renderer: Renderer::default(),
            columns: 0,
            sources: 0,
            filters: 0,
            orders: 0,
            limit: None,
            offset: None,
        })
    }

    fn finish(self) -> io::Result<()> {
        if let Some(limit) = self.limit {
            write!(self.writer, " LIMIT {limit}")?;
        }
        if let Some(offset) = self.offset {
            write!(self.writer, " OFFSET {offset}")?;
        }
        Ok(())
    }

    fn push_source_separator(&mut self) -> io::Result<()> {
        self.writer.write_all(b" ")?;
        self.sources += 1;
        Ok(())
    }

    fn push_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
        join: &str,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: PredicateAst,
    {
        let first_source = self.sources == 0;
        self.push_source_separator()?;
        if first_source {
            self.writer.write_all(b"FROM ")?;
            write_table_ref::<S>(self.writer)?;
            write!(self.writer, " AS {alias}")?;
        } else {
            write!(self.writer, "{join} ")?;
            write_table_ref::<S>(self.writer)?;
            write!(self.writer, " AS {alias} ON ")?;
            write_predicate_value(&on, self.writer, &mut self.renderer)?;
        }
        Ok(())
    }

    fn push_projection_separator(&mut self) -> io::Result<()> {
        if self.columns > 0 {
            self.writer.write_all(b", ")?;
        }
        self.columns += 1;
        Ok(())
    }
}

impl<Writer> SelectSink for PostgresSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn push_projection<Shape, P>(&mut self, projection: P) -> io::Result<()>
    where
        Shape: ProjectionShape,
        P: Projectable,
    {
        _ = std::marker::PhantomData::<Shape>;
        projection.visit_projection(self)
    }

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> io::Result<()>
    where
        S: TableProjection,
    {
        self.push_source_separator()?;
        self.writer.write_all(b"FROM ")?;
        write_table_ref::<S>(self.writer)?;
        write!(self.writer, " AS {alias}")
    }

    fn push_inner_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: PredicateAst,
    {
        self.push_join::<S, P, Ast>(alias, on, "INNER JOIN")
    }

    fn push_left_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: PredicateAst,
    {
        self.push_join::<S, P, Ast>(alias, on, "LEFT JOIN")
    }

    fn push_filter<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> io::Result<()>
    where
        P: PredicateKind,
        Ast: PredicateAst,
    {
        if self.filters == 0 {
            self.writer.write_all(b" WHERE ")?;
        } else {
            self.writer.write_all(b" AND ")?;
        }
        self.filters += 1;
        write_predicate_value(&predicate, self.writer, &mut self.renderer)
    }

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> io::Result<()>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        if self.orders == 0 {
            self.writer.write_all(b" ORDER BY ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.orders += 1;
        write_order_value(&order, self.writer, &mut self.renderer)
    }

    fn set_limit(&mut self, rows: usize) -> io::Result<()> {
        self.limit = Some(rows);
        Ok(())
    }

    fn set_offset(&mut self, rows: usize) -> io::Result<()> {
        self.offset = Some(rows);
        Ok(())
    }
}

impl<Writer> ProjectionVisitor for PostgresSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        self.push_projection_separator()?;
        write_expr_value(expr, self.writer, &mut self.renderer)?;
        self.writer.write_all(b" AS ")?;
        write_quoted_ident(&alias, self.writer)?;
        Ok(())
    }

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
    {
        self.push_projection_separator()?;
        write_column_value(column, self.writer, &mut self.renderer)?;
        self.writer.write_all(b" AS ")?;
        write_quoted_ident(&alias, self.writer)
    }
}

pub(crate) fn render_selected_prepared<'conn, 'scope, Conn, Base, Shape, Projection>(
    selected: &Selected<'scope, Base, Shape, Projection>,
    buffer: &mut PreparedSql,
) where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Shape: ProjectionShape,
    Projection: Projectable,
{
    buffer.clear();
    let mut sink = PostgresSelectSink::new(buffer).unwrap();
    selected.lower_into::<Conn, _>(&mut sink).unwrap();
    sink.finish().unwrap();
}

pub(crate) fn write_selected_into<'conn, 'scope, Conn, Base, Shape, Projection, Writer>(
    selected: &Selected<'scope, Base, Shape, Projection>,
    writer: &mut Writer,
) -> io::Result<()>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Shape: ProjectionShape,
    Projection: Projectable,
    Writer: Write,
{
    let mut writer = SqlOnly(writer);
    let mut sink = PostgresSelectSink::new(&mut writer)?;
    selected.lower_into::<Conn, _>(&mut sink)?;
    sink.finish()
}

pub(crate) fn write_selected_params<'conn, 'scope, Conn, Base, Shape, Projection, Sink>(
    selected: &Selected<'scope, Base, Shape, Projection>,
    sink: &mut Sink,
) -> Result<(), Sink::Error>
where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Shape: ProjectionShape,
    Projection: Projectable,
    Sink: BindSink,
{
    let mut writer = ParamSinkWriter { sink, error: None };
    let mut select_sink = PostgresSelectSink::new(&mut writer).unwrap();
    selected.lower_into::<Conn, _>(&mut select_sink).unwrap();
    select_sink.finish().unwrap();
    writer.finish()
}

pub(crate) fn write_table(table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"CREATE TABLE ")?;
    write_qualified_name(table.schema_name(), table.name(), writer)?;
    writer.write_all(b" (")?;
    for (index, column) in table.columns().iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(column.name(), writer)?;
        writer.write_all(b" ")?;
        write_column_type(*column, writer)?;
        if column.primary_key() {
            writer.write_all(b" PRIMARY KEY")?;
        }
        if column.auto_increment() {
            writer.write_all(b" GENERATED BY DEFAULT AS IDENTITY")?;
        }
        if !column.nullable() {
            writer.write_all(b" NOT NULL")?;
        }
        if let Some(default) = column.default() {
            writer.write_all(b" DEFAULT ")?;
            write_default(default, writer)?;
        }
        if let Some(reference) = column.references() {
            writer.write_all(b" REFERENCES ")?;
            write_qualified_name(reference.schema_name(), reference.table(), writer)?;
            writer.write_all(b"(")?;
            write_quoted_ident(reference.column(), writer)?;
            writer.write_all(b")")?;
            if let Some(on_delete) = reference.on_delete() {
                write!(writer, " ON DELETE {on_delete}")?;
            }
            if let Some(on_update) = reference.on_update() {
                write!(writer, " ON UPDATE {on_update}")?;
            }
        }
    }
    writer.write_all(b")")?;

    for (position, index) in table.indexes().iter().enumerate() {
        let unique = if index.unique() { "UNIQUE " } else { "" };
        write!(writer, "\nCREATE {unique}INDEX ")?;
        match index.name() {
            Some(name) => write_quoted_ident(name, writer)?,
            None => write_quoted_ident(&derived_index_name(table, *index, position), writer)?,
        }
        writer.write_all(b" ON ")?;
        write_qualified_name(table.schema_name(), table.name(), writer)?;
        writer.write_all(b" (")?;
        write_quoted_idents(index.columns(), writer)?;
        writer.write_all(b")")?;
    }

    Ok(())
}

/// Builds a deterministic, unique index name for an index that did not supply one.
/// Without this, every unnamed index would render as the same name and collide.
fn derived_index_name(table: &(dyn Table + Sync), index: &dyn Index, position: usize) -> String {
    let mut name = format!("idx_{}", table.name());
    for column in index.columns() {
        name.push('_');
        name.push_str(column);
    }
    if index.columns().is_empty() {
        name.push_str(&format!("_{position}"));
    }
    name
}

fn write_column_type(column: &dyn Column, writer: &mut impl Write) -> io::Result<()> {
    write_pg_sql_type(&column.column_type().into(), writer)
}

/// Whole-database DDL rendering, used by the `SchemaBackend` impl. Gated behind the `schema` feature
/// so query-only users carry none of it.
#[cfg(feature = "schema")]
pub(crate) mod ddl {
    use std::io::{self, Write};

    use squealy::{
        CheckModel, ColumnModel, DatabaseModel, DefaultValue, ForeignKeyModel, IdentityMode,
        IndexModel, TableModel,
    };

    use super::{write_pg_sql_type, write_qualified_name, write_quoted_ident, write_quoted_text};

    /// Renders ordered create-from-scratch DDL for a whole [`DatabaseModel`].
    ///
    /// Statements are emitted in phases so creation never depends on ordering: namespaces, then tables
    /// (with primary-key/unique/check constraints inline), then indexes, then foreign keys as separate
    /// `ALTER TABLE … ADD CONSTRAINT`. Statements are `;`-terminated and newline-separated.
    pub(crate) fn write_database(model: &DatabaseModel, writer: &mut impl Write) -> io::Result<()> {
        let mut first = true;

        for schema in &model.schemas {
            if let Some(name) = schema.name.as_deref() {
                statement(writer, &mut first)?;
                writer.write_all(b"CREATE SCHEMA IF NOT EXISTS ")?;
                write_quoted_ident(name, writer)?;
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                statement(writer, &mut first)?;
                write_create_table(schema.name.as_deref(), table, writer)?;
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                for index in &table.indexes {
                    statement(writer, &mut first)?;
                    write_create_index(schema.name.as_deref(), &table.name, index, writer)?;
                }
            }
        }

        for schema in &model.schemas {
            for table in &schema.tables {
                for foreign_key in &table.foreign_keys {
                    statement(writer, &mut first)?;
                    write_add_foreign_key(
                        schema.name.as_deref(),
                        &table.name,
                        foreign_key,
                        writer,
                    )?;
                }
            }
        }

        // Terminate the final statement (the separator only terminates *preceding* ones).
        if !first {
            writer.write_all(b";")?;
        }

        Ok(())
    }

    /// Terminates the previous statement and starts a new line before every statement after the first,
    /// leaving the caller to write the statement body. The final statement is terminated by the caller.
    fn statement(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
        if *first {
            *first = false;
        } else {
            writer.write_all(b";\n")?;
        }
        Ok(())
    }

    fn write_create_table(
        schema: Option<&str>,
        table: &TableModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CREATE TABLE ")?;
        write_qualified_name(schema, &table.name, writer)?;
        writer.write_all(b" (\n")?;

        let mut first_entry = true;
        for column in &table.columns {
            entry(writer, &mut first_entry)?;
            write_model_column(column, writer)?;
        }
        if let Some(primary_key) = &table.primary_key {
            entry(writer, &mut first_entry)?;
            write_named_constraint(
                "PRIMARY KEY",
                &primary_key.name,
                &primary_key.columns,
                writer,
            )?;
        }
        for unique in &table.uniques {
            entry(writer, &mut first_entry)?;
            write_named_constraint("UNIQUE", &unique.name, &unique.columns, writer)?;
        }
        for check in &table.checks {
            entry(writer, &mut first_entry)?;
            write_check(check, writer)?;
        }

        writer.write_all(b"\n)")
    }

    /// Separates entries inside a `CREATE TABLE (...)` list: each entry is on its own indented line.
    fn entry(writer: &mut impl Write, first: &mut bool) -> io::Result<()> {
        if *first {
            *first = false;
            writer.write_all(b"  ")
        } else {
            writer.write_all(b",\n  ")
        }
    }

    fn write_model_column(column: &ColumnModel, writer: &mut impl Write) -> io::Result<()> {
        write_quoted_ident(&column.name, writer)?;
        writer.write_all(b" ")?;
        write_pg_sql_type(&column.ty, writer)?;
        if let Some(identity) = &column.identity {
            match identity.mode {
                IdentityMode::Always => writer.write_all(b" GENERATED ALWAYS AS IDENTITY")?,
                IdentityMode::ByDefault | IdentityMode::AutoIncrement => {
                    writer.write_all(b" GENERATED BY DEFAULT AS IDENTITY")?
                }
            }
        }
        if let Some(generated) = &column.generated {
            writer.write_all(b" GENERATED ALWAYS AS (")?;
            writer.write_all(generated.expression.as_bytes())?;
            writer.write_all(b") STORED")?;
        }
        if !column.nullable {
            writer.write_all(b" NOT NULL")?;
        }
        if let Some(default) = &column.default {
            writer.write_all(b" DEFAULT ")?;
            write_default_value(default, writer)?;
        }
        Ok(())
    }

    /// Renders an owned [`DefaultValue`]. Mirrors [`write_default`] for the compile-time
    /// [`ColumnDefault`].
    fn write_default_value(default: &DefaultValue, writer: &mut impl Write) -> io::Result<()> {
        match default {
            DefaultValue::Null => writer.write_all(b"NULL"),
            DefaultValue::Int(value) => write!(writer, "{value}"),
            DefaultValue::UInt(value) => write!(writer, "{value}"),
            DefaultValue::Float(value) => write!(writer, "{value}"),
            DefaultValue::Text(value) => write_quoted_text(value, writer),
            DefaultValue::Bool(true) => writer.write_all(b"TRUE"),
            DefaultValue::Bool(false) => writer.write_all(b"FALSE"),
            DefaultValue::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
            DefaultValue::CurrentDate => writer.write_all(b"CURRENT_DATE"),
            DefaultValue::CurrentTime => writer.write_all(b"CURRENT_TIME"),
            DefaultValue::Raw(value) => writer.write_all(value.as_bytes()),
        }
    }

    fn write_named_constraint(
        kind: &str,
        name: &str,
        columns: &[String],
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CONSTRAINT ")?;
        write_quoted_ident(name, writer)?;
        write!(writer, " {kind} (")?;
        write_quoted_ident_list(columns, writer)?;
        writer.write_all(b")")
    }

    fn write_check(check: &CheckModel, writer: &mut impl Write) -> io::Result<()> {
        writer.write_all(b"CONSTRAINT ")?;
        write_quoted_ident(&check.name, writer)?;
        // The check expression is a backend-specific escape hatch, emitted verbatim.
        write!(writer, " CHECK ({})", check.expression)
    }

    fn write_create_index(
        schema: Option<&str>,
        table: &str,
        index: &IndexModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"CREATE ")?;
        if index.unique {
            writer.write_all(b"UNIQUE ")?;
        }
        writer.write_all(b"INDEX ")?;
        write_quoted_ident(&index.name, writer)?;
        writer.write_all(b" ON ")?;
        write_qualified_name(schema, table, writer)?;
        if let Some(method) = &index.method {
            writer.write_all(b" USING ")?;
            writer.write_all(method.postgres_sql().as_bytes())?;
        }
        writer.write_all(b" (")?;
        write_index_columns(index, writer)?;
        writer.write_all(b")")?;
        if let Some(predicate) = &index.predicate {
            writer.write_all(b" WHERE ")?;
            writer.write_all(predicate.as_bytes())?;
        }
        Ok(())
    }

    fn write_add_foreign_key(
        schema: Option<&str>,
        table: &str,
        foreign_key: &ForeignKeyModel,
        writer: &mut impl Write,
    ) -> io::Result<()> {
        writer.write_all(b"ALTER TABLE ")?;
        write_qualified_name(schema, table, writer)?;
        writer.write_all(b" ADD CONSTRAINT ")?;
        write_quoted_ident(&foreign_key.name, writer)?;
        writer.write_all(b" FOREIGN KEY (")?;
        write_quoted_ident_list(&foreign_key.columns, writer)?;
        writer.write_all(b") REFERENCES ")?;
        write_qualified_name(
            foreign_key.references_schema.as_deref(),
            &foreign_key.references_table,
            writer,
        )?;
        writer.write_all(b" (")?;
        write_quoted_ident_list(&foreign_key.references_columns, writer)?;
        writer.write_all(b")")?;
        if let Some(on_delete) = &foreign_key.on_delete {
            write!(writer, " ON DELETE {}", on_delete.as_sql())?;
        }
        if let Some(on_update) = &foreign_key.on_update {
            write!(writer, " ON UPDATE {}", on_update.as_sql())?;
        }
        Ok(())
    }

    /// Like [`write_quoted_idents`] but over owned model strings.
    fn write_quoted_ident_list(columns: &[String], writer: &mut impl Write) -> io::Result<()> {
        for (index, column) in columns.iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_ident(column, writer)?;
        }
        Ok(())
    }

    fn write_index_columns(index: &IndexModel, writer: &mut impl Write) -> io::Result<()> {
        for (position, column) in index.columns.iter().enumerate() {
            if position > 0 {
                writer.write_all(b", ")?;
            }
            write_quoted_ident(column, writer)?;
            match index.directions.get(position) {
                Some(squealy::IndexDirection::Asc) => writer.write_all(b" ASC")?,
                Some(squealy::IndexDirection::Desc) => writer.write_all(b" DESC")?,
                None => {}
            }
        }
        Ok(())
    }
} // mod ddl

/// Renders the neutral [`SqlType`] as a PostgreSQL DDL type. Used by both the whole-database renderer
/// (via the model) and `write_table` (which converts its compile-time `ColumnType`).
fn write_pg_sql_type(ty: &SqlType, writer: &mut impl Write) -> io::Result<()> {
    let name = match ty {
        SqlType::Bool => "boolean",
        SqlType::I8 | SqlType::I16 => "smallint",
        SqlType::I32 => "integer",
        SqlType::I64 | SqlType::Isize => "bigint",
        SqlType::I128 => "numeric",
        SqlType::U8 => "smallint",
        SqlType::U16 => "integer",
        SqlType::U32 | SqlType::Usize => "bigint",
        SqlType::U64 | SqlType::U128 => "numeric",
        SqlType::F32 => "real",
        SqlType::F64 => "double precision",
        SqlType::String | SqlType::Text => "text",
        SqlType::Varchar(length) => return write!(writer, "varchar({length})"),
        SqlType::Char(length) => return write!(writer, "char({length})"),
        SqlType::Decimal { precision, scale } => {
            return write!(writer, "numeric({precision},{scale})");
        }
        SqlType::Date => "date",
        SqlType::Time { tz } => {
            if *tz {
                "time with time zone"
            } else {
                "time"
            }
        }
        SqlType::Timestamp { tz } => {
            if *tz {
                "timestamp with time zone"
            } else {
                "timestamp"
            }
        }
        SqlType::Uuid => "uuid",
        SqlType::Json => "json",
        SqlType::Jsonb => "jsonb",
        SqlType::Bytes => "bytea",
        SqlType::Raw(raw) => raw.as_str(),
    };
    writer.write_all(name.as_bytes())
}

fn write_quoted_idents(values: &[&'static str], writer: &mut impl Write) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write_quoted_ident(value, writer)?;
    }
    Ok(())
}

fn write_default(default: ColumnDefault, writer: &mut impl Write) -> io::Result<()> {
    match default {
        ColumnDefault::Null => writer.write_all(b"NULL"),
        ColumnDefault::Int(value) => write!(writer, "{value}"),
        ColumnDefault::UInt(value) => write!(writer, "{value}"),
        ColumnDefault::Float(value) => write!(writer, "{value}"),
        ColumnDefault::Text(value) => write_quoted_text(value, writer),
        ColumnDefault::Bool(true) => writer.write_all(b"TRUE"),
        ColumnDefault::Bool(false) => writer.write_all(b"FALSE"),
        ColumnDefault::CurrentTimestamp => writer.write_all(b"CURRENT_TIMESTAMP"),
        ColumnDefault::CurrentDate => writer.write_all(b"CURRENT_DATE"),
        ColumnDefault::CurrentTime => writer.write_all(b"CURRENT_TIME"),
        ColumnDefault::Raw(value) => writer.write_all(value.as_bytes()),
    }
}

/// Writes `value` wrapped in `delimiter` quotes, doubling any embedded delimiter.
///
/// Whole UTF-8 slices are written between delimiters rather than individual bytes,
/// so writers that validate each `write` chunk as UTF-8 (such as the string-backed
/// SQL writer) accept multibyte identifiers and literals like `café`.
fn write_quoted(value: &str, delimiter: char, writer: &mut impl Write) -> io::Result<()> {
    let mut encoded = [0u8; 4];
    let delim = delimiter.encode_utf8(&mut encoded).as_bytes();

    writer.write_all(delim)?;
    let mut start = 0;
    for (index, _) in value.match_indices(delimiter) {
        writer.write_all(value[start..index].as_bytes())?;
        writer.write_all(delim)?;
        writer.write_all(delim)?;
        start = index + delimiter.len_utf8();
    }
    writer.write_all(value[start..].as_bytes())?;
    writer.write_all(delim)
}

fn write_quoted_text(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_quoted(value, '\'', writer)
}

/// Writes a single SQL identifier wrapped in double quotes, doubling any embedded
/// quotes. This keeps reserved words (`user`, `order`, ...) and identifiers with
/// special characters valid. Identifiers come from compile-time table metadata, so
/// this is robustness, not injection defense.
fn write_quoted_ident(value: &str, writer: &mut impl Write) -> io::Result<()> {
    write_quoted(value, '"', writer)
}

/// Writes a schema-qualified table reference with each part quoted separately,
/// e.g. `"public"."users"`.
fn write_qualified_name(
    schema: Option<&str>,
    name: &str,
    writer: &mut impl Write,
) -> io::Result<()> {
    if let Some(schema) = schema {
        write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    write_quoted_ident(name, writer)
}

/// Writes a quoted, schema-qualified reference to a `TableProjection` source.
fn write_table_ref<S>(writer: &mut impl Write) -> io::Result<()>
where
    S: TableProjection,
{
    write_qualified_name(
        <S as TableProjection>::schema_name(),
        <S as TableProjection>::name(),
        writer,
    )
}

/// Writes a quoted, schema-qualified reference to a `SchemaTable` model.
fn write_schema_table_ref<S>(writer: &mut impl Write) -> io::Result<()>
where
    S: SchemaTable,
{
    write_qualified_name(
        <S as SchemaTable>::schema_name(),
        <S as SchemaTable>::name(),
        writer,
    )
}

pub(crate) fn write_insert<S, Rows, Returning>(
    rows: &Rows,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    Rows: InsertRows,
    Returning: Projectable,
{
    let mut writer = SqlOnly(writer);
    write_insert_with_params::<S, _, _, _>(rows, returning, &mut writer)
}

fn write_insert_with_params<S, Rows, Returning, Writer>(
    rows: &Rows,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: InsertableTable,
    Rows: InsertRows,
    Returning: Projectable,
    Writer: SqlWriter,
{
    let mut renderer = Renderer::default();
    writer.write_all(b"INSERT INTO ")?;
    write_schema_table_ref::<S>(writer)?;
    if rows.len() == 1 && rows.first_row_len() == 0 {
        writer.write_all(b" DEFAULT VALUES")?;
    } else {
        writer.write_all(b" (")?;
        let mut index = 0;
        rows.try_for_each_column(|column| {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            index += 1;
            write_quoted_ident(column, writer)?;
            Ok::<(), io::Error>(())
        })?;
        writer.write_all(b") VALUES ")?;
        write_insert_rows(rows, writer, &mut renderer)?;
    }
    write_insert_returning(returning, writer, &mut renderer)?;
    Ok(())
}

struct WriteInsertRows<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    expected_columns: usize,
    row_index: usize,
}

impl<Writer> InsertRowVisitor<io::Error> for WriteInsertRows<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    fn visit_row<Columns>(&mut self, row: &InsertRow<Columns>) -> io::Result<()>
    where
        Columns: InsertAssignments,
    {
        if row.columns().len() != self.expected_columns {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "all inserted rows must assign the same columns",
            ));
        }

        if self.row_index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.row_index += 1;

        self.writer.write_all(b"(")?;
        let mut assignments = WriteAssignmentValues {
            writer: self.writer,
            renderer: self.renderer,
            index: 0,
        };
        row.columns().try_visit(&mut assignments)?;
        self.writer.write_all(b")")
    }
}

struct WriteAssignmentValues<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
}

impl<Writer> AssignmentVisitor for WriteAssignmentValues<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_assignment<Value>(
        &mut self,
        _column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: AssignmentNode,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        write_assignment_value(value, self.writer, self.renderer)
    }
}

fn write_insert_rows<Rows, Writer>(
    rows: &Rows,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    Rows: InsertRows,
    Writer: SqlWriter,
{
    let mut visitor = WriteInsertRows {
        writer,
        renderer,
        expected_columns: rows.first_row_len(),
        row_index: 0,
    };
    rows.try_for_each_row(&mut visitor)
}

pub(crate) fn write_update<S, Columns, Filters, Returning>(
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: UpdateableTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    let mut writer = SqlOnly(writer);
    write_update_with_params::<S, _, _, _, _>(alias, columns, filters, returning, &mut writer)
}

fn write_update_with_params<S, Columns, Filters, Returning, Writer>(
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: UpdateableTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Writer: SqlWriter,
{
    let mut renderer = Renderer::default();
    writer.write_all(b"UPDATE ")?;
    write_schema_table_ref::<S>(writer)?;
    write!(writer, " AS {alias} SET ")?;
    let mut assignments = WriteUpdateAssignments {
        writer,
        renderer: &mut renderer,
        index: 0,
    };
    columns.try_visit(&mut assignments)?;
    write_filters(filters, writer, &mut renderer)?;
    write_returning(returning, writer, &mut renderer)?;
    Ok(())
}

struct WriteUpdateAssignments<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
}

impl<Writer> AssignmentVisitor for WriteUpdateAssignments<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_assignment<Value>(
        &mut self,
        column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: AssignmentNode,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        write_quoted_ident(column, self.writer)?;
        self.writer.write_all(b" = ")?;
        write_assignment_value(value, self.writer, self.renderer)
    }
}

pub(crate) fn write_delete<S, Filters, Returning>(
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: TableProjection,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    let mut writer = SqlOnly(writer);
    write_delete_with_params::<S, _, _, _>(alias, filters, returning, &mut writer)
}

fn write_delete_with_params<S, Filters, Returning, Writer>(
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: TableProjection,
    Filters: PredicateNodes,
    Returning: Projectable,
    Writer: SqlWriter,
{
    let mut renderer = Renderer::default();
    writer.write_all(b"DELETE FROM ")?;
    write_table_ref::<S>(writer)?;
    write!(writer, " AS {alias}")?;
    write_filters(filters, writer, &mut renderer)?;
    write_returning(returning, writer, &mut renderer)?;
    Ok(())
}

fn write_returning(
    returning: &impl Projectable,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()> {
    write_projection(returning, writer, renderer, false)
}

fn write_insert_returning(
    returning: &impl Projectable,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()> {
    write_projection(returning, writer, renderer, true)
}

fn write_projection(
    projection: &impl Projectable,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()> {
    projection.visit_projection(&mut WriteProjection {
        writer,
        renderer,
        index: 0,
        insert_returning,
    })
}

struct WriteProjection<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    insert_returning: bool,
}

impl<Writer> WriteProjection<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    fn write_prefix(&mut self) -> io::Result<()> {
        if self.index == 0 {
            self.writer.write_all(b" RETURNING ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        Ok(())
    }
}

impl<Writer> ProjectionVisitor for WriteProjection<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        self.write_prefix()?;
        write_expr_value_node(expr, self.writer, self.renderer, self.insert_returning)?;
        self.writer.write_all(b" AS ")?;
        write_quoted_ident(&alias, self.writer)
    }

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
    {
        self.write_prefix()?;
        write_column_value_node(column, self.writer, self.renderer, self.insert_returning)?;
        self.writer.write_all(b" AS ")?;
        write_quoted_ident(&alias, self.writer)
    }
}

fn write_filters(
    filters: &impl PredicateNodes,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    writer.write_all(b" WHERE ")?;
    filters.try_visit(&mut WritePredicateFilters {
        writer,
        renderer,
        index: 0,
    })?;
    Ok(())
}

struct WritePredicateFilters<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
}

impl<Writer> PredicateVisitor for WritePredicateFilters<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_predicate<Kind, Ast>(&mut self, predicate: &Predicate<'_, Kind, Ast>) -> io::Result<()>
    where
        Kind: PredicateKind,
        Ast: PredicateAst,
    {
        if self.index > 0 {
            self.writer.write_all(b" AND ")?;
        }
        self.index += 1;
        write_predicate_value(predicate, self.writer, self.renderer)
    }
}

fn write_expr_value<K, Ast>(
    expr: &Expr<'_, K, Ast>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: ExprAst,
{
    write_expr_value_node(expr, writer, renderer, false)
}

fn write_column_value<K>(
    column: ColumnRef<'_, K>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
{
    write_column_value_node(column, writer, renderer, false)
}

fn write_expr_value_node<K, Ast>(
    expr: &Expr<'_, K, Ast>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: ExprAst,
{
    write_ast(writer, renderer, insert_returning, |visitor| {
        expr.visit(visitor)
    })
}

fn write_column_value_node<K>(
    column: ColumnRef<'_, K>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
{
    write_ast(writer, renderer, insert_returning, |visitor| {
        column.visit(visitor)
    })
}

fn write_predicate_value<K, Ast>(
    predicate: &Predicate<'_, K, Ast>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    write_ast(writer, renderer, false, |visitor| predicate.visit(visitor))
}

fn write_order_value<K, Ast>(
    order: &Order<'_, K, Ast>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: ExprAst,
{
    write_ast(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
    write!(writer, " {}", render_order_direction(order.direction()))
}

fn write_assignment_value<Value>(
    value: &Value,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    Value: AssignmentNode,
{
    value.visit_value(&mut RenderAssignmentValue { writer, renderer })
}

struct RenderAssignmentValue<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
}

impl<Writer> AssignmentValueVisitor for RenderAssignmentValue<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_static(&mut self, value: &BindValue) -> Result<(), Self::Error> {
        self.writer.push_bind(value);
        self.renderer.write_placeholder(self.writer)
    }

    fn visit_default(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b"DEFAULT")
    }

    fn visit_runtime(&mut self) -> Result<(), Self::Error> {
        let index = self.renderer.next_runtime_param();
        self.writer.push_runtime_bind(index);
        self.renderer.write_placeholder(self.writer)
    }

    fn visit_expr<K, Ast>(&mut self, expr: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        write_expr_value(expr, self.writer, self.renderer)
    }
}

fn write_ast<Writer>(
    writer: &mut Writer,
    renderer: &mut Renderer,
    insert_returning: bool,
    render: impl FnOnce(&mut RenderAst<'_, '_, Writer>) -> io::Result<()>,
) -> io::Result<()>
where
    Writer: SqlWriter,
{
    let mut visitor = RenderAst {
        writer,
        renderer,
        insert_returning,
    };
    render(&mut visitor)
}

struct RenderAst<'writer, 'renderer, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    insert_returning: bool,
}

impl<Writer> ExprVisitor for RenderAst<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;

    fn visit_column(&mut self, alias: SourceAlias, column: &str) -> Result<(), Self::Error> {
        if self.insert_returning {
            write_quoted_ident(column, self.writer)
        } else {
            write!(self.writer, "{alias}.")?;
            write_quoted_ident(column, self.writer)
        }
    }

    fn visit_literal(&mut self, value: &BindValue) -> Result<(), Self::Error> {
        self.writer.push_bind(value);
        self.renderer.write_placeholder(self.writer)
    }

    fn visit_param(&mut self) -> Result<(), Self::Error> {
        let index = self.renderer.next_runtime_param();
        self.writer.push_runtime_bind(index);
        self.renderer.write_placeholder(self.writer)
    }

    fn visit_binary<L, R>(&mut self, op: ArithmeticOp, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        if op == ArithmeticOp::Divide {
            self.writer.write_all(b"(CAST(")?;
            left(self)?;
            self.writer.write_all(b" AS double precision) / CAST(")?;
            right(self)?;
            return self.writer.write_all(b" AS double precision))");
        }

        self.writer.write_all(b"(")?;
        left(self)?;
        write!(self.writer, " {} ", render_arithmetic_op(op))?;
        right(self)?;
        self.writer.write_all(b")")
    }
}

impl<Writer> PredicateAstVisitor for RenderAst<'_, '_, Writer>
where
    Writer: SqlWriter,
{
    fn visit_compare<L, R>(&mut self, op: CompareOp, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        left(self)?;
        write!(self.writer, " {} ", render_compare_op(op))?;
        right(self)?;
        self.writer.write_all(b")")
    }

    fn visit_and<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        left(self)?;
        self.writer.write_all(b" AND ")?;
        right(self)?;
        self.writer.write_all(b")")
    }

    fn visit_or<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        left(self)?;
        self.writer.write_all(b" OR ")?;
        right(self)?;
        self.writer.write_all(b")")
    }

    fn visit_not<P>(&mut self, predicate: P) -> Result<(), Self::Error>
    where
        P: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(NOT ")?;
        predicate(self)?;
        self.writer.write_all(b")")
    }
}

fn render_arithmetic_op(op: ArithmeticOp) -> &'static str {
    match op {
        ArithmeticOp::Add => "+",
        ArithmeticOp::Subtract => "-",
        ArithmeticOp::Multiply => "*",
        ArithmeticOp::Divide => "/",
    }
}

fn render_compare_op(op: CompareOp) -> &'static str {
    match op {
        CompareOp::Equals => "=",
        CompareOp::NotEquals => "<>",
        CompareOp::LessThan => "<",
        CompareOp::LessThanOrEquals => "<=",
        CompareOp::GreaterThan => ">",
        CompareOp::GreaterThanOrEquals => ">=",
    }
}

fn render_order_direction(direction: OrderDirection) -> &'static str {
    match direction {
        OrderDirection::Asc => "ASC",
        OrderDirection::Desc => "DESC",
    }
}

pub(crate) fn render_insert_prepared<S, Rows, Returning>(
    rows: &Rows,
    returning: &Returning,
    buffer: &mut PreparedSql,
) where
    S: InsertableTable,
    Rows: InsertRows,
    Returning: Projectable,
{
    buffer.clear();
    write_insert_with_params::<S, _, _, _>(rows, returning, buffer).unwrap();
}

pub(crate) fn write_insert_params<S, Rows, Returning, Sink>(
    rows: &Rows,
    returning: &Returning,
    sink: &mut Sink,
) -> Result<(), Sink::Error>
where
    S: InsertableTable,
    Rows: InsertRows,
    Returning: Projectable,
    Sink: BindSink,
{
    sink.reserve_bind_values(rows.param_count());
    let mut writer = ParamSinkWriter { sink, error: None };
    write_insert_with_params::<S, _, _, _>(rows, returning, &mut writer).unwrap();
    writer.finish()
}

pub(crate) fn render_delete_prepared<S, Filters, Returning>(
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    buffer: &mut PreparedSql,
) where
    S: TableProjection,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    buffer.clear();
    write_delete_with_params::<S, _, _, _>(alias, filters, returning, buffer).unwrap();
}

pub(crate) fn write_delete_params<S, Filters, Returning, Sink>(
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    sink: &mut Sink,
) -> Result<(), Sink::Error>
where
    S: TableProjection,
    Filters: PredicateNodes,
    Returning: Projectable,
    Sink: BindSink,
{
    sink.reserve_bind_values(filters.len());
    let mut writer = ParamSinkWriter { sink, error: None };
    write_delete_with_params::<S, _, _, _>(alias, filters, returning, &mut writer).unwrap();
    writer.finish()
}

pub(crate) fn render_update_prepared<S, Columns, Filters, Returning>(
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    buffer: &mut PreparedSql,
) where
    S: UpdateableTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    buffer.clear();
    write_update_with_params::<S, _, _, _, _>(alias, columns, filters, returning, buffer).unwrap();
}

pub(crate) fn write_update_params<S, Columns, Filters, Returning, Sink>(
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    sink: &mut Sink,
) -> Result<(), Sink::Error>
where
    S: UpdateableTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Sink: BindSink,
{
    sink.reserve_bind_values(columns.param_count() + filters.len());
    let mut writer = ParamSinkWriter { sink, error: None };
    write_update_with_params::<S, _, _, _, _>(alias, columns, filters, returning, &mut writer)
        .unwrap();
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render_type(ty: SqlType) -> String {
        let mut out = Vec::new();
        write_pg_sql_type(&ty, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn postgres_types_map_to_postgres_ddl_types() {
        let cases = [
            (SqlType::Bool, "boolean"),
            (SqlType::I8, "smallint"),
            (SqlType::I16, "smallint"),
            (SqlType::I32, "integer"),
            (SqlType::I64, "bigint"),
            (SqlType::I128, "numeric"),
            (SqlType::Isize, "bigint"),
            (SqlType::U8, "smallint"),
            (SqlType::U16, "integer"),
            (SqlType::U32, "bigint"),
            (SqlType::U64, "numeric"),
            (SqlType::U128, "numeric"),
            (SqlType::Usize, "bigint"),
            (SqlType::F32, "real"),
            (SqlType::F64, "double precision"),
            (SqlType::String, "text"),
            (SqlType::Raw("jsonb".to_owned()), "jsonb"),
        ];

        for (ty, expected) in cases {
            assert_eq!(render_type(ty), expected);
        }
    }

    #[test]
    fn postgres_renders_structured_types() {
        assert_eq!(render_type(SqlType::Varchar(64)), "varchar(64)");
        assert_eq!(render_type(SqlType::Char(2)), "char(2)");
        assert_eq!(render_type(SqlType::Text), "text");
        assert_eq!(
            render_type(SqlType::Decimal {
                precision: 10,
                scale: 2
            }),
            "numeric(10,2)"
        );
        assert_eq!(render_type(SqlType::Date), "date");
        assert_eq!(render_type(SqlType::Timestamp { tz: false }), "timestamp");
        assert_eq!(
            render_type(SqlType::Timestamp { tz: true }),
            "timestamp with time zone"
        );
        assert_eq!(render_type(SqlType::Uuid), "uuid");
        assert_eq!(render_type(SqlType::Jsonb), "jsonb");
        assert_eq!(render_type(SqlType::Bytes), "bytea");
    }
}
