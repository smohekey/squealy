use std::borrow::Cow;
use std::marker::PhantomData;

use crate::{
    AddExpr, AvgExpr, ColumnFree, ColumnNullableValue, ColumnRef, ColumnValue, CountExpr, Decode,
    DivideExpr, Expr, ExprAst, ExprKind, MaxExpr, MinExpr, MultiplyExpr, Nullable, ProjectionClass,
    ProjectionColumns, ReturningProjection, ScalarNullable, ScalarProjection, SchemaTable,
    SourceAlias, SubtractExpr, SumExpr,
};

/// A projection shape that can produce scoped expression values for a SQL alias.
pub trait ProjectionShape {
    type Exprs<'scope>: Projectable;
    type ReboundExprs<'scope>;
    type Row: Send;

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope>;

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope>;
}

impl ProjectionShape for () {
    type Exprs<'scope> = ();
    type ReboundExprs<'scope> = ();
    type Row = ();

    fn exprs<'scope>(_alias: SourceAlias) -> Self::Exprs<'scope> {}

    fn rebound_exprs<'scope>(_alias: SourceAlias) -> Self::ReboundExprs<'scope> {}
}

macro_rules! impl_value_projection_shape {
    ($($ty:ty),* $(,)?) => {
        $(impl ProjectionShape for $ty {
            type Exprs<'scope> = Expr<'scope, $ty>;
            type ReboundExprs<'scope> = Expr<'scope, $ty>;
            type Row = $ty;

            fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }

            fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_value_projection_shape!(i8, i16, i32, i64, i128, isize);
impl_value_projection_shape!(u8, u16, u32, u64, u128, usize);
impl_value_projection_shape!(f32, f64);
impl_value_projection_shape!(String, bool);

// Fixed-size byte arrays are projectable (e.g. a single-column `select(|(row,)| (row.key,))`), the
// const-generic mirror of the macro above.
impl<const N: usize> ProjectionShape for [u8; N] {
    type Exprs<'scope> = Expr<'scope, [u8; N]>;
    type ReboundExprs<'scope> = Expr<'scope, [u8; N]>;
    type Row = [u8; N];

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        Expr::column(alias, "expr")
    }

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
        Expr::column(alias, "expr")
    }
}

// A `bytes::Bytes` column is projectable (e.g. a single-column `select(|(row,)| (row.blob,))`),
// mirroring the `Vec<u8>`/`[u8; N]` binary value kinds, behind the opt-in `bytes` feature.
#[cfg(feature = "bytes")]
impl_value_projection_shape!(bytes::Bytes);

// Timestamp value kinds — so a bare timestamp expression (`now()`, `date_trunc(...)`) is projectable.
// (Timestamp *columns* are projected via the table derive; these cover the value-as-kind path.)
#[cfg(feature = "systemtime")]
impl_value_projection_shape!(std::time::SystemTime);
#[cfg(feature = "time")]
impl_value_projection_shape!(time::OffsetDateTime);
#[cfg(feature = "chrono")]
impl_value_projection_shape!(chrono::DateTime<chrono::Utc>);

macro_rules! impl_binary_projection_shape {
    ($($ty:ident),* $(,)?) => {
        $(impl<L, R> ProjectionShape for $ty<L, R>
        where
            $ty<L, R>: ExprKind,
            <$ty<L, R> as ExprKind>::Value: Send,
        {
            type Exprs<'scope> = Expr<'scope, $ty<L, R>>;
            type ReboundExprs<'scope> = Expr<'scope, $ty<L, R>>;
            type Row = <$ty<L, R> as ExprKind>::Value;

            fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }

            fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_binary_projection_shape!(AddExpr, SubtractExpr, MultiplyExpr, DivideExpr);

macro_rules! impl_aggregate_projection_shape {
    ($($ty:ident),* $(,)?) => {
        $(impl<K> ProjectionShape for $ty<K>
        where
            $ty<K>: ExprKind,
            <$ty<K> as ExprKind>::Value: Send,
        {
            type Exprs<'scope> = Expr<'scope, $ty<K>>;
            type ReboundExprs<'scope> = Expr<'scope, $ty<K>>;
            type Row = <$ty<K> as ExprKind>::Value;

            fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }

            fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_aggregate_projection_shape!(CountExpr, SumExpr, AvgExpr, MinExpr, MaxExpr);

impl<K> ProjectionShape for Nullable<K>
where
    K: ExprKind,
    K::Value: Send,
{
    type Exprs<'scope> = ColumnRef<'scope, Nullable<K>>;
    type ReboundExprs<'scope> = Expr<'scope, Nullable<K>>;
    type Row = Option<K::Value>;

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        ColumnRef::column(alias, "expr")
    }

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
        Expr::column(alias, "expr")
    }
}

// A scalar subquery decodes as `Option<K::Value>` (it may be NULL when zero rows match), while its
// value type stays `K::Value` for comparison — see [`ScalarNullable`](crate::ScalarNullable).
impl<K> ProjectionShape for ScalarNullable<K>
where
    K: ExprKind,
    K::Value: Send,
{
    type Exprs<'scope> = ColumnRef<'scope, ScalarNullable<K>>;
    type ReboundExprs<'scope> = Expr<'scope, ScalarNullable<K>>;
    type Row = Option<K::Value>;

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        ColumnRef::column(alias, "expr")
    }

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
        Expr::column(alias, "expr")
    }
}

impl<S> ProjectionShape for S
where
    S: SchemaTable,
    <S as SchemaTable>::WithColumn<'static, ColumnValue>: Send,
    for<'scope> <S as SchemaTable>::Exprs<'scope>: Projectable,
{
    type Exprs<'scope> = <S as SchemaTable>::Exprs<'scope>;
    type ReboundExprs<'scope> =
        <<S as SchemaTable>::Exprs<'static> as Projectable>::Rebound<'scope>;
    type Row = <S as SchemaTable>::WithColumn<'static, ColumnValue>;

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        S::column_exprs(alias)
    }

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
        S::column_exprs(alias).re_alias(alias)
    }
}

/// A nullable projection shape, typically produced by a SQL `LEFT JOIN`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Maybe<S> {
    _Marker(PhantomData<S>),
}

