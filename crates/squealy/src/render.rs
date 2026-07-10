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
    CompareOp, ConflictAction, ConflictClause, DateField, DeleteUsingStyle, Dialect, Encode, Expr,
    ExprKind, ExprVisitor, InsertRow, InsertRowVisitor, InsertableTable, Order, OrderDirection,
    Predicate, PredicateAstVisitor, PredicateKind, PredicateVisitor, ProjectionShape,
    ProjectionVisitor, QueryBuilder, RenderAssignment, RenderAst, RenderCaseArms,
    RenderCoalesceArgs, RenderInsertAssignments, RenderInsertRows, RenderPredicateAst,
    RenderPredicateNodes, RenderProjectable, RenderSelectAst, RenderSimpleCaseArms,
    RenderUpdateAssignments, SchemaTable, SelectSink, Selected, SetOperandStyle, SourceAlias,
    SqlType, TableProjection, UnaryStringFunc, UpdateFromStyle, UpdateableTable,
};
use std::marker::PhantomData;

/// Write a single-quoted SQL string literal with embedded single quotes doubled. Used for the
/// `AT TIME ZONE '<tz>'` operator argument (a developer-supplied zone name); doubling is correctness,
/// not injection defense.
fn write_sql_string_literal(writer: &mut dyn Write, value: &str) -> io::Result<()> {
    writer.write_all(b"'")?;
    writer.write_all(value.replace('\'', "''").as_bytes())?;
    writer.write_all(b"'")
}

/// Threads the active [`Dialect`](crate::Dialect) and the running parameter counters through the
/// renderer. The dialect is `&'static` (backend dialects are zero-sized unit values), so carrying it
/// adds no lifetime to the renderer or the rendering structs.
#[derive(Clone, Copy)]
#[doc(hidden)]
pub struct Renderer {
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
#[doc(hidden)]
pub trait SqlWriter<B: Backend>: Write {
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
    distinct: bool,
    columns: usize,
    sources: usize,
    filters: usize,
    groups: usize,
    havings: usize,
    windows: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
    row_lock: Option<crate::RowLock>,
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
            distinct: false,
            columns: 0,
            sources: 0,
            filters: 0,
            groups: 0,
            havings: 0,
            windows: 0,
            orders: 0,
            limit: None,
            offset: None,
            row_lock: None,
            _backend: PhantomData,
        })
    }

    fn finish(self) -> io::Result<()> {
        self.renderer
            .dialect
            .write_limit_offset(self.limit, self.offset, self.writer)?;
        if let Some(lock) = self.row_lock {
            self.renderer.dialect.write_row_lock(lock, self.writer)?;
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

    fn set_distinct(&mut self) -> io::Result<()> {
        self.distinct = true;
        Ok(())
    }

    fn push_projection<Shape, P>(&mut self, projection: P) -> io::Result<()>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<B>,
    {
        _ = std::marker::PhantomData::<Shape>;
        if self.distinct {
            self.writer.write_all(b"DISTINCT ")?;
        }
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

    fn push_right_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<B>,
    {
        self.push_join::<S, P, Ast>(alias, on, "RIGHT JOIN")
    }

    fn push_full_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<B>,
    {
        self.push_join::<S, P, Ast>(alias, on, "FULL JOIN")
    }

    fn push_cross_join<S>(&mut self, alias: SourceAlias) -> io::Result<()>
    where
        S: TableProjection,
    {
        // `CROSS JOIN <table> AS <alias>` — a Cartesian product, no `ON` clause. (A cross join is
        // never the first source in practice, but the `FROM` branch keeps the helper total.)
        let first_source = self.sources == 0;
        self.push_source_separator()?;
        if first_source {
            self.writer.write_all(b"FROM ")?;
        } else {
            self.writer.write_all(b"CROSS JOIN ")?;
        }
        write_table_ref::<S>(self.renderer.dialect, self.writer)?;
        write!(self.writer, " AS {alias}")
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

    fn push_named_window<Parts, Ords>(
        &mut self,
        index: usize,
        partitions: &Parts,
        orders: &Ords,
        frame: Option<crate::FrameSpec>,
    ) -> io::Result<()>
    where
        Parts: crate::RenderWindowList<B>,
        Ords: crate::RenderWindowList<B>,
    {
        if self.windows == 0 {
            self.writer.write_all(b" WINDOW ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.windows += 1;
        write!(self.writer, "{} AS (", crate::named_window_alias(index))?;
        write_named_window_body::<Parts, Ords, B, _>(
            partitions,
            orders,
            frame,
            self.writer,
            &mut *self.renderer,
        )?;
        self.writer.write_all(b")")
    }

    fn set_limit(&mut self, rows: usize) -> io::Result<()> {
        self.limit = Some(rows);
        Ok(())
    }

    fn set_offset(&mut self, rows: usize) -> io::Result<()> {
        self.offset = Some(rows);
        Ok(())
    }

    fn set_row_lock(&mut self, lock: crate::RowLock) -> io::Result<()> {
        self.row_lock = Some(lock);
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
    // An unrenderable CTE shape (a scoped recursive arm on SQLite) is captured into the buffer's error
    // channel and surfaced from `into_parts()`, mirroring how an encode failure is reported.
    if let Err(error) = write_cte_prefix(
        dialect,
        &selected.collect_ctes::<Conn, Conn::Backend>(),
        buffer,
    ) {
        buffer.error = Some(<Conn::Backend as Backend>::render_error(error));
        return;
    }
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
    write_cte_prefix(
        dialect,
        &selected.collect_ctes::<Conn, Conn::Backend>(),
        &mut writer,
    )?;
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
    // CTE bodies are parameter-free, so the `WITH` prefix contributes no bind params; this keeps the
    // path uniform with the SQL-text renderers (the collector ignores the emitted bytes). It can still
    // reject an unrenderable CTE shape (a scoped recursive arm on SQLite), surfaced as a render error.
    write_cte_prefix(
        dialect,
        &selected.collect_ctes::<Conn, Conn::Backend>(),
        &mut writer,
    )
    .map_err(<Conn::Backend as Backend>::render_error)?;
    let mut select_sink =
        SelectRenderSink::<Conn::Backend, _>::new(&mut writer, &mut renderer).unwrap();
    selected.lower_into::<Conn, _>(&mut select_sink).unwrap();
    select_sink.finish().unwrap();
    writer.finish()
}

// ---------------------------------------------------------------------------
// Set operations (UNION / INTERSECT / EXCEPT)
// ---------------------------------------------------------------------------

/// Renders one operand of a set operation — a leaf select (parenthesized `(SELECT …)`) or a nested set
/// — into a sink that **shares** the caller's [`Renderer`], so prepared placeholders stay continuous
/// across every arm of the tree. Implemented for [`SetLeaf`](crate::SetLeaf) /
/// [`SetNode`](crate::SetNode) here (rather than in `query.rs`) because leaf rendering needs the
/// private [`SelectRenderSink`].
#[doc(hidden)]
pub trait RenderSetArm<'conn, 'scope, Conn, B>: crate::SetArm<'conn, 'scope, Conn>
where
    Conn: QueryBuilder + 'conn,
    B: Backend,
{
    /// Render this arm as a parenthesized set operand.
    fn render_operand<Writer>(
        &self,
        writer: &mut Writer,
        renderer: &mut Renderer,
    ) -> io::Result<()>
    where
        Writer: SqlWriter<B>;

    /// Render this arm as the outermost set (no enclosing parentheses, so a trailing `ORDER BY`/`LIMIT`
    /// binds to the whole set). Defaults to [`render_operand`](Self::render_operand); set nodes drop the
    /// outer parens.
    fn render_root<Writer>(&self, writer: &mut Writer, renderer: &mut Renderer) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        self.render_operand(writer, renderer)
    }

    /// Render this arm as the source of an `INSERT … <select>`. A single leaf renders bare (no parens —
    /// `INSERT INTO t (cols) SELECT …`); a set node renders its `UNION`/etc. unparenthesized (defaults
    /// to [`render_root`](Self::render_root)).
    fn render_insert_source<Writer>(
        &self,
        writer: &mut Writer,
        renderer: &mut Renderer,
    ) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        self.render_root(writer, renderer)
    }

    /// Collect the CTEs referenced by this arm (and nested arms), for hoisting into one leading `WITH`.
    fn collect_set_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>);
}

impl<'conn, 'scope, Conn, Base, Shape, Projection, B> RenderSetArm<'conn, 'scope, Conn, B>
    for crate::SetLeaf<'conn, 'scope, Conn, Base, Shape, Projection>
where
    Conn: QueryBuilder + 'conn,
    B: Backend,
    Base: RenderSelectAst<'conn, 'scope, Conn, B>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<B>,
{
    fn render_operand<Writer>(&self, writer: &mut Writer, renderer: &mut Renderer) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        // A set operand is wrapped so its `ORDER BY`/`LIMIT` (and, for a nested compound, its grouping)
        // binds to the operand and not the enclosing set. Postgres/MySQL use `(SELECT …)`; SQLite
        // rejects a parenthesized compound operand, so it uses `SELECT * FROM (SELECT …)` instead.
        let (open, close): (&[u8], &[u8]) = match renderer.dialect.set_operand_style() {
            SetOperandStyle::Parenthesized => (b"(", b")"),
            SetOperandStyle::SubquerySelect => (b"SELECT * FROM (", b")"),
        };
        writer.write_all(open)?;
        {
            let mut sink = SelectRenderSink::<B, Writer>::new(writer, renderer)?;
            self.selected.lower_into::<Conn, _>(&mut sink)?;
            sink.finish()?;
        }
        writer.write_all(close)
    }

    fn render_insert_source<Writer>(
        &self,
        writer: &mut Writer,
        renderer: &mut Renderer,
    ) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        // A bare `SELECT …` (no enclosing parens) for `INSERT INTO t (cols) SELECT …`.
        let mut sink = SelectRenderSink::<B, Writer>::new(writer, renderer)?;
        self.selected.lower_into::<Conn, _>(&mut sink)?;
        sink.finish()
    }

    fn collect_set_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        ctes.extend(self.selected.collect_ctes::<Conn, B>());
    }
}

