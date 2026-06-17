use squealy::*;

// A type alias to `Option<…>` is invisible to the macro's token check, but a primary-key column must
// still be non-null: the type-level `ColumnNullability<Nullability = NonNullableColumn>` assertion
// rejects it, so `Column::nullable()` can never disagree with the declared key.
type MaybeId = Option<i32>;

#[derive(Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, MaybeId>,
}

fn main() {}
