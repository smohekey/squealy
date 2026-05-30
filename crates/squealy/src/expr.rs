use std::borrow::Cow;
use std::marker::PhantomData;
use std::ops::{Add, BitAnd, BitOr, Div, Mul, Not, Sub};

use crate::ir::{
    ArithmeticOp, BindValue, CompareOp, ExprNode, OrderDirection, OrderNode, PredicateNode,
};

/// Converts Rust values into SQL bind parameter values.
pub trait IntoBindValue {
    fn into_bind_value(self) -> BindValue;
}

macro_rules! impl_signed_bind_value {
    ($($ty:ty),* $(,)?) => {
        $(impl IntoBindValue for $ty {
            fn into_bind_value(self) -> BindValue {
                BindValue::Int(self as i128)
            }
        })*
    };
}

macro_rules! impl_unsigned_bind_value {
    ($($ty:ty),* $(,)?) => {
        $(impl IntoBindValue for $ty {
            fn into_bind_value(self) -> BindValue {
                BindValue::UInt(self as u128)
            }
        })*
    };
}

macro_rules! impl_float_bind_value {
    ($($ty:ty),* $(,)?) => {
        $(impl IntoBindValue for $ty {
            fn into_bind_value(self) -> BindValue {
                BindValue::Float(self as f64)
            }
        })*
    };
}

impl_signed_bind_value!(i8, i16, i32, i64, i128, isize);
impl_unsigned_bind_value!(u8, u16, u32, u64, u128, usize);
impl_float_bind_value!(f32, f64);

impl IntoBindValue for String {
    fn into_bind_value(self) -> BindValue {
        BindValue::Text(self)
    }
}

impl IntoBindValue for &str {
    fn into_bind_value(self) -> BindValue {
        BindValue::Text(self.to_owned())
    }
}

impl IntoBindValue for &String {
    fn into_bind_value(self) -> BindValue {
        BindValue::Text(self.clone())
    }
}

impl IntoBindValue for bool {
    fn into_bind_value(self) -> BindValue {
        BindValue::Bool(self)
    }
}

/// Marker trait for Rust types that can participate in numeric SQL operations.
///
/// Non-numeric expressions intentionally do not expose numeric operators:
///
/// ```compile_fail
/// use squealy::Expr;
///
/// let left: Expr<'static, String> = Expr::lit(String::from("Ada"));
/// let right: Expr<'static, String> = Expr::lit(String::from("Lovelace"));
///
/// let _ = left.add(right);
/// ```
pub trait SqlNumber {}

macro_rules! impl_sql_number {
    ($($ty:ty),* $(,)?) => {
        $(impl SqlNumber for $ty {})*
    };
}

impl_sql_number!(i8, i16, i32, i64, i128, isize);
impl_sql_number!(u8, u16, u32, u64, u128, usize);
impl_sql_number!(f32, f64);

/// Type-level identity for a SQL expression.
pub trait ExprKind {
    type Value;
}

macro_rules! impl_value_expr_kind {
    ($($ty:ty),* $(,)?) => {
        $(impl ExprKind for $ty {
            type Value = $ty;
        })*
    };
}

impl_value_expr_kind!(i8, i16, i32, i64, i128, isize);
impl_value_expr_kind!(u8, u16, u32, u64, u128, usize);
impl_value_expr_kind!(f32, f64);
impl_value_expr_kind!(String, bool);

/// Type-level identity for SQL addition.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AddExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for AddExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL subtraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubtractExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for SubtractExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL multiplication.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MultiplyExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for MultiplyExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// Type-level identity for SQL division.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DivideExpr<L, R> {
    _Marker(PhantomData<(L, R)>),
}

impl<L, R> ExprKind for DivideExpr<L, R>
where
    L: ExprKind,
    R: ExprKind<Value = L::Value>,
    L::Value: SqlNumber,
{
    type Value = L::Value;
}

/// A typed SQL scalar expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Expr<'scope, K> {
    node: ExprNode,
    project_alias: Cow<'static, str>,
    _phantom: PhantomData<(&'scope (), K)>,
}