impl<'conn, 'scope, Conn, L, R, B> RenderSetArm<'conn, 'scope, Conn, B> for crate::SetNode<L, R>
where
    Conn: QueryBuilder + 'conn,
    B: Backend,
    L: RenderSetArm<'conn, 'scope, Conn, B>,
    R: RenderSetArm<'conn, 'scope, Conn, B, Row = <L as crate::SetArm<'conn, 'scope, Conn>>::Row>,
    <L as crate::SetArm<'conn, 'scope, Conn>>::Params:
        crate::HAppend<<R as crate::SetArm<'conn, 'scope, Conn>>::Params>,
{
    fn render_operand<Writer>(&self, writer: &mut Writer, renderer: &mut Renderer) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        // See `SetLeaf::render_operand`. A nested compound is wrapped so its operators group correctly
        // under the enclosing set: `(a UNION b)` for Postgres/MySQL, `SELECT * FROM (a UNION b)` for
        // SQLite (which rejects a parenthesized compound operand).
        let (open, close): (&[u8], &[u8]) = match renderer.dialect.set_operand_style() {
            SetOperandStyle::Parenthesized => (b"(", b")"),
            SetOperandStyle::SubquerySelect => (b"SELECT * FROM (", b")"),
        };
        writer.write_all(open)?;
        self.render_root(writer, renderer)?;
        writer.write_all(close)
    }

    fn render_root<Writer>(&self, writer: &mut Writer, renderer: &mut Renderer) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        self.left.render_operand(writer, renderer)?;
        write!(writer, " {} ", self.op.keyword())?;
        self.right.render_operand(writer, renderer)
    }

    fn collect_set_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.left.collect_set_ctes(ctes);
        self.right.collect_set_ctes(ctes);
    }
}

impl<'conn, 'scope, Conn, Tree, B> RenderSetArm<'conn, 'scope, Conn, B> for crate::SetGroup<Tree>
where
    Conn: QueryBuilder + 'conn,
    B: Backend,
    Tree: RenderSetArm<'conn, 'scope, Conn, B>,
{
    fn render_operand<Writer>(&self, writer: &mut Writer, renderer: &mut Renderer) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        // A nested set renders its trailing modifiers inside its own wrapper, so they bind to this
        // operand and not the enclosing set. Postgres/MySQL wrap in `(…)`; SQLite rejects a
        // parenthesized compound operand, so it uses `SELECT * FROM (…)` (see `SetLeaf::render_operand`).
        let (open, close): (&[u8], &[u8]) = match renderer.dialect.set_operand_style() {
            SetOperandStyle::Parenthesized => (b"(", b")"),
            SetOperandStyle::SubquerySelect => (b"SELECT * FROM (", b")"),
        };
        writer.write_all(open)?;
        self.tree.render_root(writer, renderer)?;
        write_set_tail(renderer.dialect, &self.tail, writer)?;
        writer.write_all(close)
    }

    fn render_insert_source<Writer>(
        &self,
        writer: &mut Writer,
        renderer: &mut Renderer,
    ) -> io::Result<()>
    where
        Writer: SqlWriter<B>,
    {
        // The whole set is the insert source, so it renders unparenthesized with its trailing
        // `ORDER BY`/`LIMIT` binding to the set: `INSERT INTO t (cols) SELECT … UNION SELECT … ORDER BY …`.
        self.tree.render_root(writer, renderer)?;
        write_set_tail(renderer.dialect, &self.tail, writer)
    }

    fn collect_set_ctes(&self, ctes: &mut Vec<&'static dyn crate::CteDef>) {
        self.tree.collect_set_ctes(ctes);
    }
}

