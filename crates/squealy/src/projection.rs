use std::borrow::Cow;

/// A selected SQL expression and its output alias.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectColumn {
    pub expr: String,
    pub alias: Cow<'static, str>,
}

impl SelectColumn {
    pub fn new(expr: impl Into<String>, alias: impl Into<Cow<'static, str>>) -> Self {
        Self {
            expr: expr.into(),
            alias: alias.into(),
        }
    }
}

/// A table-shaped value whose expression columns can be projected or rebound to a SQL alias.
pub trait Projectable: Clone {
    type Rebound<'scope>: Projectable;

    fn project(&self) -> Vec<SelectColumn>;

    fn re_alias<'scope>(&self, alias: &str) -> Self::Rebound<'scope>;
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
        (self.0.re_alias(alias), self.1.re_alias(alias))
    }
}

fn prefix_alias(prefix: &str, alias: &str) -> String {
    format!("{prefix}_{alias}")
}
