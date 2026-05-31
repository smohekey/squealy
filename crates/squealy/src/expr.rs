use std::borrow::Cow;
use std::fmt;
use std::marker::PhantomData;
use std::ops::{Add, BitAnd, BitOr, Div, Mul, Not, Sub};

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

/// A structured SQL source alias used by generated query typestates.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SourceAlias {
    depth: usize,
    index: usize,
}

impl SourceAlias {
    pub const fn new(depth: usize, index: usize) -> Self {
        Self { depth, index }
    }

    pub const fn depth(self) -> usize {
        self.depth
    }

    pub const fn index(self) -> usize {
        self.index
    }
}

impl fmt::Display for SourceAlias {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "q{}_{}", self.depth, self.index)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArithmeticOp {
    Add,
    Subtract,
    Multiply,
    Divide,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OrderDirection {
    Asc,
    Desc,
}

/// Converts Rust values into SQL bind parameter values.
pub trait IntoBindValue {
    fn into_bind_value(self) -> BindValue;
}

/// Converts Rust values, including `None`, into nullable SQL bind parameters.
pub trait IntoNullableBindValue<T> {
    fn into_nullable_bind_value(self) -> BindValue;
}

macro_rules! impl_bind_value {
    ($($ty:ty => $constructor:ident),* $(,)?) => {
        $(impl IntoBindValue for $ty {
            fn into_bind_value(self) -> BindValue {
                BindValue::$constructor(self)
            }
        })*
    };
}

impl_bind_value! {
    i8 => int8,
    i16 => int16,
    i32 => int32,
    i64 => int64,
    i128 => int128,
    isize => isize,
    u8 => uint8,
    u16 => uint16,
    u32 => uint32,
    u64 => uint64,
    u128 => uint128,
    usize => usize,
    f32 => float32,
    f64 => float64,
}

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

impl<T> IntoNullableBindValue<T> for T
where
    T: IntoBindValue,
{
    fn into_nullable_bind_value(self) -> BindValue {
        self.into_bind_value()
    }
}

impl IntoNullableBindValue<String> for &str {
    fn into_nullable_bind_value(self) -> BindValue {
        self.into_bind_value()
    }
}

impl IntoNullableBindValue<String> for &String {
    fn into_nullable_bind_value(self) -> BindValue {
        self.into_bind_value()
    }
}

impl<T> IntoNullableBindValue<T> for Option<T>
where
    T: IntoBindValue,
{
    fn into_nullable_bind_value(self) -> BindValue {
        self.map_or(BindValue::Null, IntoBindValue::into_bind_value)
    }
}

impl IntoNullableBindValue<String> for Option<&str> {
    fn into_nullable_bind_value(self) -> BindValue {
        self.map_or(BindValue::Null, IntoBindValue::into_bind_value)
    }
}

impl IntoNullableBindValue<String> for Option<&String> {
    fn into_nullable_bind_value(self) -> BindValue {
        self.map_or(BindValue::Null, IntoBindValue::into_bind_value)
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

#[doc(hidden)]
pub trait ExprAst: Clone {
    type Params: crate::HList;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor;
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct ColumnExprAst<K> {
    alias: SourceAlias,
    column: Cow<'static, str>,
    _kind: PhantomData<K>,
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct LiteralExprAst<K> {
    value: BindValue,
    _kind: PhantomData<K>,
}

#[doc(hidden)]
#[derive(Debug, PartialEq)]
pub struct ParamExprAst<K> {
    _kind: PhantomData<K>,
}

impl<K> Clone for ColumnExprAst<K> {
    fn clone(&self) -> Self {
        Self {
            alias: self.alias,
            column: self.column.clone(),
            _kind: PhantomData,
        }
    }
}

impl<K> Clone for LiteralExprAst<K> {
    fn clone(&self) -> Self {
        Self {
            value: self.value.clone(),
            _kind: PhantomData,
        }
    }
}

impl<K> Clone for ParamExprAst<K> {
    fn clone(&self) -> Self {
        Self { _kind: PhantomData }
    }
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct BinaryExprAst<Left, Right> {
    left: Left,
    op: ArithmeticOp,
    right: Right,
}

impl<K> ExprAst for ColumnExprAst<K> {
    type Params = crate::HNil;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_column(self.alias, &self.column)
    }
}

impl<K> ExprAst for LiteralExprAst<K> {
    type Params = crate::HNil;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_literal(&self.value)
    }
}

impl<K> ExprAst for ParamExprAst<K>
where
    K: ExprKind,
{
    type Params = crate::HCons<K::Value, crate::HNil>;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_param()
    }
}

impl<Left, Right> ExprAst for BinaryExprAst<Left, Right>
where
    Left: ExprAst,
    Right: ExprAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_binary(
            self.op,
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

#[doc(hidden)]
pub trait ExprVisitor {
    type Error;

    fn visit_column(&mut self, alias: SourceAlias, column: &str) -> Result<(), Self::Error>;

    fn visit_literal(&mut self, value: &BindValue) -> Result<(), Self::Error>;

    fn visit_param(&mut self) -> Result<(), Self::Error>;

    fn visit_binary<L, R>(
        &mut self,
        op: ArithmeticOp,
        left: L,
        right: R,
    ) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;
}

#[doc(hidden)]
pub trait PredicateAstVisitor: ExprVisitor {
    fn visit_compare<L, R>(&mut self, op: CompareOp, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_and<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_or<L, R>(&mut self, left: L, right: R) -> Result<(), Self::Error>
    where
        L: FnOnce(&mut Self) -> Result<(), Self::Error>,
        R: FnOnce(&mut Self) -> Result<(), Self::Error>;

    fn visit_not<P>(&mut self, predicate: P) -> Result<(), Self::Error>
    where
        P: FnOnce(&mut Self) -> Result<(), Self::Error>;
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

/// Type-level identity for a nullable SQL expression.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Nullable<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for Nullable<K>
where
    K: ExprKind,
{
    type Value = Option<K::Value>;
}

/// Type-level identity for a prepared statement runtime parameter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RuntimeParam<K> {
    _Marker(PhantomData<K>),
}

impl<K> ExprKind for RuntimeParam<K>
where
    K: ExprKind,
{
    type Value = K::Value;
}

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

/// Type-level identity for a SQL predicate.
pub trait PredicateKind {}

macro_rules! predicate_kind {
    ($($name:ident),* $(,)?) => {
        $(
            #[derive(Clone, Debug, PartialEq, Eq)]
            pub enum $name<L, R> {
                _Marker(PhantomData<(L, R)>),
            }

            impl<L, R> PredicateKind for $name<L, R> {}
        )*
    };
}

/// Type-level identity for predicates whose exact shape has been erased.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AnyPredicate {}

impl PredicateKind for AnyPredicate {}

predicate_kind!(
    EqualsPredicate,
    NotEqualsPredicate,
    LessThanPredicate,
    LessThanOrEqualsPredicate,
    GreaterThanPredicate,
    GreaterThanOrEqualsPredicate,
    AndPredicate,
    OrPredicate,
);

/// Type-level identity for SQL predicate negation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NotPredicate<P> {
    _Marker(PhantomData<P>),
}

impl<P> PredicateKind for NotPredicate<P> {}

#[doc(hidden)]
pub trait PredicateAst: Clone {
    type Params: crate::HList;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor;
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct ComparePredicateAst<Left, Right> {
    left: Left,
    op: CompareOp,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct AndPredicateAst<Left, Right> {
    left: Left,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct OrPredicateAst<Left, Right> {
    left: Left,
    right: Right,
}

#[doc(hidden)]
#[derive(Clone, Debug, PartialEq)]
pub struct NotPredicateAst<Predicate> {
    predicate: Predicate,
}

impl<Left, Right> PredicateAst for ComparePredicateAst<Left, Right>
where
    Left: ExprAst,
    Right: ExprAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
    {
        visitor.visit_compare(
            self.op,
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Left, Right> PredicateAst for AndPredicateAst<Left, Right>
where
    Left: PredicateAst,
    Right: PredicateAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
    {
        visitor.visit_and(
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Left, Right> PredicateAst for OrPredicateAst<Left, Right>
where
    Left: PredicateAst,
    Right: PredicateAst,
    Left::Params: crate::HAppend<Right::Params>,
{
    type Params = <Left::Params as crate::HAppend<Right::Params>>::Output;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
    {
        visitor.visit_or(
            |visitor| self.left.visit(visitor),
            |visitor| self.right.visit(visitor),
        )
    }
}

impl<Predicate> PredicateAst for NotPredicateAst<Predicate>
where
    Predicate: PredicateAst,
{
    type Params = Predicate::Params;

    fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
    {
        visitor.visit_not(|visitor| self.predicate.visit(visitor))
    }
}

/// A typed SQL scalar expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Expr<'scope, K, Ast = ColumnExprAst<K>>
where
    Ast: ExprAst,
{
    ast: Ast,
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

impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind,
{
    #[doc(hidden)]
    pub fn column(alias: SourceAlias, column: &'static str) -> Self {
        Self::column_with_project_alias(alias, column, column)
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: SourceAlias,
        column: &'static str,
        project_alias: &'static str,
    ) -> Self {
        Self {
            alias,
            column,
            project_alias,
            _phantom: PhantomData,
        }
    }

    /// Materialize this column reference as an owned typed expression.
    pub fn into_expr(self) -> Expr<'scope, K> {
        Expr::column_with_project_alias(self.alias, self.column, self.project_alias)
    }

    #[doc(hidden)]
    pub fn visit<V>(self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        visitor.visit_column(self.alias, self.column)
    }

    /// The default output alias when this column is selected directly.
    pub fn project_alias(self) -> &'static str {
        self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(
        self,
        other: R,
    ) -> Predicate<'scope, EqualsPredicate<K, R::Kind>, ComparePredicateAst<ColumnExprAst<K>, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().equals(other)
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(
        self,
        other: R,
    ) -> Predicate<
        'scope,
        NotEqualsPredicate<K, R::Kind>,
        ComparePredicateAst<ColumnExprAst<K>, R::Ast>,
    >
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().not_equals(other)
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(
        self,
        other: R,
    ) -> Predicate<
        'scope,
        LessThanPredicate<K, R::Kind>,
        ComparePredicateAst<ColumnExprAst<K>, R::Ast>,
    >
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().less_than(other)
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(
        self,
        other: R,
    ) -> Predicate<
        'scope,
        LessThanOrEqualsPredicate<K, R::Kind>,
        ComparePredicateAst<ColumnExprAst<K>, R::Ast>,
    >
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().less_than_or_equals(other)
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(
        self,
        other: R,
    ) -> Predicate<
        'scope,
        GreaterThanPredicate<K, R::Kind>,
        ComparePredicateAst<ColumnExprAst<K>, R::Ast>,
    >
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().greater_than(other)
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(
        self,
        other: R,
    ) -> Predicate<
        'scope,
        GreaterThanOrEqualsPredicate<K, R::Kind>,
        ComparePredicateAst<ColumnExprAst<K>, R::Ast>,
    >
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr().greater_than_or_equals(other)
    }

    /// Sort by this column in ascending order.
    pub fn asc(self) -> Order<'scope, K, ColumnExprAst<K>> {
        self.into_expr().asc()
    }

    /// Sort by this column in descending order.
    pub fn desc(self) -> Order<'scope, K, ColumnExprAst<K>> {
        self.into_expr().desc()
    }
}

impl<'scope, K> ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<R>(
        self,
        other: R,
    ) -> Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr() + other
    }

    /// SQL numeric subtraction.
    pub fn subtract<R>(
        self,
        other: R,
    ) -> Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr() - other
    }

    /// SQL numeric multiplication.
    pub fn multiply<R>(
        self,
        other: R,
    ) -> Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr() * other
    }

    /// SQL numeric division.
    pub fn divide<R>(
        self,
        other: R,
    ) -> Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        self.into_expr() / other
    }
}

impl<'scope, K> Expr<'scope, K, ColumnExprAst<K>>
where
    K: ExprKind,
{
    #[doc(hidden)]
    pub fn column(alias: SourceAlias, column: impl Into<Cow<'static, str>>) -> Self {
        let column = column.into();
        let project_alias = column.clone();
        Self {
            ast: ColumnExprAst {
                alias,
                column,
                _kind: PhantomData,
            },
            project_alias,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn column_with_project_alias(
        alias: SourceAlias,
        column: impl Into<Cow<'static, str>>,
        project_alias: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            ast: ColumnExprAst {
                alias,
                column: column.into(),
                _kind: PhantomData,
            },
            project_alias: project_alias.into(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K> Expr<'scope, K, LiteralExprAst<K>>
where
    K: ExprKind,
{
    fn lit_bind(value: BindValue) -> Self {
        Self {
            ast: LiteralExprAst {
                value,
                _kind: PhantomData,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }

    /// Construct a SQL literal expression.
    pub fn lit(value: impl IntoBindValue) -> Self {
        Self::lit_bind(value.into_bind_value())
    }
}

impl<'scope, K> Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ExprKind,
{
    /// Construct a prepared statement runtime parameter expression.
    pub fn param() -> Self {
        Self {
            ast: ParamExprAst { _kind: PhantomData },
            project_alias: Cow::Borrowed("param"),
            _phantom: PhantomData,
        }
    }
}

/// Construct a prepared statement runtime parameter expression.
pub fn param<'scope, K>() -> Expr<'scope, RuntimeParam<K>, ParamExprAst<K>>
where
    K: ExprKind,
{
    Expr::param()
}

impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    #[doc(hidden)]
    pub fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        self.ast.visit(visitor)
    }

    /// The default output alias when this expression is selected directly.
    pub fn project_alias(&self) -> &str {
        &self.project_alias
    }

    /// SQL equality.
    pub fn equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, EqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::Equals,
            right: other.into_expr().ast,
        })
    }

    /// SQL inequality.
    pub fn not_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, NotEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::NotEquals,
            right: other.into_expr().ast,
        })
    }

