//! Shared SQL query renderer.
//!
//! Renders SELECT/INSERT/UPDATE/DELETE from the typed query AST. The logic is identical across
//! backends except for the dialect seams (placeholder, identifier quoting, cast type names), which
//! are supplied by a [`Dialect`](crate::Dialect) threaded through the [`Renderer`]. Backends call the
//! `write_*`/`render_*` entry points with their own dialect.
#![allow(clippy::result_unit_err)]

use std::borrow::Cow;
use std::io::{self, Write};

use crate::{
    ArithmeticOp, AssignmentNode, AssignmentValueVisitor, AssignmentVisitor, BindSink, BindValue,
    ColumnRef, CompareOp, Dialect, Expr, ExprAst, ExprKind, ExprVisitor, InsertAssignments,
    InsertRow, InsertRowVisitor, InsertRows, InsertableTable, Order, OrderDirection, Predicate,
    PredicateAst, PredicateAstVisitor, PredicateKind, PredicateNodes, PredicateVisitor,
    Projectable, ProjectionShape, ProjectionVisitor, QueryBuilder, SchemaTable, SelectAst,
    SelectSink, Selected, SourceAlias, SqlType, TableProjection, UpdateAssignments,
    UpdateableTable,
};

/// Threads the active [`Dialect`](crate::Dialect) and the running parameter counters through the
/// renderer. The dialect is `&'static` (backend dialects are zero-sized unit values), so carrying it
/// adds no lifetime to the renderer or the rendering structs.
#[derive(Clone, Copy)]
struct Renderer {
    dialect: &'static dyn Dialect,
    next_param: usize,
    next_runtime_param: usize,
}

impl Renderer {
    fn new(dialect: &'static dyn Dialect) -> Self {
        Self {
            dialect,
            next_param: 0,
            next_runtime_param: 0,
        }
    }

    fn write_placeholder(&mut self, writer: &mut impl Write) -> io::Result<()> {
        let index = self.next_param;
        self.next_param += 1;
        self.dialect.write_placeholder(index, writer)
    }

    fn next_runtime_param(&mut self) -> usize {
        let index = self.next_runtime_param;
        self.next_runtime_param += 1;
        index
    }
}

