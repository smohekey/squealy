use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Event<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    label: C::Type<'scope, Option<String>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    events: Event<'static, ColumnName>,
}

/// Accepts only a `ScalarNullable<T>`-kinded expression (an `Option<T>` result).
fn assert_scalar_nullable<'scope, T, A: ExprAst>(_: Expr<'scope, ScalarNullable<T>, A>) {}

fn main() {
    // COALESCE collapses nullability: a non-null fallback makes the result non-null (`String`, not
    // `ScalarNullable<String>`), so it does not match a nullable-result expectation.
    let _ = TestConnection.from::<Event>().select(|(event,)| {
        assert_scalar_nullable(coalesce(event.label).or_else("default").end());
        event.id
    });
}