/// De-duplicate a set's collected CTEs by definition identity (each arm's list is already topo-ordered;
/// a CTE used in several arms is kept once, at first occurrence). Two *distinct* CTEs that derive the
/// same bare name — each valid in its own arm, but colliding once merged — are rejected, mirroring the
/// single-select check in [`Selected::collect_ctes`](crate::Selected::collect_ctes).
fn dedup_set_ctes(ctes: Vec<&'static dyn crate::CteDef>) -> Vec<&'static dyn crate::CteDef> {
    let mut kept: Vec<&'static dyn crate::CteDef> = Vec::new();
    for def in ctes {
        if kept
            .iter()
            .any(|existing| existing.type_key() == def.type_key())
        {
            continue;
        }
        assert!(
            !kept.iter().any(|existing| existing.name() == def.name()),
            "two distinct CTEs are both named {:?}; a set operation cannot combine arms whose CTEs \
             have colliding names (the CTE derive names by struct name, ignoring module/schema)",
            def.name(),
        );
        kept.push(def);
    }
    kept
}

/// Writes a set's trailing `ORDER BY <output col> [ASC|DESC], … [LIMIT n] [OFFSET n]` (referencing the
/// set's output column names, not source aliases).
fn write_set_tail(
    dialect: &dyn Dialect,
    tail: &crate::SetTail,
    writer: &mut dyn Write,
) -> io::Result<()> {
    if !tail.orders.is_empty() {
        writer.write_all(b" ORDER BY ")?;
        for (index, order) in tail.orders.iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            dialect.write_quoted_ident(&order.column, writer)?;
            writer.write_all(match order.direction {
                OrderDirection::Asc => b" ASC" as &[u8],
                OrderDirection::Desc => b" DESC",
            })?;
        }
    }
    dialect.write_limit_offset(tail.limit, tail.offset, writer)
}

pub fn render_set_prepared<'conn, 'scope, Conn, Tree>(
    dialect: &'static dyn Dialect,
    tree: &Tree,
    tail: &crate::SetTail,
    buffer: &mut PreparedSql<Conn::Backend>,
) where
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
{
    buffer.clear();
    let mut renderer = Renderer::new(dialect);
    let mut ctes = Vec::new();
    tree.collect_set_ctes(&mut ctes);
    // See `render_selected_prepared`: an unrenderable CTE shape is captured into the buffer's error
    // channel rather than panicking, and surfaced from `into_parts()`.
    if let Err(error) = write_cte_prefix(dialect, &dedup_set_ctes(ctes), buffer) {
        buffer.error = Some(<Conn::Backend as Backend>::render_error(error));
        return;
    }
    tree.render_root(buffer, &mut renderer).unwrap();
    write_set_tail(dialect, tail, buffer).unwrap();
}

pub fn write_set_into<'conn, 'scope, Conn, Tree, Writer>(
    dialect: &'static dyn Dialect,
    tree: &Tree,
    tail: &crate::SetTail,
    writer: &mut Writer,
) -> io::Result<()>
where
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
    Writer: Write,
{
    let mut writer = SqlOnly(writer);
    let mut renderer = Renderer::new(dialect);
    let mut ctes = Vec::new();
    tree.collect_set_ctes(&mut ctes);
    write_cte_prefix(dialect, &dedup_set_ctes(ctes), &mut writer)?;
    tree.render_root(&mut writer, &mut renderer)?;
    write_set_tail(dialect, tail, &mut writer)
}

pub fn write_set_params<'conn, 'scope, Conn, Tree>(
    dialect: &'static dyn Dialect,
    tree: &Tree,
    tail: &crate::SetTail,
    params: &mut Vec<<Conn::Backend as Backend>::Param>,
) -> Result<(), <Conn::Backend as Backend>::Error>
where
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
{
    let mut writer = ParamCollector::<Conn::Backend>::new(params);
    let mut renderer = Renderer::new(dialect);
    let mut ctes = Vec::new();
    tree.collect_set_ctes(&mut ctes);
    write_cte_prefix(dialect, &dedup_set_ctes(ctes), &mut writer)
        .map_err(<Conn::Backend as Backend>::render_error)?;
    tree.render_root(&mut writer, &mut renderer).unwrap();
    write_set_tail(dialect, tail, &mut writer).unwrap();
    writer.finish()
}

/// Writes a query's `WITH` prefix — `WITH "n1" AS (<body>), "n2" AS (<body>) ` (with a trailing space
/// before the main `SELECT`) — when the select references any CTEs. The defs are already de-duplicated
/// and ordered by [`Selected::collect_ctes`]; each body is parameter-free (literals only), so it
/// neither perturbs the main query's placeholder numbering nor contributes bind params.
fn write_cte_prefix(
    dialect: &dyn Dialect,
    ctes: &[&'static dyn crate::CteDef],
    writer: &mut dyn Write,
) -> io::Result<()> {
    // The runtime `WITH` prefix and a view body's `WITH` prelude are one and the same: build a
    // `CteModel` per collected `CteDef` and hand off to the shared renderer, which detects a recursive
    // CTE by self-reference and renders its arms bare.
    let models = cte_models(ctes);
    let recursive = ctes.iter().any(|def| def.is_recursive());
    crate::view_render::render_with_prefix(recursive, &models, dialect, writer)
}

/// Builds a [`CteModel`](crate::CteModel) per collected [`CteDef`](crate::CteDef) — its declared name and
/// column names plus its lowered [`ViewBody`](crate::ViewBody) — for the shared `WITH` renderer. Shared by
/// the plain and set-query CTE prefix paths.
fn cte_models(ctes: &[&'static dyn crate::CteDef]) -> Vec<crate::CteModel> {
    ctes.iter()
        .map(|def| crate::CteModel {
            name: def.name().to_string(),
            columns: def
                .columns()
                .into_iter()
                .map(|column| column.name)
                .collect(),
            body: def.body(),
        })
        .collect()
}

fn write_table_ref<S>(dialect: &dyn Dialect, writer: &mut impl Write) -> io::Result<()>
where
    S: TableProjection,
{
    if dialect.qualify_schema()
        && let Some(schema) = <S as TableProjection>::schema_name()
    {
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
    if dialect.qualify_schema()
        && let Some(schema) = <S as SchemaTable>::schema_name()
    {
        dialect.write_quoted_ident(schema, writer)?;
        writer.write_all(b".")?;
    }
    dialect.write_quoted_ident(<S as SchemaTable>::name(), writer)
}

pub fn write_insert<S, B, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    conflict: Option<&ConflictClause>,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, conflict, &mut writer)
}

fn write_insert_with_params<S, B, Rows, Returning, Writer>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    conflict: Option<&ConflictClause>,
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
    if let Some(clause) = conflict {
        write_conflict_clause::<B, _, _>(clause, rows, dialect, writer)?;
    }
    write_insert_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

/// Renders `INSERT INTO t (cols) <select>` — the inserted rows come from a query (`source`, a set-op
/// arm; a single leaf renders bare). Any CTEs the source references are hoisted into one leading `WITH`.
fn write_insert_select_with_params<'conn, 'scope, S, Conn, Tree, Returning, Writer>(
    dialect: &'static dyn Dialect,
    columns: &[&str],
    source: &Tree,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: InsertableTable,
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
    Returning: RenderProjectable<Conn::Backend>,
    Writer: SqlWriter<Conn::Backend>,
{
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"INSERT INTO ")?;
    write_schema_table_ref::<S>(dialect, writer)?;
    writer.write_all(b" (")?;
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        dialect.write_quoted_ident(column, writer)?;
    }
    writer.write_all(b") ")?;
    // The source query's CTEs belong to the SELECT, so the `WITH` goes *after* the column list
    // (`INSERT INTO t (cols) WITH … SELECT …`) — valid on both PostgreSQL and MySQL, unlike a leading
    // `WITH … INSERT` which MySQL rejects.
    let mut ctes = Vec::new();
    source.collect_set_ctes(&mut ctes);
    write_cte_prefix(dialect, &dedup_set_ctes(ctes), writer)?;
    source.render_insert_source(writer, &mut renderer)?;
    write_insert_returning::<Conn::Backend, _>(returning, writer, &mut renderer)
}

