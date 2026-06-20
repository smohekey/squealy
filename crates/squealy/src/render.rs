//! Shared SQL query renderer.
//!
//! Renders SELECT/INSERT/UPDATE/DELETE from the typed query AST. The logic is identical across
//! backends except for two seams:
//!
//! * **Syntax** — placeholder style, identifier quoting, cast type names — supplied by a
//!   [`Dialect`](crate::Dialect) threaded through the [`Renderer`].
//! * **Value encoding** — each literal is encoded to the backend's native bound-parameter type
//!   ([`Backend::Param`](crate::Backend::Param)) via [`Encode<B>`](crate::Encode) at render time,
//!   the mirror of how [`Decode<B>`](crate::Decode) reads a value back. There is no neutral value
//!   form: the renderer is generic over the backend `B` so a `uuid`/`jsonb`/extension literal binds
//!   natively without passing through a closed enum.
#![allow(clippy::result_unit_err)]

use std::borrow::Cow;
use std::io::{self, Write};

use crate::{
    AggregateFunc, ArithmeticOp, AssignmentValueVisitor, AssignmentVisitor, Backend, ColumnRef,
    CompareOp, Dialect, Encode, Expr, ExprKind, ExprVisitor, InsertRow, InsertRowVisitor,
    InsertableTable, Order, OrderDirection, Predicate, PredicateAstVisitor, PredicateKind,
    PredicateVisitor, ProjectionShape, ProjectionVisitor, QueryBuilder, RenderAssignment,
    RenderAst, RenderInsertAssignments, RenderInsertRows, RenderPredicateAst, RenderPredicateNodes,
    RenderProjectable, RenderSelectAst, RenderUpdateAssignments, SchemaTable, SelectSink, Selected,
    SourceAlias, SqlType, TableProjection, UpdateableTable,
};
use std::marker::PhantomData;

/// Threads the active [`Dialect`](crate::Dialect) and the running parameter counters through the
/// renderer. The dialect is `&'static` (backend dialects are zero-sized unit values), so carrying it
/// adds no lifetime to the renderer or the rendering structs.
#[derive(Clone, Copy)]
pub(crate) struct Renderer {
    dialect: &'static dyn Dialect,
    next_param: usize,
    next_runtime_param: usize,
}

impl Renderer {
    pub(crate) fn new(dialect: &'static dyn Dialect) -> Self {
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

/// A rendered placeholder slot: either a literal already encoded to the backend's native param, or a
/// runtime-parameter slot resolved from user-supplied values at execution.
pub enum SqlParam<B: Backend> {
    Static(B::Param),
    Runtime(usize),
}

impl<B: Backend> Clone for SqlParam<B>
where
    B::Param: Clone,
{
    fn clone(&self) -> Self {
        match self {
            SqlParam::Static(param) => SqlParam::Static(param.clone()),
            SqlParam::Runtime(index) => SqlParam::Runtime(*index),
        }
    }
}

impl<B: Backend> std::fmt::Debug for SqlParam<B>
where
    B::Param: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SqlParam::Static(param) => f.debug_tuple("Static").field(param).finish(),
            SqlParam::Runtime(index) => f.debug_tuple("Runtime").field(index).finish(),
        }
    }
}

impl<B: Backend> PartialEq for SqlParam<B>
where
    B::Param: PartialEq,
{
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (SqlParam::Static(a), SqlParam::Static(b)) => a == b,
            (SqlParam::Runtime(a), SqlParam::Runtime(b)) => a == b,
            _ => false,
        }
    }
}

/// A rendered statement: SQL text plus the ordered placeholder binds. Literal binds are already
/// encoded to [`Backend::Param`]; runtime binds carry the user-parameter index to resolve at
/// execution. An encode failure is captured and surfaced from [`into_parts`](Self::into_parts).
pub struct PreparedSql<B: Backend> {
    sql: String,
    params: Vec<SqlParam<B>>,
    error: Option<B::Error>,
}

impl<B: Backend> Default for PreparedSql<B> {
    fn default() -> Self {
        Self {
            sql: String::new(),
            params: Vec::new(),
            error: None,
        }
    }
}