    /// SQL less-than comparison.
    pub fn less_than<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, LessThanPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::LessThan,
            right: other.into_expr().ast,
        })
    }

    /// SQL less-than-or-equal comparison.
    pub fn less_than_or_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, LessThanOrEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::LessThanOrEquals,
            right: other.into_expr().ast,
        })
    }

    /// SQL greater-than comparison.
    pub fn greater_than<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, GreaterThanPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::GreaterThan,
            right: other.into_expr().ast,
        })
    }

    /// SQL greater-than-or-equal comparison.
    pub fn greater_than_or_equals<'other, R>(
        &self,
        other: R,
    ) -> Predicate<'scope, GreaterThanOrEqualsPredicate<K, R::Kind>, ComparePredicateAst<Ast, R::Ast>>
    where
        R: IntoExpr<'other>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Predicate::new(ComparePredicateAst {
            left: self.ast.clone(),
            op: CompareOp::GreaterThanOrEquals,
            right: other.into_expr().ast,
        })
    }

    /// Sort by this expression in ascending order.
    pub fn asc(&self) -> Order<'scope, K, Ast> {
        Order::new(self.ast.clone(), OrderDirection::Asc)
    }

    /// Sort by this expression in descending order.
    pub fn desc(&self) -> Order<'scope, K, Ast> {
        Order::new(self.ast.clone(), OrderDirection::Desc)
    }
}

