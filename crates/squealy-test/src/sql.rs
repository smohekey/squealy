use std::borrow::Cow;
use std::io::{self, Write};

use squealy::{
    ArithmeticOp, AssignmentValueVisitor, AssignmentVisitor, Column, ColumnDefault, ColumnRef,
    ColumnType, CompareOp, Encode, Expr, ExprKind, ExprVisitor, InsertRow, InsertRowVisitor,
    InsertableTable, Order, OrderDirection, Predicate, PredicateAstVisitor, PredicateKind,
    PredicateVisitor, ProjectionShape, ProjectionVisitor, QueryBuilder, RenderAssignment,
    RenderAst, RenderInsertAssignments, RenderInsertRows, RenderPredicateAst, RenderPredicateNodes,
    RenderProjectable, RenderSelectAst, RenderUpdateAssignments, SchemaTable, SelectSink, Selected,
    SourceAlias, Table, TableProjection, UpdateableTable,
};

use crate::query::{TestParam, TestParamWriter};

trait SqlWriter: Write {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<crate::TestBackend>;

    fn push_runtime_bind(&mut self);
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
    fn push_bind<T>(&mut self, _value: &T)
    where
        T: Encode<crate::TestBackend>,
    {
    }

    fn push_runtime_bind(&mut self) {}
}

/// Collects literal binds into a [`TestParam`] vector for inspection, discarding SQL text.
struct ParamCollector<'params> {
    params: &'params mut Vec<TestParam>,
    error: Option<crate::TestError>,
}

impl<'params> ParamCollector<'params> {
    fn new(params: &'params mut Vec<TestParam>) -> Self {
        Self {
            params,
            error: None,
        }
    }

    fn finish(self) -> Result<(), crate::TestError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl Write for ParamCollector<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SqlWriter for ParamCollector<'_> {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<crate::TestBackend>,
    {
        if self.error.is_none() {
            let mut writer = TestParamWriter::new(self.params);
            self.error = value.encode(&mut writer).err();
        }
    }

    fn push_runtime_bind(&mut self) {}
}

struct TestSelectSink<'writer, Writer> {
    writer: &'writer mut Writer,
    columns: usize,
    sources: usize,
    filters: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl<'writer, Writer> TestSelectSink<'writer, Writer>
where
    Writer: SqlWriter,
{
    fn new(writer: &'writer mut Writer) -> io::Result<Self> {
        writer.write_all(b"SELECT ")?;
        Ok(Self {
            writer,
            columns: 0,
            sources: 0,
            filters: 0,
            orders: 0,
            limit: None,
            offset: None,
        })
    }
}

