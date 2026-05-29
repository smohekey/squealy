//! SQL ORM for Rust.

extern crate self as squealy;

mod expr;
mod generator;
mod query;
mod table;

pub use expr::Expr;
pub use generator::Generator;
pub use query::{Q, Query, query};
pub use squealy_macros::Table;
pub use table::{
    Column, ColumnExpr, ColumnName, ColumnSchema, ColumnValue, ForeignKeySchema, IndexSchema,
    Projectable, SelectColumn, Table,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Table)]
    #[index(name = "users_name_id_idx", columns = [name, id], unique)]
    struct User<'scope, C: Column = ColumnExpr> {
        #[column(primary_key, auto_increment, index)]
        id: C::Type<'scope, i32>,
        #[column(index, nullable, default = "anonymous", db_type = "text")]
        name: C::Type<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    struct Post<'scope, C: Column = ColumnExpr> {
        #[column(primary_key)]
        id: C::Type<'scope, i32>,
        #[column(
            index,
            references(table = "users", column = "id", on_delete = "cascade")
        )]
        user_id: C::Type<'scope, i32>,
        body: C::Type<'scope, String>,
    }

    struct TestGenerator;

    impl Generator for TestGenerator {
        fn write_table<T: Table>(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
            write!(writer, "CREATE TABLE {} (", T::name())?;
            for (index, column) in T::columns().iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                write!(
                    writer,
                    "{} {}",
                    column.name,
                    column.db_type.unwrap_or("text")
                )?;
                if column.primary_key {
                    writer.write_all(b" PRIMARY KEY")?;
                }
                if column.auto_increment {
                    writer.write_all(b" AUTOINCREMENT")?;
                }
                if !column.nullable {
                    writer.write_all(b" NOT NULL")?;
                }
                if let Some(default) = column.default {
                    write!(writer, " DEFAULT {default}")?;
                }
                if let Some(reference) = column.references {
                    write!(
                        writer,
                        " REFERENCES {}({})",
                        reference.table, reference.column
                    )?;
                    if let Some(on_delete) = reference.on_delete {
                        write!(writer, " ON DELETE {on_delete}")?;
                    }
                    if let Some(on_update) = reference.on_update {
                        write!(writer, " ON UPDATE {on_update}")?;
                    }
                }
            }
            writer.write_all(b")")?;

            for index in T::indexes() {
                let unique = if index.unique { "UNIQUE " } else { "" };
                let name = index.name.unwrap_or("unnamed_idx");
                let columns = index.columns.join(", ");
                write!(
                    writer,
                    "\nCREATE {unique}INDEX {name} ON {} ({columns})",
                    T::name()
                )?;
            }

            Ok(())
        }
    }

    fn posts_of_user(user_id: Expr<'static, i32>) -> Query<Post<'static, ColumnExpr>> {
        query(|q| {
            let post = q.q(Query::each::<Post>());
            q.where_(post.user_id.clone().equals(user_id));
            post
        })
    }

    #[test]
    fn derive_table_populates_table_metadata() {
        let columns = <User as Table>::column_names();
        let column_schema = <User as Table>::columns();
        let indexes = <User as Table>::indexes();

        assert_eq!(<User as Table>::name(), "users");
        assert_eq!(columns.id, "id");
        assert_eq!(columns.name, "name");
        assert_eq!(column_schema.len(), 2);
        assert!(column_schema[0].primary_key);
        assert!(column_schema[0].indexed);
        assert!(column_schema[0].auto_increment);
        assert!(column_schema[1].indexed);
        assert!(column_schema[1].nullable);
        assert_eq!(column_schema[1].default, Some("anonymous"));
        assert_eq!(column_schema[1].db_type, Some("text"));
        assert_eq!(indexes.len(), 3);
        assert_eq!(indexes[2].name, Some("users_name_id_idx"));
        assert_eq!(indexes[2].columns, &["name", "id"]);
        assert!(indexes[2].unique);
    }

    #[test]
    fn derive_table_populates_foreign_key_metadata() {
        let columns = <Post as Table>::columns();
        let user_id = &columns[1];
        let references = user_id.references.expect("user_id should reference users");

        assert!(user_id.indexed);
        assert_eq!(references.table, "users");
        assert_eq!(references.column, "id");
        assert_eq!(references.on_delete, Some("cascade"));
    }

    #[test]
    fn generator_creates_schema_sql() {
        let mut sql = Vec::new();
        TestGenerator.write_table::<User>(&mut sql).unwrap();
        let sql = String::from_utf8(sql).unwrap();

        assert!(sql.contains(
            "CREATE TABLE users (id text PRIMARY KEY AUTOINCREMENT NOT NULL, name text DEFAULT anonymous)"
        ));
        assert!(sql.contains("CREATE UNIQUE INDEX users_name_id_idx ON users (name, id)"));
    }

    #[test]
    fn each_selects_from_derived_table_metadata() {
        let users = Query::each::<User>();

        assert_eq!(
            users.to_sql(),
            r#"SELECT t0.id AS id, t0.name AS name FROM users AS t0"#
        );
    }

    #[test]
    fn query_composes_subqueries_with_lateral_joins() {
        let users_and_posts = query(|q| {
            let user = q.q(Query::each::<User>());
            let post = q.q(posts_of_user(user.id.clone()));
            (user, post)
        });

        assert_eq!(
            users_and_posts.to_sql(),
            r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM (SELECT t0.id AS id, t0.name AS name FROM users AS t0) AS q0_0 INNER JOIN LATERAL (SELECT q1_0.id AS id, q1_0.user_id AS user_id, q1_0.body AS body FROM (SELECT t0.id AS id, t0.user_id AS user_id, t0.body AS body FROM posts AS t0) AS q1_0 WHERE (q1_0.user_id = q0_0.id)) AS q0_1 ON TRUE"#
        );
    }
}