#[derive(Clone, Debug, Default)]
pub struct PreparedSql {
    sql: String,
    params: Vec<SqlParam>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum SqlParam {
    Static(BindValue),
    Runtime(usize),
}

impl PreparedSql {
    pub fn into_parts(self) -> (String, Vec<SqlParam>) {
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

struct SelectRenderSink<'writer, Writer> {
    writer: &'writer mut Writer,
    renderer: Renderer,
    columns: usize,
    sources: usize,
    filters: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl<'writer, Writer> SelectRenderSink<'writer, Writer>
where
    Writer: SqlWriter,
{
    fn new(writer: &'writer mut Writer, dialect: &'static dyn Dialect) -> io::Result<Self> {
        writer.write_all(b"SELECT ")?;
        Ok(Self {
            writer,
            renderer: Renderer::new(dialect),
            columns: 0,
            sources: 0,
            filters: 0,
            orders: 0,
            limit: None,
            offset: None,
        })
    }

    fn finish(self) -> io::Result<()> {
        self.renderer
            .dialect
            .write_limit_offset(self.limit, self.offset, self.writer)
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
            write_table_ref::<S>(self.renderer.dialect, self.writer)?;
            write!(self.writer, " AS {alias}")?;
        } else {
            write!(self.writer, "{join} ")?;
            write_table_ref::<S>(self.renderer.dialect, self.writer)?;
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

impl<Writer> SelectSink for SelectRenderSink<'_, Writer>
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
        write_table_ref::<S>(self.renderer.dialect, self.writer)?;
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

impl<Writer> ProjectionVisitor for SelectRenderSink<'_, Writer>
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
        self.renderer
            .dialect
            .write_quoted_ident(&alias, self.writer)?;
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
        self.renderer
            .dialect
            .write_quoted_ident(&alias, self.writer)
    }
}

pub fn render_selected_prepared<'conn, 'scope, Conn, Base, Shape, Projection>(
    dialect: &'static dyn Dialect,
    selected: &Selected<'scope, Base, Shape, Projection>,
    buffer: &mut PreparedSql,
) where
    Conn: QueryBuilder + 'conn,
    Base: SelectAst<'conn, 'scope, Conn>,
    Shape: ProjectionShape,
    Projection: Projectable,
{
    buffer.clear();
    let mut sink = SelectRenderSink::new(buffer, dialect).unwrap();
    selected.lower_into::<Conn, _>(&mut sink).unwrap();
    sink.finish().unwrap();
}

pub fn write_selected_into<'conn, 'scope, Conn, Base, Shape, Projection, Writer>(
    dialect: &'static dyn Dialect,
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
    let mut sink = SelectRenderSink::new(&mut writer, dialect)?;
    selected.lower_into::<Conn, _>(&mut sink)?;
    sink.finish()
}

pub fn write_selected_params<'conn, 'scope, Conn, Base, Shape, Projection, Sink>(
    dialect: &'static dyn Dialect,
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
    let mut select_sink = SelectRenderSink::new(&mut writer, dialect).unwrap();
    selected.lower_into::<Conn, _>(&mut select_sink).unwrap();
    select_sink.finish().unwrap();
    writer.finish()
}
fn write_table_ref<S>(dialect: &dyn Dialect, writer: &mut impl Write) -> io::Result<()>
where
    S: TableProjection,
{
    if let Some(schema) = <S as TableProjection>::schema_name() {
        dialect.write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    dialect.write_quoted_ident(<S as TableProjection>::name(), writer)
}

/// Writes a quoted, schema-qualified reference to a `SchemaTable` model.
fn write_schema_table_ref<S>(dialect: &dyn Dialect, writer: &mut impl Write) -> io::Result<()>
where
    S: SchemaTable,
{
    if let Some(schema) = <S as SchemaTable>::schema_name() {
        dialect.write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    dialect.write_quoted_ident(<S as SchemaTable>::name(), writer)
}

pub fn write_insert<S, Rows, Returning>(
    dialect: &'static dyn Dialect,
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
    write_insert_with_params::<S, _, _, _>(dialect, rows, returning, &mut writer)
}

fn write_insert_with_params<S, Rows, Returning, Writer>(
    dialect: &'static dyn Dialect,
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
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"INSERT INTO ")?;
    write_schema_table_ref::<S>(dialect, writer)?;
    if rows.len() == 1 && rows.first_row_len() == 0 {
        dialect.write_default_row_insert(writer)?;
    } else {
        writer.write_all(b" (")?;
        let mut index = 0;
        rows.try_for_each_column(|column| {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            index += 1;
            dialect.write_quoted_ident(column, writer)?;
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

pub fn write_update<S, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
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
    write_update_with_params::<S, _, _, _, _>(
        dialect,
        alias,
        columns,
        filters,
        returning,
        &mut writer,
    )
}

fn write_update_with_params<S, Columns, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
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
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"UPDATE ")?;
    write_schema_table_ref::<S>(renderer.dialect, writer)?;
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
        self.renderer
            .dialect
            .write_quoted_ident(column, self.writer)?;
        self.writer.write_all(b" = ")?;
        write_assignment_value(value, self.writer, self.renderer)
    }
}

pub fn write_delete<S, Filters, Returning>(
    dialect: &'static dyn Dialect,
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
    write_delete_with_params::<S, _, _, _>(dialect, alias, filters, returning, &mut writer)
}

fn write_delete_with_params<S, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
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
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"DELETE FROM ")?;
    write_table_ref::<S>(renderer.dialect, writer)?;
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
        self.renderer
            .dialect
            .write_quoted_ident(&alias, self.writer)
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
        self.renderer
            .dialect
            .write_quoted_ident(&alias, self.writer)
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
            self.renderer
                .dialect
                .write_quoted_ident(column, &mut *self.writer)
        } else {
            write!(self.writer, "{alias}.")?;
            self.renderer
                .dialect
                .write_quoted_ident(column, &mut *self.writer)
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
        if op == ArithmeticOp::Divide && self.renderer.dialect.integer_division_needs_float_cast() {
            // Cast operands to float so integer `/` matches the builder's always-fractional division.
            // Dialects where `/` is already float division (MySQL) skip this and fall through to a
            // plain `/`.
            let dialect = self.renderer.dialect;
            self.writer.write_all(b"(CAST(")?;
            left(self)?;
            self.writer.write_all(b" AS ")?;
            dialect.write_cast_type(&SqlType::F64, &mut *self.writer)?;
            self.writer.write_all(b") / CAST(")?;
            right(self)?;
            self.writer.write_all(b" AS ")?;
            dialect.write_cast_type(&SqlType::F64, &mut *self.writer)?;
            return self.writer.write_all(b"))");
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

pub fn render_insert_prepared<S, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    buffer: &mut PreparedSql,
) where
    S: InsertableTable,
    Rows: InsertRows,
    Returning: Projectable,
{
    buffer.clear();
    write_insert_with_params::<S, _, _, _>(dialect, rows, returning, buffer).unwrap();
}

pub fn write_insert_params<S, Rows, Returning, Sink>(
    dialect: &'static dyn Dialect,
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
    write_insert_with_params::<S, _, _, _>(dialect, rows, returning, &mut writer).unwrap();
    writer.finish()
}

pub fn render_delete_prepared<S, Filters, Returning>(
    dialect: &'static dyn Dialect,
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
    write_delete_with_params::<S, _, _, _>(dialect, alias, filters, returning, buffer).unwrap();
}

pub fn write_delete_params<S, Filters, Returning, Sink>(
    dialect: &'static dyn Dialect,
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
    write_delete_with_params::<S, _, _, _>(dialect, alias, filters, returning, &mut writer)
        .unwrap();
    writer.finish()
}

pub fn render_update_prepared<S, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
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
    write_update_with_params::<S, _, _, _, _>(dialect, alias, columns, filters, returning, buffer)
        .unwrap();
}

pub fn write_update_params<S, Columns, Filters, Returning, Sink>(
    dialect: &'static dyn Dialect,
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
    write_update_with_params::<S, _, _, _, _>(
        dialect,
        alias,
        columns,
        filters,
        returning,
        &mut writer,
    )
    .unwrap();
    writer.finish()
}
