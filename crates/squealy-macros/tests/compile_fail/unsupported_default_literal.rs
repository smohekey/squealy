use squealy::*;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, db_type = "integer")]
    id: C::Type<'scope, i32>,
    #[column(default = value(nonsense))]
    name: C::Type<'scope, String>,
}

fn main() {}
