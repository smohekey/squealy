use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
struct RequiredRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    title: C::Type<'scope, String>,
}

fn main() {
    let _insert = TestConnection
        .to_columns::<RequiredRecord, (RequiredRecordTitle,)>()
        .row((None::<String>,))
        .insert();
}
