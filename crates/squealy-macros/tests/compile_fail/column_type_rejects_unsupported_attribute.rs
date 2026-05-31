use squealy::*;

#[derive(ColumnType)]
#[column_type(foo = "bar")]
struct UserId(i32);

fn main() {}
