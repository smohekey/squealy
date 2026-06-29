use std::borrow::Cow;
use std::io::{self, Write};

use squealy::{
    AggregateFunc, ArithmeticOp, AssignmentValueVisitor, AssignmentVisitor, Column, ColumnDefault,
    ColumnRef, ColumnType, CompareOp, CteDef, DateField, Dialect, Encode, Expr, ExprKind,
    ExprVisitor, InsertRow, InsertRowVisitor, InsertableTable, Order, OrderDirection, OrderNulls,
    Predicate, PredicateAstVisitor, PredicateKind, PredicateVisitor, ProjectionShape,
    ProjectionVisitor, QueryBuilder, RenderAssignment, RenderAst, RenderCaseArms,
    RenderCoalesceArgs, RenderInsertAssignments, RenderInsertRows, RenderPredicateAst,
    RenderPredicateNodes, RenderProjectable, RenderSelectAst, RenderSimpleCaseArms, RenderSubquery,
    RenderUpdateAssignments, RowLock, SchemaTable, SelectSink, Selected, SourceAlias, SqlType,
    Table, TableProjection, UnaryStringFunc, UpdateableTable, WindowFunc,
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
    distinct: bool,
    columns: usize,
    sources: usize,
    filters: usize,
    groups: usize,
    havings: usize,
    orders: usize,
    limit: Option<usize>,
    offset: Option<usize>,
    row_lock: Option<RowLock>,
}

impl<'writer, Writer> TestSelectSink<'writer, Writer>
where
    Writer: SqlWriter,
{
    fn new(writer: &'writer mut Writer) -> io::Result<Self> {
        writer.write_all(b"SELECT ")?;
        Ok(Self {
            writer,
            distinct: false,
            columns: 0,
            sources: 0,
            filters: 0,
            groups: 0,
            havings: 0,
            orders: 0,
            limit: None,
            offset: None,
            row_lock: None,
        })
    }
}