impl<B: Backend> PreparedSql<B> {
    /// Consume the rendered statement, returning `(sql, params)` or the captured encode error.
    pub fn into_parts(self) -> Result<(String, Vec<SqlParam<B>>), B::Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok((self.sql, self.params)),
        }
    }

    fn clear(&mut self) {
        self.sql.clear();
        self.params.clear();
        self.error = None;
    }

    fn push_runtime_param(&mut self, index: usize) {
        self.params.push(SqlParam::Runtime(index));
    }
}

impl<B: Backend> Write for PreparedSql<B> {
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

/// Encode a single literal into the backend's native param representation.
fn encode_static<B, T>(value: &T) -> Result<Vec<B::Param>, B::Error>
where
    B: Backend,
    T: Encode<B>,
{
    let mut params = Vec::new();
    {
        let mut writer = B::param_writer(&mut params);
        value.encode(&mut writer)?;
    }
    Ok(params)
}

/// Encode-side render sink: produces SQL text and records each placeholder's bind, either a literal
/// (encoded now via [`Encode`]) or a runtime-parameter slot resolved later.
pub(crate) trait SqlWriter<B: Backend>: Write {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<B>;

    fn push_runtime_bind(&mut self, index: usize);
}

impl<B: Backend> SqlWriter<B> for PreparedSql<B> {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<B>,
    {
        if self.error.is_some() {
            return;
        }
        match encode_static::<B, T>(value) {
            Ok(encoded) => self
                .params
                .extend(encoded.into_iter().map(SqlParam::Static)),
            Err(error) => self.error = Some(error),
        }
    }

    fn push_runtime_bind(&mut self, index: usize) {
        self.push_runtime_param(index);
    }
}

/// A render sink that emits SQL text only, discarding binds. Used by the to-SQL path.
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

impl<B, Writer> SqlWriter<B> for SqlOnly<'_, Writer>
where
    B: Backend,
    Writer: Write,
{
    fn push_bind<T>(&mut self, _value: &T)
    where
        T: Encode<B>,
    {
    }

    fn push_runtime_bind(&mut self, _index: usize) {}
}

/// A render sink that collects literal binds directly into a native param vector, discarding SQL
/// text. Used by the one-shot (non-prepared) execution path.
struct ParamCollector<'params, B: Backend> {
    params: &'params mut Vec<B::Param>,
    error: Option<B::Error>,
}

impl<'params, B: Backend> ParamCollector<'params, B> {
    fn new(params: &'params mut Vec<B::Param>) -> Self {
        Self {
            params,
            error: None,
        }
    }

    fn finish(self) -> Result<(), B::Error> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

impl<B: Backend> Write for ParamCollector<'_, B> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<B: Backend> SqlWriter<B> for ParamCollector<'_, B> {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<B>,
    {
        if self.error.is_some() {
            return;
        }
        match encode_static::<B, T>(value) {
            Ok(encoded) => self.params.extend(encoded),
            Err(error) => self.error = Some(error),
        }
    }

    fn push_runtime_bind(&mut self, _index: usize) {}
}

struct SelectRenderSink<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    columns: usize,
    sources: usize,
    filters: usize,
    groups: usize,
    havings: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
    _backend: PhantomData<B>,
}

