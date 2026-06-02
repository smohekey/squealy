use squealy::SchemaBackend;
use squealy::*;
use squealy_postgresql::Postgres;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct DefaultedRecord<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Counter<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    count: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
    users: User<'static, ColumnName>,
}

#[test]
fn postgres_select_uses_numbered_placeholders() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .select(|(user,)| user.id + 2);

    assert_eq!(
        users.to_sql(),
        "SELECT (q0_0.\"id\" + $1) AS \"expr\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $2)"
    );
    let mut written = Vec::new();
    users.write_params(&mut written).unwrap();
    assert_eq!(written, vec![BindValue::Int(2), BindValue::Int(1)]);
    assert_eq!(
        users.collect_params(),
        vec![BindValue::Int(2), BindValue::Int(1)]
    );
}

#[test]
fn postgres_division_renders_fractional_result() {
    let users = Postgres.from::<User>().select(|(user,)| user.id / 2);

    assert_eq!(
        users.to_sql(),
        "SELECT (CAST(q0_0.\"id\" AS double precision) / CAST($1 AS double precision)) AS \"expr\" FROM \"public\".\"users\" AS q0_0"
    );
    assert_eq!(users.collect_params(), vec![BindValue::Int(2)]);
}

#[test]
fn postgres_runtime_prepared_params_render_without_captured_values() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.name.equals(param::<UserName>()))
        .select(|(user,)| user.name);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"name\" = $1)"
    );
    assert_eq!(users.collect_params(), Vec::<BindValue>::new());
}

#[test]
fn postgres_runtime_prepared_assignment_params_render_without_captured_values() {
    let insert = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .insert_returning(|user| user.id);
    let insert_multiple = Postgres
        .to_columns::<User, (UserName,)>()
        .row((param::<UserName>(),))
        .row((param::<UserName>(),))
        .insert_returning(|user| user.id);
    let update = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .where_(|user| user.id.equals(param::<UserId>()))
        .update_returning(|user| user.name);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        insert_multiple.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1), ($2) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"name\" AS \"name\""
    );
    assert_eq!(insert.collect_params(), Vec::<BindValue>::new());
    assert_eq!(insert_multiple.collect_params(), Vec::<BindValue>::new());
    assert_eq!(update.collect_params(), Vec::<BindValue>::new());
}

#[test]
fn postgres_update_renders_explicit_defaults() {
    let update = Postgres
        .to::<User>()
        .name(default())
        .where_(|user| user.id.equals(1))
        .update_returning(|user| user.name);

    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = DEFAULT WHERE (q0_0.\"id\" = $1) RETURNING q0_0.\"name\" AS \"name\""
    );
    assert_eq!(update.collect_params(), vec![BindValue::Int(1)]);
}

#[test]
fn postgres_explicit_update_columns_render_expression_assignments() {
    let update = Postgres
        .to_columns::<Counter, (CounterCount,)>()
        .set(|counter| (counter.count + 1,))
        .where_(|counter| counter.id.equals(7))
        .update_returning(|counter| counter.count);

    assert_eq!(
        update.to_sql(),
        "UPDATE \"counters\" AS q0_0 SET \"count\" = (q0_0.\"count\" + $1) WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"count\" AS \"count\""
    );
    assert_eq!(
        update.collect_params(),
        vec![BindValue::Int(1), BindValue::Int(7)]
    );
}

#[test]
fn postgres_source_first_select_renders_from_backend_selected_ast() {
    let users = Postgres
        .from::<User>()
        .order_by(|(user,)| (user.id + 2).desc())
        .where_(|(user,)| user.id.equals(1))
        .limit(10)
        .offset(5)
        .select(|(user,)| user.name);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.\"name\" AS \"name\" FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $1) ORDER BY (q0_0.\"id\" + $2) DESC LIMIT 10 OFFSET 5"
    );
    assert_eq!(
        users.collect_params(),
        vec![BindValue::Int(1), BindValue::Int(2)]
    );
}

#[test]
fn postgres_insert_update_and_delete_render_returning() {
    let insert = Postgres
        .to::<User>()
        .name("Ada")
        .insert_returning(|user| user.id);
    let update = Postgres
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update_returning(|user| (user.id, user.name));
    let delete = Postgres
        .from::<User>()
        .where_(|user| user.id.equals(1))
        .delete_returning(|user| user);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING q0_0.\"id\" AS \"t0_id\", q0_0.\"name\" AS \"t1_name\""
    );
    assert_eq!(
        delete.to_sql(),
        "DELETE FROM \"public\".\"users\" AS q0_0 WHERE (q0_0.\"id\" = $1) RETURNING q0_0.\"id\" AS \"id\", q0_0.\"name\" AS \"name\""
    );
    assert_eq!(
        insert.collect_params(),
        vec![BindValue::Text("Ada".to_owned())]
    );
    assert_eq!(
        update.collect_params(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::Int(1)]
    );
    assert_eq!(delete.collect_params(), vec![BindValue::Int(1)]);
}