/// A copyable reference to a source column scoped to a query builder invocation.
#[derive(Debug, PartialEq, Eq)]
pub struct ColumnRef<'scope, K> {
    alias: SourceAlias,
    column: &'static str,
    project_alias: &'static str,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K> Clone for ColumnRef<'scope, K> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'scope, K> Copy for ColumnRef<'scope, K> {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SourceAlias {
    depth: usize,
    index: usize,
}

impl SourceAlias {
    fn parse(alias: &str) -> Self {
        let Some(alias) = alias.strip_prefix('q') else {
            panic!("squealy source aliases must start with `q`");
        };
        let Some((depth, index)) = alias.split_once('_') else {
            panic!("squealy source aliases must contain `_`");
        };

        Self {
            depth: depth
                .parse()
                .expect("squealy source alias depth must be numeric"),
            index: index
                .parse()
                .expect("squealy source alias index must be numeric"),
        }
    }

    fn name(self) -> String {
        format!("q{}_{}", self.depth, self.index)
    }
}

impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind,
{
    #[doc(hidden)]
    pub fn column(alias: &str, column: &'static str) -> Self {
        Self::column_with_project_alias(alias, column, column)
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: &str,
        column: &'static str,
        project_alias: &'static str,
    ) -> Self {
        Self {
            alias: SourceAlias::parse(alias),
            column,
            project_alias,
            _phantom: PhantomData,
        }
    }

    /// Materialize this column reference as owned expression IR.
    pub fn into_expr(self) -> Expr<'scope, K> {
        Expr::column_with_project_alias(&self.alias.name(), self.column, self.project_alias)
    }

    /// The core-owned expression IR node for this column.
    pub fn node(self) -> ExprNode {
        self.into_expr().node
    }

    /// The default output alias when this column is selected directly.
    pub fn project_alias(self) -> &'static str {
        self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().equals(other)
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().not_equals(other)
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().less_than(other)
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().less_than_or_equals(other)
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().greater_than(other)
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr().greater_than_or_equals(other)
    }

    /// Sort by this column in ascending order.
    pub fn asc(self) -> Order<'scope> {
        self.into_expr().asc()
    }

    /// Sort by this column in descending order.
    pub fn desc(self) -> Order<'scope> {
        self.into_expr().desc()
    }
}

impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<R>(self, other: R) -> Expr<'scope, AddExpr<K, R::Kind>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr() + other
    }

    /// SQL numeric subtraction.
    pub fn subtract<R>(self, other: R) -> Expr<'scope, SubtractExpr<K, R::Kind>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr() - other
    }

    /// SQL numeric multiplication.
    pub fn multiply<R>(self, other: R) -> Expr<'scope, MultiplyExpr<K, R::Kind>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr() * other
    }

    /// SQL numeric division.
    pub fn divide<R>(self, other: R) -> Expr<'scope, DivideExpr<K, R::Kind>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        self.into_expr() / other
    }
}

impl<'scope, K> Expr<'scope, K>
where
    K: ExprKind,
{
    fn from_node(node: ExprNode, project_alias: impl Into<Cow<'static, str>>) -> Self {
        Self {
            node,
            project_alias: project_alias.into(),
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn column(alias: &str, column: &str) -> Self {
        Self::column_with_project_alias(alias, column, column.to_owned())
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: &str,
        column: &str,
        project_alias: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self::from_node(
            ExprNode::Column {
                alias: alias.to_owned(),
                column: column.to_owned(),
            },
            project_alias,
        )
    }

    /// Construct a SQL literal expression.
    pub fn lit(value: impl IntoBindValue) -> Self {
        Self::from_node(ExprNode::Literal(value.into_bind_value()), "expr")
    }

    /// The core-owned expression IR node.
    pub fn node(&self) -> &ExprNode {
        &self.node
    }

    /// The default output alias when this expression is selected directly.
    pub fn project_alias(&self) -> &str {
        &self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(&self.node, CompareOp::Equals, other.into_expr().node)
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(&self.node, CompareOp::NotEquals, other.into_expr().node)
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(&self.node, CompareOp::LessThan, other.into_expr().node)
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(
            &self.node,
            CompareOp::LessThanOrEquals,
            other.into_expr().node,
        )
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(&self.node, CompareOp::GreaterThan, other.into_expr().node)
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Predicate::compare(
            &self.node,
            CompareOp::GreaterThanOrEquals,
            other.into_expr().node,
        )
    }

    /// Sort by this expression in ascending order.
    pub fn asc(&self) -> Order<'scope> {
        Order::new(OrderNode {
            expr: self.node.clone(),
            direction: OrderDirection::Asc,
        })
    }

    /// Sort by this expression in descending order.
    pub fn desc(&self) -> Order<'scope> {
        Order::new(OrderNode {
            expr: self.node.clone(),
            direction: OrderDirection::Desc,
        })
    }
}