impl<'scope, K, Ast> Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
{
    /// SQL numeric addition.
    pub fn add<R>(&self, other: R) -> Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<AddExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Add, other)
    }

    /// SQL numeric subtraction.
    pub fn subtract<R>(
        &self,
        other: R,
    ) -> Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<SubtractExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Subtract, other)
    }

    /// SQL numeric multiplication.
    pub fn multiply<R>(
        &self,
        other: R,
    ) -> Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<MultiplyExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Multiply, other)
    }

    /// SQL numeric division.
    pub fn divide<R>(
        &self,
        other: R,
    ) -> Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>
    where
        R: IntoExpr<'scope>,
        R::Kind: ExprKind<Value = K::Value>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        Self::binary::<DivideExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Divide, other)
    }

    fn binary<ResultKind, R>(
        left: Ast,
        op: ArithmeticOp,
        right: R,
    ) -> Expr<'scope, ResultKind, BinaryExprAst<Ast, R::Ast>>
    where
        ResultKind: ExprKind,
        R: IntoExpr<'scope>,
        Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
    {
        let right = right.into_expr();
        Expr {
            ast: BinaryExprAst {
                left,
                op,
                right: right.ast,
            },
            project_alias: Cow::Borrowed("expr"),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K, Ast> Clone for Expr<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            project_alias: self.project_alias.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, K, Ast, R> Add<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<AddExpr<K, R::Kind>, _>(self.ast, ArithmeticOp::Add, other)
    }
}