impl<Writer> SelectSink for TestSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn push_projection<Shape, P>(&mut self, projection: P) -> io::Result<()>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<crate::TestBackend>,
    {
        _ = std::marker::PhantomData::<Shape>;
        projection.visit_projection(self)
    }

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> io::Result<()>
    where
        S: TableProjection,
    {
        self.push_source_separator()?;
        write!(self.writer, "FROM {} AS {alias}", S::qualified_name())
    }

    fn push_inner_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<crate::TestBackend>,
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
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        self.push_join::<S, P, Ast>(alias, on, "LEFT JOIN")
    }

    fn push_filter<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> io::Result<()>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        if self.filters == 0 {
            self.writer.write_all(b" WHERE ")?;
        } else {
            self.writer.write_all(b" AND ")?;
        }
        self.filters += 1;
        write_predicate_value(&predicate, self.writer)
    }

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<crate::TestBackend>,
    {
        if self.orders == 0 {
            self.writer.write_all(b" ORDER BY ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.orders += 1;
        write_order_value(&order, self.writer)
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

impl<Writer> ProjectionVisitor for TestSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<crate::TestBackend>,
    {
        self.push_projection_separator()?;
        write_expr_value(expr, self.writer)?;
        write!(self.writer, " AS {alias}")?;
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
        write_column_value(column, self.writer)?;
        write!(self.writer, " AS {alias}")?;
        Ok(())
    }
}

pub(crate) fn write_selected_into<'conn, 'scope, Conn, Base, Shape, Projection, Writer>(
    selected: &Selected<'scope, Base, Shape, Projection>,
    writer: &mut Writer,
) -> io::Result<()>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, crate::TestBackend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<crate::TestBackend>,
    Writer: Write,
{
    let mut writer = SqlOnly(writer);
    let mut sink = TestSelectSink::new(&mut writer)?;
    selected.lower_into::<Conn, _>(&mut sink)?;
    sink.finish()
}

pub(crate) fn write_selected_params<'conn, 'scope, Conn, Base, Shape, Projection>(
    selected: &Selected<'scope, Base, Shape, Projection>,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, crate::TestBackend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<crate::TestBackend>,
{
    let mut writer = ParamCollector::new(params);
    let mut select_sink = TestSelectSink::new(&mut writer).unwrap();
    selected.lower_into::<Conn, _>(&mut select_sink).unwrap();
    select_sink.finish().unwrap();
    writer.finish()
}

impl<Writer> TestSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
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
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        let first_source = self.sources == 0;
        self.push_source_separator()?;
        if first_source {
            write!(self.writer, "FROM {} AS {alias}", S::qualified_name())?;
        } else {
            write!(self.writer, "{join} {} AS {alias} ON ", S::qualified_name(),)?;
            write_predicate_value(&on, self.writer)?;
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

pub(crate) fn write_table(table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()> {
    write!(writer, "CREATE TABLE {} (", table.qualified_name())?;
    for (index, column) in table.columns().iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} ", column.name())?;
        write_column_type(*column, writer)?;
        if column.primary_key() {
            writer.write_all(b" PRIMARY KEY")?;
        }
        if column.auto_increment() {
            writer.write_all(b" AUTOINCREMENT")?;
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
            if let Some(schema) = reference.schema_name() {
                write!(writer, "{schema}.")?;
            }
            write!(writer, "{}({})", reference.table(), reference.column())?;
            if let Some(on_delete) = reference.on_delete() {
                write!(writer, " ON DELETE {on_delete}")?;
            }
            if let Some(on_update) = reference.on_update() {
                write!(writer, " ON UPDATE {on_update}")?;
            }
        }
    }
    // A table-level `#[primary_key(columns = [..])]` is not hung off any single column, so render it
    // as a trailing constraint here (the per-column `primary_key` form above covers single columns).
    if let Some(primary_key) = table.primary_key() {
        writer.write_all(b", ")?;
        if let Some(name) = primary_key.name {
            write!(writer, "CONSTRAINT {name} ")?;
        }
        writer.write_all(b"PRIMARY KEY (")?;
        write_comma_separated(primary_key.columns, writer)?;
        writer.write_all(b")")?;
    }
    writer.write_all(b")")?;

    for index in table.indexes() {
        let unique = if index.unique() { "UNIQUE " } else { "" };
        let name = index.name().unwrap_or("unnamed_idx");
        write!(
            writer,
            "\nCREATE {unique}INDEX {name} ON {} (",
            table.qualified_name()
        )?;
        write_comma_separated(index.columns(), writer)?;
        writer.write_all(b")")?;
    }

    Ok(())
}

fn write_column_type(column: &dyn Column, writer: &mut impl Write) -> io::Result<()> {
    write_test_column_type(column.column_type(), writer)
}

fn write_test_column_type(column_type: ColumnType, writer: &mut impl Write) -> io::Result<()> {
    let name = match column_type {
        ColumnType::Raw(db_type) => db_type,
        ColumnType::Bool => "boolean",
        ColumnType::I8 | ColumnType::I16 => "smallint",
        ColumnType::I32 => "integer",
        ColumnType::I64 | ColumnType::Isize => "bigint",
        ColumnType::I128 => "numeric",
        ColumnType::U8 => "smallint",
        ColumnType::U16 => "integer",
        ColumnType::U32 | ColumnType::Usize => "bigint",
        ColumnType::U64 | ColumnType::U128 => "numeric",
        ColumnType::F32 => "real",
        ColumnType::F64 => "double precision",
        ColumnType::String | ColumnType::Text => "text",
        ColumnType::Varchar(length) => return write!(writer, "varchar({length})"),
        ColumnType::Char(length) => return write!(writer, "char({length})"),
        ColumnType::Decimal { precision, scale } => {
            return write!(writer, "numeric({precision},{scale})");
        }
        ColumnType::Date => "date",
        ColumnType::Time { tz } => {
            if tz {
                "time with time zone"
            } else {
                "time"
            }
        }
        ColumnType::Timestamp { tz } => {
            if tz {
                "timestamp with time zone"
            } else {
                "timestamp"
            }
        }
        ColumnType::Uuid => "uuid",
        ColumnType::Json => "json",
        ColumnType::Jsonb => "jsonb",
        ColumnType::Bytes => "bytea",
    };
    writer.write_all(name.as_bytes())
}

fn write_comma_separated(values: &[&'static str], writer: &mut impl Write) -> io::Result<()> {
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        writer.write_all(value.as_bytes())?;
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

fn write_quoted_text(value: &str, writer: &mut impl Write) -> io::Result<()> {
    writer.write_all(b"'")?;
    for byte in value.as_bytes() {
        if *byte == b'\'' {
            writer.write_all(b"''")?;
        } else {
            writer.write_all(std::slice::from_ref(byte))?;
        }
    }
    writer.write_all(b"'")
}

pub(crate) fn write_insert<S, Rows, Returning>(
    rows: &Rows,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    Rows: RenderInsertRows<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
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
    Rows: RenderInsertRows<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
    Writer: SqlWriter,
{
    write!(
        writer,
        "INSERT INTO {}",
        <S as SchemaTable>::qualified_name()
    )?;
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
            writer.write_all(column.as_bytes())?;
            Ok::<(), io::Error>(())
        })?;
        writer.write_all(b") VALUES ")?;
        write_insert_rows(rows, writer)?;
    }
    write_returning(returning, writer)?;
    Ok(())
}

struct WriteInsertRows<'writer, Writer> {
    writer: &'writer mut Writer,
    expected_columns: usize,
    row_index: usize,
}

impl<Writer> InsertRowVisitor<io::Error> for WriteInsertRows<'_, Writer>
where
    Writer: SqlWriter,
{
    type Backend = crate::TestBackend;

    fn visit_row<Columns>(&mut self, row: &InsertRow<Columns>) -> io::Result<()>
    where
        Columns: RenderInsertAssignments<crate::TestBackend>,
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
            index: 0,
        };
        row.columns().try_visit(&mut assignments)?;
        self.writer.write_all(b")")
    }
}

