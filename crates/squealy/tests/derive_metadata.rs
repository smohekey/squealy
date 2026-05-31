use squealy::*;
use squealy_test::{TestBackend, TestConnection, TestDelete, TestInsert, TestSelect, TestUpdate};

#[derive(Debug, PartialEq, Eq)]
enum LoweringEvent {
    Projection { shape: &'static str },
    Table { table: String, alias: SourceAlias },
    InnerJoin { table: String, alias: SourceAlias },
    LeftJoin { table: String, alias: SourceAlias },
    Filter,
    Order,
    Limit(usize),
    Offset(usize),
}

#[derive(Default)]
struct RecordingSelectSink {
    events: Vec<LoweringEvent>,
}

#[derive(Debug, PartialEq)]
struct RecordingBindSink {
    values: Vec<BindValue>,
    reserved: usize,
}

impl BindSink for RecordingBindSink {
    type Error = std::convert::Infallible;

    fn reserve_bind_values(&mut self, additional: usize) {
        self.reserved += additional;
    }

    fn push_bind_value(&mut self, value: BindValue) -> Result<(), Self::Error> {
        self.values.push(value);
        Ok(())
    }
}

impl SelectSink for RecordingSelectSink {
    type Error = std::convert::Infallible;

    fn push_projection<Shape, P>(&mut self, projection: P) -> Result<(), Self::Error>
    where
        Shape: ProjectionShape,
        P: Projectable,
    {
        _ = projection;
        _ = std::marker::PhantomData::<Shape>;
        self.events.push(LoweringEvent::Projection {
            shape: std::any::type_name::<Shape>(),
        });
        Ok(())
    }

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection,
    {
        self.events.push(LoweringEvent::Table {
            table: S::qualified_name().into_owned(),
            alias,
        });
        Ok(())
    }

    fn push_inner_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        _on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: PredicateAst,
    {
        self.events.push(LoweringEvent::InnerJoin {
            table: S::qualified_name().into_owned(),
            alias,
        });
        Ok(())
    }

    fn push_left_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        _on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: PredicateAst,
    {
        self.events.push(LoweringEvent::LeftJoin {
            table: S::qualified_name().into_owned(),
            alias,
        });
        Ok(())
    }

