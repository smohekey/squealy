use std::borrow::Cow;
use std::marker::PhantomData;

use crate::ir::SelectColumn;
use crate::{
    AddExpr, ColumnNullableValue, ColumnRef, ColumnValue, DivideExpr, Expr, ExprKind,
    IntoBindValue, MultiplyExpr, Nullable, ReturningProjection, SchemaTable, SubtractExpr,
};

/// A projection shape that can produce scoped expression values for a SQL alias.
pub trait ProjectionShape {
    type Exprs<'scope>: Projectable;
    type ReboundExprs<'scope>: Projectable;
    type Row: Send;

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope>;

    fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope>;

    fn project_columns(exprs: &Self::Exprs<'_>) -> Vec<SelectColumn> {
        exprs.project()
    }
}

impl ProjectionShape for () {
    type Exprs<'scope> = ();
    type ReboundExprs<'scope> = ();
    type Row = ();

    fn exprs<'scope>(_alias: &str) -> Self::Exprs<'scope> {}

    fn rebound_exprs<'scope>(_alias: &str) -> Self::ReboundExprs<'scope> {}
}

macro_rules! impl_value_projection_shape {
    ($($ty:ty),* $(,)?) => {
        $(impl ProjectionShape for $ty {
            type Exprs<'scope> = Expr<'scope, $ty>;
            type ReboundExprs<'scope> = Expr<'scope, $ty>;
            type Row = $ty;

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }

            fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_value_projection_shape!(i8, i16, i32, i64, i128, isize);
impl_value_projection_shape!(u8, u16, u32, u64, u128, usize);
impl_value_projection_shape!(f32, f64);
impl_value_projection_shape!(String, bool);

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

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }

            fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_binary_projection_shape!(AddExpr, SubtractExpr, MultiplyExpr, DivideExpr);

impl<K> ProjectionShape for Nullable<K>
where
    K: ExprKind,
    K::Value: Send,
{
    type Exprs<'scope> = ColumnRef<'scope, Nullable<K>>;
    type ReboundExprs<'scope> = Expr<'scope, Nullable<K>>;
    type Row = Option<K::Value>;

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        ColumnRef::column(alias, "expr")
    }

    fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
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

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        S::column_exprs(alias)
    }

    fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
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

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        S::nullable_column_exprs(alias)
    }

    fn rebound_exprs<'scope>(alias: &str) -> Self::ReboundExprs<'scope> {
        S::nullable_column_exprs(alias).re_alias(alias)
    }
}

/// A table-backed projection shape that can also provide its SQL source name.
pub trait TableProjection: ProjectionShape {
    fn qualified_name() -> Cow<'static, str>;
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
}

/// A table-shaped value whose expression columns can be projected or rebound to a SQL alias.
pub trait Projectable: Clone {
    type Rebound<'scope>: Projectable;

    fn project(&self) -> Vec<SelectColumn>;

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope>;

    fn re_alias_with_prefix<'scope>(&self, alias: &str, _prefix: &str) -> Self::Rebound<'scope> {
        self.re_alias(alias)
    }
}

impl Projectable for () {
    type Rebound<'scope> = ();

    fn project(&self) -> Vec<SelectColumn> {
        Vec::new()
    }

    fn re_alias<'scope>(&self, _alias: &str) -> Self::Rebound<'scope> {}
}

squealy_macros::tuple_projection_shapes!(32);

impl<'expr, K> Projectable for Expr<'expr, K>
where
    K: ExprKind,
{
    type Rebound<'scope> = Expr<'scope, K>;

    fn project(&self) -> Vec<SelectColumn> {
        vec![SelectColumn::new(
            self.node().clone(),
            self.project_alias().to_owned(),
        )]
    }

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope> {
        Expr::column(alias, self.project_alias())
    }

    fn re_alias_with_prefix<'scope>(&self, alias: &str, prefix: &str) -> Self::Rebound<'scope> {
        Expr::column_with_project_alias(
            alias,
            &prefix_alias(prefix, self.project_alias()),
            self.project_alias().to_owned(),
        )
    }
}

impl<'expr, K> Projectable for ColumnRef<'expr, K>
where
    K: ExprKind,
{
    type Rebound<'scope> = Expr<'scope, K>;

    fn project(&self) -> Vec<SelectColumn> {
        vec![SelectColumn::new(self.node(), self.project_alias())]
    }

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope> {
        Expr::column(alias, self.project_alias())
    }

    fn re_alias_with_prefix<'scope>(&self, alias: &str, prefix: &str) -> Self::Rebound<'scope> {
        Expr::column_with_project_alias(
            alias,
            &prefix_alias(prefix, self.project_alias()),
            self.project_alias().to_owned(),
        )
    }
}

impl<T> Projectable for T
where
    T: ExprKind + IntoBindValue + Clone,
{
    type Rebound<'scope> = Expr<'scope, T>;

    fn project(&self) -> Vec<SelectColumn> {
        Expr::<T>::lit(self.clone()).project()
    }

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope> {
        Expr::column(alias, "expr")
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}
