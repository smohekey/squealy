use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct Dst<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Other<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    slug: C::Type<'scope, String>,
}

fn main() {
    // A target column must belong to the *destination* table. Capturing another table's column
    // (`Other::slug`) in the target-list closure is a compile error, even though it is insertable for
    // `Other` — it would render `INSERT INTO dsts (name, slug) …` against a column `dsts` doesn't have.
    let conn = TestConnection;
    let other = <Other<'static, ColumnExpr> as ProjectionShape>::exprs(SourceAlias::new(0, 0));
    let _query = conn.to::<Dst>().insert_select(
        |dst| (dst.name, other.slug),
        conn.from::<Dst>().select(|(dst,)| (dst.name, dst.name)),
    );
}
