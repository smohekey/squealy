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

fn posts_of_user<'conn, 'scope, K>(
    connection: &'conn TestConnection,
    user_id: &Expr<'scope, K>,
) -> TestSelect<'conn, Post<'static, ColumnExpr>>
where
    K: ExprKind<Value = i32>,
{
    connection.select(|q| {
        let post = q.from::<Post>();
        q.where_(post.user_id.equals(user_id));
        q.returning(post)
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
fn from_selects_from_derived_table_metadata() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user)
    });

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

fn assert_user_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = User<'static, ColumnValue>>,
{
}

fn assert_i32_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = i32>,
{
}

fn assert_optional_i32_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = Option<i32>>,
{
}

fn assert_user_id_and_post_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = (i32, Post<'static, ColumnValue>)>,
{
}

fn assert_user_id_name_and_post_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = (i32, String, Post<'static, ColumnValue>)>,
{
}

fn assert_user_and_maybe_post_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<
            'conn,
            Row = (
                User<'static, ColumnValue>,
                Post<'static, ColumnNullableValue>,
            ),
        >,
{
}

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

fn assert_thirty_two_i32_row<'conn, Qry>(_: &'conn Qry)
where
    Qry: SelectQuery<'conn, Row = ThirtyTwoI32s>,
{
}

fn assert_expr_kind<'scope, K>(_: &Expr<'scope, K>)
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

#[test]
fn from_select_carries_table_projection_shape() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user)
    });

    assert_table_select_shape::<_, User>(&users);
    assert_user_row(&users);
}

#[test]
fn from_uses_generated_column_expression_kinds() {
    let _users = TestConnection.select(|q| {
        let user = q.from::<User>();
        assert_column_kind::<UserId>(user.id);
        assert_column_kind::<UserName>(user.name);
        assert_copy(user.id);
        assert_copy(user.name);
        q.returning(user)
    });
}

#[test]
fn insert_builder_executes_with_optional_columns() {
    let _execute = TestConnection.insert::<User>().name("Ada").execute();
}

#[test]
fn insert_builder_requires_required_columns() {
    let _execute = TestConnection
        .insert::<Post>()
        .id(1)
        .user_id(1)
        .body("Hello")
        .execute();
}

#[test]
fn insert_builder_skips_non_insertable_columns() {
    let _execute = TestConnection
        .insert::<ComputedRecord>()
        .title("Ada")
        .execute();
}

#[test]
fn insert_query_builds_column_bindings() {
    let insert = TestConnection.insert_query::<User>(vec![InsertColumn::new(
        "name",
        BindValue::Text("Ada".to_owned()),
    )]);

    let _execute = insert.execute();
    assert_eq!(
        insert.to_sql(),
        r#"INSERT INTO public.users (name) VALUES (?)"#
    );
    assert_eq!(insert.params(), vec![BindValue::Text("Ada".to_owned())]);
}

#[test]
fn update_builder_executes_after_a_column_is_set() {
    let _execute = TestConnection
        .update::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .execute();
}

#[test]
fn update_builder_can_explicitly_target_all_rows() {
    let _execute = TestConnection.update::<User>().name("Ada").all().execute();
}

#[test]
fn update_builder_skips_non_updateable_columns() {
    let _execute = TestConnection
        .update::<ComputedRecord>()
        .title("Ada")
        .all()
        .execute();
}

#[test]
fn update_query_builds_column_bindings_and_filters() {
    let update = TestConnection.update_query::<User>(
        "q0_0".to_owned(),
        vec![UpdateColumn::new("name", BindValue::Text("Ada".to_owned()))],
        vec![Filter::new(PredicateNode::Compare {
            left: ExprNode::Column {
                alias: "q0_0".to_owned(),
                column: "id".to_owned(),
            },
            op: CompareOp::Equals,
            right: ExprNode::Literal(BindValue::Int(1)),
        })],
    );

    let _execute = update.execute();
    assert_eq!(
        update.to_sql(),
        r#"UPDATE public.users AS q0_0 SET name = ? WHERE (q0_0.id = ?)"#
    );
    assert_eq!(
        update.params(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::Int(1)]
    );
}

#[test]
fn delete_builds_typed_table_filters() {
    let _execute = TestConnection
        .delete::<User>()
        .where_(|user| user.id.equals(1))
        .where_(|user| user.name.equals("Ada"))
        .execute();
}