impl<'scope, K> Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<'other, R>(&self, other: R) -> Expr<'scope, AddExpr<K, R::Kind>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Self::binary::<AddExpr<K, R::Kind>>(&self.node, ArithmeticOp::Add, other.into_expr().node)
    }

    /// SQL numeric subtraction.
    pub fn subtract<'other, R>(&self, other: R) -> Expr<'scope, SubtractExpr<K, R::Kind>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Self::binary::<SubtractExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Subtract,
            other.into_expr().node,
        )
    }

    /// SQL numeric multiplication.
    pub fn multiply<'other, R>(&self, other: R) -> Expr<'scope, MultiplyExpr<K, R::Kind>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Self::binary::<MultiplyExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Multiply,
            other.into_expr().node,
        )
    }

    /// SQL numeric division.
    pub fn divide<'other, R>(&self, other: R) -> Expr<'scope, DivideExpr<K, R::Kind>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
    {
        Self::binary::<DivideExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Divide,
            other.into_expr().node,
        )
    }

    fn binary<ResultKind>(
        left: &ExprNode,
        op: ArithmeticOp,
        right: ExprNode,
    ) -> Expr<'scope, ResultKind>
    where
        ResultKind: ExprKind,
    {
        Expr::<ResultKind>::from_node(
            ExprNode::Binary {
                left: Box::new(left.clone()),
                op,
                right: Box::new(right),
            },
            "expr",
        )
    }
}

impl<'scope, K> Clone for Expr<'scope, K> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            project_alias: self.project_alias.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K, R> Add<R> for Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K>::binary::<AddExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Add,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Add<R> for &Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K>::binary::<AddExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Add,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Add<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>>;

    fn add(self, other: R) -> Self::Output {
        self.into_expr() + other
    }
}

impl<'scope, K, R> Add<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>>;

    fn add(self, other: R) -> Self::Output {
        (*self).into_expr() + other
    }
}

impl<'scope, K, R> Sub<R> for Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K>::binary::<SubtractExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Subtract,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Sub<R> for &Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K>::binary::<SubtractExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Subtract,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Sub<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>>;

    fn sub(self, other: R) -> Self::Output {
        self.into_expr() - other
    }
}

impl<'scope, K, R> Sub<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>>;

    fn sub(self, other: R) -> Self::Output {
        (*self).into_expr() - other
    }
}

impl<'scope, K, R> Mul<R> for Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K>::binary::<MultiplyExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Multiply,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Mul<R> for &Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K>::binary::<MultiplyExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Multiply,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Mul<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>>;

    fn mul(self, other: R) -> Self::Output {
        self.into_expr() * other
    }
}

impl<'scope, K, R> Mul<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>>;

    fn mul(self, other: R) -> Self::Output {
        (*self).into_expr() * other
    }
}

impl<'scope, K, R> Div<R> for Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K>::binary::<DivideExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Divide,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Div<R> for &Expr<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K>::binary::<DivideExpr<K, R::Kind>>(
            &self.node,
            ArithmeticOp::Divide,
            other.into_expr().node,
        )
    }
}

impl<'scope, K, R> Div<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>>;

    fn div(self, other: R) -> Self::Output {
        self.into_expr() / other
    }
}

impl<'scope, K, R> Div<R> for &ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>>;

    fn div(self, other: R) -> Self::Output {
        (*self).into_expr() / other
    }
}

