use std::io::{self, Write};

use squealy::{
    ArithmeticOp, BindValue, CompareOp, Delete, ExprNode, Insert, OrderDirection, OrderNode,
    PredicateNode, Select, SelectColumn, Sort, Source, SourceKind, SourceTarget, Table, Update,
};

pub(crate) fn write_table(table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()> {
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

pub(crate) fn write_select(select: &Select, writer: &mut impl Write) -> io::Result<()> {
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

pub(crate) fn write_insert(insert: &Insert, writer: &mut impl Write) -> io::Result<()> {
    write!(writer, "INSERT INTO {} (", insert.table())?;
    for (index, column) in insert.columns().iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        writer.write_all(column.column().as_bytes())?;
    }
    writer.write_all(b") VALUES (")?;
    for index in 0..insert.columns().len() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        writer.write_all(b"?")?;
    }
    writer.write_all(b")")?;
    write_returning(insert.returning(), writer)?;
    Ok(())
}

pub(crate) fn write_update(update: &Update, writer: &mut impl Write) -> io::Result<()> {
    write!(
        writer,
        "UPDATE {} AS {} SET ",
        update.table(),
        update.alias()
    )?;
    for (index, column) in update.columns().iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} = ?", column.column())?;
    }
    write_filters(update.filters(), writer)?;
    write_returning(update.returning(), writer)?;
    Ok(())
}

pub(crate) fn write_delete(delete: &Delete, writer: &mut impl Write) -> io::Result<()> {
    write!(
        writer,
        "DELETE FROM {} AS {}",
        delete.table(),
        delete.alias()
    )?;
    write_filters(delete.filters(), writer)?;
    write_returning(delete.returning(), writer)?;
    Ok(())
}

fn write_returning(columns: &[SelectColumn], writer: &mut impl Write) -> io::Result<()> {
    if !columns.is_empty() {
        writer.write_all(b" RETURNING ")?;
        write_select_columns(columns, writer)?;
    }
    Ok(())
}

fn write_select_columns(columns: &[SelectColumn], writer: &mut impl Write) -> io::Result<()> {
    for (index, column) in columns.iter().enumerate() {
        if index > 0 {
            writer.write_all(b", ")?;
        }
        write!(writer, "{} AS {}", render_expr(&column.expr), column.alias)?;
    }
    Ok(())
}

fn write_sources(sources: &[Source], writer: &mut impl Write) -> io::Result<()> {
    for (index, source) in sources.iter().enumerate() {
        if index > 0 {
            writer.write_all(b" ")?;
        }
        write_source(source, index, writer)?;
    }
    Ok(())
}

fn write_source(source: &Source, position: usize, writer: &mut impl Write) -> io::Result<()> {
    match (source.kind(), source.target(), position) {
        (SourceKind::From, SourceTarget::Table(table), _) => {
            write!(writer, "FROM {table} AS {}", source.alias())
        }
        (SourceKind::From, SourceTarget::Query(query), _) => {
            writer.write_all(b"FROM (")?;
            write_select(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Query(query), 0) => {
            writer.write_all(b"FROM (")?;
            write_select(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerLateral, SourceTarget::Query(query), _) => {
            writer.write_all(b"INNER JOIN LATERAL (")?;
            write_select(query, writer)?;
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
            write_select(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::InnerJoin { on }, SourceTarget::Query(query), _) => {
            writer.write_all(b"INNER JOIN (")?;
            write_select(query, writer)?;
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
            write_select(query, writer)?;
            write!(writer, ") AS {}", source.alias())
        }
        (SourceKind::LeftJoin { on }, SourceTarget::Query(query), _) => {
            writer.write_all(b"LEFT JOIN (")?;
            write_select(query, writer)?;
            write!(
                writer,
                ") AS {} ON {}",
                source.alias(),
                render_predicate(on)
            )
        }
    }
}

fn write_filters(filters: &[squealy::Filter], writer: &mut impl Write) -> io::Result<()> {
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

fn write_orders(orders: &[Sort], writer: &mut impl Write) -> io::Result<()> {
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

pub(crate) fn select_params(select: &Select) -> Vec<BindValue> {
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

pub(crate) fn insert_params(insert: &Insert) -> Vec<BindValue> {
    let mut params = insert
        .columns()
        .iter()
        .map(|column| column.value().clone())
        .collect::<Vec<_>>();
    for column in insert.returning() {
        collect_expr_params(&column.expr, &mut params);
    }
    params
}

pub(crate) fn delete_params(delete: &Delete) -> Vec<BindValue> {
    let mut params = Vec::new();
    for filter in delete.filters() {
        collect_predicate_params(filter.predicate(), &mut params);
    }
    for column in delete.returning() {
        collect_expr_params(&column.expr, &mut params);
    }
    params
}

pub(crate) fn update_params(update: &Update) -> Vec<BindValue> {
    let mut params = update
        .columns()
        .iter()
        .map(|column| column.value().clone())
        .collect::<Vec<_>>();
    for filter in update.filters() {
        collect_predicate_params(filter.predicate(), &mut params);
    }
    for column in update.returning() {
        collect_expr_params(&column.expr, &mut params);
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