struct WriteAssignmentValues<'writer, Writer> {
    writer: &'writer mut Writer,
    index: usize,
}

impl<Writer> AssignmentVisitor for WriteAssignmentValues<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_assignment<Value>(
        &mut self,
        _column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: RenderAssignment<crate::TestBackend>,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        write_assignment_value(value, self.writer)
    }
}

fn write_insert_rows<Rows, Writer>(rows: &Rows, writer: &mut Writer) -> io::Result<()>
where
    Rows: RenderInsertRows<crate::TestBackend>,
    Writer: SqlWriter,
{
    let mut visitor = WriteInsertRows {
        writer,
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
    Columns: RenderUpdateAssignments<crate::TestBackend>,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
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
    Columns: RenderUpdateAssignments<crate::TestBackend>,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
    Writer: SqlWriter,
{
    write!(
        writer,
        "UPDATE {} AS {} SET ",
        <S as SchemaTable>::qualified_name(),
        alias
    )?;
    let mut assignments = WriteUpdateAssignments { writer, index: 0 };
    columns.try_visit(&mut assignments)?;
    write_filters(filters, writer)?;
    write_returning(returning, writer)?;
    Ok(())
}

struct WriteUpdateAssignments<'writer, Writer> {
    writer: &'writer mut Writer,
    index: usize,
}

impl<Writer> AssignmentVisitor for WriteUpdateAssignments<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_assignment<Value>(
        &mut self,
        column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: RenderAssignment<crate::TestBackend>,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        write!(self.writer, "{column} = ")?;
        write_assignment_value(value, self.writer)
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
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
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
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
    Writer: SqlWriter,
{
    write!(writer, "DELETE FROM {} AS {}", S::qualified_name(), alias)?;
    write_filters(filters, writer)?;
    write_returning(returning, writer)?;
    Ok(())
}

fn write_returning(
    returning: &impl RenderProjectable<crate::TestBackend>,
    writer: &mut impl SqlWriter,
) -> io::Result<()> {
    returning.visit_projection(&mut WriteProjection {
        writer,
        index: 0,
        prefix: b" RETURNING ",
    })
}

struct WriteProjection<'writer, Writer> {
    writer: &'writer mut Writer,
    index: usize,
    prefix: &'static [u8],
}

impl<Writer> WriteProjection<'_, Writer>
where
    Writer: SqlWriter,
{
    fn write_projection_separator(&mut self) -> io::Result<()> {
        if self.index == 0 {
            self.writer.write_all(self.prefix)?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        Ok(())
    }
}

impl<Writer> ProjectionVisitor for WriteProjection<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<crate::TestBackend>,
    {
        self.write_projection_separator()?;
        write_expr_value(expr, self.writer)?;
        write!(self.writer, " AS {alias}")
    }

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
    {
        self.write_projection_separator()?;
        write_column_value(column, self.writer)?;
        write!(self.writer, " AS {alias}")
    }
}

fn write_filters(
    filters: &impl RenderPredicateNodes<crate::TestBackend>,
    writer: &mut impl SqlWriter,
) -> io::Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    writer.write_all(b" WHERE ")?;
    filters.try_visit(&mut WritePredicateFilters { writer, index: 0 })?;
    Ok(())
}