impl<'scope, K, Ast, R> Add<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<AddExpr<K, R::Kind>, _>(self.ast.clone(), ArithmeticOp::Add, other)
    }
}

impl<'scope, K, R> Add<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

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
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, AddExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn add(self, other: R) -> Self::Output {
        (*self).into_expr() + other
    }
}

impl<'scope, K, Ast, R> Sub<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<SubtractExpr<K, R::Kind>, _>(
            self.ast,
            ArithmeticOp::Subtract,
            other,
        )
    }
}

impl<'scope, K, Ast, R> Sub<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<SubtractExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Subtract,
            other,
        )
    }
}

impl<'scope, K, R> Sub<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

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
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, SubtractExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn sub(self, other: R) -> Self::Output {
        (*self).into_expr() - other
    }
}

impl<'scope, K, Ast, R> Mul<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<MultiplyExpr<K, R::Kind>, _>(
            self.ast,
            ArithmeticOp::Multiply,
            other,
        )
    }
}

impl<'scope, K, Ast, R> Mul<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<MultiplyExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Multiply,
            other,
        )
    }
}

impl<'scope, K, R> Mul<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

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
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, MultiplyExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn mul(self, other: R) -> Self::Output {
        (*self).into_expr() * other
    }
}

impl<'scope, K, Ast, R> Div<R> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<DivideExpr<K, R::Kind>, _>(self.ast, ArithmeticOp::Divide, other)
    }
}

