# squealy

<!-- cargo-rdme start -->

SQL ORM for Rust.

### Why Squealy?

Squealy is a typed query builder and schema metadata layer for Rust applications that want SQL
without treating SQL as unstructured strings. Table derives turn your Rust row types into typed
column expressions, row decoding shapes, DDL metadata, and mutation builders. Query methods then
compose those generated types into a type-level query AST: sources, joins, filters, projections,
ordering, mutation assignments, and runtime parameter shapes are all carried by Rust types.

The core crate deliberately stops at describing queries and schema. Backend crates, such as a
PostgreSQL backend, own SQL rendering, bind handling, preparation, execution, streaming rows,
and transaction behavior. That split lets each backend decide how to turn the typed AST into the
best SQL for that database, while the shared builder API keeps user code backend-shaped rather
than string-shaped.

Runtime values are explicit. Literal values can be captured directly in a concrete query, while
[`param`] creates typed runtime parameters that must be prepared before execution. Streaming is
the default result model through `fetch`; allocating helpers such as `collect`, `to_sql`, and
`collect_params` are convenience APIs for callers that choose them.

### Model your database with derives

Start by deriving [`Table`] for each row type. Table structs currently use this shape:

- a lifetime named `'scope`
- a column mode parameter `C: ColumnMode = ColumnExpr`
- fields typed as `C::Type<'scope, Value>`

```rust
use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,

    #[column(index, default = value("anonymous"))]
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,

    #[column(index, references(User::id, on_delete = "cascade"))]
    user_id: C::Type<'scope, i32>,

    title: C::Type<'scope, String>,
}

#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
    posts: Post<'static, ColumnName>,
}

#[derive(Database)]
struct AppDatabase {
    public: Public,
}
```

The derive generates table metadata, typed expression projections, row decoding shapes, and a
write builder for `conn.to::<Table>()`. It also generates a marker type for each column by
combining the table and field names: `User::id` becomes `UserId`, `Post::title` becomes
`PostTitle`, and so on. Those marker types are useful when declaring runtime parameters with
[`param`].

Common column attributes include:

- `primary_key`, `auto_increment`, `index`, and `unique`
- `nullable`
- `generated`, `insert = false`, and `update = false`
- `default = value(...)`, `default = current_timestamp`, `default = current_date`,
  `default = current_time`, and `default_raw = "..."`
- `check = "..."`, plus `db_type = "..."` as a raw backend-specific DDL type override
- `references(OtherTable::column, on_delete = "...", on_update = "...")`

If `db_type` is omitted, the field's Rust value type must implement [`HasColumnType`].
Primitive Rust types already do, and backend crates map those logical types to appropriate
database DDL. For example, the PostgreSQL backend renders `i32` as `integer` and `String` as
`text`. Use `db_type` only when you need an explicit backend-specific escape hatch such as
`varchar(64)`, `jsonb`, or a domain type. If a custom field type does not implement
[`HasColumnType`] and does not provide `db_type`, the table derive fails to compile.

For newtype wrappers, derive `ColumnType` on the wrapper. Single-field tuple structs and
single-field named structs are transparent by default, so the wrapper uses the same database
type, bind conversion, row decoding, and literal expression support as its inner value. Use
`#[column_type(db_type = "...")]` when the wrapper should still bind/decode transparently but use
a backend-specific database type:

```rust
use squealy::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
pub struct UserId(i32);

#[derive(Clone, Debug, PartialEq, ColumnType)]
#[column_type(db_type = "jsonb")]
pub struct JsonPayload(String);

#[derive(Clone, Debug, PartialEq, Table)]
struct Event<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, UserId>,
    payload: C::Type<'scope, JsonPayload>,
}
```

`#[schema(Type)]` attaches a table to a schema namespace. `#[derive(Schema)]` lists the tables
in that namespace, and `#[derive(Database)]` lists schemas for DDL/backends that want database
metadata.

### Stream rows from select queries

Select queries start from a source table and finish with `select`:

```rust
let query = conn
    .from::<User>()
    .where_(|user| user.name.equals("Ada"))
    .join::<Post>()
    .on(|(user,), post| post.user_id.equals(user.id))
    .order_by(|(_user, post)| post.id.desc())
    .select(|(user, post)| (user.id, post.title));

let mut rows = pin!(query.fetch());
while let Some(row) = poll_fn(|cx| rows.as_mut().poll_next(cx)).await {
    let (user_id, title) = row?;
    // Process each row as it arrives instead of collecting every row first.
}
```

For smaller result sets where allocation is acceptable, use `collect()`:

```rust
let rows = conn
    .from::<User>()
    .where_(|user| user.name.equals("Ada"))
    .select(|(user,)| (user.id, user.name))
    .collect()
    .await?;
```

### Write data with type-checked mutations

Mutations use the same source/destination vocabulary: `to` for insert and update, `from` for
delete. Returning mutations use explicit verb names so the final action stays clear.

```rust
conn.to::<User>().name("Ada").insert().await?;

let created = conn
    .to::<User>()
    .name("Ada")
    .insert_returning(|user| user.id)
    .fetch_one()
    .await?;

conn.to::<User>()
    .name("Grace")
    .where_(|user| user.name.equals("Ada"))
    .update()
    .await?;

conn.from::<User>()
    .where_(|user| user.id.equals(1))
    .delete()
    .await?;
```

### Prepare runtime-parameterized queries

Runtime parameters make a query preparable instead of directly executable. Prepared statements
keep SQL generation inside the backend and accept typed values at execution time.

```rust
let query = conn
    .from::<User>()
    .where_(|user| user.name.equals(squealy::param::<UserName>()))
    .select(|(user,)| user.id);
let by_name = query.prepare().await?;

let ids = by_name.collect(("Ada",)).await?;
```

Streaming methods such as `fetch` avoid collecting rows up front. Convenience methods like
`collect`, `to_sql`, and `collect_params` allocate by design.

<!-- cargo-rdme end -->
