use squealy::*;

// A column may be nullable at most once: `Option<Option<T>>` does not satisfy `ColumnNullability`
// (its `Option<T>` impl requires a non-null inner).
#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    nested: C::Type<'scope, Option<Option<i32>>>,
}

fn main() {}
