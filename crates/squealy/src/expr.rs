use std::marker::PhantomData;
use std::ops::{Add, BitAnd, BitOr, Not, Sub};

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

/// A typed SQL scalar expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Expr<'scope, T> {
    node: ExprNode,
    _phantom: PhantomData<(&'scope (), T)>,
}

impl<'scope, T> Expr<'scope, T> {
    fn from_node(node: ExprNode) -> Self {
        Self {
            node,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn column(alias: &str, column: &str) -> Self {
        Self::from_node(ExprNode::Column {
            alias: alias.to_owned(),
            column: column.to_owned(),
        })
    }

    /// Construct a SQL literal expression.
    pub fn lit(value: impl IntoBindValue) -> Self {
        Self::from_node(ExprNode::Literal(value.into_bind_value()))
    }

    /// The core-owned expression IR node.
    pub fn node(&self) -> &ExprNode {
        &self.node
    }

    /// SQL equality.
    pub fn equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other, T>,
    {
        Predicate::compare(&self.node, CompareOp::Equals, other.into_expr().node)
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other, T>,
    {
        Predicate::compare(&self.node, CompareOp::NotEquals, other.into_expr().node)
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other, T>,
    {
        Predicate::compare(&self.node, CompareOp::LessThan, other.into_expr().node)
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other, T>,
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
        R: IntoExpr<'other, T>,
    {
        Predicate::compare(&self.node, CompareOp::GreaterThan, other.into_expr().node)
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(&self, other: R) -> Predicate<'scope>
    where
        R: IntoExpr<'other, T>,
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

impl<'scope, T> Expr<'scope, T>
where
    T: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<'other, R>(&self, other: R) -> Self
    where
        R: IntoExpr<'other, T>,
    {
        Self::binary(&self.node, ArithmeticOp::Add, other.into_expr().node)
    }

    /// SQL numeric subtraction.
    pub fn subtract<'other, R>(&self, other: R) -> Self
    where
        R: IntoExpr<'other, T>,
    {
        Self::binary(&self.node, ArithmeticOp::Subtract, other.into_expr().node)
    }

    fn binary(left: &ExprNode, op: ArithmeticOp, right: ExprNode) -> Self {
        Self::from_node(ExprNode::Binary {
            left: Box::new(left.clone()),
            op,
            right: Box::new(right),
        })
    }
}

impl<'scope, T> Clone for Expr<'scope, T> {
    fn clone(&self) -> Self {
        Self {
            node: self.node.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, T, R> Add<R> for Expr<'scope, T>
where
    T: SqlNumber,
    R: IntoExpr<'scope, T>,
{
    type Output = Self;

    fn add(self, other: R) -> Self::Output {
        Expr::binary(&self.node, ArithmeticOp::Add, other.into_expr().node)
    }
}

impl<'scope, T, R> Add<R> for &Expr<'scope, T>
where
    T: SqlNumber,
    R: IntoExpr<'scope, T>,
{
    type Output = Expr<'scope, T>;

    fn add(self, other: R) -> Self::Output {
        Expr::binary(&self.node, ArithmeticOp::Add, other.into_expr().node)
    }
}

impl<'scope, T, R> Sub<R> for Expr<'scope, T>
where
    T: SqlNumber,
    R: IntoExpr<'scope, T>,
{
    type Output = Self;

    fn sub(self, other: R) -> Self::Output {
        Expr::binary(&self.node, ArithmeticOp::Subtract, other.into_expr().node)
    }
}

impl<'scope, T, R> Sub<R> for &Expr<'scope, T>
where
    T: SqlNumber,
    R: IntoExpr<'scope, T>,
{
    type Output = Expr<'scope, T>;

    fn sub(self, other: R) -> Self::Output {
        Expr::binary(&self.node, ArithmeticOp::Subtract, other.into_expr().node)
    }
}

macro_rules! impl_primitive_left_arithmetic {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<'scope> Add<Expr<'scope, $ty>> for $ty {
                type Output = Expr<'scope, $ty>;

                fn add(self, other: Expr<'scope, $ty>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope> Add<&Expr<'scope, $ty>> for $ty {
                type Output = Expr<'scope, $ty>;

                fn add(self, other: &Expr<'scope, $ty>) -> Self::Output {
                    Expr::lit(self) + other
                }
            }

            impl<'scope> Sub<Expr<'scope, $ty>> for $ty {
                type Output = Expr<'scope, $ty>;

                fn sub(self, other: Expr<'scope, $ty>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }

            impl<'scope> Sub<&Expr<'scope, $ty>> for $ty {
                type Output = Expr<'scope, $ty>;

                fn sub(self, other: &Expr<'scope, $ty>) -> Self::Output {
                    Expr::lit(self) - other
                }
            }
        )*
    };
}

impl_primitive_left_arithmetic!(i8, i16, i32, i64, i128, isize);
impl_primitive_left_arithmetic!(u8, u16, u32, u64, u128, usize);
impl_primitive_left_arithmetic!(f32, f64);

/// Converts Rust values into scoped SQL expressions.
pub trait IntoExpr<'scope, T> {
    fn into_expr(self) -> Expr<'scope, T>;
}

impl<'scope, T> IntoExpr<'scope, T> for Expr<'scope, T> {
    fn into_expr(self) -> Expr<'scope, T> {
        self
    }
}

impl<'scope, T> IntoExpr<'scope, T> for &Expr<'scope, T> {
    fn into_expr(self) -> Expr<'scope, T> {
        self.clone()
    }
}

impl<'scope, T> IntoExpr<'scope, T> for T
where
    T: IntoBindValue,
{
    fn into_expr(self) -> Expr<'scope, T> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope, String> for &str {
    fn into_expr(self) -> Expr<'scope, String> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope, String> for &String {
    fn into_expr(self) -> Expr<'scope, String> {
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
///     let user = q.each::<User>();
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
