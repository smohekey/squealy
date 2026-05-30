use std::borrow::Cow;

/// A SQL bind parameter value.
#[derive(Clone, Debug)]
pub struct BindValue {
    kind: BindValueKind,
}

#[derive(Clone, Debug, PartialEq)]
pub enum BindValueKind {
    Int { value: i128, width: IntWidth },
    UInt { value: u128, width: UIntWidth },
    Float { value: f64, width: FloatWidth },
    Text(String),
    Bool(bool),
    Null,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntWidth {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UIntWidth {
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FloatWidth {
    F32,
    F64,
}

impl BindValue {
    #[allow(non_snake_case)]
    pub const fn Int(value: i128) -> Self {
        Self::int128(value)
    }

    #[allow(non_snake_case)]
    pub const fn UInt(value: u128) -> Self {
        Self::uint128(value)
    }

    #[allow(non_snake_case)]
    pub const fn Float(value: f64) -> Self {
        Self::float64(value)
    }

    #[allow(non_snake_case)]
    pub fn Text(value: String) -> Self {
        Self::text(value)
    }

    #[allow(non_snake_case)]
    pub const fn Bool(value: bool) -> Self {
        Self::bool(value)
    }

    #[allow(non_upper_case_globals)]
    pub const Null: Self = Self {
        kind: BindValueKind::Null,
    };

    pub const fn int8(value: i8) -> Self {
        Self::int(value as i128, IntWidth::I8)
    }

    pub const fn int16(value: i16) -> Self {
        Self::int(value as i128, IntWidth::I16)
    }

    pub const fn int32(value: i32) -> Self {
        Self::int(value as i128, IntWidth::I32)
    }

    pub const fn int64(value: i64) -> Self {
        Self::int(value as i128, IntWidth::I64)
    }

    pub const fn int128(value: i128) -> Self {
        Self::int(value, IntWidth::I128)
    }

    pub const fn isize(value: isize) -> Self {
        Self::int(value as i128, IntWidth::Isize)
    }

    pub const fn uint8(value: u8) -> Self {
        Self::uint(value as u128, UIntWidth::U8)
    }

    pub const fn uint16(value: u16) -> Self {
        Self::uint(value as u128, UIntWidth::U16)
    }

    pub const fn uint32(value: u32) -> Self {
        Self::uint(value as u128, UIntWidth::U32)
    }

    pub const fn uint64(value: u64) -> Self {
        Self::uint(value as u128, UIntWidth::U64)
    }

    pub const fn uint128(value: u128) -> Self {
        Self::uint(value, UIntWidth::U128)
    }

    pub const fn usize(value: usize) -> Self {
        Self::uint(value as u128, UIntWidth::Usize)
    }

    pub const fn float32(value: f32) -> Self {
        Self::float(value as f64, FloatWidth::F32)
    }

    pub const fn float64(value: f64) -> Self {
        Self::float(value, FloatWidth::F64)
    }

    pub fn text(value: impl Into<String>) -> Self {
        Self {
            kind: BindValueKind::Text(value.into()),
        }
    }

    pub const fn bool(value: bool) -> Self {
        Self {
            kind: BindValueKind::Bool(value),
        }
    }

    pub const fn kind(&self) -> &BindValueKind {
        &self.kind
    }

    pub fn into_kind(self) -> BindValueKind {
        self.kind
    }

    const fn int(value: i128, width: IntWidth) -> Self {
        Self {
            kind: BindValueKind::Int { value, width },
        }
    }

    const fn uint(value: u128, width: UIntWidth) -> Self {
        Self {
            kind: BindValueKind::UInt { value, width },
        }
    }

    const fn float(value: f64, width: FloatWidth) -> Self {
        Self {
            kind: BindValueKind::Float { value, width },
        }
    }
}

impl PartialEq for BindValue {
    fn eq(&self, other: &Self) -> bool {
        match (&self.kind, &other.kind) {
            (
                BindValueKind::Int {
                    value: left_value, ..
                },
                BindValueKind::Int {
                    value: right_value, ..
                },
            ) => left_value == right_value,
            (
                BindValueKind::UInt {
                    value: left_value, ..
                },
                BindValueKind::UInt {
                    value: right_value, ..
                },
            ) => left_value == right_value,
            (
                BindValueKind::Float {
                    value: left_value, ..
                },
                BindValueKind::Float {
                    value: right_value, ..
                },
            ) => left_value == right_value,
            (BindValueKind::Text(left), BindValueKind::Text(right)) => left == right,
            (BindValueKind::Bool(left), BindValueKind::Bool(right)) => left == right,
            (BindValueKind::Null, BindValueKind::Null) => true,
            _ => false,
        }
    }
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
pub struct InsertColumn {
    column: Cow<'static, str>,
    value: BindValue,
}

impl InsertColumn {
    pub fn new(column: impl Into<Cow<'static, str>>, value: BindValue) -> Self {
        Self {
            column: column.into(),
            value,
        }
    }

    pub fn column(&self) -> &str {
        &self.column
    }

    pub fn value(&self) -> &BindValue {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Insert {
    table: String,
    columns: Vec<InsertColumn>,
    returning: Vec<SelectColumn>,
}

impl Insert {
    pub(crate) fn new(
        table: impl ToString,
        columns: Vec<InsertColumn>,
        returning: Vec<SelectColumn>,
    ) -> Self {
        Self {
            table: table.to_string(),
            columns,
            returning,
        }
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn columns(&self) -> &[InsertColumn] {
        &self.columns
    }

    pub fn returning(&self) -> &[SelectColumn] {
        &self.returning
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct UpdateColumn {
    column: Cow<'static, str>,
    value: BindValue,
}

impl UpdateColumn {
    pub fn new(column: impl Into<Cow<'static, str>>, value: BindValue) -> Self {
        Self {
            column: column.into(),
            value,
        }
    }

    pub fn column(&self) -> &str {
        &self.column
    }

    pub fn value(&self) -> &BindValue {
        &self.value
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Update {
    table: String,
    alias: String,
    columns: Vec<UpdateColumn>,
    filters: Vec<Filter>,
    returning: Vec<SelectColumn>,
}

impl Update {
    pub(crate) fn new(
        table: impl ToString,
        alias: impl Into<String>,
        columns: Vec<UpdateColumn>,
        filters: Vec<Filter>,
        returning: Vec<SelectColumn>,
    ) -> Self {
        Self {
            table: table.to_string(),
            alias: alias.into(),
            columns,
            filters,
            returning,
        }
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn columns(&self) -> &[UpdateColumn] {
        &self.columns
    }

    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    pub fn returning(&self) -> &[SelectColumn] {
        &self.returning
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Delete {
    table: String,
    alias: String,
    filters: Vec<Filter>,
    returning: Vec<SelectColumn>,
}

impl Delete {
    pub(crate) fn new(
        table: impl ToString,
        alias: impl Into<String>,
        returning: Vec<SelectColumn>,
    ) -> Self {
        Self {
            table: table.to_string(),
            alias: alias.into(),
            filters: Vec::new(),
            returning,
        }
    }

    pub(crate) fn with_filters(mut self, filters: Vec<Filter>) -> Self {
        self.filters = filters;
        self
    }

    pub fn table(&self) -> &str {
        &self.table
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn filters(&self) -> &[Filter] {
        &self.filters
    }

    pub fn returning(&self) -> &[SelectColumn] {
        &self.returning
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
    pub fn new(predicate: PredicateNode) -> Self {
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