impl<'writer, 'renderer, B, Writer> SelectRenderSink<'writer, 'renderer, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    /// Open a SELECT sharing the caller's [`Renderer`]. Borrowing (rather than owning) the renderer
    /// is what lets a nested subquery continue the parent's placeholder numbering instead of
    /// restarting from zero — see [`RenderExpr`]'s subquery visitor methods.
    fn new(writer: &'writer mut Writer, renderer: &'renderer mut Renderer) -> io::Result<Self> {
        writer.write_all(b"SELECT ")?;
        Ok(Self {
            writer,
            renderer,
            columns: 0,
            sources: 0,
            filters: 0,
            groups: 0,
            havings: 0,
            orders: 0,
            limit: None,
            offset: None,
            _backend: PhantomData,
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
        Ast: RenderPredicateAst<B>,
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
            write_predicate_value(&on, self.writer, &mut *self.renderer)?;
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

impl<B, Writer> SelectSink for SelectRenderSink<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn push_projection<Shape, P>(&mut self, projection: P) -> io::Result<()>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<B>,
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
        Ast: RenderPredicateAst<B>,
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
        Ast: RenderPredicateAst<B>,
    {
        self.push_join::<S, P, Ast>(alias, on, "LEFT JOIN")
    }

    fn push_filter<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> io::Result<()>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<B>,
    {
        if self.filters == 0 {
            self.writer.write_all(b" WHERE ")?;
        } else {
            self.writer.write_all(b" AND ")?;
        }
        self.filters += 1;
        write_predicate_value(&predicate, self.writer, &mut *self.renderer)
    }

    fn push_group<K, Ast>(&mut self, key: &Expr<'_, K, Ast>) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<B>,
    {
        if self.groups == 0 {
            self.writer.write_all(b" GROUP BY ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.groups += 1;
        write_expr_value(key, self.writer, &mut *self.renderer)
    }

    fn push_having<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> io::Result<()>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<B>,
    {
        if self.havings == 0 {
            self.writer.write_all(b" HAVING ")?;
        } else {
            self.writer.write_all(b" AND ")?;
        }
        self.havings += 1;
        write_predicate_value(&predicate, self.writer, &mut *self.renderer)
    }

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<B>,
    {
        if self.orders == 0 {
            self.writer.write_all(b" ORDER BY ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.orders += 1;
        write_order_value(&order, self.writer, &mut *self.renderer)
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

impl<B, Writer> ProjectionVisitor for SelectRenderSink<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<B>,
    {
        self.push_projection_separator()?;
        write_expr_value(expr, self.writer, &mut *self.renderer)?;
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
        write_column_value(column, self.writer, &mut *self.renderer)?;
        self.writer.write_all(b" AS ")?;
        self.renderer
            .dialect
            .write_quoted_ident(&alias, self.writer)
    }
}

pub fn render_selected_prepared<'conn, 'scope, Conn, Base, Shape, Projection>(
    dialect: &'static dyn Dialect,
    selected: &Selected<'scope, Base, Shape, Projection>,
    buffer: &mut PreparedSql<Conn::Backend>,
) where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, Conn::Backend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<Conn::Backend>,
{
    buffer.clear();
    let mut renderer = Renderer::new(dialect);
    let mut sink = SelectRenderSink::<Conn::Backend, _>::new(buffer, &mut renderer).unwrap();
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
    Base: RenderSelectAst<'conn, 'scope, Conn, Conn::Backend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<Conn::Backend>,
    Writer: Write,
{
    let mut writer = SqlOnly(writer);
    let mut renderer = Renderer::new(dialect);
    let mut sink = SelectRenderSink::<Conn::Backend, _>::new(&mut writer, &mut renderer)?;
    selected.lower_into::<Conn, _>(&mut sink)?;
    sink.finish()
}

pub fn write_selected_params<'conn, 'scope, Conn, Base, Shape, Projection>(
    dialect: &'static dyn Dialect,
    selected: &Selected<'scope, Base, Shape, Projection>,
    params: &mut Vec<<Conn::Backend as Backend>::Param>,
) -> Result<(), <Conn::Backend as Backend>::Error>
where
    Conn: QueryBuilder + 'conn,
    Base: RenderSelectAst<'conn, 'scope, Conn, Conn::Backend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<Conn::Backend>,
{
    let mut writer = ParamCollector::<Conn::Backend>::new(params);
    let mut renderer = Renderer::new(dialect);
    let mut select_sink =
        SelectRenderSink::<Conn::Backend, _>::new(&mut writer, &mut renderer).unwrap();
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

pub fn write_insert<S, B, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, &mut writer)
}

fn write_insert_with_params<S, B, Rows, Returning, Writer>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
    Writer: SqlWriter<B>,
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
        write_insert_rows::<B, _, _>(rows, writer, &mut renderer)?;
    }
    write_insert_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

struct WriteInsertRows<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    expected_columns: usize,
    row_index: usize,
    _backend: PhantomData<B>,
}

impl<B, Writer> InsertRowVisitor<io::Error> for WriteInsertRows<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Backend = B;

    fn visit_row<Columns>(&mut self, row: &InsertRow<Columns>) -> io::Result<()>
    where
        Columns: RenderInsertAssignments<B>,
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
            _backend: PhantomData::<B>,
        };
        row.columns().try_visit(&mut assignments)?;
        self.writer.write_all(b")")
    }
}

struct WriteAssignmentValues<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    _backend: PhantomData<B>,
}

impl<B, Writer> AssignmentVisitor for WriteAssignmentValues<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_assignment<Value>(
        &mut self,
        _column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: RenderAssignment<B>,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        write_assignment_value::<B, _>(value, self.writer, self.renderer)
    }
}

fn write_insert_rows<B, Rows, Writer>(
    rows: &Rows,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    B: Backend,
    Rows: RenderInsertRows<B>,
    Writer: SqlWriter<B>,
{
    let mut visitor = WriteInsertRows {
        writer,
        renderer,
        expected_columns: rows.first_row_len(),
        row_index: 0,
        _backend: PhantomData::<B>,
    };
    rows.try_for_each_row(&mut visitor)
}

pub fn write_update<S, B, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: UpdateableTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_update_with_params::<S, B, _, _, _, _>(
        dialect,
        alias,
        columns,
        filters,
        returning,
        &mut writer,
    )
}

fn write_update_with_params<S, B, Columns, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: UpdateableTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
    Writer: SqlWriter<B>,
{
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"UPDATE ")?;
    write_schema_table_ref::<S>(renderer.dialect, writer)?;
    write!(writer, " AS {alias} SET ")?;
    let mut assignments = WriteUpdateAssignments {
        writer,
        renderer: &mut renderer,
        index: 0,
        _backend: PhantomData::<B>,
    };
    columns.try_visit(&mut assignments)?;
    write_filters::<B, _>(filters, writer, &mut renderer)?;
    write_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

struct WriteUpdateAssignments<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    _backend: PhantomData<B>,
}

