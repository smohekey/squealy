use squealy::*;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    name: String,
}

fn main() {}