impl<'scope, K, Ast, R> Div<R> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    Ast::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<Ast, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        Expr::<K, Ast>::binary::<DivideExpr<K, R::Kind>, _>(
            self.ast.clone(),
            ArithmeticOp::Divide,
            other,
        )
    }
}

impl<'scope, K, R> Div<R> for ColumnRef<'scope, K>
where
    K: ExprKind,
    K::Value: SqlNumber,
    R: IntoExpr<'scope>,
    R::Kind: ExprKind<Value = K::Value>,
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

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
    <ColumnExprAst<K> as ExprAst>::Params: crate::HAppend<<R::Ast as ExprAst>::Params>,
{
    type Output = Expr<'scope, DivideExpr<K, R::Kind>, BinaryExprAst<ColumnExprAst<K>, R::Ast>>;

    fn div(self, other: R) -> Self::Output {
        (*self).into_expr() / other
    }
}

macro_rules! impl_primitive_left_arithmetic {
    ($($ty:ty),* $(,)?) => {
        $(
            impl<'scope, K, Ast> Add<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn add(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K, Ast> Add<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn add(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K> Add<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn add(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K> Add<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, AddExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn add(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) + other
                }
            }

            impl<'scope, K, Ast> Sub<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn sub(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K, Ast> Sub<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn sub(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K> Sub<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn sub(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K> Sub<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, SubtractExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn sub(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) - other
                }
            }

            impl<'scope, K, Ast> Mul<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn mul(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K, Ast> Mul<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn mul(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K> Mul<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn mul(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K> Mul<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, MultiplyExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn mul(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) * other
                }
            }

            impl<'scope, K, Ast> Div<Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn div(self, other: Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K, Ast> Div<&Expr<'scope, K, Ast>> for $ty
            where
                K: ExprKind<Value = $ty>,
                Ast: ExprAst,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<Ast::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, Ast>>;

                fn div(self, other: &Expr<'scope, K, Ast>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K> Div<ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn div(self, other: ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
                }
            }

            impl<'scope, K> Div<&ColumnRef<'scope, K>> for $ty
            where
                K: ExprKind<Value = $ty>,
                <LiteralExprAst<$ty> as ExprAst>::Params: crate::HAppend<<ColumnExprAst<K> as ExprAst>::Params>,
            {
                type Output = Expr<'scope, DivideExpr<$ty, K>, BinaryExprAst<LiteralExprAst<$ty>, ColumnExprAst<K>>>;

                fn div(self, other: &ColumnRef<'scope, K>) -> Self::Output {
                    Expr::<$ty, LiteralExprAst<$ty>>::lit(self) / other
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
    type Ast: ExprAst;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast>;
}

impl<'scope, K, Ast> IntoExpr<'scope> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Kind = K;
    type Ast = Ast;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self
    }
}