impl<B, Writer> AssignmentVisitor for WriteUpdateAssignments<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_assignment<Value>(
        &mut self,
        column: &'static str,
        value: &Value,
    ) -> Result<(), Self::Error>
    where
        Value: RenderAssignment<B>,
    {
        if self.index > 0 {
            self.writer.write_all(b", ")?;
        }
        self.index += 1;
        self.renderer
            .dialect
            .write_quoted_ident(column, self.writer)?;
        self.writer.write_all(b" = ")?;
        write_assignment_value::<B, _>(value, self.writer, self.renderer)
    }
}

pub fn write_delete<S, B, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_delete_with_params::<S, B, _, _, _>(dialect, alias, filters, returning, &mut writer)
}

fn write_delete_with_params<S, B, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
    Writer: SqlWriter<B>,
{
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"DELETE FROM ")?;
    write_table_ref::<S>(renderer.dialect, writer)?;
    write!(writer, " AS {alias}")?;
    write_filters::<B, _>(filters, writer, &mut renderer)?;
    write_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

fn write_returning<B, Writer>(
    returning: &impl RenderProjectable<B>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_projection::<B, _>(returning, writer, renderer, false)
}

fn write_insert_returning<B, Writer>(
    returning: &impl RenderProjectable<B>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_projection::<B, _>(returning, writer, renderer, true)
}

fn write_projection<B, Writer>(
    projection: &impl RenderProjectable<B>,
    writer: &mut Writer,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    projection.visit_projection(&mut WriteProjection {
        writer,
        renderer,
        index: 0,
        insert_returning,
        _backend: PhantomData::<B>,
    })
}

struct WriteProjection<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    insert_returning: bool,
    _backend: PhantomData<B>,
}

impl<B, Writer> WriteProjection<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
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