#[test]
fn delete_builder_can_explicitly_target_all_rows() {
    let _execute = TestConnection.delete::<User>().all().execute();
}

#[test]
fn delete_query_builds_typed_table_filters() {
    let delete = TestConnection.delete_query::<User>(
        "q0_0".to_owned(),
        vec![
            Filter::new(PredicateNode::Compare {
                left: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "id".to_owned(),
                },
                op: CompareOp::Equals,
                right: ExprNode::Literal(BindValue::Int(1)),
            }),
            Filter::new(PredicateNode::Compare {
                left: ExprNode::Column {
                    alias: "q0_0".to_owned(),
                    column: "name".to_owned(),
                },
                op: CompareOp::Equals,
                right: ExprNode::Literal(BindValue::Text("Ada".to_owned())),
            }),
        ],
    );

    let _execute = delete.execute();
    assert_eq!(
        delete.to_sql(),
        r#"DELETE FROM public.users AS q0_0 WHERE (q0_0.id = ?) AND (q0_0.name = ?)"#
    );
    assert_eq!(
        delete.params(),
        vec![BindValue::Int(1), BindValue::Text("Ada".to_owned())]
    );
}

#[test]
fn select_can_project_a_generated_column_expression_kind() {
    let user_ids = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user.id)
    });

    assert_i32_row(&user_ids);
    assert_eq!(
        user_ids.to_sql(),
        r#"SELECT q0_0.id AS id FROM public.users AS q0_0"#
    );
}

#[test]
fn select_can_mix_column_and_table_projection_shapes() {
    let user_ids_and_posts = TestConnection.select(|q| {
        let user = q.from::<User>();
        let post = q.join::<Post>(|post| post.user_id.equals(user.id));
        q.returning((user.id, post))
    });

    assert_user_id_and_post_row(&user_ids_and_posts);
    assert_eq!(
        user_ids_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_project_three_part_tuple_shapes() {
    let user_ids_names_and_posts = TestConnection.select(|q| {
        let user = q.from::<User>();
        let post = q.join::<Post>(|post| post.user_id.equals(user.id));
        q.returning((user.id, user.name, post))
    });

    assert_user_id_name_and_post_row(&user_ids_names_and_posts);
    assert_eq!(
        user_ids_names_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t1_name, q0_1.id AS t2_id, q0_1.user_id AS t2_user_id, q0_1.body AS t2_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_rebinds_three_part_tuple_subquery_shape() {
    let rebound = TestConnection.select(|q| {
        let tuple_select = TestConnection.select(|q| {
            let user = q.from::<User>();
            let post = q.join::<Post>(|post| post.user_id.equals(user.id));
            q.returning((user.id, user.name, post))
        });
        let tuple = q.q(&tuple_select);

        q.where_(tuple.0.equals(&tuple.2.user_id));
        q.returning(&tuple.0 + 0)
    });

    assert_i32_row(&rebound);
    assert_eq!(
        rebound.to_sql(),
        r#"SELECT (q0_0.t0_id + ?) AS expr FROM (SELECT q1_0.id AS t0_id, q1_0.name AS t1_name, q1_1.id AS t2_id, q1_1.user_id AS t2_user_id, q1_1.body AS t2_body FROM public.users AS q1_0 INNER JOIN public.posts AS q1_1 ON (q1_1.user_id = q1_0.id)) AS q0_0 WHERE (q0_0.t0_id = q0_0.t2_user_id)"#
    );
    assert_eq!(rebound.params(), vec![BindValue::Int(0)]);
}

#[test]
fn select_can_project_thirty_two_part_tuple_shapes() {
    let values = TestConnection.select(|q| {
        q.returning((
            0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23,
            24, 25, 26, 27, 28, 29, 30, 31,
        ))
    });

    assert_thirty_two_i32_row(&values);
    assert_eq!(
        values.to_sql(),
        r#"SELECT ? AS t0_expr, ? AS t1_expr, ? AS t2_expr, ? AS t3_expr, ? AS t4_expr, ? AS t5_expr, ? AS t6_expr, ? AS t7_expr, ? AS t8_expr, ? AS t9_expr, ? AS t10_expr, ? AS t11_expr, ? AS t12_expr, ? AS t13_expr, ? AS t14_expr, ? AS t15_expr, ? AS t16_expr, ? AS t17_expr, ? AS t18_expr, ? AS t19_expr, ? AS t20_expr, ? AS t21_expr, ? AS t22_expr, ? AS t23_expr, ? AS t24_expr, ? AS t25_expr, ? AS t26_expr, ? AS t27_expr, ? AS t28_expr, ? AS t29_expr, ? AS t30_expr, ? AS t31_expr"#
    );
    assert_eq!(
        values.params(),
        (0..32).map(BindValue::Int).collect::<Vec<_>>()
    );
}

#[test]
fn select_can_project_arithmetic_expression_shapes() {
    let adjusted_ids = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user.id + 1)
    });
    let scaled_ids = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning((user.id * 2) / 2)
    });

    assert_i32_row(&adjusted_ids);
    assert_eq!(
        adjusted_ids.to_sql(),
        r#"SELECT (q0_0.id + ?) AS expr FROM public.users AS q0_0"#
    );
    assert_eq!(adjusted_ids.params(), vec![BindValue::Int(1)]);
    assert_i32_row(&scaled_ids);
    assert_eq!(
        scaled_ids.to_sql(),
        r#"SELECT ((q0_0.id * ?) / ?) AS expr FROM public.users AS q0_0"#
    );
    assert_eq!(
        scaled_ids.params(),
        vec![BindValue::Int(2), BindValue::Int(2)]
    );
}

