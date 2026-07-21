use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
pub struct PublicUser<'scope, C: ColumnMode = ColumnExpr> {
    pub id: C::Type<'scope, i32>,
    pub name: C::Type<'scope, String>,
}

fn main() {}
