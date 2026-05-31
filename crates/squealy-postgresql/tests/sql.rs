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
        "SELECT (q0_0.id + $1) AS expr FROM public.users AS q0_0 WHERE (q0_0.id = $2)"
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
fn postgres_runtime_prepared_params_render_without_captured_values() {
    let users = Postgres
        .from::<User>()
        .where_(|user| user.name.equals(param::<UserName>()))
        .select(|(user,)| user.name);

    assert_eq!(
        users.to_sql(),
        "SELECT q0_0.name AS name FROM public.users AS q0_0 WHERE (q0_0.name = $1)"
    );
    assert_eq!(users.collect_params(), Vec::<BindValue>::new());
}

#[test]
fn postgres_runtime_prepared_assignment_params_render_without_captured_values() {
    let insert = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .insert_returning(|user| user.id);
    let update = Postgres
        .to::<User>()
        .name(param::<UserName>())
        .where_(|user| user.id.equals(param::<UserId>()))
        .update_returning(|user| user.name);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO public.users (name) VALUES ($1) RETURNING id AS id"
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE public.users AS q0_0 SET name = $1 WHERE (q0_0.id = $2) RETURNING q0_0.name AS name"
    );
    assert_eq!(insert.collect_params(), Vec::<BindValue>::new());
    assert_eq!(update.collect_params(), Vec::<BindValue>::new());
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
        "SELECT q0_0.name AS name FROM public.users AS q0_0 WHERE (q0_0.id = $1) ORDER BY (q0_0.id + $2) DESC LIMIT 10 OFFSET 5"
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
        "INSERT INTO public.users (name) VALUES ($1) RETURNING id AS id"
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE public.users AS q0_0 SET name = $1 WHERE (q0_0.id = $2) RETURNING q0_0.id AS t0_id, q0_0.name AS t1_name"
    );
    assert_eq!(
        delete.to_sql(),
        "DELETE FROM public.users AS q0_0 WHERE (q0_0.id = $1) RETURNING q0_0.id AS id, q0_0.name AS name"
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
fn postgres_insert_can_use_default_values() {
    let insert = Postgres
        .to::<DefaultedRecord>()
        .insert_returning(|record| record.id);

    assert_eq!(
        insert.to_sql(),
        "INSERT INTO defaulted_records DEFAULT VALUES RETURNING id AS id"
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
        "INSERT INTO public.users (name) VALUES ($1) RETURNING (id + $2) AS expr"
    );
    assert_eq!(
        update.to_sql(),
        "UPDATE public.users AS q0_0 SET name = $1 WHERE (q0_0.id = $2) RETURNING (q0_0.id + $3) AS expr"
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
        "CREATE TABLE public.users (id integer PRIMARY KEY GENERATED BY DEFAULT AS IDENTITY NOT NULL, name text NOT NULL)"
    );
}