#[test]
fn postgres_insert_renders_multiple_rows() {
    let insert = Postgres
        .to_columns::<User, (UserName,)>()
        .row(("Ada",))
        .row(("Grace",))
        .insert_returning(|user| user.id);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1), ($2) RETURNING \"id\" AS \"id\""
    );
    assert_eq!(
        insert.collect_params(),
        vec![
            BindValue::Text("Ada".to_owned()),
            BindValue::Text("Grace".to_owned())
        ]
    );
}

#[test]
fn postgres_insert_renders_explicit_defaults() {
    let insert = Postgres
        .to_columns::<User, (UserName,)>()
        .row((default(),))
        .row(("Grace",))
        .insert_returning(|user| user.id + 1);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES (DEFAULT), ($1) RETURNING (\"id\" + $2) AS \"expr\""
    );
    assert_eq!(
        insert.collect_params(),
        vec![BindValue::Text("Grace".to_owned()), BindValue::Int(1)]
    );
}

#[test]
fn postgres_insert_can_use_default_values() {
    let insert = Postgres
        .to::<DefaultedRecord>()
        .insert_returning(|record| record.id);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"defaulted_records\" DEFAULT VALUES RETURNING \"id\" AS \"id\""
    );
    assert_eq!(insert.collect_params(), Vec::<BindValue>::new());
}

#[test]
fn postgres_mutation_returning_expressions_continue_placeholder_numbering() {
    let insert = Postgres
        .to::<User>()
        .name("Ada")
        .insert_returning(|user| user.id + 1);
    let update = Postgres
        .to::<User>()
        .name("Ada")
        .where_(|user| user.id.equals(1))
        .update_returning(|user| user.id + 2);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO \"public\".\"users\" (\"name\") VALUES ($1) RETURNING (\"id\" + $2) AS \"expr\""
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE \"public\".\"users\" AS q0_0 SET \"name\" = $1 WHERE (q0_0.\"id\" = $2) RETURNING (q0_0.\"id\" + $3) AS \"expr\""
    );
    assert_eq!(
        insert.collect_params(),
        vec![BindValue::Text("Ada".to_owned()), BindValue::Int(1)]
    );
    assert_eq!(
        update.collect_params(),
        vec![
            BindValue::Text("Ada".to_owned()),
            BindValue::Int(1),
            BindValue::Int(2),
        ]
    );
}

#[test]
fn postgres_backend_writes_table_ddl() {
    let mut sql = Vec::new();
    let tables = <Public as Schema>::tables().collect::<Vec<_>>();
    Postgres.write_table(tables[0], &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert_eq!(
        sql,
        "CREATE TABLE \"public\".\"users\" (\"id\" integer PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY NOT NULL, \"name\" text NOT NULL)"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Account<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
}

// Reserved words, defaults, nullable, foreign keys, and multiple unnamed indexes
// all exercise the DDL identifier-quoting and index-naming paths.
#[derive(Clone, Debug, PartialEq, Table)]
#[index(columns = [email])]
#[index(columns = [order, select])]
struct Member<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,

    #[column(references(Account::id, on_delete = "cascade"))]
    account_id: C::Type<'scope, i32>,

    // `order` is a reserved word; it must be quoted to produce valid DDL.
    order: C::Type<'scope, i32>,

    #[column(nullable)]
    select: C::Type<'scope, i32>,

    #[column(default = value("anonymous"))]
    email: C::Type<'scope, String>,
}

fn member_metadata() -> Member<'static, ColumnName> {
    <Member<'static> as SchemaTable>::column_names()
}

fn render_ddl(table: &(dyn Table + Sync)) -> String {
    let mut sql = Vec::new();
    Postgres.write_table(table, &mut sql).unwrap();
    String::from_utf8(sql).unwrap()
}

#[test]
fn postgres_ddl_quotes_reserved_word_identifiers() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    // Reserved-word column names are quoted so the DDL stays valid.
    assert!(
        sql.contains("\"order\" integer NOT NULL"),
        "reserved-word column not quoted: {sql}"
    );
    assert!(
        sql.contains("\"select\" integer"),
        "nullable reserved-word column missing: {sql}"
    );
    // The nullable column has no NOT NULL constraint.
    assert!(
        !sql.contains("\"select\" integer NOT NULL"),
        "nullable column should not be NOT NULL: {sql}"
    );
}