impl<B, Writer> ProjectionVisitor for WriteProjection<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<B>,
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

fn write_filters<B, Writer>(
    filters: &impl RenderPredicateNodes<B>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    if filters.is_empty() {
        return Ok(());
    }

    writer.write_all(b" WHERE ")?;
    filters.try_visit(&mut WritePredicateFilters {
        writer,
        renderer,
        index: 0,
        _backend: PhantomData::<B>,
    })?;
    Ok(())
}

struct WritePredicateFilters<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    _backend: PhantomData<B>,
}

impl<B, Writer> PredicateVisitor for WritePredicateFilters<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_predicate<Kind, Ast>(&mut self, predicate: &Predicate<'_, Kind, Ast>) -> io::Result<()>
    where
        Kind: PredicateKind,
        Ast: RenderPredicateAst<B>,
    {
        if self.index > 0 {
            self.writer.write_all(b" AND ")?;
        }
        self.index += 1;
        write_predicate_value(predicate, self.writer, self.renderer)
    }
}

pub(crate) fn write_expr_value<K, Ast, B, Writer>(
    expr: &Expr<'_, K, Ast>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_expr_value_node(expr, writer, renderer, false)
}

pub(crate) fn write_column_value<K, B, Writer>(
    column: ColumnRef<'_, K>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_column_value_node(column, writer, renderer, false)
}

fn write_expr_value_node<K, Ast, B, Writer>(
    expr: &Expr<'_, K, Ast>,
    writer: &mut Writer,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, insert_returning, |visitor| {
        expr.visit(visitor)
    })
}

fn write_column_value_node<K, B, Writer>(
    column: ColumnRef<'_, K>,
    writer: &mut Writer,
    renderer: &mut Renderer,
    insert_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, insert_returning, |visitor| {
        column.visit(visitor)
    })
}

pub(crate) fn write_predicate_value<K, Ast, B, Writer>(
    predicate: &Predicate<'_, K, Ast>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: PredicateKind,
    Ast: RenderPredicateAst<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, false, |visitor| predicate.visit(visitor))
}

/// Render an embedded subquery as a nested `SELECT …`, reusing the caller's [`Renderer`] so the
/// subquery's placeholders continue the parent's numbering instead of restarting at zero.
fn write_subselect<Sub, B, Writer>(
    subquery: &Sub,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    Sub: crate::RenderSubquery<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    let mut sink = SelectRenderSink::<B, Writer>::new(writer, renderer)?;
    subquery.lower_subquery(&mut sink)?;
    sink.finish()
}

pub(crate) fn write_order_value<K, Ast, B, Writer>(
    order: &Order<'_, K, Ast>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
    write!(writer, " {}", render_order_direction(order.direction()))
}

fn write_assignment_value<B, Value>(
    value: &Value,
    writer: &mut impl SqlWriter<B>,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    B: Backend,
    Value: RenderAssignment<B>,
{
    value.visit_value(&mut RenderAssignmentValueVisitor {
        writer,
        renderer,
        _backend: PhantomData::<B>,
    })
}

struct RenderAssignmentValueVisitor<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    _backend: PhantomData<B>,
}

impl<B, Writer> AssignmentValueVisitor for RenderAssignmentValueVisitor<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

    fn visit_static<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: Encode<B>,
    {
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
        Ast: RenderAst<B>,
    {
        write_expr_value(expr, self.writer, self.renderer)
    }
}

fn write_ast<B, Writer>(
    writer: &mut Writer,
    renderer: &mut Renderer,
    insert_returning: bool,
    render: impl FnOnce(&mut RenderExpr<'_, '_, B, Writer>) -> io::Result<()>,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    let mut visitor = RenderExpr {
        writer,
        renderer,
        insert_returning,
        _backend: PhantomData::<B>,
    };
    render(&mut visitor)
}

struct RenderExpr<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    insert_returning: bool,
    _backend: PhantomData<B>,
}