    fn push_filter<P, Ast>(&mut self, _predicate: Predicate<'_, P, Ast>) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        Ast: PredicateAst,
    {
        self.events.push(LoweringEvent::Filter);
        Ok(())
    }

    fn push_order<K, Ast>(&mut self, _order: Order<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: ExprAst,
    {
        self.events.push(LoweringEvent::Order);
        Ok(())
    }

    fn set_limit(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.events.push(LoweringEvent::Limit(rows));
        Ok(())
    }

    fn set_offset(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.events.push(LoweringEvent::Offset(rows));
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
#[index(name = "users_name_id_idx", columns = [name, id], unique)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, index)]
    id: C::Type<'scope, i32>,
    #[column(index, nullable, default = value("anonymous"))]
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct Post<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key)]
    id: C::Type<'scope, i32>,
    #[column(index, references(User::id, on_delete = "cascade"))]
    user_id: C::Type<'scope, i32>,
    body: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct ComputedRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    title: C::Type<'scope, String>,
    #[column(insert = false, update = false)]
    created_at: C::Type<'scope, String>,
    #[column(generated)]
    search_vector: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct DefaultedRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct DefaultVariant<'scope, C: ColumnMode = ColumnExpr> {
    #[column(default = value(42))]
    count: C::Type<'scope, i32>,
    #[column(default = value(true))]
    enabled: C::Type<'scope, bool>,
    #[column(default = current_timestamp)]
    created_at: C::Type<'scope, String>,
    #[column(default_raw = "lower('ADA')")]
    code: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct JsonPayload;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ColumnType)]
pub struct RecordId(i32);

#[derive(Clone, Debug, PartialEq, ColumnType)]
#[column_type(db_type = "jsonb")]
pub struct JsonColumn(String);

#[derive(Clone, Debug, PartialEq, Table)]
struct RawTypeRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(db_type = "jsonb")]
    payload: C::Type<'scope, JsonPayload>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct DerivedColumnTypeRecord<'scope, C: ColumnMode = ColumnExpr> {
    id: C::Type<'scope, RecordId>,
    payload: C::Type<'scope, JsonColumn>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
    posts: Post<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDatabase {
    public: Public,
}

#[test]
fn derive_table_populates_table_metadata() {
    let columns = <User as SchemaTable>::column_names();

    assert_eq!(<Public as Schema>::name(), Some("public"));
    let schema_tables = <Public as Schema>::tables().collect::<Vec<_>>();
    assert_eq!(schema_tables.len(), 2);
    assert_eq!(schema_tables[0].schema_name(), Some("public"));
    assert_eq!(schema_tables[0].name(), "users");
    assert_eq!(schema_tables[0].qualified_name(), "public.users");
    assert_eq!(schema_tables[1].schema_name(), Some("public"));
    assert_eq!(schema_tables[1].qualified_name(), "public.posts");
    let column_metadata = schema_tables[0].columns();
    let indexes = schema_tables[0].indexes();
    let database_schemas = <AppDatabase as Database>::schemas().collect::<Vec<_>>();
    assert_eq!(database_schemas.len(), 1);
    assert_eq!(database_schemas[0].name(), Some("public"));
    let database_schema_tables = database_schemas[0].tables().collect::<Vec<_>>();
    assert_eq!(database_schema_tables.len(), 2);
    assert_eq!(database_schema_tables[0].qualified_name(), "public.users");
    assert_eq!(database_schema_tables[1].qualified_name(), "public.posts");
    assert_eq!(columns.id, "id");
    assert_eq!(columns.name, "name");
    assert_eq!(column_metadata.len(), 2);
    assert!(column_metadata[0].primary_key());
    assert!(column_metadata[0].indexed());
    assert!(column_metadata[0].auto_increment());
    assert!(column_metadata[1].indexed());
    assert!(column_metadata[1].nullable());
    assert_eq!(
        column_metadata[1].default(),
        Some(ColumnDefault::Text("anonymous"))
    );
    assert_eq!(column_metadata[0].column_type(), ColumnType::I32);
    assert_eq!(column_metadata[1].column_type(), ColumnType::String);
    assert_eq!(indexes.len(), 3);
    assert_eq!(indexes[2].name(), Some("users_name_id_idx"));
    assert_eq!(indexes[2].columns(), &["name", "id"]);
    assert!(indexes[2].unique());
}

#[test]
fn derive_table_populates_foreign_key_metadata() {
    let schema_tables = <Public as Schema>::tables().collect::<Vec<_>>();
    let columns = schema_tables[1].columns();
    let user_id = &columns[1];
    let references = user_id
        .references()
        .expect("user_id should reference users");

    assert!(user_id.indexed());
    assert_eq!(references.schema_name(), Some("public"));
    assert_eq!(references.table(), "users");
    assert_eq!(references.column(), "id");
    assert_eq!(references.on_delete(), Some("cascade"));
}

#[test]
fn derive_table_populates_column_capability_metadata() {
    let columns = <ComputedRecord as SchemaTable>::columns();

    assert!(columns[0].auto_increment());
    assert!(!columns[0].insertable());
    assert!(!columns[0].updateable());
    assert!(columns[1].insertable());
    assert!(columns[1].updateable());
    assert!(!columns[1].generated());
    assert!(!columns[2].insertable());
    assert!(!columns[2].updateable());
    assert!(!columns[2].generated());
    assert!(columns[3].generated());
    assert!(!columns[3].insertable());
    assert!(!columns[3].updateable());
}

#[test]
fn derive_table_populates_typed_default_metadata() {
    let columns = <DefaultVariant as SchemaTable>::columns();

    assert_eq!(columns[0].default(), Some(ColumnDefault::Int(42)));
    assert_eq!(columns[1].default(), Some(ColumnDefault::Bool(true)));
    assert_eq!(columns[2].default(), Some(ColumnDefault::CurrentTimestamp));
    assert_eq!(
        columns[3].default(),
        Some(ColumnDefault::Raw("lower('ADA')"))
    );
}

#[test]
fn derive_table_treats_db_type_as_raw_backend_type_override() {
    let columns = <RawTypeRecord as SchemaTable>::columns();

    assert_eq!(columns[0].column_type(), ColumnType::Raw("jsonb"));
}

#[test]
fn derive_column_type_maps_newtype_columns() {
    let columns = <DerivedColumnTypeRecord as SchemaTable>::columns();

    assert_eq!(columns[0].column_type(), ColumnType::I32);
    assert_eq!(columns[1].column_type(), ColumnType::Raw("jsonb"));
}

#[test]
fn derive_column_type_maps_newtype_bind_values() {
    let insert = TestConnection
        .to::<DerivedColumnTypeRecord>()
        .id(RecordId(7))
        .payload(JsonColumn("{\"ok\":true}".to_owned()))
        .insert_returning(|record| record.id);

    assert_eq!(
        insert.collect_params(),
        vec![
            BindValue::Int(7),
            BindValue::Text("{\"ok\":true}".to_owned())
        ]
    );
}

#[test]
fn backend_creates_schema_sql() {
    let mut sql = Vec::new();
    let schema_tables = <Public as Schema>::tables().collect::<Vec<_>>();
    TestBackend.write_table(schema_tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(sql.contains(
        "CREATE TABLE public.users (id integer PRIMARY KEY AUTOINCREMENT NOT NULL, name text DEFAULT 'anonymous')"
    ));
    assert!(sql.contains("CREATE UNIQUE INDEX users_name_id_idx ON public.users (name, id)"));

    let mut sql = Vec::new();
    TestBackend.write_table(schema_tables[1], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(sql.contains("REFERENCES public.users(id) ON DELETE cascade"));
}

#[test]
fn from_selects_from_derived_table_metadata() {
    let users = TestConnection.from::<User>().select(|(user,)| user);

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

trait HasSelectShape<S> {}

impl<'conn, 'scope, S, Base, Projection> HasSelectShape<S>
    for TestSelect<'conn, 'scope, S, Base, Projection>
where
    S: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
}

trait HasSelectRow<Row> {}

impl<'conn, 'scope, Shape, Base, Projection, Row> HasSelectRow<Row>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape<Row = Row>,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
}

fn assert_table_select_shape<S>(_: &impl HasSelectShape<S>)
where
    S: ProjectionShape,
{
}

fn assert_user_row(_: &impl HasSelectRow<__SquealyUserRowShape>) {}

fn assert_i32_row(_: &impl HasSelectRow<i32>) {}

fn assert_optional_i32_row(_: &impl HasSelectRow<Option<i32>>) {}

fn assert_user_id_and_post_row(_: &impl HasSelectRow<(i32, __SquealyPostRowShape)>) {}

fn assert_user_id_name_and_post_row(_: &impl HasSelectRow<(i32, String, __SquealyPostRowShape)>) {}

fn assert_user_id_post_id_title_row(_: &impl HasSelectRow<(i32, i32, String)>) {}

fn assert_user_id_post_id_title_default_id_row(_: &impl HasSelectRow<(i32, i32, String, i32)>) {}

fn assert_user_and_maybe_post_row(
    _: &impl HasSelectRow<(__SquealyUserRowShape, Post<'static, ColumnNullableValue>)>,
) {
}

fn assert_thirty_two_i32_row(_: &impl HasSelectRow<ThirtyTwoI32s>) {}

trait HasInsertRow<Row> {}

impl<'conn, S, Shape, Columns, Returning, Row> HasInsertRow<Row>
    for TestInsert<'conn, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape<Row = Row>,
    Columns: InsertAssignments,
    Returning: Projectable,
{
}

trait HasUpdateRow<Row> {}

impl<'conn, S, Shape, Columns, Filters, Returning, Row> HasUpdateRow<Row>
    for TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape<Row = Row>,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
}

trait HasDeleteRow<Row> {}

impl<'conn, S, Shape, Filters, Returning, Row> HasDeleteRow<Row>
    for TestDelete<'conn, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape<Row = Row>,
    Filters: PredicateNodes,
    Returning: Projectable,
{
}

fn assert_insert_i32_row(_: &impl HasInsertRow<i32>) {}

fn assert_update_id_name_row(_: &impl HasUpdateRow<(i32, String)>) {}

fn assert_delete_user_row(_: &impl HasDeleteRow<__SquealyUserRowShape>) {}

type ThirtyTwoI32s = (
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
    i32,
);

trait HasExprKind<K> {}

impl<'scope, K, Ast> HasExprKind<K> for Expr<'scope, K, Ast>
where
    K: ExprKind,
    Ast: ExprAst,
{
}

fn assert_expr_kind<K>(_: &impl HasExprKind<K>)
where
    K: ExprKind,
{
}

fn assert_column_kind<'scope, K>(_: ColumnRef<'scope, K>)
where
    K: ExprKind,
{
}

fn assert_copy<T: Copy>(_: T) {}

fn assert_decode<T>()
where
    T: Decode<TestBackend>,
{
}

#[test]
fn from_select_carries_table_projection_shape() {
    let users = TestConnection.from::<User>().select(|(user,)| user);

    assert_table_select_shape::<__SquealyUserRowShape>(&users);
    assert_user_row(&users);
    assert!(users.built_from_selected());
}

#[test]
fn source_chain_selects_from_typed_root_and_join() {
    let posts = TestConnection
        .from::<User>()
        .where_(|user| user.name.equals("John"))
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, post.id, post.body));

    assert_user_id_post_id_title_row(&posts);
    assert!(posts.built_from_selected());
    assert_eq!(
        posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_1.body AS t2_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id) WHERE (q0_0.name = ?)"#
    );
    assert_eq!(
        posts.collect_params(),
        vec![BindValue::Text("John".to_owned())]
    );
}