impl<S> ProjectionShape for Maybe<S>
where
    S: SchemaTable,
    <S as SchemaTable>::WithColumn<'static, ColumnNullableValue>: Send,
    for<'scope> <S as SchemaTable>::NullableExprs<'scope>: Projectable,
{
    type Exprs<'scope> = <S as SchemaTable>::NullableExprs<'scope>;
    type ReboundExprs<'scope> =
        <<S as SchemaTable>::NullableExprs<'static> as Projectable>::Rebound<'scope>;
    type Row = <S as SchemaTable>::WithColumn<'static, ColumnNullableValue>;

    fn exprs<'scope>(alias: SourceAlias) -> Self::Exprs<'scope> {
        S::nullable_column_exprs(alias)
    }

    fn rebound_exprs<'scope>(alias: SourceAlias) -> Self::ReboundExprs<'scope> {
        S::nullable_column_exprs(alias).re_alias(alias)
    }
}

/// A table-backed projection shape that can also provide its SQL source name.
pub trait TableProjection: ProjectionShape {
    fn qualified_name() -> Cow<'static, str>;

    /// Returns the containing schema namespace for this table, if one is configured.
    fn schema_name() -> Option<&'static str>;

    /// Returns the unqualified table name for this table.
    fn name() -> &'static str;
}

impl<S> TableProjection for S
where
    S: SchemaTable,
    <S as SchemaTable>::WithColumn<'static, ColumnValue>: Send,
    for<'scope> <S as SchemaTable>::Exprs<'scope>: Projectable,
{
    fn qualified_name() -> Cow<'static, str> {
        <S as SchemaTable>::qualified_name()
    }

    fn schema_name() -> Option<&'static str> {
        <S as SchemaTable>::schema_name()
    }

    fn name() -> &'static str {
        <S as SchemaTable>::name()
    }
}

/// A table-shaped value whose expression columns can be projected or rebound to a SQL alias.
pub trait Projectable: Clone {
    type Rebound<'scope>;

    fn re_alias<'scope>(&self, alias: SourceAlias) -> Self::Rebound<'scope>;

    fn re_alias_with_prefix<'scope>(
        &self,
        alias: SourceAlias,
        _prefix: &str,
    ) -> Self::Rebound<'scope> {
        self.re_alias(alias)
    }
}

/// Backend-parameterized projection rendering (mirror of [`RenderAst`]).
#[doc(hidden)]
pub trait RenderProjectable<B>: Projectable
where
    B: crate::Backend,
{
    fn visit_projection<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>;

    fn visit_projection_with_prefix<V>(&self, prefix: &str, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        _ = prefix;
        self.visit_projection(visitor)
    }
}

#[doc(hidden)]
pub trait ProjectionVisitor {
    type Error;
    type Backend: crate::Backend;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: crate::RenderAst<Self::Backend>;

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind;
}

