use squealy::*;

#[derive(Clone, Debug, PartialEq)]
struct JsonPayload;

#[derive(Clone, Debug, PartialEq, Table)]
struct Event<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, i32>,
    payload: C::Type<'scope, JsonPayload>,
}

fn main() {}