struct WritePredicateFilters<'writer, Writer> {
    writer: &'writer mut Writer,
    index: usize,
}

impl<Writer> PredicateVisitor for WritePredicateFilters<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_predicate<Kind, Ast>(&mut self, predicate: &Predicate<'_, Kind, Ast>) -> io::Result<()>
    where
        Kind: PredicateKind,
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        if self.index > 0 {
            self.writer.write_all(b" AND ")?;
        }
        self.index += 1;
        write_predicate_value(predicate, self.writer)
    }
}

fn write_expr_value<K, Ast>(expr: &Expr<'_, K, Ast>, writer: &mut impl SqlWriter) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<crate::TestBackend>,
{
    expr.visit(&mut RenderExpr { writer })
}

fn write_column_value<K>(column: ColumnRef<'_, K>, writer: &mut impl SqlWriter) -> io::Result<()>
where
    K: ExprKind,
{
    column.visit(&mut RenderExpr { writer })
}

fn write_predicate_value<K, Ast>(
    predicate: &Predicate<'_, K, Ast>,
    writer: &mut impl SqlWriter,
) -> io::Result<()>
where
    K: PredicateKind,
    Ast: RenderPredicateAst<crate::TestBackend>,
{
    predicate.visit(&mut RenderExpr { writer })
}

fn write_order_value<K, Ast>(
    order: &Order<'_, K, Ast>,
    writer: &mut impl SqlWriter,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<crate::TestBackend>,
{
    order.visit_expr(&mut RenderExpr { writer })?;
    write!(writer, " {}", render_order_direction(order.direction()))
}

fn write_assignment_value<Value>(value: &Value, writer: &mut impl SqlWriter) -> io::Result<()>
where
    Value: RenderAssignment<crate::TestBackend>,
{
    value.visit_value(&mut RenderAssignmentValue { writer })
}

struct RenderAssignmentValue<'writer, Writer> {
    writer: &'writer mut Writer,
}

impl<Writer> AssignmentValueVisitor for RenderAssignmentValue<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_static<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: Encode<crate::TestBackend>,
    {
        self.writer.push_bind(value);
        self.writer.write_all(b"?")
    }

    fn visit_default(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b"DEFAULT")
    }

    fn visit_runtime(&mut self) -> Result<(), Self::Error> {
        self.writer.push_runtime_bind();
        self.writer.write_all(b"?")
    }

    fn visit_expr<K, Ast>(&mut self, expr: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<crate::TestBackend>,
    {
        write_expr_value(expr, self.writer)
    }
}

struct RenderExpr<'writer, Writer> {
    writer: &'writer mut Writer,
}