/// Renders `INSERT INTO t (cols) <select>` into SQL text (discarding binds).
pub fn write_insert_select<'conn, 'scope, S, Conn, Tree, Returning>(
    dialect: &'static dyn Dialect,
    columns: &[&str],
    source: &Tree,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: InsertableTable,
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
    Returning: RenderProjectable<Conn::Backend>,
{
    let mut writer = SqlOnly(writer);
    write_insert_select_with_params::<S, Conn, _, _, _>(
        dialect,
        columns,
        source,
        returning,
        &mut writer,
    )
}

/// Collects the bind parameters of an `INSERT INTO t (cols) <select>` (from the source query), in
/// render order.
pub fn write_insert_select_params<'conn, 'scope, S, Conn, Tree, Returning>(
    dialect: &'static dyn Dialect,
    columns: &[&str],
    source: &Tree,
    returning: &Returning,
    params: &mut Vec<<Conn::Backend as Backend>::Param>,
) -> Result<(), <Conn::Backend as Backend>::Error>
where
    S: InsertableTable,
    Conn: QueryBuilder + 'conn,
    Tree: RenderSetArm<'conn, 'scope, Conn, Conn::Backend>,
    Returning: RenderProjectable<Conn::Backend>,
{
    let mut writer = ParamCollector::<Conn::Backend>::new(params);
    // The source may be an unrenderable CTE shape (a scoped recursive arm on SQLite); surface that
    // render reject rather than swallowing it, so `collect_params()` mirrors the select/set params path.
    write_insert_select_with_params::<S, Conn, _, _, _>(
        dialect,
        columns,
        source,
        returning,
        &mut writer,
    )
    .map_err(<Conn::Backend as Backend>::render_error)?;
    writer.finish()
}

/// Renders an upsert's conflict clause. The dialect-divergent structure (PostgreSQL `ON CONFLICT
/// (<target>) DO NOTHING | DO UPDATE SET …` vs MySQL `ON DUPLICATE KEY UPDATE …`) goes through the
/// [`Dialect`] seams; the replace-all `DO UPDATE` SET list (every inserted column to its excluded value,
/// no bind parameters) is shared here.
fn write_conflict_clause<B, Rows, Writer>(
    clause: &ConflictClause,
    rows: &Rows,
    dialect: &'static dyn Dialect,
    writer: &mut Writer,
) -> io::Result<()>
where
    B: Backend,
    Rows: RenderInsertRows<B>,
    Writer: SqlWriter<B>,
{
    // A `DEFAULT VALUES` insert assigns no columns, so there is nothing to replace — `DO UPDATE SET`
    // with an empty list is invalid SQL. Treat a column-less `do_update` as `DO NOTHING`.
    let action = match clause.action {
        ConflictAction::DoUpdateExcluded if rows.first_row_len() == 0 => ConflictAction::DoNothing,
        other => other,
    };
    match action {
        ConflictAction::DoNothing => {
            // The first inserted column, for dialects (MySQL) that emulate `DO NOTHING` by
            // self-assigning a column.
            let mut first_column = None;
            rows.try_for_each_column(|column| {
                first_column.get_or_insert(column);
                Ok::<(), io::Error>(())
            })?;
            dialect.write_upsert_do_nothing(&clause.target, first_column, writer)?;
        }
        ConflictAction::DoUpdateExcluded => {
            dialect.write_upsert_set_prefix(&clause.target, writer)?;
            let mut index = 0;
            rows.try_for_each_column(|column| {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                index += 1;
                dialect.write_quoted_ident(column, writer)?;
                writer.write_all(b" = ")?;
                dialect.write_excluded_column(column, writer)?;
                Ok::<(), io::Error>(())
            })?;
        }
    }
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
        // Single-table `UPDATE`: the bare column is unambiguous.
        qualify: None,
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
    /// When set, the assignment target is qualified with this alias (`alias.col = …`). MySQL's
    /// multi-table `UPDATE … JOIN … SET` resolves a bare column against the whole join list, so the
    /// target must be qualified; PostgreSQL's `UPDATE t SET col … FROM …` requires it unqualified.
    qualify: Option<SourceAlias>,
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
        if let Some(alias) = self.qualify {
            write!(self.writer, "{alias}.")?;
        }
        self.renderer
            .dialect
            .write_quoted_ident(column, self.writer)?;
        self.writer.write_all(b" = ")?;
        write_assignment_value::<B, _>(value, self.writer, self.renderer)
    }
}

/// Renders a correlated `UPDATE … <source>` into SQL text (discarding binds).
pub fn write_update_from<S, O, B, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: UpdateableTable,
    O: SchemaTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_update_from_with_params::<S, O, B, _, _, _, _>(
        dialect,
        target_alias,
        source_alias,
        columns,
        filters,
        returning,
        &mut writer,
    )
}

