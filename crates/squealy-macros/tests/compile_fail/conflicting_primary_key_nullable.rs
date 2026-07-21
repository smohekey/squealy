use squealy::*;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, Option<i32>>,
}

fn main() {}