impl<Writer> ExprVisitor for RenderExpr<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn visit_column(&mut self, alias: SourceAlias, column: &str) -> Result<(), Self::Error> {
        write!(self.writer, "{alias}.{column}")
    }

    fn visit_literal<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: Encode<crate::TestBackend>,
    {
        self.writer.push_bind(value);
        self.writer.write_all(b"?")
    }

    fn visit_param(&mut self) -> Result<(), Self::Error> {
        self.writer.push_runtime_bind();
        self.writer.write_all(b"?")
    }

    fn visit_binary<L, R>(&mut self, op: ArithmeticOp, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        left(self)?;
        write!(self.writer, " {} ", render_arithmetic_op(op))?;
        right(self)?;
        self.writer.write_all(b")")
    }
}

impl<Writer> PredicateAstVisitor for RenderExpr<'_, Writer>
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

    fn visit_is_null<O>(&mut self, negated: bool, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        operand(self)?;
        if negated {
            self.writer.write_all(b" IS NOT NULL)")
        } else {
            self.writer.write_all(b" IS NULL)")
        }
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

pub(crate) fn write_insert_params<S, Rows, Returning>(
    rows: &Rows,
    returning: &Returning,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    S: InsertableTable,
    Rows: RenderInsertRows<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
{
    params.reserve(rows.param_count());
    let mut writer = ParamCollector::new(params);
    write_insert_with_params::<S, _, _, _>(rows, returning, &mut writer).unwrap();
    writer.finish()
}

pub(crate) fn write_delete_params<S, Filters, Returning>(
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    S: TableProjection,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
{
    params.reserve(filters.len());
    let mut writer = ParamCollector::new(params);
    write_delete_with_params::<S, _, _, _>(alias, filters, returning, &mut writer).unwrap();
    writer.finish()
}

pub(crate) fn write_update_params<S, Columns, Filters, Returning>(
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    S: UpdateableTable,
    Columns: RenderUpdateAssignments<crate::TestBackend>,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
{
    params.reserve(columns.param_count() + filters.len());
    let mut writer = ParamCollector::new(params);
    write_update_with_params::<S, _, _, _, _>(alias, columns, filters, returning, &mut writer)
        .unwrap();
    writer.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_column_types_map_to_test_backend_ddl_types() {
        let cases = [
            (ColumnType::Bool, "boolean"),
            (ColumnType::I8, "smallint"),
            (ColumnType::I16, "smallint"),
            (ColumnType::I32, "integer"),
            (ColumnType::I64, "bigint"),
            (ColumnType::I128, "numeric"),
            (ColumnType::Isize, "bigint"),
            (ColumnType::U8, "smallint"),
            (ColumnType::U16, "integer"),
            (ColumnType::U32, "bigint"),
            (ColumnType::U64, "numeric"),
            (ColumnType::U128, "numeric"),
            (ColumnType::Usize, "bigint"),
            (ColumnType::F32, "real"),
            (ColumnType::F64, "double precision"),
            (ColumnType::String, "text"),
            (ColumnType::Raw("jsonb"), "jsonb"),
        ];

        for (column_type, db_type) in cases {
            let mut out = Vec::new();
            write_test_column_type(column_type, &mut out).unwrap();
            assert_eq!(String::from_utf8(out).unwrap(), db_type);
        }
    }
}
