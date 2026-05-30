use std::future::{Future, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    ArithmeticOp, Backend, BindValue, CompareOp, Connection, ExprNode, OrderDirection, OrderNode,
    PredicateNode, ProjectionShape, Returning, Select, SelectBuilder, SelectColumn, SelectQuery,
    Sort, Source, SourceKind, SourceTarget, Table, build_select,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TestConnection;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestError {
    NoRows,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyRows<Row> {
    _row: PhantomData<Row>,
}

impl<Row> Default for EmptyRows<Row> {
    fn default() -> Self {
        Self { _row: PhantomData }
    }
}

impl<Row> Stream for EmptyRows<Row> {
    type Item = Result<Row, TestError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    select: Select,
    _connection: PhantomData<&'conn TestConnection>,
    _shape: PhantomData<Shape>,
}

impl<'conn, Shape> SelectQuery<'conn> for TestSelect<'conn, Shape>
where
    Shape: ProjectionShape,
{
    type Connection = TestConnection;
    type Shape = Shape;
    type Row = Shape::Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn ir(&self) -> &Select {
        &self.select
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        EmptyRows::default()
    }

    fn fetch_all(
        &self,
    ) -> impl Future<Output = Result<Vec<Self::Row>, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Ok(Vec::new()))
    }

    fn fetch_one(
        &self,
    ) -> impl Future<Output = Result<Self::Row, <Self::Connection as Connection>::Error>> + Send + '_
    {
        ready(Err(TestError::NoRows))
    }

    fn fetch_optional(
        &self,
    ) -> impl Future<Output = Result<Option<Self::Row>, <Self::Connection as Connection>::Error>>
    + Send
    + '_ {
        ready(Ok(None))
    }
}

impl<Shape> TestSelect<'_, Shape>
where
    Shape: ProjectionShape,
{
    pub fn to_sql(&self) -> String {
        let mut sql = Vec::new();
        write_select_sql(&self.select, &mut sql).unwrap();
        String::from_utf8(sql).unwrap()
    }

    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        write_select_sql(&self.select, writer)
    }

    pub fn params(&self) -> Vec<BindValue> {
        select_params(&self.select)
    }
}

impl Backend for TestConnection {
    fn write_table(
        &self,
        table: &(dyn Table + Sync),
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()> {
        write!(writer, "CREATE TABLE {} (", table.qualified_name())?;
        for (index, column) in table.columns().iter().enumerate() {
            if index > 0 {
                writer.write_all(b", ")?;
            }
            write!(
                writer,
                "{} {}",
                column.name(),
                column.db_type().unwrap_or("text")
            )?;
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
                write!(writer, " DEFAULT {default}")?;
            }
            if let Some(reference) = column.references() {
                write!(
                    writer,
                    " REFERENCES {}{}({})",
                    reference
                        .schema_name()
                        .map(|schema| format!("{schema}."))
                        .unwrap_or_default(),
                    reference.table(),
                    reference.column()
                )?;
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
            let columns = index.columns().join(", ");
            write!(
                writer,
                "\nCREATE {unique}INDEX {name} ON {} ({columns})",
                table.qualified_name()
            )?;
        }

        Ok(())
    }
}

impl Connection for TestConnection {
    type Error = TestError;

    type Select<'conn, Shape>
        = TestSelect<'conn, Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(&mut SelectBuilder<'_, 'scope, Self>) -> Returning<Shape>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape,
    {
        TestSelect {
            select: build_select::<Self, Shape>(f),
            _connection: PhantomData,
            _shape: PhantomData,
        }
    }
}

fn write_select_sql(select: &Select, writer: &mut impl std::io::Write) -> std::io::Result<()> {
    writer.write_all(b"SELECT ")?;
    write_select_columns(select.columns(), writer)?;
    if !select.sources().is_empty() {
        writer.write_all(b" ")?;
        write_sources(select.sources(), writer)?;
    }
    write_filters(select.filters(), writer)?;
    write_orders(select.orders(), writer)?;
    if let Some(limit) = select.limit() {
        write!(writer, " LIMIT {limit}")?;
    }
    if let Some(offset) = select.offset() {
        write!(writer, " OFFSET {offset}")?;
    }
    Ok(())
}

fn write_select_columns(
    columns: &[SelectColumn],
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} AS {}", render_expr(&column.expr), column.alias)?;
    }
    Ok(())
}

fn write_sources(sources: &[Source], writer: &mut impl std::io::Write) -> std::io::Result<()> {
    for (index, source) in sources.iter().enumerate() {
        if index > 0 {
            writer.write_all(b" ")?;
        }
        write_source(source, index, writer)?;
    }
    Ok(())
}

