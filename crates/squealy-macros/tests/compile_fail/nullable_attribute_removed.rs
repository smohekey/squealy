use squealy::*;

// `#[column(nullable)]` was removed; nullability is declared in the column type as `Option<T>`.
#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(nullable)]
    name: C::Type<'scope, String>,
}

fn main() {}
