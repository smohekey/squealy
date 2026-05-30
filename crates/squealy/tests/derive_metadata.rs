use squealy::*;
use squealy_test::{TestConnection, TestSelect};

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
#[index(name = "users_name_id_idx", columns = [name, id], unique)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment, index)]
    id: C::Type<'scope, i32>,
    #[column(index, nullable, default = "anonymous", db_type = "text")]
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

fn posts_of_user<'conn, 'scope>(
    connection: &'conn TestConnection,
    user_id: &Expr<'scope, i32>,
) -> TestSelect<'conn, Post<'static, ColumnExpr>> {
    connection.select::<Post>(|q| {
        let posts = connection.select::<Post>(|q| q.each::<Post>());
        let post = q.q(&posts);
        q.where_(post.user_id.equals(user_id));
        post
    })
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
    assert_eq!(column_metadata[1].default(), Some("anonymous"));
    assert_eq!(column_metadata[1].db_type(), Some("text"));
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
fn backend_creates_schema_sql() {
    let mut sql = Vec::new();
    let schema_tables = <Public as Schema>::tables().collect::<Vec<_>>();
    TestConnection
        .write_table(schema_tables[0], &mut sql)
        .unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(sql.contains(
            "CREATE TABLE public.users (id text PRIMARY KEY AUTOINCREMENT NOT NULL, name text DEFAULT anonymous)"
        ));
    assert!(sql.contains("CREATE UNIQUE INDEX users_name_id_idx ON public.users (name, id)"));

    let mut sql = Vec::new();
    TestConnection
        .write_table(schema_tables[1], &mut sql)
        .unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(sql.contains("REFERENCES public.users(id) ON DELETE cascade"));
}

#[test]
fn each_selects_from_derived_table_metadata() {
    let users = TestConnection.select::<User>(|q| q.each::<User>());

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

fn assert_table_select_shape<'conn, Qry, S>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Shape = S>,
    S: TableProjection,
{
}

#[test]
fn each_select_carries_table_projection_shape() {
    let users = TestConnection.select::<User>(|q| q.each::<User>());

    assert_table_select_shape::<_, User>(&users);
}

#[test]
fn select_can_use_scoped_table_sources_directly() {
    let users = TestConnection.select::<User>(|q| q.each::<User>());

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_can_order_by_typed_expressions() {
    let users = TestConnection.select::<User>(|q| {
        let user = q.each::<User>();
        q.order_by(user.name.desc());
        q.order_by(user.id.asc());
        user
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.name DESC, q0_0.id ASC"#
    );
}

#[test]
fn select_can_limit_and_offset_rows() {
    let users = TestConnection.select::<User>(|q| {
        let user = q.each::<User>();
        q.order_by(user.id.asc());
        q.limit(10);
        q.offset(20);
        user
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.id ASC LIMIT 10 OFFSET 20"#
    );
}

#[test]
fn select_can_inner_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection.select::<(User, Post)>(|q| {
        let user = q.each::<User>();
        let post = q.join::<Post>(|post| post.user_id.equals(&user.id));
        (user, post)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_left_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection.select::<(User, Post)>(|q| {
        let user = q.each::<User>();
        let post = q.left_join::<Post>(|post| post.user_id.equals(&user.id));
        (user, post)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_writes_sql_to_writer() {
    let users = TestConnection.select::<User>(|q| q.each::<User>());
    let mut sql = Vec::new();

    users.write_sql(&mut sql).unwrap();

    assert_eq!(
        String::from_utf8(sql).unwrap(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_composes_subqueries_with_lateral_joins() {
    let users_and_posts = TestConnection.select::<(User, Post)>(|q| {
        let users = TestConnection.select::<User>(|q| q.each::<User>());
        let user = q.q(&users);
        let posts = posts_of_user(&TestConnection, &user.id);
        let post = q.q(&posts);
        q.where_(
            (&user.id + 1 - 1)
                .greater_than(0)
                .and(user.id.not_equals(42).not_())
                .or(user.name.equals("Bob")),
        );
        (user, post)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM (SELECT q1_0.id AS id, q1_0.name AS name FROM public.users AS q1_0) AS q0_0 INNER JOIN LATERAL (SELECT q1_0.id AS id, q1_0.user_id AS user_id, q1_0.body AS body FROM (SELECT q2_0.id AS id, q2_0.user_id AS user_id, q2_0.body AS body FROM public.posts AS q2_0) AS q1_0 WHERE (q1_0.user_id = q0_0.id)) AS q0_1 ON TRUE WHERE (((((q0_0.id + ?) - ?) > ?) AND (NOT (q0_0.id <> ?))) OR (q0_0.name = ?))"#
    );
    assert_eq!(
        users_and_posts.params(),
        vec![
            BindValue::Int(1),
            BindValue::Int(1),
            BindValue::Int(0),
            BindValue::Int(42),
            BindValue::Text("Bob".to_owned()),
        ]
    );
}

#[test]
fn select_rebinds_tuple_subquery_shape_through_output_aliases() {
    let users_and_posts = TestConnection.select::<(User, Post)>(|q| {
        let pair_select = TestConnection.select::<(User, Post)>(|q| {
            let user = q.each::<User>();
            let post = q.join::<Post>(|post| post.user_id.equals(&user.id));
            (user, post)
        });
        let pair = q.q(&pair_select);

        q.where_(pair.0.id.equals(&pair.1.user_id));
        pair
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.left_id AS left_id, q0_0.left_name AS left_name, q0_0.right_id AS right_id, q0_0.right_user_id AS right_user_id, q0_0.right_body AS right_body FROM (SELECT q1_0.id AS left_id, q1_0.name AS left_name, q1_1.id AS right_id, q1_1.user_id AS right_user_id, q1_1.body AS right_body FROM public.users AS q1_0 INNER JOIN public.posts AS q1_1 ON (q1_1.user_id = q1_0.id)) AS q0_0 WHERE (q0_0.left_id = q0_0.right_user_id)"#
    );
}

#[test]
fn select_accepts_primitive_literals_and_expression_operators() {
    let users = TestConnection.select::<User>(|q| {
        let user = q.each::<User>();
        q.where_(
            ((&user.id + 1 - 1).greater_than(0) & !user.id.not_equals(42))
                | user.name.equals("Bob"),
        );
        q.where_((1 + &user.id).less_than(100));
        user
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 WHERE (((((q0_0.id + ?) - ?) > ?) AND (NOT (q0_0.id <> ?))) OR (q0_0.name = ?)) AND ((? + q0_0.id) < ?)"#
    );
    assert_eq!(
        users.params(),
        vec![
            BindValue::Int(1),
            BindValue::Int(1),
            BindValue::Int(0),
            BindValue::Int(42),
            BindValue::Text("Bob".to_owned()),
            BindValue::Int(1),
            BindValue::Int(100),
        ]
    );
}

#[test]
fn select_collects_source_and_filter_params_in_sql_order() {
    let users_and_posts = TestConnection.select::<(User, Post)>(|q| {
        let user_select = TestConnection.select::<User>(|q| {
            let user = q.each::<User>();
            q.where_(user.id.greater_than(10));
            user
        });
        let user = q.q(&user_select);
        let post = q.join::<Post>(|post| post.user_id.equals(7));
        q.where_(user.name.equals("Ada"));
        (user, post)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM (SELECT q1_0.id AS id, q1_0.name AS name FROM public.users AS q1_0 WHERE (q1_0.id > ?)) AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = ?) WHERE (q0_0.name = ?)"#
    );
    assert_eq!(
        users_and_posts.params(),
        vec![
            BindValue::Int(10),
            BindValue::Int(7),
            BindValue::Text("Ada".to_owned()),
        ]
    );
}