impl<Writer> SelectSink for TestSelectSink<'_, Writer>
where
    Writer: SqlWriter,
{
    type Error = io::Error;
    type Backend = crate::TestBackend;

    fn set_distinct(&mut self) -> io::Result<()> {
        self.distinct = true;
        Ok(())
    }

    fn push_projection<Shape, P>(&mut self, projection: P) -> io::Result<()>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<crate::TestBackend>,
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

    fn push_right_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> io::Result<()>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<crate::TestBackend>,
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
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        self.push_join::<S, P, Ast>(alias, on, "FULL JOIN")
    }

    fn push_cross_join<S>(&mut self, alias: SourceAlias) -> io::Result<()>
    where
        S: TableProjection,
    {
        // `CROSS JOIN <table> AS <alias>` — Cartesian product, no `ON`.
        let first_source = self.sources == 0;
        self.push_source_separator()?;
        if first_source {
            write!(self.writer, "FROM {} AS {alias}", S::qualified_name())
        } else {
            write!(self.writer, "CROSS JOIN {} AS {alias}", S::qualified_name())
        }
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

    fn push_group<K, Ast>(&mut self, key: &Expr<'_, K, Ast>) -> io::Result<()>
    where
        K: ExprKind,
        Ast: RenderAst<crate::TestBackend>,
    {
        if self.groups == 0 {
            self.writer.write_all(b" GROUP BY ")?;
        } else {
            self.writer.write_all(b", ")?;
        }
        self.groups += 1;
        write_expr_value(key, self.writer)
    }

    fn push_having<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> io::Result<()>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<crate::TestBackend>,
    {
        if self.havings == 0 {
            self.writer.write_all(b" HAVING ")?;
        } else {
            self.writer.write_all(b" AND ")?;
        }
        self.havings += 1;
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

    fn set_row_lock(&mut self, lock: RowLock) -> io::Result<()> {
        self.row_lock = Some(lock);
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

/// The test backend's SQL dialect: bare (unquoted) identifiers and `?` placeholders, matching the
/// hand-rolled rendering in [`TestSelectSink`]. Only used to render CTE bodies (which go through the
/// shared [`squealy::view_render`]); the main query is rendered directly by the sink.
struct TestDialect;

impl Dialect for TestDialect {
    fn write_placeholder(&self, _index: usize, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(b"?")
    }

    fn write_quoted_ident(&self, ident: &str, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(ident.as_bytes())
    }

    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        write_test_cast_type(ty, writer)
    }

    fn write_order_nulls(&self, nulls: OrderNulls, writer: &mut dyn Write) -> io::Result<()> {
        writer.write_all(match nulls {
            OrderNulls::First => b" NULLS FIRST",
            OrderNulls::Last => b" NULLS LAST",
        })
    }
}

/// Writes a query's `WITH` prefix — `WITH n1 AS (<body>), n2 AS (<body>) ` — when the select
/// references any CTEs. The defs are de-duplicated/ordered by `Selected::collect_ctes`; each body is
/// parameter-free, so it adds no bind params and is rendered via the shared view renderer.
fn write_cte_prefix(ctes: &[&'static dyn CteDef], writer: &mut dyn Write) -> io::Result<()> {
    if ctes.is_empty() {
        return Ok(());
    }
    if ctes.iter().any(|def| def.is_recursive()) {
        writer.write_all(b"WITH RECURSIVE ")?;
    } else {
        writer.write_all(b"WITH ")?;
    }
    for (index, def) in ctes.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} (", def.name())?;
        for (column_index, column) in def.columns().iter().enumerate() {
            if column_index > 0 {
                writer.write_all(b", ")?;
            }
            writer.write_all(column.name.as_bytes())?;
        }
        writer.write_all(b") AS (")?;
        match def.body() {
            squealy::CteBody::Plain(model) => {
                squealy::view_render::render_cte_body(&model, &TestDialect, writer)?;
            }
            squealy::CteBody::Recursive {
                anchor,
                union_all,
                recursive,
            } => {
                squealy::view_render::render_recursive_cte_body(
                    &anchor,
                    union_all,
                    &recursive,
                    &TestDialect,
                    writer,
                )?;
            }
        }
        writer.write_all(b")")?;
    }
    writer.write_all(b" ")
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
    write_cte_prefix(
        &selected.collect_ctes::<Conn, crate::TestBackend>(),
        &mut writer,
    )?;
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
    // CTE bodies are parameter-free, so the `WITH` prefix contributes no bind params (the collector
    // ignores the emitted bytes); keeping it uniform with the SQL-text path.
    write_cte_prefix(
        &selected.collect_ctes::<Conn, crate::TestBackend>(),
        &mut writer,
    )
    .unwrap();
    let mut select_sink = TestSelectSink::new(&mut writer).unwrap();
    selected.lower_into::<Conn, _>(&mut select_sink).unwrap();
    select_sink.finish().unwrap();
    writer.finish()
}

/// Render a set-operation query for the test backend, reusing the shared core set renderer with the
/// test dialect (bare identifiers, `?` placeholders) so it matches the test backend's SQL style.
pub(crate) fn write_set_into<'conn, 'scope, Tree, Writer>(
    tree: &Tree,
    tail: &squealy::SetTail,
    writer: &mut Writer,
) -> io::Result<()>
where
    Tree: squealy::render::RenderSetArm<'conn, 'scope, crate::TestConnection, crate::TestBackend>,
    Writer: Write,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_set_into::<crate::TestConnection, Tree, _>(&DIALECT, tree, tail, writer)
}

pub(crate) fn write_set_params<'conn, 'scope, Tree>(
    tree: &Tree,
    tail: &squealy::SetTail,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    Tree: squealy::render::RenderSetArm<'conn, 'scope, crate::TestConnection, crate::TestBackend>,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_set_params::<crate::TestConnection, Tree>(&DIALECT, tree, tail, params)
}

pub(crate) fn write_insert_select<'conn, 'scope, S, Tree, Returning, Writer>(
    columns: &[&str],
    source: &Tree,
    returning: &Returning,
    writer: &mut Writer,
) -> io::Result<()>
where
    S: squealy::InsertableTable,
    Tree: squealy::render::RenderSetArm<'conn, 'scope, crate::TestConnection, crate::TestBackend>,
    Returning: squealy::RenderProjectable<crate::TestBackend>,
    Writer: Write,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_insert_select::<S, crate::TestConnection, Tree, Returning>(
        &DIALECT, columns, source, returning, writer,
    )
}

pub(crate) fn write_insert_select_params<'conn, 'scope, S, Tree, Returning>(
    columns: &[&str],
    source: &Tree,
    returning: &Returning,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    S: squealy::InsertableTable,
    Tree: squealy::render::RenderSetArm<'conn, 'scope, crate::TestConnection, crate::TestBackend>,
    Returning: squealy::RenderProjectable<crate::TestBackend>,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_insert_select_params::<S, crate::TestConnection, Tree, Returning>(
        &DIALECT, columns, source, returning, params,
    )
}

pub(crate) fn write_update_from<S, O, Columns, Filters, Returning>(
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
    Columns: RenderUpdateAssignments<crate::TestBackend>,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_update_from::<S, O, crate::TestBackend, Columns, Filters, Returning>(
        &DIALECT,
        target_alias,
        source_alias,
        columns,
        filters,
        returning,
        writer,
    )
}

pub(crate) fn write_update_from_params<S, O, Columns, Filters, Returning>(
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: &Columns,
    filters: &Filters,
    returning: &Returning,
    params: &mut Vec<TestParam>,
) -> Result<(), crate::TestError>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: RenderUpdateAssignments<crate::TestBackend>,
    Filters: RenderPredicateNodes<crate::TestBackend>,
    Returning: RenderProjectable<crate::TestBackend>,
{
    static DIALECT: TestDialect = TestDialect;
    squealy::render::write_update_from_params::<S, O, crate::TestBackend, Columns, Filters, Returning>(
        &DIALECT,
        target_alias,
        source_alias,
        columns,
        filters,
        returning,
        params,
    )
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
        if let Some(lock) = self.row_lock {
            self.writer.write_all(match lock {
                RowLock::Update => b" FOR UPDATE" as &[u8],
                RowLock::Share => b" FOR SHARE",
            })?;
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

/// Writes a `CAST(… AS <type>)` target type for the test dialect, mirroring the DDL names in
/// [`write_test_column_type`]. Used only when a CTE body contains a cast (e.g. float division).
fn write_test_cast_type(ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
    let name = match ty {
        SqlType::Raw(db_type) => return writer.write_all(db_type.as_bytes()),
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
        SqlType::FixedBytes(length) => return write!(writer, "binary({length})"),
    };
    writer.write_all(name.as_bytes())
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
        ColumnType::FixedBytes(length) => return write!(writer, "binary({length})"),
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

/// Render an embedded subquery as a nested `SELECT …` into the same writer (the test backend uses
/// bare `?` placeholders, so there is no parameter counter to share).
fn write_subselect<Sub>(subquery: &Sub, writer: &mut impl SqlWriter) -> io::Result<()>
where
    Sub: RenderSubquery<crate::TestBackend>,
{
    let mut sink = TestSelectSink::new(writer)?;
    subquery.lower_subquery(&mut sink)?;
    sink.finish()
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
    write!(writer, " {}", render_order_direction(order.direction()))?;
    if let Some(nulls) = order.nulls() {
        writer.write_all(match nulls {
            OrderNulls::First => b" NULLS FIRST" as &[u8],
            OrderNulls::Last => b" NULLS LAST",
        })?;
    }
    Ok(())
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

    fn visit_nullif<L, R>(
        &mut self,
        left: L,
        _left_needs_cast: bool,
        right: R,
        _right_needs_cast: bool,
        _result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // The in-memory test backend renders the bare NULLIF (no dialect cast), like its aggregates.
        self.writer.write_all(b"NULLIF(")?;
        left(self)?;
        self.writer.write_all(b", ")?;
        right(self)?;
        self.writer.write_all(b")")
    }

    fn visit_coalesce<Args>(
        &mut self,
        args: &Args,
        _all_args_need_cast: bool,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        Args: RenderCoalesceArgs<Self::Backend>,
    {
        // Bare COALESCE (the test backend's cast hooks are no-ops).
        self.writer.write_all(b"COALESCE(")?;
        args.render(self, result, true)?;
        self.writer.write_all(b")")
    }

    fn visit_coalesce_separator(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b", ")
    }

    fn visit_aggregate<O>(
        &mut self,
        func: AggregateFunc,
        distinct: bool,
        _cast: Option<&SqlType>,
        operand: O,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // The in-memory test backend renders the bare aggregate (no dialect casts), matching how it
        // skips the integer-division cast the real backends emit.
        let distinct = if distinct { "DISTINCT " } else { "" };
        write!(self.writer, "{}({distinct}", render_aggregate_func(func))?;
        operand(self)?;
        self.writer.write_all(b")")
    }

    fn visit_scalar_subquery<Sub>(&mut self, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: RenderSubquery<crate::TestBackend>,
    {
        self.writer.write_all(b"(")?;
        write_subselect(subquery, &mut *self.writer)?;
        self.writer.write_all(b")")
    }

    fn visit_window<Operand, Partitions, Orders>(
        &mut self,
        func: WindowFunc,
        _cast: Option<&SqlType>,
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
        // The in-memory test backend renders the bare window call (no dialect cast), matching how it
        // skips the aggregate casts the real backends emit.
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
        self.writer.write_all(b")")
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
        Arms: RenderCaseArms<Self::Backend>,
        Else: RenderAst<Self::Backend>,
    {
        // The in-memory test backend renders the bare CASE (no dialect cast), like its aggregates.
        self.writer.write_all(b"CASE")?;
        arms.render(self, result)?;
        if let Some(else_) = else_ {
            self.writer.write_all(b" ELSE ")?;
            else_.visit(self)?;
        }
        self.writer.write_all(b" END")
    }

    fn visit_simple_case<Operand, Arms, Else>(
        &mut self,
        operand: Operand,
        _operand_needs_cast: bool,
        _cmp: Option<&SqlType>,
        arms: &Arms,
        else_: Option<&Else>,
        result: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        Operand: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Arms: RenderSimpleCaseArms<Self::Backend>,
        Else: RenderAst<Self::Backend>,
    {
        // Bare simple CASE (the test backend's cast hooks are no-ops).
        self.writer.write_all(b"CASE ")?;
        operand(self)?;
        arms.render(self, result)?;
        if let Some(else_) = else_ {
            self.writer.write_all(b" ELSE ")?;
            else_.visit(self)?;
        }
        self.writer.write_all(b" END")
    }

    fn visit_unary_fn<O>(&mut self, func: UnaryStringFunc, operand: O) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(func.sql_name().as_bytes())?;
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer.write_all(b")")
    }

    fn visit_concat<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        self.writer.write_all(b"CONCAT(")?;
        left(self)?;
        self.writer.write_all(b", ")?;
        right(self)?;
        self.writer.write_all(b")")
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
        self.writer.write_all(b"SUBSTRING(")?;
        string(self)?;
        self.writer.write_all(b" FROM ")?;
        start(self)?;
        self.writer.write_all(b" FOR ")?;
        len(self)?;
        self.writer.write_all(b")")
    }

    fn visit_now(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b"CURRENT_TIMESTAMP")
    }

    fn visit_extract<O>(
        &mut self,
        field: DateField,
        operand: O,
        _cast: &SqlType,
        timezone: Option<&str>,
        _operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // Bare EXTRACT (the in-memory test backend ignores the dialect cast, like its aggregates, and
        // binds by value so it needs no operand type anchor). The timezone-explicit form is
        // PostgreSQL-only (gated on `SupportsDateTrunc`, which this backend does not implement), so
        // `timezone` is always `None` here. `Second` is floored to the whole-seconds component (see
        // the shared renderer).
        debug_assert!(timezone.is_none());
        let floor = field == DateField::Second;
        if floor {
            self.writer.write_all(b"FLOOR(")?;
        }
        self.writer.write_all(b"EXTRACT(")?;
        self.writer.write_all(field.extract_keyword().as_bytes())?;
        self.writer.write_all(b" FROM ")?;
        operand(self)?;
        self.writer.write_all(b")")?;
        if floor {
            self.writer.write_all(b")")?;
        }
        Ok(())
    }

    fn visit_date_trunc<O>(
        &mut self,
        unit: DateField,
        operand: O,
        _timezone: Option<&str>,
        _operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // The test backend stands in for a MySQL-like dialect (no `SupportsDateTrunc`), so this is
        // never reached; provided to satisfy the visitor trait.
        self.writer.write_all(b"date_trunc('")?;
        self.writer.write_all(unit.trunc_literal().as_bytes())?;
        self.writer.write_all(b"', ")?;
        operand(self)?;
        self.writer.write_all(b")")
    }

    fn visit_extract_second<O>(
        &mut self,
        operand: O,
        _cast: &SqlType,
        _operand_cast: Option<&SqlType>,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
    {
        // MySQL-like: fractional seconds via the composite `SECOND_MICROSECOND` unit (bare, no cast —
        // the in-memory backend ignores the dialect cast and binds by value).
        self.writer.write_all(b"EXTRACT(SECOND_MICROSECOND FROM ")?;
        operand(self)?;
        self.writer.write_all(b") / 1000000.0")
    }

    fn visit_case_when(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b" WHEN ")
    }

    fn visit_case_then(&mut self) -> Result<(), Self::Error> {
        self.writer.write_all(b" THEN ")
    }

    fn visit_case_value_open(&mut self, _cast: Option<&SqlType>) -> Result<(), Self::Error> {
        Ok(())
    }

    fn visit_case_value_close(&mut self, _cast: Option<&SqlType>) -> Result<(), Self::Error> {
        Ok(())
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
        self.writer.write_all(match (case_insensitive, negated) {
            (false, false) => b" LIKE " as &[u8],
            (false, true) => b" NOT LIKE ",
            (true, false) => b" ILIKE ",
            (true, true) => b" NOT ILIKE ",
        })?;
        pattern(self)?;
        self.writer.write_all(b")")
    }

    fn visit_in<O, T>(&mut self, negated: bool, operand: O, values: &[T]) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        T: Encode<crate::TestBackend>,
    {
        if values.is_empty() {
            // Render the operand once so its runtime params stay aligned; see the shared renderer.
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
            self.writer.write_all(b"?")?;
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
        } else {
            self.writer.write_all(b"(")?;
        }
        operand(self)?;
        self.writer.write_all(b")")
    }

    fn visit_in_subquery<O, Sub>(
        &mut self,
        negated: bool,
        operand: O,
        subquery: &Sub,
    ) -> Result<(), Self::Error>
    where
        O: FnOnce(&mut Self) -> Result<(), Self::Error>,
        Sub: RenderSubquery<crate::TestBackend>,
    {
        self.writer.write_all(b"(")?;
        operand(self)?;
        self.writer
            .write_all(if negated { b" NOT IN (" } else { b" IN (" })?;
        write_subselect(subquery, &mut *self.writer)?;
        self.writer.write_all(b"))")
    }

    fn visit_exists<Sub>(&mut self, negated: bool, subquery: &Sub) -> Result<(), Self::Error>
    where
        Sub: RenderSubquery<crate::TestBackend>,
    {
        self.writer.write_all(if negated {
            b"(NOT EXISTS ("
        } else {
            b"(EXISTS ("
        })?;
        write_subselect(subquery, &mut *self.writer)?;
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

fn render_window_func(func: WindowFunc) -> &'static str {
    match func {
        WindowFunc::Aggregate(aggregate) => render_aggregate_func(aggregate),
        WindowFunc::RowNumber => "ROW_NUMBER",
        WindowFunc::Rank => "RANK",
        WindowFunc::DenseRank => "DENSE_RANK",
        WindowFunc::Ntile => "NTILE",
        WindowFunc::Lag => "LAG",
        WindowFunc::Lead => "LEAD",
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