impl<B, Writer> ExprVisitor for RenderExpr<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    type Error = io::Error;
    type Backend = B;

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

    fn visit_literal<T>(&mut self, value: &T) -> Result<(), Self::Error>
    where
        T: Encode<B>,
    {
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

    fn visit_aggregate<O>(
        &mut self,
        func: AggregateFunc,
        cast: Option<&SqlType>,
        operand: O,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // Some aggregates (`SUM`/`AVG`) have a database result type that differs from the Rust type
        // Squealy advertises (e.g. PostgreSQL `avg(int)` is `numeric`); a cast pins the wire type.
        match cast {
            Some(ty) => {
                write!(self.writer, "CAST({}(", render_aggregate_func(func))?;
                operand(self)?;
                self.writer.write_all(b") AS ")?;
                self.renderer
                    .dialect
                    .write_cast_type(ty, &mut *self.writer)?;
                self.writer.write_all(b")")
            }
            None => {
                write!(self.writer, "{}(", render_aggregate_func(func))?;
                operand(self)?;
                self.writer.write_all(b")")
            }
        }
    }

    fn visit_scalar_subquery<Sub>(&mut self, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: crate::RenderSubquery<B>,
    {
        self.writer.write_all(b"(")?;
        write_subselect::<Sub, B, _>(subquery, &mut *self.writer, &mut *self.renderer)?;
        self.writer.write_all(b")")
    }

    fn visit_window<Operand, Partitions, Orders>(
        &mut self,
        func: crate::WindowFunc,
        cast: Option<&SqlType>,
        operand: Operand,
        has_partitions: bool,
        partitions: Partitions,
        has_orders: bool,
        orders: Orders,
    ) -> Result<(), Self::Error>
    where
        Operand: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Partitions: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Orders: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        if cast.is_some() {
            self.writer.write_all(b"CAST(")?;
        }
        write!(self.writer, "{}(", render_window_func(func))?;
        operand(self)?;
        self.writer.write_all(b") OVER (")?;
        if has_partitions {
            self.writer.write_all(b"PARTITION BY ")?;
            partitions(self)?;
        }
        if has_orders {
            if has_partitions {
                self.writer.write_all(b" ")?;
            }
            self.writer.write_all(b"ORDER BY ")?;
            orders(self)?;
        }
        self.writer.write_all(b")")?;
        if let Some(ty) = cast {
            self.writer.write_all(b" AS ")?;
            self.renderer
                .dialect
                .write_cast_type(ty, &mut *self.writer)?;
            self.writer.write_all(b")")?;
        }
        Ok(())
    }

    fn visit_window_separator(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b", ")
    }

    fn visit_window_order_direction(
        &mut self,
        direction: OrderDirection,
    ) -> Result<(), Self::Error> {
        write!(self.writer, " {}", render_order_direction(direction))
    }
}