#[test]
fn source_chain_can_append_multiple_typed_joins() {
    let rows = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .join::<ComputedRecord>()
        .on(|(_user, post), record| record.id.equals(post.id))
        .select(|(user, post, record)| (user.id, post.id, record.title));

    assert_user_id_post_id_title_row(&rows);
    assert_eq!(
        rows.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_2.title AS t2_title FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id) INNER JOIN computed_records AS q0_2 ON (q0_2.id = q0_1.id)"#
    );
}

#[test]
fn source_chain_can_filter_after_joining_sources() {
    let rows = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .where_(|(_user, post)| post.body.equals("Hello"))
        .select(|(user, post)| (user.id, post.id, post.body));

    assert_user_id_post_id_title_row(&rows);
    assert_eq!(
        rows.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_1.body AS t2_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id) WHERE (q0_1.body = ?)"#
    );
    assert_eq!(
        rows.collect_params(),
        vec![BindValue::Text("Hello".to_owned())]
    );
}

#[test]
fn source_chain_lowers_into_typed_sink_events() {
    let query = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .where_(|(_user, post)| post.body.equals("Hello"))
        .left_join::<ComputedRecord>()
        .on(|(_user, post), record| record.id.equals(post.id))
        .order_by(|(_user, post, _record)| post.id.asc())
        .limit(10)
        .offset(20);

    let (_user, post, record) = query.exprs().to_tuple();
    let projection = (post.id, record.title);
    let mut sink = RecordingSelectSink::default();
    sink.push_projection::<(i32, String), _>(projection)
        .unwrap();
    query.lower_into(&mut sink).unwrap();

    assert_eq!(
        sink.events,
        vec![
            LoweringEvent::Projection {
                shape: "(i32, alloc::string::String)",
            },
            LoweringEvent::Table {
                table: "public.users".to_owned(),
                alias: SourceAlias::new(0, 0),
            },
            LoweringEvent::InnerJoin {
                table: "public.posts".to_owned(),
                alias: SourceAlias::new(0, 1),
            },
            LoweringEvent::LeftJoin {
                table: "computed_records".to_owned(),
                alias: SourceAlias::new(0, 2),
            },
            LoweringEvent::Filter,
            LoweringEvent::Order,
            LoweringEvent::Limit(10),
            LoweringEvent::Offset(20),
        ]
    );
}

