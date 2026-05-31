use std::borrow::Cow;
use std::io::{self, Write};

use squealy::{
    ArithmeticOp, AssignmentValueRef, BindSink, BindValue, Column, ColumnDefault, ColumnRef,
    ColumnType, CompareOp, Expr, ExprAst, ExprKind, ExprVisitor, InsertAssignments,
    InsertableTable, Order, OrderDirection, Predicate, PredicateAst, PredicateAstVisitor,
    PredicateKind, PredicateNodes, PredicateVisitor, Projectable, ProjectionShape,
    ProjectionVisitor, QueryBuilder, SchemaTable, SelectAst, SelectSink, Selected, SourceAlias,
    Table, TableProjection, UpdateAssignments, UpdateableTable,
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
            write!(self.writer, "FROM {} AS {alias}", S::qualified_name())?;
        } else {
            write!(self.writer, "{join} {} AS {alias} ON ", S::qualified_name())?;
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
        write_column_value(column, self.writer, &mut self.renderer)?;
        write!(self.writer, " AS {alias}")
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
    writer.write_all(postgres_column_type(column.column_type()).as_bytes())
}

fn postgres_column_type(column_type: ColumnType) -> &'static str {
    match column_type {
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
        ColumnType::String => "text",
    }
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

pub(crate) fn write_insert<S, Columns, Returning>(
    columns: &Columns,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    Columns: InsertAssignments,
    Returning: Projectable,
{
    let mut writer = SqlOnly(writer);
    write_insert_with_params::<S, _, _, _>(columns, returning, &mut writer)
}

fn write_insert_with_params<S, Columns, Returning, Writer>(
    columns: &Columns,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: InsertableTable,
    Columns: InsertAssignments,
    Returning: Projectable,
    Writer: SqlWriter,
{
    let mut renderer = Renderer::default();
    write!(
        writer,
        "INSERT INTO {}",
        <S as SchemaTable>::qualified_name()
    )?;
    if columns.is_empty() {
        writer.write_all(b" DEFAULT VALUES")?;
    } else {
        writer.write_all(b" (")?;
        let mut index = 0;
        columns.try_for_each(|column, _value| {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            index += 1;
            writer.write_all(column.as_bytes())?;
            Ok::<(), io::Error>(())
        })?;
        writer.write_all(b") VALUES (")?;
        let mut index = 0;
        columns.try_for_each(|_column, value| {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            index += 1;
            write_assignment_value(value, writer, &mut renderer)?;
            Ok::<(), io::Error>(())
        })?;
        writer.write_all(b")")?;
    }
    write_insert_returning(returning, writer, &mut renderer)?;
    Ok(())
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
    write!(
        writer,
        "UPDATE {} AS {} SET ",
        <S as SchemaTable>::qualified_name(),
        alias
    )?;
    let mut index = 0;
    columns.try_for_each(|column, value| {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        index += 1;
        write!(writer, "{column} = ")?;
        write_assignment_value(value, writer, &mut renderer)?;
        Ok::<(), io::Error>(())
    })?;
    write_filters(filters, writer, &mut renderer)?;
    write_returning(returning, writer, &mut renderer)?;
    Ok(())
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
    write!(writer, "DELETE FROM {} AS {}", S::qualified_name(), alias)?;
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
        self.write_prefix()?;
        write_column_value_node(column, self.writer, self.renderer, self.insert_returning)?;
        write!(self.writer, " AS {alias}")
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

fn write_assignment_value(
    value: AssignmentValueRef<'_>,
    writer: &mut impl SqlWriter,
    renderer: &mut Renderer,
) -> io::Result<()> {
    match value {
        AssignmentValueRef::Static(value) => writer.push_bind(value),
        AssignmentValueRef::Runtime => {
            let index = renderer.next_runtime_param();
            writer.push_runtime_bind(index);
        }
    }
    renderer.write_placeholder(writer)
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
            self.writer.write_all(column.as_bytes())
        } else {
            write!(self.writer, "{alias}.{column}")
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

pub(crate) fn render_insert_prepared<S, Columns, Returning>(
    columns: &Columns,
    returning: &Returning,
    buffer: &mut PreparedSql,
) where
    S: InsertableTable,
    Columns: InsertAssignments,
    Returning: Projectable,
{
    buffer.clear();
    write_insert_with_params::<S, _, _, _>(columns, returning, buffer).unwrap();
}

pub(crate) fn write_insert_params<S, Columns, Returning, Sink>(
    columns: &Columns,
    returning: &Returning,
    sink: &mut Sink,
) -> Result<(), Sink::Error>
where
    S: InsertableTable,
    Columns: InsertAssignments,
    Returning: Projectable,
    Sink: BindSink,
{
    sink.reserve_bind_values(columns.len());
    let mut writer = ParamSinkWriter { sink, error: None };
    write_insert_with_params::<S, _, _, _>(columns, returning, &mut writer).unwrap();
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
    sink.reserve_bind_values(columns.len() + filters.len());
    let mut writer = ParamSinkWriter { sink, error: None };
    write_update_with_params::<S, _, _, _, _>(alias, columns, filters, returning, &mut writer)
        .unwrap();
    writer.finish()
}
