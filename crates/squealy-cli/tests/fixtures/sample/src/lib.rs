use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
pub struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    name: C::Type<'scope, String>,
}

#[allow(dead_code)]
#[derive(Schema)]
pub struct Public {
    widgets: Widget<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
pub struct SampleDb {
    public: Public,
}