#[test]
fn source_chain_join_grows_through_hlist_push_back() {
    let rows = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .join::<ComputedRecord>()
        .on(|(_user, post), record| record.id.equals(post.id))
        .join::<DefaultedRecord>()
        .on(|(_user, _post, record), defaulted| defaulted.id.equals(record.id))
        .select(|(user, post, record, defaulted)| (user.id, post.id, record.title, defaulted.id));

    assert_user_id_post_id_title_default_id_row(&rows);
    assert_eq!(
        rows.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_2.title AS t2_title, q0_3.id AS t3_id FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id) INNER JOIN computed_records AS q0_2 ON (q0_2.id = q0_1.id) INNER JOIN defaulted_records AS q0_3 ON (q0_3.id = q0_2.id)"#
    );
}

#[test]
fn source_chain_can_left_join_nullable_table_shapes() {
    let rows = TestConnection
        .from::<User>()
        .left_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user, post));

    assert_user_and_maybe_post_row(&rows);
    assert_eq!(
        rows.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t0_name, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn source_chain_can_order_limit_and_offset_rows() {
    let users = TestConnection
        .from::<User>()
        .order_by(|(user,)| user.name.desc())
        .order_by(|(user,)| user.id.asc())
        .limit(10)
        .offset(20)
        .select(|(user,)| user);

    assert_user_row(&users);
    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.name DESC, q0_0.id ASC LIMIT 10 OFFSET 20"#
    );
}

#[test]
fn from_uses_generated_column_expression_kinds() {
    let _users = TestConnection.from::<User>().select(|(user,)| {
        assert_column_kind::<UserId>(user.id);
        assert_column_kind::<UserName>(user.name);
        assert_copy(user.id);
        assert_copy(user.name);
        user
    });
}