#[test]
fn select_can_project_primitive_literal_shapes() {
    let values = TestConnection.select(|q| q.returning(1));

    assert_i32_row(&values);
    assert_eq!(values.to_sql(), r#"SELECT ? AS expr"#);
    assert_eq!(values.params(), vec![BindValue::Int(1)]);
}

#[test]
fn select_exposes_stream_and_convenience_fetch_methods() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user)
    });

    let _stream = users.fetch();
    let _all = users.fetch_all();
    let _one = users.fetch_one();
    let _optional = users.fetch_optional();
}

#[test]
fn select_can_use_scoped_table_sources_directly() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user)
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_can_order_by_typed_expressions() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.order_by(user.name.desc());
        q.order_by(user.id.asc());
        q.returning(user)
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.name DESC, q0_0.id ASC"#
    );
}

#[test]
fn select_can_limit_and_offset_rows() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.order_by(user.id.asc());
        q.limit(10);
        q.offset(20);
        q.returning(user)
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 ORDER BY q0_0.id ASC LIMIT 10 OFFSET 20"#
    );
}

#[test]
fn select_can_inner_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection.select(|q| {
        let user = q.from::<User>();
        let post = q.join::<Post>(|post| post.user_id.equals(user.id));
        q.returning((user, post))
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t0_name, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_can_left_join_tables_with_typed_predicates() {
    let users_and_posts = TestConnection.select(|q| {
        let user = q.from::<User>();
        let post = q.left_join::<Post>(|post| post.user_id.equals(user.id));
        assert_column_kind::<Nullable<PostId>>(post.id);
        assert_column_kind::<Nullable<PostUserId>>(post.user_id);
        q.returning((user, post))
    });

    assert_user_and_maybe_post_row(&users_and_posts);
    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT q0_0.id AS t0_id, q0_0.name AS t0_name, q0_1.id AS t1_id, q0_1.user_id AS t1_user_id, q0_1.body AS t1_body FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn left_join_projects_nullable_column_shapes() {
    let post_ids = TestConnection.select(|q| {
        let user = q.from::<User>();
        let post = q.left_join::<Post>(|post| post.user_id.equals(user.id));
        q.returning(post.id)
    });

    assert_optional_i32_row(&post_ids);
    assert_eq!(
        post_ids.to_sql(),
        r#"SELECT q0_1.id AS id FROM public.users AS q0_0 LEFT JOIN public.posts AS q0_1 ON (q0_1.user_id = q0_0.id)"#
    );
}

#[test]
fn select_writes_sql_to_writer() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        q.returning(user)
    });
    let mut sql = Vec::new();

    users.write_sql(&mut sql).unwrap();

    assert_eq!(
        String::from_utf8(sql).unwrap(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0"#
    );
}

#[test]
fn select_composes_subqueries_with_lateral_joins() {
    let users_and_posts = TestConnection.select(|q| {
        let users = TestConnection.select(|q| {
            let user = q.from::<User>();
            q.returning(user)
        });
        let user = q.q(&users);
        let posts = posts_of_user(&TestConnection, &user.id);
        let post = q.q(&posts);
        q.where_(
            (&user.id + 1 - 1)
                .greater_than(0)
                .and(user.id.not_equals(42).not_())
                .or(user.name.equals("Bob")),
        );
        q.returning(&post.user_id + 0)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT (q0_1.user_id + ?) AS expr FROM (SELECT q1_0.id AS id, q1_0.name AS name FROM public.users AS q1_0) AS q0_0 INNER JOIN LATERAL (SELECT q1_0.id AS id, q1_0.user_id AS user_id, q1_0.body AS body FROM public.posts AS q1_0 WHERE (q1_0.user_id = q0_0.id)) AS q0_1 ON TRUE WHERE (((((q0_0.id + ?) - ?) > ?) AND (NOT (q0_0.id <> ?))) OR (q0_0.name = ?))"#
    );
    assert_eq!(
        users_and_posts.params(),
        vec![
            BindValue::Int(0),
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
    let users_and_posts = TestConnection.select(|q| {
        let pair_select = TestConnection.select(|q| {
            let user = q.from::<User>();
            let post = q.join::<Post>(|post| post.user_id.equals(user.id));
            q.returning((user, post))
        });
        let pair = q.q(&pair_select);

        q.where_(pair.0.id.equals(&pair.1.user_id));
        q.returning(&pair.0.id + 0)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT (q0_0.t0_id + ?) AS expr FROM (SELECT q1_0.id AS t0_id, q1_0.name AS t0_name, q1_1.id AS t1_id, q1_1.user_id AS t1_user_id, q1_1.body AS t1_body FROM public.users AS q1_0 INNER JOIN public.posts AS q1_1 ON (q1_1.user_id = q1_0.id)) AS q0_0 WHERE (q0_0.t0_id = q0_0.t1_user_id)"#
    );
    assert_eq!(users_and_posts.params(), vec![BindValue::Int(0)]);
}

#[test]
fn select_accepts_primitive_literals_and_expression_operators() {
    let users = TestConnection.select(|q| {
        let user = q.from::<User>();
        let adjusted_id = user.id + 1;
        let scaled_id = (user.id * 2) / 2;
        assert_expr_kind::<AddExpr<UserId, i32>>(&adjusted_id);
        assert_expr_kind::<DivideExpr<MultiplyExpr<UserId, i32>, i32>>(&scaled_id);
        q.where_(
            ((user.id + 1 - 1).greater_than(0) & !user.id.not_equals(42)) | user.name.equals("Bob"),
        );
        q.where_((1 + user.id).less_than(100));
        q.where_(scaled_id.equals(user.id));
        q.where_((2 * user.id / 2).equals(user.id));
        q.returning(user)
    });

    assert_eq!(
        users.to_sql(),
        r#"SELECT q0_0.id AS id, q0_0.name AS name FROM public.users AS q0_0 WHERE (((((q0_0.id + ?) - ?) > ?) AND (NOT (q0_0.id <> ?))) OR (q0_0.name = ?)) AND ((? + q0_0.id) < ?) AND (((q0_0.id * ?) / ?) = q0_0.id) AND (((? * q0_0.id) / ?) = q0_0.id)"#
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
            BindValue::Int(2),
            BindValue::Int(2),
            BindValue::Int(2),
            BindValue::Int(2),
        ]
    );
}

#[test]
fn select_collects_source_and_filter_params_in_sql_order() {
    let users_and_posts = TestConnection.select(|q| {
        let user_select = TestConnection.select(|q| {
            let user = q.from::<User>();
            q.where_(user.id.greater_than(10));
            q.returning(user)
        });
        let user = q.q(&user_select);
        let post = q.join::<Post>(|post| post.user_id.equals(7));
        q.where_(user.name.equals("Ada"));
        q.returning(&user.id + post.user_id)
    });

    assert_eq!(
        users_and_posts.to_sql(),
        r#"SELECT (q0_0.id + q0_1.user_id) AS expr FROM (SELECT q1_0.id AS id, q1_0.name AS name FROM public.users AS q1_0 WHERE (q1_0.id > ?)) AS q0_0 INNER JOIN public.posts AS q0_1 ON (q0_1.user_id = ?) WHERE (q0_0.name = ?)"#
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