impl<B, Writer> PredicateAstVisitor for RenderExpr<'_, '_, B, Writer>
where
    B: Backend,
    Writer: SqlWriter<B>,
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

    fn visit_like<O, P>(
        &mut self,
        case_insensitive: bool,
        negated: bool,
        operand: O,
        pattern: P,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        P: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.renderer
            .dialect
            .write_like_operator(case_insensitive, negated, &mut *self.writer)?;
        pattern(self)?;
        self.writer.write_all(b")")
    }

    fn visit_in<O, T>(&mut self, negated: bool, operand: O, values: &[T]) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        T: Encode<B>,
    {
        // SQL has no `IN ()` form. For an empty list, render the operand once — so any runtime
        // parameters it carries are still emitted in order and stay aligned with later
        // placeholders — guarded by a constant that fixes the truth value: an empty `IN` is always
        // false, an empty `NOT IN` always true.
        if values.is_empty() {
            self.writer.write_all(b"(")?;
            operand(self)?;
            let tail: &[u8] = if negated {
                b" IS NOT NULL OR 1 = 1)"
            } else {
                b" IS NOT NULL AND 1 = 0)"
            };
            return self.writer.write_all(tail);
        }
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer
            .write_all(if negated { b" NOT IN (" } else { b" IN (" })?;
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                self.writer.write_all(b", ")?;
            }
            self.writer.push_bind(value);
            self.renderer.write_placeholder(self.writer)?;
        }
        self.writer.write_all(b"))")
    }

    fn visit_between<O, Lo, Hi>(
        &mut self,
        negated: bool,
        operand: O,
        lo: Lo,
        hi: Hi,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Lo: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Hi: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer.write_all(if negated {
            b" NOT BETWEEN "
        } else {
            b" BETWEEN "
        })?;
        lo(self)?;
        self.writer.write_all(b" AND ")?;
        hi(self)?;
        self.writer.write_all(b")")
    }

    fn visit_bool_test<O>(&mut self, negated: bool, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        if negated {
            self.writer.write_all(b"(NOT ")?;
            operand(self)?;
            self.writer.write_all(b")")
        } else {
            self.writer.write_all(b"(")?;
            operand(self)?;
            self.writer.write_all(b")")
        }
    }

    fn visit_in_subquery<O, Sub>(
        &mut self,
        negated: bool,
        operand: O,
        subquery: &Sub,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Sub: crate::RenderSubquery<B>,
    {
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer
            .write_all(if negated { b" NOT IN (" } else { b" IN (" })?;
        write_subselect::<Sub, B, _>(subquery, &mut *self.writer, &mut *self.renderer)?;
        self.writer.write_all(b"))")
    }

    fn visit_exists<Sub>(&mut self, negated: bool, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: crate::RenderSubquery<B>,
    {
        self.writer.write_all(if negated {
            b"(NOT EXISTS ("
        } else {
            b"(EXISTS ("
        })?;
        write_subselect::<Sub, B, _>(subquery, &mut *self.writer, &mut *self.renderer)?;
        self.writer.write_all(b"))")
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

fn render_aggregate_func(func: AggregateFunc) -> &'static str {
    match func {
        AggregateFunc::Count => "COUNT",
        AggregateFunc::Sum => "SUM",
        AggregateFunc::Avg => "AVG",
        AggregateFunc::Min => "MIN",
        AggregateFunc::Max => "MAX",
    }
}

fn render_window_func(func: crate::WindowFunc) -> &'static str {
    match func {
        crate::WindowFunc::Aggregate(aggregate) => render_aggregate_func(aggregate),
        crate::WindowFunc::RowNumber => "ROW_NUMBER",
        crate::WindowFunc::Rank => "RANK",
        crate::WindowFunc::DenseRank => "DENSE_RANK",
        crate::WindowFunc::Ntile => "NTILE",
        crate::WindowFunc::Lag => "LAG",
        crate::WindowFunc::Lead => "LEAD",
    }
}

pub(crate) fn render_compare_op(op: CompareOp) -> &'static str {
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

pub fn render_insert_prepared<S, B, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    buffer: &mut PreparedSql<B>,
) where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    buffer.clear();
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, buffer).unwrap();
}

pub fn write_insert_params<S, B, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    params: &mut Vec<B::Param>,
) -> Result<(), B::Error>
where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, &mut writer).unwrap();
    writer.finish()
}

pub fn render_delete_prepared<S, B, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    buffer: &mut PreparedSql<B>,
) where
    S: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    buffer.clear();
    write_delete_with_params::<S, B, _, _, _>(dialect, alias, filters, returning, buffer).unwrap();
}

pub fn write_delete_params<S, B, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<B::Param>,
) -> Result<(), B::Error>
where
    S: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    write_delete_with_params::<S, B, _, _, _>(dialect, alias, filters, returning, &mut writer)
        .unwrap();
    writer.finish()
}

pub fn render_update_prepared<S, B, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    buffer: &mut PreparedSql<B>,
) where
    S: UpdateableTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    buffer.clear();
    write_update_with_params::<S, B, _, _, _, _>(
        dialect, alias, columns, filters, returning, buffer,
    )
    .unwrap();
}

pub fn write_update_params<S, B, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
    alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<B::Param>,
) -> Result<(), B::Error>
where
    S: UpdateableTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    write_update_with_params::<S, B, _, _, _, _>(
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