#[test]
fn table_rows_implement_backend_decode() {
    assert_decode::<()>();
    assert_decode::<i32>();
    assert_decode::<Option<String>>();
    assert_decode::<RecordId>();
    assert_decode::<JsonColumn>();
    assert_decode::<User<'static, ColumnValue>>();
    assert_decode::<DerivedColumnTypeRecord<'static, ColumnValue>>();
    assert_decode::<User<'static, ColumnNullableValue>>();
    assert_decode::<__SquealyUserRowShape>();
    assert_decode::<(i32, User<'static, ColumnValue>)>();
}

#[test]
fn table_row_shapes_respect_column_nullability() {
    let row = __SquealyUserRowShape { id: 1, name: None };
    let name: Option<String> = row.name;

    assert_eq!(row.id, 1);
    assert_eq!(name, None);
}

#[test]
fn insert_builder_executes_with_optional_columns() {
    let _execute = TestConnection.to::<User>().name("Ada").insert();
}

#[test]
fn insert_builder_requires_required_columns() {
    let _execute = TestConnection
        .to::<Post>()
        .id(1)
        .user_id(1)
        .body("Hello")
        .insert();
}

#[test]
fn insert_builder_skips_non_insertable_columns() {
    let _execute = TestConnection.to::<ComputedRecord>().title("Ada").insert();
}

#[test]
fn insert_builder_can_use_default_values() {
    let insert = TestConnection
        .to::<DefaultedRecord>()
        .insert_returning(|record| record.id);

    assert_eq!(
        insert.to_sql(),
        r#"INSERT INTO defaulted_records DEFAULT VALUES RETURNING q0_0.id AS id"#
    );
    assert_eq!(insert.collect_params(), Vec::<BindValue>::new());
}

#[test]
fn insert_query_builds_column_bindings() {
    let columns = HNil.push_back(InsertAssignment::<UserName>::new(BindValue::Text(
        "Ada".to_owned(),
    )));
    let insert = <<TestConnection as QueryBuilder>::Insert<'_, User, (), _, ()> as InsertQuery<
        '_,
        _,
        (),
    >>::build(&TestConnection, columns, ());

    let _execute = insert.execute();
    assert_eq!(
        insert.to_sql(),
        r#"INSERT INTO public.users (name) VALUES (?)"#
    );
    assert_eq!(
        insert.collect_params(),
        vec![BindValue::Text("Ada".to_owned())]
    );
    let mut sink = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };
    insert.write_params(&mut sink).unwrap();
    assert_eq!(sink.reserved, 1);
}

#[test]
fn insert_builder_can_return_projected_rows() {
    let insert = TestConnection
        .to::<User>()
        .name("Ada")
        .insert_returning(|user| user.id);

    assert_insert_i32_row(&insert);
    let _stream = insert.fetch();
    let _all = insert.collect();
    let _all_with_affected = insert.collect_with_affected();
    let _one_with_affected = insert.fetch_one_with_affected();
    let _optional_with_affected = insert.fetch_optional_with_affected();
    let _one = insert.fetch_one();
    let _optional = insert.fetch_optional();
    assert_eq!(
        insert.to_sql(),
        r#"INSERT INTO public.users (name) VALUES (?) RETURNING q0_0.id AS id"#
    );
    assert_eq!(
        insert.collect_params(),
        vec![BindValue::Text("Ada".to_owned())]
    );
}

#[test]
fn insert_builder_accepts_null_for_nullable_columns() {
    let insert = TestConnection
        .to::<User>()
        .name(None::<String>)
        .insert_returning(|user| user.id);

    assert_eq!(
        insert.to_sql(),
        r#"INSERT INTO public.users (name) VALUES (?) RETURNING q0_0.id AS id"#
    );
    assert_eq!(insert.collect_params(), vec![BindValue::Null]);
}

#[test]
fn update_builder_executes_after_a_column_is_set() {
    let _execute = TestConnection
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update();
}

#[test]
fn update_builder_can_explicitly_target_all_rows() {
    let _execute = TestConnection.to::<User>().name("Ada").all().update();
}

#[test]
fn update_builder_skips_non_updateable_columns() {
    let _execute = TestConnection
        .to::<ComputedRecord>()
        .title("Ada")
        .all()
        .update();
}