/// Renders `UPDATE t AS a SET … <correlated source>` — the assignments and the correlation/filter
/// predicates may reference both the target (`target_alias`) and the joined source (`source_alias`).
/// The shape is dialect-specific ([`Dialect::update_from_style`]): PostgreSQL appends
/// `FROM other AS b WHERE <predicates>`; MySQL joins `JOIN other AS b ON <predicates>` before `SET`.
/// (For an inner-join update the join's `ON` and a trailing `WHERE` are equivalent, so the correlation
/// and any extra filters render as one predicate list.)
fn write_update_from_with_params<S, O, B, Columns, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: UpdateableTable,
    O: SchemaTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
    Writer: SqlWriter<B>,
{
    let mut renderer = Renderer::new(dialect);
    writer.write_all(b"UPDATE ")?;
    write_schema_table_ref::<S>(renderer.dialect, writer)?;
    write!(writer, " AS {target_alias}")?;
    match dialect.update_from_style() {
        UpdateFromStyle::PgFrom => {
            writer.write_all(b" SET ")?;
            // PostgreSQL forbids qualifying the `SET` target (it is implicitly the updated table).
            write_update_assignment_list::<B, _, _>(columns, writer, &mut renderer, None)?;
            writer.write_all(b" FROM ")?;
            write_schema_table_ref::<O>(renderer.dialect, writer)?;
            write!(writer, " AS {source_alias}")?;
            write_filters::<B, _>(filters, writer, &mut renderer)?;
        }
        UpdateFromStyle::MysqlJoin => {
            writer.write_all(b" JOIN ")?;
            write_schema_table_ref::<O>(renderer.dialect, writer)?;
            write!(writer, " AS {source_alias} ON ")?;
            let mut predicates = WritePredicateFilters {
                writer,
                renderer: &mut renderer,
                index: 0,
                _backend: PhantomData::<B>,
            };
            filters.try_visit(&mut predicates)?;
            writer.write_all(b" SET ")?;
            // MySQL's multi-table `UPDATE … JOIN … SET` resolves a bare column against the whole join
            // list, so qualify the target with its alias to avoid ambiguity with a same-named source
            // column.
            write_update_assignment_list::<B, _, _>(
                columns,
                writer,
                &mut renderer,
                Some(target_alias),
            )?;
        }
    }
    write_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

/// Collects the binds of a correlated `UPDATE … <source>` (left-to-right in render order).
pub fn write_update_from_params<S, O, B, Columns, Filters, Returning>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<<B as Backend>::Param>,
) -> Result<(), <B as Backend>::Error>
where
    S: UpdateableTable,
    O: SchemaTable,
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    // Propagate a render reject rather than swallowing it (uniform with the other params collectors),
    // though a correlated-update source is a `SchemaTable` and so carries no unrenderable CTE shape.
    write_update_from_with_params::<S, O, B, _, _, _, _>(
        dialect,
        target_alias,
        source_alias,
        columns,
        filters,
        returning,
        &mut writer,
    )
    .map_err(<B as Backend>::render_error)?;
    writer.finish()
}

/// Renders a comma-separated `col = <value>` assignment list (shared by `UPDATE` and `UPDATE … FROM`).
/// `qualify` qualifies each target column with an alias (MySQL `UPDATE … JOIN`); `None` leaves it bare.
fn write_update_assignment_list<B, Columns, Writer>(
    columns: &Columns,
    writer: &mut Writer,
    renderer: &mut Renderer,
    qualify: Option<SourceAlias>,
) -> io::Result<()>
where
    B: Backend,
    Columns: RenderUpdateAssignments<B>,
    Writer: SqlWriter<B>,
{
    columns.try_visit(&mut WriteUpdateAssignments {
        writer,
        renderer,
        index: 0,
        qualify,
        _backend: PhantomData::<B>,
    })
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

/// Renders a correlated `DELETE … <source>` into SQL text (discarding binds).
pub fn write_delete_using<S, O, B, Filters, Returning>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut impl Write,
) -> io::Result<()>
where
    S: TableProjection,
    O: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = SqlOnly(writer);
    write_delete_using_with_params::<S, O, B, _, _, _>(
        dialect,
        target_alias,
        source_alias,
        filters,
        returning,
        &mut writer,
    )
}

/// Renders `DELETE FROM t AS a <correlated source>` — the correlation/filter predicates may reference
/// both the target (`target_alias`) and the joined source (`source_alias`). Dialect-specific
/// ([`Dialect::update_from_style`]): PostgreSQL `DELETE FROM t AS a USING other AS b WHERE <predicates>`;
/// MySQL `DELETE a FROM t AS a JOIN other AS b ON <predicates>` (the leading alias picks the table whose
/// rows are deleted).
fn write_delete_using_with_params<S, O, B, Filters, Returning, Writer>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: TableProjection,
    O: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
    Writer: SqlWriter<B>,
{
    let mut renderer = Renderer::new(dialect);
    match dialect.delete_using_style() {
        DeleteUsingStyle::PgUsing => {
            writer.write_all(b"DELETE FROM ")?;
            write_table_ref::<S>(renderer.dialect, writer)?;
            write!(writer, " AS {target_alias} USING ")?;
            write_table_ref::<O>(renderer.dialect, writer)?;
            write!(writer, " AS {source_alias}")?;
            write_filters::<B, _>(filters, writer, &mut renderer)?;
        }
        DeleteUsingStyle::MysqlJoin => {
            write!(writer, "DELETE {target_alias} FROM ")?;
            write_table_ref::<S>(renderer.dialect, writer)?;
            write!(writer, " AS {target_alias} JOIN ")?;
            write_table_ref::<O>(renderer.dialect, writer)?;
            write!(writer, " AS {source_alias} ON ")?;
            let mut predicates = WritePredicateFilters {
                writer,
                renderer: &mut renderer,
                index: 0,
                _backend: PhantomData::<B>,
            };
            filters.try_visit(&mut predicates)?;
        }
        DeleteUsingStyle::SqliteExists => {
            // SQLite has no join-delete, so the correlated delete becomes a correlated `EXISTS`
            // subquery: `DELETE FROM t AS a WHERE EXISTS (SELECT 1 FROM other AS b WHERE <correlation>)`
            // (SQLite's `DELETE` target allows an alias, and the subquery may reference it).
            writer.write_all(b"DELETE FROM ")?;
            write_table_ref::<S>(renderer.dialect, writer)?;
            write!(writer, " AS {target_alias} WHERE EXISTS (SELECT 1 FROM ")?;
            write_table_ref::<O>(renderer.dialect, writer)?;
            write!(writer, " AS {source_alias} WHERE ")?;
            let mut predicates = WritePredicateFilters {
                writer,
                renderer: &mut renderer,
                index: 0,
                _backend: PhantomData::<B>,
            };
            filters.try_visit(&mut predicates)?;
            writer.write_all(b")")?;
        }
    }
    write_returning::<B, _>(returning, writer, &mut renderer)?;
    Ok(())
}

