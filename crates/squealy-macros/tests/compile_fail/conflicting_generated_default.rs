use squealy::*;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(generated, default = value("x"))]
    label: C::Type<'scope, String>,
}

fn main() {}