#[test]
fn update_query_builds_column_bindings_and_filters() {
    let user = <User as ProjectionShape>::exprs(SourceAlias::new(0, 0));
    let columns = HNil.push_back(UpdateAssignment::<UserName>::new(BindValue::Text(
        "Ada".to_owned(),
    )));
    let filters = HNil.push_back(user.id.equals(1));
    let update =
        <<TestConnection as QueryBuilder>::Update<'_, User, (), _, _, ()> as UpdateQuery<
            '_,
            _,
            _,
            (),
        >>::build(
            &TestConnection,
            SourceAlias::new(0, 0),
            columns,
            filters,
            (),
        );

    let _execute = update.execute();
    assert_eq!(
        update.to_sql(),
        r#"UPDATE public.users AS q0_0 SET name = ? WHERE (q0_0.id = ?)"#
    );
    assert_eq!(
        update.collect_params(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::Int(1)]
    );
    let mut sink = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };
    update.write_params(&mut sink).unwrap();
    assert_eq!(sink.reserved, 2);
}

#[test]
fn update_builder_can_return_projected_rows() {
    let update = TestConnection
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update_returning(|user| (user.id, user.name));

    assert_update_id_name_row(&update);
    let _stream = update.fetch();
    let _all = update.collect();
    let _all_with_affected = update.collect_with_affected();
    let _one_with_affected = update.fetch_one_with_affected();
    let _optional_with_affected = update.fetch_optional_with_affected();
    let _one = update.fetch_one();
    let _optional = update.fetch_optional();
    assert_eq!(
        update.to_sql(),
        r#"UPDATE public.users AS q0_0 SET name = ? WHERE (q0_0.id = ?) RETURNING q0_0.id AS t0_id, q0_0.name AS t1_name"#
    );
    assert_eq!(
        update.collect_params(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::Int(1)]
    );
}

#[test]
fn update_builder_accepts_null_for_nullable_columns() {
    let update = TestConnection
        .to::<User>()
        .name(None::<String>)
        .where_(|user| user.id.equals(1))
        .update_returning(|user| user.id);

    assert_eq!(
        update.to_sql(),
        r#"UPDATE public.users AS q0_0 SET name = ? WHERE (q0_0.id = ?) RETURNING q0_0.id AS id"#
    );
    assert_eq!(
        update.collect_params(),
        vec![BindValue::Null, BindValue::Int(1)]
    );
}

#[test]
fn delete_builds_typed_table_filters() {
    let _execute = TestConnection
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .where_(|(user,)| user.name.equals("Ada"))
        .delete();
}

#[test]
fn delete_builder_can_explicitly_target_all_rows() {
    let _execute = TestConnection.from::<User>().all().delete();
}

#[test]
fn delete_query_builds_typed_table_filters() {
    let user = <User as ProjectionShape>::exprs(SourceAlias::new(0, 0));
    let filters = HNil
        .push_back(user.id.equals(1))
        .push_back(user.name.equals("Ada"));
    let delete = <<TestConnection as QueryBuilder>::Delete<'_, User, (), _, ()> as DeleteQuery<
        '_,
        _,
        (),
    >>::build(&TestConnection, SourceAlias::new(0, 0), filters, ());

    let _execute = delete.execute();
    assert_eq!(
        delete.to_sql(),
        r#"DELETE FROM public.users AS q0_0 WHERE (q0_0.id = ?) AND (q0_0.name = ?)"#
    );
    assert_eq!(
        delete.collect_params(),
        vec![BindValue::Int(1), BindValue::Text("Ada".to_owned())]
    );
    let mut sink = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };
    delete.write_params(&mut sink).unwrap();
    assert_eq!(sink.reserved, 2);
}

#[test]
fn delete_builder_can_return_projected_rows() {
    let delete = TestConnection
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .delete_returning(|user| user);

    assert_delete_user_row(&delete);
    let _stream = delete.fetch();
    let _all = delete.collect();
    let _all_with_affected = delete.collect_with_affected();
    let _one_with_affected = delete.fetch_one_with_affected();
    let _optional_with_affected = delete.fetch_optional_with_affected();
    let _one = delete.fetch_one();
    let _optional = delete.fetch_optional();
    assert_eq!(
        delete.to_sql(),
        r#"DELETE FROM public.users AS q0_0 WHERE (q0_0.id = ?) RETURNING q0_0.id AS id, q0_0.name AS name"#
    );
    assert_eq!(delete.collect_params(), vec![BindValue::Int(1)]);
}

#[test]
fn select_can_project_a_generated_column_expression_kind() {
    let user_ids = TestConnection.from::<User>().select(|(user,)| user.id);

    assert_i32_row(&user_ids);
    assert_eq!(
        user_ids.to_sql(),
        r#"SELECT q0_0.id AS id FROM public.users AS q0_0"#
    );
}