/// Collects the binds of a correlated `DELETE … <source>` (left-to-right in render order).
pub fn write_delete_using_params<S, O, B, Filters, Returning>(
    dialect: &'static dyn Dialect,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<<B as Backend>::Param>,
) -> Result<(), <B as Backend>::Error>
where
    S: TableProjection,
    O: TableProjection,
    B: Backend,
    Filters: RenderPredicateNodes<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    // Propagate a render reject rather than swallowing it (uniform with the other params collectors),
    // though a correlated-delete source is a `TableProjection` and so carries no unrenderable CTE shape.
    write_delete_using_with_params::<S, O, B, _, _, _>(
        dialect,
        target_alias,
        source_alias,
        filters,
        returning,
        &mut writer,
    )
    .map_err(<B as Backend>::render_error)?;
    writer.finish()
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
    // An UPDATE/DELETE aliases its target, so its RETURNING columns are alias-qualified by default
    // (PostgreSQL); SQLite cannot resolve that alias in RETURNING and renders them bare instead.
    let unqualified = renderer.dialect.returning_omits_target_alias();
    write_projection::<B, _>(returning, writer, renderer, unqualified)
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
    // An INSERT has no target alias, so its RETURNING columns are always unqualified.
    write_projection::<B, _>(returning, writer, renderer, true)
}

fn write_projection<B, Writer>(
    projection: &impl RenderProjectable<B>,
    writer: &mut Writer,
    renderer: &mut Renderer,
    unqualified_returning: bool,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    projection.visit_projection(&mut WriteProjection {
        writer,
        renderer,
        index: 0,
        unqualified_returning,
        _backend: PhantomData::<B>,
    })
}

struct WriteProjection<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    index: usize,
    unqualified_returning: bool,
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
        write_expr_value_node(expr, self.writer, self.renderer, self.unqualified_returning)?;
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
        write_column_value_node(
            column,
            self.writer,
            self.renderer,
            self.unqualified_returning,
        )?;
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
    unqualified_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
    Ast: RenderAst<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, unqualified_returning, |visitor| {
        expr.visit(visitor)
    })
}