impl<'scope, K, Ast> IntoExpr<'scope> for &Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Kind = K;
    type Ast = Ast;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self.clone()
    }
}

impl<'scope, K> IntoExpr<'scope> for ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;
    type Ast = ColumnExprAst<K>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        self.into_expr()
    }
}

impl<'scope, K> IntoExpr<'scope> for &ColumnRef<'scope, K>
where
    K: ExprKind,
{
    type Kind = K;
    type Ast = ColumnExprAst<K>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        (*self).into_expr()
    }
}

impl<'scope, T> IntoExpr<'scope> for T
where
    T: ExprKind + IntoBindValue,
{
    type Kind = T;
    type Ast = LiteralExprAst<T>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &str {
    type Kind = String;
    type Ast = LiteralExprAst<String>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

impl<'scope> IntoExpr<'scope> for &String {
    type Kind = String;
    type Ast = LiteralExprAst<String>;

    fn into_expr(self) -> Expr<'scope, Self::Kind, Self::Ast> {
        Expr::lit(self)
    }
}

/// A typed SQL ordering expression scoped to a query builder invocation.
#[derive(Debug, PartialEq)]
pub struct Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    ast: Ast,
    direction: OrderDirection,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K, Ast> Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn new(ast: Ast, direction: OrderDirection) -> Self {
        Self {
            ast,
            direction,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn visit_expr<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ExprVisitor,
    {
        self.ast.visit(visitor)
    }

    #[doc(hidden)]
    pub fn direction(&self) -> OrderDirection {
        self.direction
    }
}

impl<'scope, K, Ast> Clone for Order<'scope, K, Ast>
where
    Ast: ExprAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            direction: self.direction,
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
/// use squealy_test::TestConnection;
///
/// #[derive(Clone, Table)]
/// struct User<'scope, C: ColumnMode = ColumnExpr> {
///     id: C::Type<'scope, i32>,
/// }
///
/// let conn = TestConnection;
/// let _ = conn
///     .from::<User>()
///     .where_(|(user,)| user.id)
///     .select(|(user,)| user);
/// ```
#[derive(Debug, PartialEq)]
pub struct Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    ast: Ast,
    _phantom: PhantomData<(&'scope (), K)>,
}

impl<'scope, K, Ast> Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    fn new(ast: Ast) -> Self {
        Self {
            ast,
            _phantom: PhantomData,
        }
    }

    #[doc(hidden)]
    pub fn visit<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: PredicateAstVisitor,
    {
        self.ast.visit(visitor)
    }

    /// SQL conjunction.
    pub fn and<'other, R, OtherAst>(
        &self,
        other: Predicate<'other, R, OtherAst>,
    ) -> Predicate<'scope, AndPredicate<K, R>, AndPredicateAst<Ast, OtherAst>>
    where
        R: PredicateKind,
        OtherAst: PredicateAst,
        Ast::Params: crate::HAppend<OtherAst::Params>,
    {
        Predicate::new(AndPredicateAst {
            left: self.ast.clone(),
            right: other.ast,
        })
    }

    /// SQL disjunction.
    pub fn or<'other, R, OtherAst>(
        &self,
        other: Predicate<'other, R, OtherAst>,
    ) -> Predicate<'scope, OrPredicate<K, R>, OrPredicateAst<Ast, OtherAst>>
    where
        R: PredicateKind,
        OtherAst: PredicateAst,
        Ast::Params: crate::HAppend<OtherAst::Params>,
    {
        Predicate::new(OrPredicateAst {
            left: self.ast.clone(),
            right: other.ast,
        })
    }

    /// SQL negation.
    pub fn not_(&self) -> Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>> {
        Predicate::new(NotPredicateAst {
            predicate: self.ast.clone(),
        })
    }
}