#[test]
fn select_can_mix_column_and_table_projection_shapes() {
    let user_ids_and_posts = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, post));

    assert_user_id_and_post_row(&user_ids_and_posts);
    assert_eq!(
        user_ids_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_project_three_part_tuple_shapes() {
    let user_ids_names_and_posts = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user.id, user.name, post));

    assert_user_id_name_and_post_row(&user_ids_names_and_posts);
    assert_eq!(
        user_ids_names_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t1_name, q0_1.id AS t2_id, q0_1.user_id AS t2_user_id, q0_1.body AS t2_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_project_thirty_two_part_tuple_shapes() {
    let values = TestConnection.select((
        0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24,
        25, 26, 27, 28, 29, 30, 31,
    ));

    assert_thirty_two_i32_row(&values);
    assert_eq!(
        values.to_sql(),
        r#"SELECT ? AS t0_expr, ? AS t1_expr, ? AS t2_expr, ? AS t3_expr, ? AS t4_expr, ? AS t5_expr, ? AS t6_expr, ? AS t7_expr, ? AS t8_expr, ? AS t9_expr, ? AS t10_expr, ? AS t11_expr, ? AS t12_expr, ? AS t13_expr, ? AS t14_expr, ? AS t15_expr, ? AS t16_expr, ? AS t17_expr, ? AS t18_expr, ? AS t19_expr, ? AS t20_expr, ? AS t21_expr, ? AS t22_expr, ? AS t23_expr, ? AS t24_expr, ? AS t25_expr, ? AS t26_expr, ? AS t27_expr, ? AS t28_expr, ? AS t29_expr, ? AS t30_expr, ? AS t31_expr"#
    );
    assert_eq!(
        values.collect_params(),
        (0..32).map(BindValue::Int).collect::<Vec<_>>()
    );
}

#[test]
fn select_can_project_arithmetic_expression_shapes() {
    let adjusted_ids = TestConnection.from::<User>().select(|(user,)| user.id + 1);
    let scaled_ids = TestConnection
        .from::<User>()
        .select(|(user,)| (user.id * 2) / 2);

    assert_i32_row(&adjusted_ids);
    assert_eq!(
        adjusted_ids.to_sql(),
        r#"SELECT (q0_0.id + ?) AS expr FROM public.users AS q0_0"#
    );
    assert_eq!(adjusted_ids.collect_params(), vec![BindValue::Int(1)]);
    assert_i32_row(&scaled_ids);
    assert_eq!(
        scaled_ids.to_sql(),
        r#"SELECT ((q0_0.id * ?) / ?) AS expr FROM public.users AS q0_0"#
    );
    assert_eq!(
        scaled_ids.collect_params(),
        vec![BindValue::Int(2), BindValue::Int(2)]
    );
}

#[test]
fn select_can_project_primitive_literal_shapes() {
    let values = TestConnection.select(1);

    assert_i32_row(&values);
    assert_eq!(values.to_sql(), r#"SELECT ? AS expr"#);
    assert_eq!(values.collect_params(), vec![BindValue::Int(1)]);
}

#[test]
fn select_exposes_stream_and_convenience_fetch_methods() {
    let users = TestConnection.from::<User>().select(|(user,)| user);

    let _stream = users.fetch();
    let _all = users.collect();
    let _one = users.fetch_one();
    let _optional = users.fetch_optional();
}

#[test]
fn select_can_use_scoped_table_sources_directly() {
    let users = TestConnection.from::<User>().select(|(user,)| user);

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_can_order_by_typed_expressions() {
    let users = TestConnection
        .from::<User>()
        .order_by(|(user,)| user.name.desc())
        .order_by(|(user,)| user.id.asc())
        .select(|(user,)| user);

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.name DESC, q0_0.id ASC"#
    );
}

#[test]
fn select_can_limit_and_offset_rows() {
    let users = TestConnection
        .from::<User>()
        .order_by(|(user,)| user.id.asc())
        .limit(10)
        .offset(20)
        .select(|(user,)| user);

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.id ASC LIMIT 10 OFFSET 20"#
    );
}

#[test]
fn select_can_inner_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection
        .from::<User>()
        .join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| (user, post));

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t0_name, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_left_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection
        .from::<User>()
        .left_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(user, post)| {
            assert_column_kind::<Nullable<PostId>>(post.id);
            assert_column_kind::<Nullable<PostUserId>>(post.user_id);
            (user, post)
        });

    assert_user_and_maybe_post_row(&users_and_posts);
    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t0_name, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn left_join_projects_nullable_column_shapes() {
    let post_ids = TestConnection
        .from::<User>()
        .left_join::<Post>()
        .on(|(user,), post| post.user_id.equals(user.id))
        .select(|(_user, post)| post.id);

    assert_optional_i32_row(&post_ids);
    assert_eq!(
        post_ids.to_sql(),
        r#"SELECT q0_1.id AS id FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_writes_sql_to_writer() {
    let users = TestConnection.from::<User>().select(|(user,)| user);
    let mut sql = Vec::new();

    users.write_sql(&mut sql).unwrap();

    assert_eq!(
        String::from_utf8(sql).unwrap(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_accepts_primitive_literals_and_expression_operators() {
    let users = TestConnection
        .from::<User>()
        .where_(|user| {
            ((user.id + 1 - 1).greater_than(0) & !user.id.not_equals(42)) | user.name.equals("Bob")
        })
        .where_(|(user,)| (1 + user.id).less_than(100))
        .where_(|(user,)| {
            let scaled_id = (user.id * 2) / 2;
            scaled_id.equals(user.id)
        })
        .where_(|(user,)| (2 * user.id / 2).equals(user.id))
        .select(|(user,)| {
            let adjusted_id = user.id + 1;
            let scaled_id = (user.id * 2) / 2;
            assert_expr_kind::<AddExpr<UserId, i32>>(&adjusted_id);
            assert_expr_kind::<DivideExpr<MultiplyExpr<UserId, i32>, i32>>(&scaled_id);
            user
        });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 WHERE (((((q0_0.id + ?) - ?) > ?) AND (NOT (q0_0.id <> ?))) OR (q0_0.name = ?)) AND ((? + q0_0.id) < ?) AND (((q0_0.id * ?) / ?) = q0_0.id) AND (((? * q0_0.id) / ?) = q0_0.id)"#
    );
    assert_eq!(
        users.collect_params(),
        vec![
            BindValue::Int(1),
            BindValue::Int(1),
            BindValue::Int(0),
            BindValue::Int(42),
            BindValue::Text("Bob".to_owned()),
            BindValue::Int(1),
            BindValue::Int(100),
            BindValue::Int(2),
            BindValue::Int(2),
            BindValue::Int(2),
            BindValue::Int(2),
        ]
    );
}

#[test]
fn primitive_literals_preserve_bind_value_widths() {
    assert_eq!(
        1_i16.into_bind_value().kind(),
        &BindValueKind::Int {
            value: 1,
            width: IntWidth::I16,
        }
    );
    assert_eq!(
        1_i32.into_bind_value().kind(),
        &BindValueKind::Int {
            value: 1,
            width: IntWidth::I32,
        }
    );
    assert_eq!(
        1_i64.into_bind_value().kind(),
        &BindValueKind::Int {
            value: 1,
            width: IntWidth::I64,
        }
    );
    assert_eq!(
        1.0_f32.into_bind_value().kind(),
        &BindValueKind::Float {
            value: 1.0,
            width: FloatWidth::F32,
        }
    );
}

#[test]
fn prepared_param_values_can_write_into_bind_sinks() {
    type Params = HCons<String, HCons<i32, HNil>>;

    let params = ("Ada".to_owned(), 7_i32);
    let mut sink = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };

    <(String, i32) as PreparedParamValues<Params>>::write_bind_values(&params, &mut sink).unwrap();
    assert_eq!(
        sink.values,
        vec![BindValue::Text("Ada".to_owned()), BindValue::int32(7)]
    );

    let mut indexed = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };
    assert!(
        <(String, i32) as PreparedParamValues<Params>>::write_bind_value_at(
            &params,
            1,
            &mut indexed,
        )
        .unwrap()
    );
    assert_eq!(indexed.values, vec![BindValue::int32(7)]);
    assert!(
        !<(String, i32) as PreparedParamValues<Params>>::write_bind_value_at(
            &params,
            2,
            &mut indexed,
        )
        .unwrap()
    );

    assert_eq!(
        params.collect_bind_values(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::int32(7)]
    );

    let borrowed = ("Grace", 8_i32);
    let mut borrowed_sink = RecordingBindSink {
        values: Vec::new(),
        reserved: 0,
    };
    <(&str, i32) as PreparedParamValues<Params>>::write_bind_values(&borrowed, &mut borrowed_sink)
        .unwrap();
    assert_eq!(
        borrowed_sink.values,
        vec![BindValue::Text("Grace".to_owned()), BindValue::int32(8)]
    );
}
