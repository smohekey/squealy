use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct GeneratedRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(generated)]
    slug: C::Type<'scope, String>,
    title: C::Type<'scope, String>,
}

fn main() {
    let _update = TestConnection
        .to_columns::<GeneratedRecord, (GeneratedRecordSlug,)>()
        .set(|_| ("not-updateable",))
        .all()
        .update();
}