impl<'scope, K, Ast> Clone for Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    fn clone(&self) -> Self {
        Self {
            ast: self.ast.clone(),
            _phantom: PhantomData,
        }
    }
}

impl<'scope, L, LeftAst, R, RightAst> BitAnd<Predicate<'scope, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast,
            right: rhs.ast,
        })
    }
}

impl<'scope, 'rhs, L, LeftAst, R, RightAst> BitAnd<&Predicate<'rhs, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: &Predicate<'rhs, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast,
            right: rhs.ast.clone(),
        })
    }
}

impl<'scope, 'lhs, L, LeftAst, R, RightAst> BitAnd<Predicate<'scope, R, RightAst>>
    for &Predicate<'lhs, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, AndPredicate<L, R>, AndPredicateAst<LeftAst, RightAst>>;

    fn bitand(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(AndPredicateAst {
            left: self.ast.clone(),
            right: rhs.ast,
        })
    }
}

impl<'scope, L, LeftAst, R, RightAst> BitOr<Predicate<'scope, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast,
            right: rhs.ast,
        })
    }
}

impl<'scope, 'rhs, L, LeftAst, R, RightAst> BitOr<&Predicate<'rhs, R, RightAst>>
    for Predicate<'scope, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: &Predicate<'rhs, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast,
            right: rhs.ast.clone(),
        })
    }
}

impl<'scope, 'lhs, L, LeftAst, R, RightAst> BitOr<Predicate<'scope, R, RightAst>>
    for &Predicate<'lhs, L, LeftAst>
where
    L: PredicateKind,
    LeftAst: PredicateAst,
    R: PredicateKind,
    RightAst: PredicateAst,
    LeftAst::Params: crate::HAppend<RightAst::Params>,
{
    type Output = Predicate<'scope, OrPredicate<L, R>, OrPredicateAst<LeftAst, RightAst>>;

    fn bitor(self, rhs: Predicate<'scope, R, RightAst>) -> Self::Output {
        Predicate::new(OrPredicateAst {
            left: self.ast.clone(),
            right: rhs.ast,
        })
    }
}

impl<'scope, K, Ast> Not for Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    type Output = Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>>;

    fn not(self) -> Self::Output {
        Predicate::new(NotPredicateAst {
            predicate: self.ast,
        })
    }
}

impl<'scope, K, Ast> Not for &Predicate<'scope, K, Ast>
where
    K: PredicateKind,
    Ast: PredicateAst,
{
    type Output = Predicate<'scope, NotPredicate<K>, NotPredicateAst<Ast>>;

    fn not(self) -> Self::Output {
        Predicate::new(NotPredicateAst {
            predicate: self.ast.clone(),
        })
    }
}