fn write_column_value_node<K, B, Writer>(
    column: ColumnRef<'_, K>,
    writer: &mut Writer,
    renderer: &mut Renderer,
    unqualified_returning: bool,
) -> io::Result<()>
where
    K: ExprKind,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, unqualified_returning, |visitor| {
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
    let dialect = renderer.dialect;
    let direction = render_order_direction(order.direction());
    match order.nulls() {
        // MySQL has no `NULLS FIRST/LAST`; emulate it with a leading `(<expr> IS NULL)` sort key. The
        // expr is rendered twice — the param-collection pass runs this same path over a `ParamCollector`
        // writer, so SQL placeholders and binds stay in lock-step.
        Some(nulls) if dialect.emulates_order_nulls() => {
            // `NULLS LAST` => non-nulls (0) before nulls (1) => the IS-NULL key sorts ASC; FIRST => DESC.
            let nulls_key_direction = match nulls {
                crate::OrderNulls::Last => "ASC",
                crate::OrderNulls::First => "DESC",
            };
            writer.write_all(b"(")?;
            write_ast::<B, _>(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
            write!(writer, " IS NULL) {nulls_key_direction}, ")?;
            write_ast::<B, _>(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
            write!(writer, " {direction}")
        }
        Some(nulls) => {
            write_ast::<B, _>(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
            write!(writer, " {direction}")?;
            dialect.write_order_nulls(nulls, writer)
        }
        None => {
            write_ast::<B, _>(writer, renderer, false, |visitor| order.visit_expr(visitor))?;
            write!(writer, " {direction}")
        }
    }
}

/// Render a named window definition's interior — `PARTITION BY … ORDER BY … <frame>` — between the
/// `WINDOW w<n> AS (` and closing `)` written by [`SelectRenderSink::push_named_window`]. Mirrors the
/// `OVER (…)` body of [`RenderExpr::visit_window`] minus the function call.
fn write_named_window_body<Parts, Ords, B, Writer>(
    partitions: &Parts,
    orders: &Ords,
    frame: Option<crate::FrameSpec>,
    writer: &mut Writer,
    renderer: &mut Renderer,
) -> io::Result<()>
where
    Parts: crate::RenderWindowList<B>,
    Ords: crate::RenderWindowList<B>,
    B: Backend,
    Writer: SqlWriter<B>,
{
    write_ast::<B, _>(writer, renderer, false, |visitor| {
        let mut wrote = false;
        if <Parts as crate::RenderWindowList<B>>::NON_EMPTY {
            visitor.writer.write_all(b"PARTITION BY ")?;
            let mut first = true;
            partitions.render(visitor, &mut first)?;
            wrote = true;
        }
        if <Ords as crate::RenderWindowList<B>>::NON_EMPTY {
            if wrote {
                visitor.writer.write_all(b" ")?;
            }
            visitor.writer.write_all(b"ORDER BY ")?;
            let mut first = true;
            orders.render(visitor, &mut first)?;
            wrote = true;
        }
        if let Some(frame) = frame {
            if wrote {
                visitor.writer.write_all(b" ")?;
            }
            frame.render(&mut visitor.writer)?;
        }
        Ok(())
    })
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
    unqualified_returning: bool,
    render: impl FnOnce(&mut RenderExpr<'_, '_, B, Writer>) -> io::Result<()>,
) -> io::Result<()>
where
    B: Backend,
    Writer: SqlWriter<B>,
{
    let mut visitor = RenderExpr {
        writer,
        renderer,
        unqualified_returning,
        _backend: PhantomData::<B>,
    };
    render(&mut visitor)
}

struct RenderExpr<'writer, 'renderer, B, Writer> {
    writer: &'writer mut Writer,
    renderer: &'renderer mut Renderer,
    unqualified_returning: bool,
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
        if self.unqualified_returning {
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

    fn visit_nullif<L, R>(
        &mut self,
        left: L,
        left_needs_cast: bool,
        right: R,
        right_needs_cast: bool,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // Cast the operands only when *both* are bare literals/params (no typed operand to anchor the
        // type); otherwise the typed operand anchors the other and neither is cast, so `NULLIF`'s
        // equality keeps the operand's own type/collation (e.g. a `citext`/`decimal` column).
        let cast_both = left_needs_cast && right_needs_cast;
        let left_cast = if cast_both { result } else { None };
        let right_cast = if cast_both { result } else { None };
        self.writer.write_all(b"NULLIF(")?;
        self.visit_case_value_open(left_cast)?;
        left(self)?;
        self.visit_case_value_close(left_cast)?;
        self.writer.write_all(b", ")?;
        self.visit_case_value_open(right_cast)?;
        right(self)?;
        self.visit_case_value_close(right_cast)?;
        self.writer.write_all(b")")
    }

    fn visit_coalesce<Args>(
        &mut self,
        args: &Args,
        all_args_need_cast: bool,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        Args: RenderCoalesceArgs<B>,
    {
        // Cast the arguments only when every one is a bare literal/param (no typed operand to anchor
        // the result type); otherwise a typed column/expression anchors them and none are cast.
        let cast = if all_args_need_cast { result } else { None };
        self.writer.write_all(b"COALESCE(")?;
        args.render(self, cast, true)?;
        self.writer.write_all(b")")
    }

    fn visit_coalesce_separator(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b", ")
    }

    fn visit_aggregate<O>(
        &mut self,
        func: AggregateFunc,
        distinct: bool,
        cast: Option<&SqlType>,
        operand: O,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        let distinct = if distinct { "DISTINCT " } else { "" };
        // Some aggregates (`SUM`/`AVG`) have a database result type that differs from the Rust type
        // Squealy advertises (e.g. PostgreSQL `avg(int)` is `numeric`); a cast pins the wire type.
        match cast {
            Some(ty) => {
                write!(
                    self.writer,
                    "CAST({}({distinct}",
                    render_aggregate_func(func)
                )?;
                operand(self)?;
                self.writer.write_all(b") AS ")?;
                self.renderer
                    .dialect
                    .write_cast_type(ty, &mut *self.writer)?;
                self.writer.write_all(b")")
            }
            None => {
                write!(self.writer, "{}({distinct}", render_aggregate_func(func))?;
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
        frame: Option<crate::FrameSpec>,
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
        let mut wrote = false;
        if has_partitions {
            self.writer.write_all(b"PARTITION BY ")?;
            partitions(self)?;
            wrote = true;
        }
        if has_orders {
            if wrote {
                self.writer.write_all(b" ")?;
            }
            self.writer.write_all(b"ORDER BY ")?;
            orders(self)?;
            wrote = true;
        }
        if let Some(frame) = frame {
            if wrote {
                self.writer.write_all(b" ")?;
            }
            frame.render(&mut self.writer)?;
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

    fn visit_named_window<Operand>(
        &mut self,
        func: crate::WindowFunc,
        cast: Option<&SqlType>,
        operand: Operand,
        window_index: usize,
    ) -> Result<(), Self::Error>
    where
        Operand: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        if cast.is_some() {
            self.writer.write_all(b"CAST(")?;
        }
        write!(self.writer, "{}(", render_window_func(func))?;
        operand(self)?;
        write!(
            self.writer,
            ") OVER {}",
            crate::named_window_alias(window_index)
        )?;
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

    fn visit_case<Arms, Else>(
        &mut self,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        Arms: RenderCaseArms<B>,
        Else: RenderAst<B>,
    {
        // Each branch value is wrapped in `CAST(… AS result)` (not the whole `CASE`): an outer cast
        // does not type the branch parameters, but casting each branch does.
        self.writer.write_all(b"CASE")?;
        arms.render(self, result)?;
        if let Some(else_) = else_ {
            self.writer.write_all(b" ELSE ")?;
            self.visit_case_value_open(result)?;
            else_.visit(self)?;
            self.visit_case_value_close(result)?;
        }
        self.writer.write_all(b" END")
    }

    fn visit_simple_case<Operand, Arms, Else>(
        &mut self,
        operand: Operand,
        operand_needs_cast: bool,
        cmp: Option<&SqlType>,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        Operand: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Arms: RenderSimpleCaseArms<B>,
        Else: RenderAst<B>,
    {
        // A bare literal/param operand is cast to the comparison type (so Postgres can prepare an
        // all-parameter operand); a column operand keeps its own type. `WHEN` values are anchored by
        // the operand's type, so they are not cast.
        let operand_cast = if operand_needs_cast { cmp } else { None };
        self.writer.write_all(b"CASE ")?;
        self.visit_case_value_open(operand_cast)?;
        operand(self)?;
        self.visit_case_value_close(operand_cast)?;
        arms.render(self, result)?;
        if let Some(else_) = else_ {
            self.writer.write_all(b" ELSE ")?;
            self.visit_case_value_open(result)?;
            else_.visit(self)?;
            self.visit_case_value_close(result)?;
        }
        self.writer.write_all(b" END")
    }

    fn visit_unary_fn<O>(&mut self, func: UnaryStringFunc, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        let name = self.renderer.dialect.unary_string_fn_name(func);
        self.writer.write_all(name.as_bytes())?;
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer.write_all(b")")
    }

    fn visit_concat<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // `||` (PostgreSQL) propagates NULL and infers a bare parameter's type; `CONCAT` (MySQL) also
        // propagates NULL. Both match the builder's "nullable iff either operand is" model.
        if self.renderer.dialect.concat_uses_pipe_operator() {
            self.writer.write_all(b"(")?;
            left(self)?;
            self.writer.write_all(b" || ")?;
            right(self)?;
            self.writer.write_all(b")")
        } else {
            self.writer.write_all(b"CONCAT(")?;
            left(self)?;
            self.writer.write_all(b", ")?;
            right(self)?;
            self.writer.write_all(b")")
        }
    }

    fn visit_substring<S, Start, Len>(
        &mut self,
        string: S,
        start: Start,
        len: Len,
    ) -> Result<(), Self::Error>
    where
        S: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Start: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Len: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // The SQL-standard `SUBSTRING(s FROM start FOR len)` form (supported by PostgreSQL and MySQL).
        // PostgreSQL needs the `start`/`len` bounds cast to `integer` so a bare parameter is the
        // positional count — otherwise it resolves `SUBSTRING(text FROM unknown FOR unknown)` to its
        // regex `substring(text FROM pattern FOR escape)` overload. MySQL binds `?` by value (no
        // inference, no regex overload), so it needs no cast. The text operand is anchored by the
        // function and is never cast.
        // SQLite has no `SUBSTRING(s FROM start FOR len)` syntax — it spells it as the comma-argument
        // call `substr(s, start, len)` (1-based `start`, same as the standard form), and binds `?` by
        // value so the bounds need no cast.
        if self.renderer.dialect.substring_uses_function_call() {
            self.writer.write_all(b"substr(")?;
            string(self)?;
            self.writer.write_all(b", ")?;
            start(self)?;
            self.writer.write_all(b", ")?;
            len(self)?;
            return self.writer.write_all(b")");
        }
        let bound_cast = if self.renderer.dialect.substring_bounds_need_cast() {
            Some(SqlType::I32)
        } else {
            None
        };
        self.writer.write_all(b"SUBSTRING(")?;
        string(self)?;
        self.writer.write_all(b" FROM ")?;
        self.visit_case_value_open(bound_cast.as_ref())?;
        start(self)?;
        self.visit_case_value_close(bound_cast.as_ref())?;
        self.writer.write_all(b" FOR ")?;
        self.visit_case_value_open(bound_cast.as_ref())?;
        len(self)?;
        self.visit_case_value_close(bound_cast.as_ref())?;
        self.writer.write_all(b")")
    }

    fn visit_now(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b"CURRENT_TIMESTAMP")?;
        // MySQL's bare `CURRENT_TIMESTAMP` is fsp 0; render `CURRENT_TIMESTAMP(6)` so a `now()` value
        // keeps its microseconds into a `TIMESTAMP(6)` column (PostgreSQL's is already microsecond).
        if let Some(digits) = self.renderer.dialect.now_fractional_digits() {
            write!(self.writer, "({digits})")?;
        }
        Ok(())
    }

    fn visit_extract<O>(
        &mut self,
        field: DateField,
        operand: O,
        cast: &SqlType,
        timezone: Option<&str>,
        operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // A bare literal/param operand is cast to its timestamp type so PostgreSQL can resolve the
        // overloaded EXTRACT; a column is already typed (`operand_cast` is `None`).
        let operand_cast =
            operand_cast.filter(|_| self.renderer.dialect.timestamp_operand_needs_cast());
        // `Second` is the whole-seconds component: PostgreSQL's `EXTRACT(SECOND …)` is fractional, so
        // floor it to match MySQL's integer value (`FLOOR` is a no-op on MySQL's integer). Use
        // `extract_second` for the fractional part.
        let floor = field == DateField::Second;
        // The native EXTRACT type differs by dialect (PG numeric/double vs MySQL integer), so cast to
        // a uniform result type.
        self.writer.write_all(b"CAST(")?;
        if floor {
            self.writer.write_all(b"FLOOR(")?;
        }
        self.writer.write_all(b"EXTRACT(")?;
        self.writer.write_all(field.extract_keyword().as_bytes())?;
        self.writer.write_all(b" FROM ")?;
        match timezone {
            Some(tz) => {
                self.writer.write_all(b"(")?;
                self.visit_case_value_open(operand_cast)?;
                operand(self)?;
                self.visit_case_value_close(operand_cast)?;
                self.writer.write_all(b" AT TIME ZONE ")?;
                write_sql_string_literal(&mut *self.writer, tz)?;
                self.writer.write_all(b")")?;
            }
            None => {
                self.visit_case_value_open(operand_cast)?;
                operand(self)?;
                self.visit_case_value_close(operand_cast)?;
            }
        }
        self.writer.write_all(b")")?; // close EXTRACT
        if floor {
            self.writer.write_all(b")")?; // close FLOOR
        }
        self.writer.write_all(b" AS ")?;
        self.renderer
            .dialect
            .write_cast_type(cast, &mut *self.writer)?;
        self.writer.write_all(b")")
    }

    fn visit_date_trunc<O>(
        &mut self,
        unit: DateField,
        operand: O,
        timezone: Option<&str>,
        operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // A bare literal/param operand is cast to its timestamp type so PostgreSQL can resolve the
        // overloaded date_trunc; a column is already typed (`operand_cast` is `None`).
        let operand_cast =
            operand_cast.filter(|_| self.renderer.dialect.timestamp_operand_needs_cast());
        match timezone {
            // PostgreSQL's 3-argument `date_trunc('unit', ts, 'tz')` (PG 12+) truncates `ts` in `tz`
            // and returns a `timestamptz` directly. This avoids reinterpreting an ambiguous local wall
            // time — a `… AT TIME ZONE 'tz'` round-trip would resolve a DST fall-back repeated hour to
            // the wrong offset; PostgreSQL handles the zone math (including DST) internally.
            Some(tz) => {
                self.writer.write_all(b"date_trunc('")?;
                self.writer.write_all(unit.trunc_literal().as_bytes())?;
                self.writer.write_all(b"', ")?;
                self.visit_case_value_open(operand_cast)?;
                operand(self)?;
                self.visit_case_value_close(operand_cast)?;
                self.writer.write_all(b", ")?;
                write_sql_string_literal(&mut *self.writer, tz)?;
                self.writer.write_all(b")")?;
            }
            None => {
                self.writer.write_all(b"date_trunc('")?;
                self.writer.write_all(unit.trunc_literal().as_bytes())?;
                self.writer.write_all(b"', ")?;
                self.visit_case_value_open(operand_cast)?;
                operand(self)?;
                self.visit_case_value_close(operand_cast)?;
                self.writer.write_all(b")")?;
            }
        }
        Ok(())
    }

    fn visit_extract_second<O>(
        &mut self,
        operand: O,
        cast: &SqlType,
        operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        let operand_cast =
            operand_cast.filter(|_| self.renderer.dialect.timestamp_operand_needs_cast());
        // PostgreSQL's `EXTRACT(SECOND …)` is fractional; MySQL's is integer-only, so it uses the
        // composite `SECOND_MICROSECOND` unit (returns `SSffffff`) divided back to fractional seconds.
        let micro = self.renderer.dialect.extract_second_uses_microsecond_unit();
        self.writer.write_all(b"CAST(EXTRACT(")?;
        self.writer.write_all(if micro {
            b"SECOND_MICROSECOND".as_slice()
        } else {
            b"SECOND".as_slice()
        })?;
        self.writer.write_all(b" FROM ")?;
        self.visit_case_value_open(operand_cast)?;
        operand(self)?;
        self.visit_case_value_close(operand_cast)?;
        self.writer.write_all(b")")?; // close EXTRACT
        if micro {
            self.writer.write_all(b" / 1000000.0")?;
        }
        self.writer.write_all(b" AS ")?;
        self.renderer
            .dialect
            .write_cast_type(cast, &mut *self.writer)?;
        self.writer.write_all(b")")
    }

    fn visit_case_when(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b" WHEN ")
    }

    fn visit_case_then(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b" THEN ")
    }

    fn visit_case_value_open(&mut self, cast: Option<&SqlType>) -> Result<(), Self::Error> {
        if cast.is_some() {
            self.writer.write_all(b"CAST(")?;
        }
        Ok(())
    }

    fn visit_case_value_close(&mut self, cast: Option<&SqlType>) -> Result<(), Self::Error> {
        if let Some(ty) = cast {
            self.writer.write_all(b" AS ")?;
            self.renderer
                .dialect
                .write_cast_type(ty, &mut *self.writer)?;
            self.writer.write_all(b")")?;
        }
        Ok(())
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
    conflict: Option<&ConflictClause>,
    buffer: &mut PreparedSql<B>,
) where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    buffer.clear();
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, conflict, buffer).unwrap();
}

pub fn write_insert_params<S, B, Rows, Returning>(
    dialect: &'static dyn Dialect,
    rows: &Rows,
    returning: &Returning,
    conflict: Option<&ConflictClause>,
    params: &mut Vec<B::Param>,
) -> Result<(), B::Error>
where
    S: InsertableTable,
    B: Backend,
    Rows: RenderInsertRows<B>,
    Returning: RenderProjectable<B>,
{
    let mut writer = ParamCollector::<B>::new(params);
    write_insert_with_params::<S, B, _, _, _>(dialect, rows, returning, conflict, &mut writer)
        .unwrap();
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