fn write_source(
    source: &Source,
    position: usize,
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    match (source.kind(), source.target(), position) {
        (SourceKind::From, SourceTarget::Table(table), _) => {
            write!(writer, "FROM {table} AS {}", source.alias())
        }
        (SourceKind::From, SourceTarget::Query(query), _) => {
            writer.write_all(b"FROM (")?;
            write_select_sql(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Query(query), 0) => {
            writer.write_all(b"FROM (")?;
            write_select_sql(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Query(query), _) => {
            writer.write_all(b"INNER JOIN LATERAL (")?;
            write_select_sql(query, writer)?;
            write!(writer, ") AS {} ON TRUE", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Table(table), 0) => {
            write!(writer, "FROM {table} AS {}", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Table(table), _) => {
            write!(
                writer,
                "INNER JOIN LATERAL {table} AS {} ON TRUE",
                source.alias()
            )
        }
        (SourceKind::InnerJoin { on: _ }, SourceTarget::Table(table), 0) => {
            write!(writer, "FROM {table} AS {}", source.alias())
        }
        (SourceKind::InnerJoin { on }, SourceTarget::Table(table), _) => {
            write!(
                writer,
                "INNER JOIN {table} AS {} ON {}",
                source.alias(),
                render_predicate(on)
            )
        }
        (SourceKind::InnerJoin { on: _ }, SourceTarget::Query(query), 0) => {
            writer.write_all(b"FROM (")?;
            write_select_sql(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerJoin { on }, SourceTarget::Query(query), _) => {
            writer.write_all(b"INNER JOIN (")?;
            write_select_sql(query, writer)?;
            write!(
                writer,
                ") AS {} ON {}",
                source.alias(),
                render_predicate(on)
            )
        }
        (SourceKind::LeftJoin { on: _ }, SourceTarget::Table(table), 0) => {
            write!(writer, "FROM {table} AS {}", source.alias())
        }
        (SourceKind::LeftJoin { on }, SourceTarget::Table(table), _) => {
            write!(
                writer,
                "LEFT JOIN {table} AS {} ON {}",
                source.alias(),
                render_predicate(on)
            )
        }
        (SourceKind::LeftJoin { on: _ }, SourceTarget::Query(query), 0) => {
            writer.write_all(b"FROM (")?;
            write_select_sql(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::LeftJoin { on }, SourceTarget::Query(query), _) => {
            writer.write_all(b"LEFT JOIN (")?;
            write_select_sql(query, writer)?;
            write!(
                writer,
                ") AS {} ON {}",
                source.alias(),
                render_predicate(on)
            )
        }
    }
}

fn write_filters(
    filters: &[squealy::Filter],
    writer: &mut impl std::io::Write,
) -> std::io::Result<()> {
    if filters.is_empty() {
        return Ok(());
    }

    writer.write_all(b" WHERE ")?;
    for (index, filter) in filters.iter().enumerate() {
        if index > 0 {
            writer.write_all(b" AND ")?;
        }
        writer.write_all(render_predicate(filter.predicate()).as_bytes())?;
    }
    Ok(())
}

fn write_orders(orders: &[Sort], writer: &mut impl std::io::Write) -> std::io::Result<()> {
    if orders.is_empty() {
        return Ok(());
    }

    writer.write_all(b" ORDER BY ")?;
    for (index, order) in orders.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        writer.write_all(render_order(order.order()).as_bytes())?;
    }
    Ok(())
}

fn render_expr(expr: &ExprNode) -> String {
    match expr {
        ExprNode::Column { alias, column } => format!("{alias}.{column}"),
        ExprNode::Literal(_) => "?".to_owned(),
        ExprNode::Binary { left, op, right } => {
            format!(
                "({} {} {})",
                render_expr(left),
                render_arithmetic_op(*op),
                render_expr(right)
            )
        }
    }
}

fn render_predicate(predicate: &PredicateNode) -> String {
    match predicate {
        PredicateNode::Compare { left, op, right } => {
            format!(
                "({} {} {})",
                render_expr(left),
                render_compare_op(*op),
                render_expr(right)
            )
        }
        PredicateNode::And { left, right } => {
            format!(
                "({} AND {})",
                render_predicate(left),
                render_predicate(right)
            )
        }
        PredicateNode::Or { left, right } => {
            format!(
                "({} OR {})",
                render_predicate(left),
                render_predicate(right)
            )
        }
        PredicateNode::Not(predicate) => format!("(NOT {})", render_predicate(predicate)),
    }
}

fn render_order(order: &OrderNode) -> String {
    format!(
        "{} {}",
        render_expr(&order.expr),
        render_order_direction(order.direction)
    )
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

fn select_params(select: &Select) -> Vec<BindValue> {
    let mut params = Vec::new();
    for column in select.columns() {
        collect_expr_params(&column.expr, &mut params);
    }
    for (position, source) in select.sources().iter().enumerate() {
        collect_source_params(source, position, &mut params);
    }
    for filter in select.filters() {
        collect_predicate_params(filter.predicate(), &mut params);
    }
    for order in select.orders() {
        collect_order_params(order.order(), &mut params);
    }
    params
}

fn collect_source_params(source: &Source, position: usize, params: &mut Vec<BindValue>) {
    if let SourceTarget::Query(query) = source.target() {
        params.extend(select_params(query));
    }

    if position > 0 {
        match source.kind() {
            SourceKind::InnerJoin { on } | SourceKind::LeftJoin { on } => {
                collect_predicate_params(on, params)
            }
            SourceKind::From | SourceKind::InnerLateral => {}
        }
    }
}

fn collect_expr_params(expr: &ExprNode, params: &mut Vec<BindValue>) {
    match expr {
        ExprNode::Column { .. } => {}
        ExprNode::Literal(value) => params.push(value.clone()),
        ExprNode::Binary { left, right, .. } => {
            collect_expr_params(left, params);
            collect_expr_params(right, params);
        }
    }
}

fn collect_predicate_params(predicate: &PredicateNode, params: &mut Vec<BindValue>) {
    match predicate {
        PredicateNode::Compare { left, right, .. } => {
            collect_expr_params(left, params);
            collect_expr_params(right, params);
        }
        PredicateNode::And { left, right } | PredicateNode::Or { left, right } => {
            collect_predicate_params(left, params);
            collect_predicate_params(right, params);
        }
        PredicateNode::Not(predicate) => collect_predicate_params(predicate, params),
    }
}

fn collect_order_params(order: &OrderNode, params: &mut Vec<BindValue>) {
    collect_expr_params(&order.expr, params);
}