macro_rules! impl_primitive_left_arithmetic {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<'scope, K> Add<Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>>;

                fn add(self, other: Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope, K> Add<&Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>>;

                fn add(self, other: &Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope, K> Add<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>>;

                fn add(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope, K> Add<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>>;

                fn add(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope, K> Sub<Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>>;

                fn sub(self, other: Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }

            impl<'scope, K> Sub<&Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>>;

                fn sub(self, other: &Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }

            impl<'scope, K> Sub<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>>;

                fn sub(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }

            impl<'scope, K> Sub<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>>;

                fn sub(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }

            impl<'scope, K> Mul<Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>>;

                fn mul(self, other: Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) * other
                }
            }

            impl<'scope, K> Mul<&Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>>;

                fn mul(self, other: &Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) * other
                }
            }

            impl<'scope, K> Mul<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>>;

                fn mul(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) * other
                }
            }

            impl<'scope, K> Mul<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>>;

                fn mul(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) * other
                }
            }

            impl<'scope, K> Div<Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>>;

                fn div(self, other: Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) / other
                }
            }

            impl<'scope, K> Div<&Expr<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>>;

                fn div(self, other: &Expr<'scope, K>) -> Self::Output {
                    Expr::lit(self) / other
                }
            }

            impl<'scope, K> Div<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>>;

                fn div(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) / other
                }
            }

            impl<'scope, K> Div<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>>;

                fn div(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::lit(self) / other
                }
            }
        )*
    };
}

impl_primitive_left_arithmetic!(i8, i16, i32, i64, i128, isize);
impl_primitive_left_arithmetic!(u8, u16, u32, u64, u128, usize);
impl_primitive_left_arithmetic!(f32, f64);

/// Converts Rust values into scoped SQL expressions.
pub trait IntoExpr<'scope> {
    type Kind: ExprKind;

    fn into_expr(self) -> Expr<'scope, Self::Kind>;
}

impl<'scope, K> IntoExpr<'scope> for Expr<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        self
    }
}

impl<'scope, K> IntoExpr<'scope> for &Expr<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        self.clone()
    }
}

impl<'scope, K> IntoExpr<'scope> for ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        self.into_expr()
    }
}

impl<'scope, K> IntoExpr<'scope> for &ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        (*self).into_expr()
    }
}

impl<'scope, T> IntoExpr<'scope> for T
where
    T: ExprKind + IntoBindValue,
{
    type Kind = T;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &str {
    type Kind = String;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &String {
    type Kind = String;

    fn into_expr(self) -> Expr<'scope, Self::Kind> {
        Expr::lit(self)
    }
}

/// A typed SQL ordering expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Order<'scope> {
    node: OrderNode,
    _phantom: PhantomData<&'scope ()>,
}

impl<'scope> Order<'scope> {
    fn new(node: OrderNode) -> Self {
        Self {
            node,
            _phantom: PhantomData,
        }
    }

    /// The core-owned ordering IR node.
    pub fn node(&self) -> &OrderNode {
        &self.node
    }
}

impl<'scope> Clone for Order<'scope> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            _phantom: PhantomData,
        }
    }
}

/// A typed SQL boolean predicate scoped to a query builder invocation.
///
/// `WHERE` clauses require predicates rather than arbitrary scalar expressions:
///
/// ```compile_fail
/// use squealy::*;
/// # use std::marker::PhantomData;
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
/// #
/// # struct DocConnection;
/// #
/// # struct DocSelect<'conn, Shape> {
/// #     select: Select,
/// #     _connection: PhantomData<&'conn DocConnection>,
/// #     _shape: PhantomData<Shape>,
/// # }
/// #
/// # impl<'conn, Shape> SelectQuery<'conn> for DocSelect<'conn, Shape>
/// # where
/// #     Shape: ProjectionShape,
/// # {
/// #     type Connection = DocConnection;
/// #     type Shape = Shape;
/// #
/// #     fn ir(&self) -> &Select {
/// #         &self.select
/// #     }
/// # }
/// #
/// # impl Connection for DocConnection {
/// #     type Error = ();
/// #
/// #     type Select<'conn, Shape> = DocSelect<'conn, Shape>
/// #     where
/// #         Self: 'conn,
/// #         Shape: ProjectionShape;
/// #
/// #     fn select<Shape>(
/// #         &self,
/// #         f: impl for<'scope> FnOnce(
/// #             &mut ::squealy::SelectBuilder<'_, 'scope, Self>,
/// #         ) -> <Shape as ProjectionShape>::Exprs<'scope>,
/// #     ) -> Self::Select<'_, Shape>
/// #     where
/// #         Shape: ProjectionShape,
/// #     {
/// #         DocSelect {
/// #             select: build_select::<Self, Shape>(f),
/// #             _connection: PhantomData,
/// #             _shape: PhantomData,
/// #         }
/// #     }
/// # }
///
/// let conn = DocConnection;
/// let _ = conn.select::<User>(|q| {
///     let user = q.from::<User>();
///     q.where_(user.id.clone());
///     user
/// });
/// ```
#[derive(Debug, PartialEq)]
pub struct Predicate<'scope> {
    node: PredicateNode,
    _phantom: PhantomData<&'scope ()>,
}

