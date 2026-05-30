use std::borrow::Cow;

use crate::ir::SelectColumn;
use crate::{ColumnExpr, SchemaTable};

/// A projection shape that can produce scoped expression values for a SQL alias.
pub trait ProjectionShape {
    type Exprs<'scope>: Projectable;

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope>;

    fn select(exprs: &Self::Exprs<'_>) -> Vec<SelectColumn> {
        exprs.project()
    }
}

impl<L, R> ProjectionShape for (L, R)
where
    L: ProjectionShape,
    R: ProjectionShape,
    L::Exprs<'static>: Projectable,
    R::Exprs<'static>: Projectable,
{
    type Exprs<'scope> = (
        <L::Exprs<'static> as Projectable>::Rebound<'scope>,
        <R::Exprs<'static> as Projectable>::Rebound<'scope>,
    );

    fn exprs<'scope>(alias: &str) -> Self::Exprs<'scope> {
        let left = L::exprs(alias);
        let right = R::exprs(alias);

        (
            left.re_alias_with_prefix(alias, "left"),
            right.re_alias_with_prefix(alias, "right"),
        )
    }
}

impl<S> ProjectionShape for S
where
    S: SchemaTable,
    for<'scope> <S as SchemaTable>::WithColumn<'scope, ColumnExpr>: Projectable,
{
    type Exprs<'scope> = <S as SchemaTable>::WithColumn<'scope, ColumnExpr>;

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
    for<'scope> <S as SchemaTable>::WithColumn<'scope, ColumnExpr>: Projectable,
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

impl<L, R> Projectable for (L, R)
where
    L: Projectable,
    R: Projectable,
{
    type Rebound<'scope> = (L::Rebound<'scope>, R::Rebound<'scope>);

    fn project(&self) -> Vec<SelectColumn> {
        let mut columns = Vec::new();
        columns.extend(
            self.0
                .project()
                .into_iter()
                .map(|column| SelectColumn::new(column.expr, prefix_alias("left", &column.alias))),
        );
        columns.extend(
            self.1
                .project()
                .into_iter()
                .map(|column| SelectColumn::new(column.expr, prefix_alias("right", &column.alias))),
        );
        columns
    }

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope> {
        (
            self.0.re_alias_with_prefix(alias, "left"),
            self.1.re_alias_with_prefix(alias, "right"),
        )
    }

    fn re_alias_with_prefix<'scope>(&self, alias: &str, prefix: &str) -> Self::Rebound<'scope> {
        (
            self.0
                .re_alias_with_prefix(alias, &format!("{prefix}_left")),
            self.1
                .re_alias_with_prefix(alias, &format!("{prefix}_right")),
        )
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}
