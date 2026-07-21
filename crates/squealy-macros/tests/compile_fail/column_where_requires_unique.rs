use squealy::*;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(where = |row| row.deleted_at.is_null())]
    slug: C::Type<'scope, String>,
    deleted_at: C::Type<'scope, Option<i64>>,
}

fn main() {}