impl Projectable for () {
    type Rebound<'scope> = ();

    fn re_alias<'scope>(&self, _alias: SourceAlias) -> Self::Rebound<'scope> {}
}

impl ProjectionClass for () {
    type Class = ScalarProjection;
}

// A bare value literal projects as a scalar. (Mirrors the `Projectable for T` blanket below.)
impl<T> ProjectionClass for T
where
    T: ExprKind<Value = T>,
{
    type Class = ScalarProjection;
}

impl ProjectionColumns for () {
    type Columns = ColumnFree;
}

// A bare value literal is a constant, so it is column-free. (Mirrors the `ProjectionClass` blanket.)
impl<T> ProjectionColumns for T
where
    T: ExprKind<Value = T>,
{
    type Columns = ColumnFree;
}

impl crate::ReturnableProjection for () {}

// A bare value literal contains no window function.
impl<T> crate::ReturnableProjection for T where T: ExprKind<Value = T> {}

impl<B> RenderProjectable<B> for ()
where
    B: crate::Backend,
{
    fn visit_projection<V>(&self, _visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        Ok(())
    }
}

squealy_macros::tuple_projection_shapes!(32);

impl<'expr, K, Ast> Projectable for Expr<'expr, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
    type Rebound<'scope> = Expr<'scope, K>;

    fn re_alias<'scope>(&self, alias: SourceAlias) -> Self::Rebound<'scope> {
        Expr::column(alias, self.project_alias().to_owned())
    }

    fn re_alias_with_prefix<'scope>(
        &self,
        alias: SourceAlias,
        prefix: &str,
    ) -> Self::Rebound<'scope> {
        Expr::column_with_project_alias(
            alias,
            prefix_alias(prefix, self.project_alias()),
            self.project_alias().to_owned(),
        )
    }
}

impl<'expr, K, Ast, B> RenderProjectable<B> for Expr<'expr, K, Ast>
where
    K: ExprKind,
    Ast: crate::RenderAst<B>,
    B: crate::Backend,
{
    fn visit_projection<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        visitor.visit_expr(self, Cow::Owned(self.project_alias().to_owned()))
    }

    fn visit_projection_with_prefix<V>(&self, prefix: &str, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        visitor.visit_expr(self, Cow::Owned(prefix_alias(prefix, self.project_alias())))
    }
}

impl<'expr, K> Projectable for ColumnRef<'expr, K>
where
    K: ExprKind,
{
    type Rebound<'scope> = Expr<'scope, K>;

    fn re_alias<'scope>(&self, alias: SourceAlias) -> Self::Rebound<'scope> {
        Expr::column(alias, self.project_alias())
    }

    fn re_alias_with_prefix<'scope>(
        &self,
        alias: SourceAlias,
        prefix: &str,
    ) -> Self::Rebound<'scope> {
        Expr::column_with_project_alias(
            alias,
            prefix_alias(prefix, self.project_alias()),
            self.project_alias().to_owned(),
        )
    }
}

impl<'expr, K, B> RenderProjectable<B> for ColumnRef<'expr, K>
where
    K: ExprKind,
    B: crate::Backend,
{
    fn visit_projection<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        visitor.visit_column(*self, Cow::Borrowed(self.project_alias()))
    }

    fn visit_projection_with_prefix<V>(&self, prefix: &str, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        visitor.visit_column(
            *self,
            Cow::Owned(prefix_alias(prefix, self.project_alias())),
        )
    }
}

impl<T> Projectable for T
where
    T: ExprKind<Value = T> + Clone,
{
    type Rebound<'scope> = Expr<'scope, T>;

    fn re_alias<'scope>(&self, alias: SourceAlias) -> Self::Rebound<'scope> {
        Expr::column(alias, "expr")
    }
}

impl<T, B> RenderProjectable<B> for T
where
    T: ExprKind<Value = T> + Clone + crate::Encode<B>,
    B: crate::Backend,
{
    fn visit_projection<V>(&self, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        RenderProjectable::<B>::visit_projection(
            &Expr::<T, crate::LiteralExprAst<T>>::lit(self.clone()),
            visitor,
        )
    }

    fn visit_projection_with_prefix<V>(&self, prefix: &str, visitor: &mut V) -> Result<(), V::Error>
    where
        V: ProjectionVisitor<Backend = B>,
    {
        RenderProjectable::<B>::visit_projection_with_prefix(
            &Expr::<T, crate::LiteralExprAst<T>>::lit(self.clone()),
            prefix,
            visitor,
        )
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}