impl<'scope> Predicate<'scope> {
    fn from_node(node: PredicateNode) -> Self {
        Self {
            node,
            _phantom: PhantomData,
        }
    }

    fn compare(left: &ExprNode, op: CompareOp, right: ExprNode) -> Self {
        Self::from_node(PredicateNode::Compare {
            left: left.clone(),
            op,
            right,
        })
    }

    /// The core-owned predicate IR node.
    pub fn node(&self) -> &PredicateNode {
        &self.node
    }

    /// SQL conjunction.
    pub fn and<'other>(&self, other: Predicate<'other>) -> Self {
        Self::from_node(PredicateNode::And {
            left: Box::new(self.node.clone()),
            right: Box::new(other.node),
        })
    }

    /// SQL disjunction.
    pub fn or<'other>(&self, other: Predicate<'other>) -> Self {
        Self::from_node(PredicateNode::Or {
            left: Box::new(self.node.clone()),
            right: Box::new(other.node),
        })
    }

    /// SQL negation.
    pub fn not_(&self) -> Self {
        Self::from_node(PredicateNode::Not(Box::new(self.node.clone())))
    }
}

impl<'scope> Clone for Predicate<'scope> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope> BitAnd for Predicate<'scope> {
    type Output = Self;

    fn bitand(self, rhs: Self) -> Self::Output {
        Self::from_node(PredicateNode::And {
            left: Box::new(self.node),
            right: Box::new(rhs.node),
        })
    }
}

impl<'scope, 'rhs> BitAnd<&Predicate<'rhs>> for Predicate<'scope> {
    type Output = Self;

    fn bitand(self, rhs: &Predicate<'rhs>) -> Self::Output {
        Self::from_node(PredicateNode::And {
            left: Box::new(self.node),
            right: Box::new(rhs.node.clone()),
        })
    }
}

impl<'scope, 'lhs> BitAnd<Predicate<'scope>> for &Predicate<'lhs> {
    type Output = Predicate<'scope>;

    fn bitand(self, rhs: Predicate<'scope>) -> Self::Output {
        Predicate::from_node(PredicateNode::And {
            left: Box::new(self.node.clone()),
            right: Box::new(rhs.node),
        })
    }
}

impl<'scope> BitOr for Predicate<'scope> {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self::from_node(PredicateNode::Or {
            left: Box::new(self.node),
            right: Box::new(rhs.node),
        })
    }
}

impl<'scope, 'rhs> BitOr<&Predicate<'rhs>> for Predicate<'scope> {
    type Output = Self;

    fn bitor(self, rhs: &Predicate<'rhs>) -> Self::Output {
        Self::from_node(PredicateNode::Or {
            left: Box::new(self.node),
            right: Box::new(rhs.node.clone()),
        })
    }
}

impl<'scope, 'lhs> BitOr<Predicate<'scope>> for &Predicate<'lhs> {
    type Output = Predicate<'scope>;

    fn bitor(self, rhs: Predicate<'scope>) -> Self::Output {
        Predicate::from_node(PredicateNode::Or {
            left: Box::new(self.node.clone()),
            right: Box::new(rhs.node),
        })
    }
}

impl<'scope> Not for Predicate<'scope> {
    type Output = Self;

    fn not(self) -> Self::Output {
        Self::from_node(PredicateNode::Not(Box::new(self.node)))
    }
}

impl<'scope> Not for &Predicate<'scope> {
    type Output = Predicate<'scope>;

    fn not(self) -> Self::Output {
        Predicate::from_node(PredicateNode::Not(Box::new(self.node.clone())))
    }
}
