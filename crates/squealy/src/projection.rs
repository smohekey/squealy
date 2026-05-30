use std::borrow::Cow;

use crate::ir::SelectColumn;
use crate::{
    AddExpr, ColumnValue, DivideExpr, Expr, ExprKind, MultiplyExpr, SchemaTable, SubtractExpr,
};

/// A projection shape that can produce scoped expression values for a SQL alias.
pub trait ProjectionShape {
    type Exprs<'scope>: Projectable;
    type Row: Send;

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope>;

    fn select(exprs: &Self::Exprs<'_>) -> Vec<SelectColumn> {
        exprs.project()
    }
}

macro_rules! impl_value_projection_shape {
    ($($ty:ty),* $(,)?) => {
        $(impl ProjectionShape for $ty {
            type Exprs<'scope> = Expr<'scope, $ty>;
            type Row = $ty;

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
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
            type Row = <$ty<L, R> as ExprKind>::Value;

            fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
                Expr::column(alias, "expr")
            }
        })*
    };
}

impl_binary_projection_shape!(AddExpr, SubtractExpr, MultiplyExpr, DivideExpr);

impl<S> ProjectionShape for S
where
    S: SchemaTable,
    <S as SchemaTable>::WithColumn<'static, ColumnValue>: Send,
    for<'scope> <S as SchemaTable>::Exprs<'scope>: Projectable,
{
    type Exprs<'scope> = <S as SchemaTable>::Exprs<'scope>;
    type Row = <S as SchemaTable>::WithColumn<'static, ColumnValue>;

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        S::column_exprs(alias)
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

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}
