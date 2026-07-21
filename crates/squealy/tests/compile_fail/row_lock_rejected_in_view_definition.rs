use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
struct Job<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

fn main() {
    // A view/CTE body is built against `ModelConn`, whose `ModelBackend` does not render row locks
    // (`FOR UPDATE` is invalid in a view and disallowed in a CTE). `for_update()` is therefore gated
    // out of that context — it requires `Conn::Backend: RendersRowLock`.
    let _body = ModelConn
        .from::<Job>()
        .for_update()
        .project(|(job,)| (job.id, job.name));
}
