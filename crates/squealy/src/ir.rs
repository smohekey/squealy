use std::borrow::Cow;

/// A SQL bind parameter value.
#[derive(Clone, Debug, PartialEq)]
pub enum BindValue {
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(String),
    Bool(bool),
    Null,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ExprNode {
    Column {
        alias: String,
        column: String,
    },
    Literal(BindValue),
    Binary {
        left: Box<ExprNode>,
        op: ArithmeticOp,
        right: Box<ExprNode>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticOp {
    Add,
    Subtract,
    Multiply,
    Divide,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PredicateNode {
    Compare {
        left: ExprNode,
        op: CompareOp,
        right: ExprNode,
    },
    And {
        left: Box<PredicateNode>,
        right: Box<PredicateNode>,
    },
    Or {
        left: Box<PredicateNode>,
        right: Box<PredicateNode>,
    },
    Not(Box<PredicateNode>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompareOp {
    Equals,
    NotEquals,
    LessThan,
    LessThanOrEquals,
    GreaterThan,
    GreaterThanOrEquals,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OrderNode {
    pub expr: ExprNode,
    pub direction: OrderDirection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

/// A selected SQL expression and its output alias.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectColumn {
    pub expr: ExprNode,
    pub alias: Cow<'static, str>,
}

impl SelectColumn {
    pub fn new(expr: ExprNode, alias: impl Into<Cow<'static, str>>) -> Self {
        Self {
            expr,
            alias: alias.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Select {
    columns: Vec<SelectColumn>,
    sources: Vec<Source>,
    filters: Vec<Filter>,
    orders: Vec<Sort>,
    limit: Option<usize>,
    offset: Option<usize>,
}

impl Select {
    pub(crate) fn new(columns: Vec<SelectColumn>, sources: Vec<Source>) -> Self {
        Self {
            columns,
            sources,
            filters: Vec::new(),
            orders: Vec::new(),
            limit: None,
            offset: None,
        }
    }

    pub(crate) fn with_filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters = filters;
        self
    }

    pub(crate) fn with_orders(mut self, orders: Vec<Sort>) -> Self {
        self.orders = orders;
        self
    }

    pub(crate) fn with_limit(mut self, limit: Option<usize>) -> Self {
        self.limit = limit;
        self
    }

    pub(crate) fn with_offset(mut self, offset: Option<usize>) -> Self {
        self.offset = offset;
        self
    }

    pub fn columns(&self) -> &[SelectColumn] {
        &self.columns
    }

    pub fn sources(&self) -> &[Source] {
        &self.sources
    }

    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    pub fn orders(&self) -> &[Sort] {
        &self.orders
    }

    pub fn limit(&self) -> Option<usize> {
        self.limit
    }

    pub fn offset(&self) -> Option<usize> {
        self.offset
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Source {
    alias: String,
    kind: SourceKind,
    target: SourceTarget,
}

impl Source {
    pub(crate) fn table(alias: impl Into<String>, table: impl ToString) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::From,
            target: SourceTarget::Table(table.to_string()),
        }
    }

    pub(crate) fn lateral(alias: impl Into<String>, query: Select) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::InnerLateral,
            target: SourceTarget::Query(Box::new(query)),
        }
    }

    pub(crate) fn join(alias: impl Into<String>, table: impl ToString, on: PredicateNode) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::InnerJoin { on },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    pub(crate) fn left_join(
        alias: impl Into<String>,
        table: impl ToString,
        on: PredicateNode,
    ) -> Self {
        Self {
            alias: alias.into(),
            kind: SourceKind::LeftJoin { on },
            target: SourceTarget::Table(table.to_string()),
        }
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn kind(&self) -> &SourceKind {
        &self.kind
    }

    pub fn target(&self) -> &SourceTarget {
        &self.target
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceKind {
    From,
    InnerLateral,
    InnerJoin { on: PredicateNode },
    LeftJoin { on: PredicateNode },
}

#[derive(Clone, Debug, PartialEq)]
pub enum SourceTarget {
    Table(String),
    Query(Box<Select>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    predicate: PredicateNode,
}

impl Filter {
    pub(crate) fn new(predicate: PredicateNode) -> Self {
        Self { predicate }
    }

    pub fn predicate(&self) -> &PredicateNode {
        &self.predicate
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Sort {
    order: OrderNode,
}

impl Sort {
    pub(crate) fn new(order: OrderNode) -> Self {
        Self { order }
    }

    pub fn order(&self) -> &OrderNode {
        &self.order
    }
}