#[test]
fn postgres_ddl_renders_foreign_key_and_default() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    assert!(
        sql.contains(
            "\"account_id\" integer NOT NULL REFERENCES \"accounts\"(\"id\") ON DELETE cascade"
        ),
        "foreign key not rendered as expected: {sql}"
    );
    assert!(
        sql.contains("\"email\" text NOT NULL DEFAULT 'anonymous'"),
        "default literal not rendered as expected: {sql}"
    );
}

#[test]
fn postgres_ddl_gives_unnamed_indexes_distinct_names() {
    let table = member_metadata();
    let sql = render_ddl(&table);

    // Each unnamed index gets a deterministic, distinct name derived from its columns.
    assert!(
        sql.contains("CREATE INDEX \"idx_members_email\" ON \"members\" (\"email\")"),
        "first unnamed index missing or wrong: {sql}"
    );
    assert!(
        sql.contains(
            "CREATE INDEX \"idx_members_order_select\" ON \"members\" (\"order\", \"select\")"
        ),
        "second unnamed index missing or wrong: {sql}"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
struct Accented<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(name = "café")]
    cafe: C::Type<'scope, String>,
}

#[test]
fn postgres_renders_non_ascii_identifiers() {
    // The string-backed SQL writer validates each write chunk as UTF-8, so quoting
    // must emit whole characters rather than individual bytes. Rendering a multibyte
    // identifier through to_sql() would otherwise panic mid-character.
    let query = Postgres
        .from::<Accented>()
        .select(|(row,)| (row.id, row.cafe));

    assert_eq!(
        query.to_sql(),
        "SELECT q0_0.\"id\" AS \"t0_id\", q0_0.\"café\" AS \"t1_café\" FROM \"accenteds\" AS q0_0"
    );
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Tenant<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(unique)]
    slug: C::Type<'scope, String>,
}

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Catalog)]
struct Membership<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    #[column(index, references(Tenant::id, on_delete = "cascade"))]
    tenant_id: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Catalog {
    tenants: Tenant<'static, ColumnName>,
    memberships: Membership<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct CatalogDb {
    catalog: Catalog,
}

#[test]
fn postgres_renders_create_from_scratch() {
    let model = DatabaseModel::from_database::<CatalogDb>();
    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    // Phases: namespace, tables (with inline PK/unique), indexes, then FKs as ALTER ADD CONSTRAINT.
    assert_eq!(
        sql,
        "CREATE SCHEMA IF NOT EXISTS \"catalog\";\n\
CREATE TABLE \"catalog\".\"tenants\" (\n  \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n  \"slug\" text NOT NULL,\n  CONSTRAINT \"pk_tenants\" PRIMARY KEY (\"id\"),\n  CONSTRAINT \"uq_tenants_slug\" UNIQUE (\"slug\")\n);\n\
CREATE TABLE \"catalog\".\"memberships\" (\n  \"id\" integer GENERATED BY DEFAULT AS IDENTITY NOT NULL,\n  \"tenant_id\" integer NOT NULL,\n  CONSTRAINT \"pk_memberships\" PRIMARY KEY (\"id\")\n);\n\
CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" (\"tenant_id\");\n\
ALTER TABLE \"catalog\".\"memberships\" ADD CONSTRAINT \"fk_memberships_tenant_id\" FOREIGN KEY (\"tenant_id\") REFERENCES \"catalog\".\"tenants\" (\"id\") ON DELETE CASCADE;"
    );
}

#[test]
fn postgres_renders_partial_indexes() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("catalog".to_owned()),
            tables: vec![TableModel {
                name: "memberships".to_owned(),
                columns: vec![ColumnModel {
                    name: "tenant_id".to_owned(),
                    ty: SqlType::I32,
                    nullable: false,
                    default: None,
                    identity: None,
                    generated: None,
                }],
                primary_key: None,
                foreign_keys: Vec::new(),
                uniques: Vec::new(),
                checks: Vec::new(),
                indexes: vec![IndexModel {
                    name: "idx_memberships_tenant_id".to_owned(),
                    columns: vec!["tenant_id".to_owned()],
                    unique: false,
                    method: Some(IndexMethod::BTree),
                    directions: vec![IndexDirection::Desc],
                    predicate: Some("(tenant_id > 0)".to_owned()),
                }],
            }],
        }],
    };

    let mut sql = Vec::new();
    Postgres.render_create(&model, &mut sql).unwrap();
    let sql = String::from_utf8(sql).unwrap();

    assert!(
        sql.contains(
            "CREATE INDEX \"idx_memberships_tenant_id\" ON \"catalog\".\"memberships\" USING btree (\"tenant_id\" DESC) WHERE (tenant_id > 0)"
        ),
        "partial index not rendered as expected: {sql}"
    );
}
