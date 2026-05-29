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
    Projectable, SelectColumn, Table, TableSchema,
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
        fn create_table_statement(&self, schema: &TableSchema) -> String {
            let columns = schema
                .columns
                .iter()
                .map(|column| self.column_definition(column))
                .collect::<Vec<_>>()
                .join(", ");
            format!("CREATE TABLE {} ({columns})", schema.name)
        }

        fn create_index_statement(&self, table: &TableSchema, index: &IndexSchema) -> String {
            let unique = if index.unique { "UNIQUE " } else { "" };
            let name = index.name.unwrap_or("unnamed_idx");
            let columns = index.columns.join(", ");
            format!("CREATE {unique}INDEX {name} ON {} ({columns})", table.name)
        }

        fn column_definition(&self, column: &ColumnSchema) -> String {
            let mut definition = format!("{} {}", column.name, column.db_type.unwrap_or("text"));
            if column.primary_key {
                definition.push_str(" PRIMARY KEY");
            }
            if column.auto_increment {
                definition.push_str(" AUTOINCREMENT");
            }
            if !column.nullable {
                definition.push_str(" NOT NULL");
            }
            if let Some(default) = column.default {
                definition.push_str(&format!(" DEFAULT {default}"));
            }
            if let Some(reference) = column.references {
                definition.push(' ');
                definition.push_str(&self.foreign_key_reference(&reference));
            }
            definition
        }

        fn foreign_key_reference(&self, reference: &ForeignKeySchema) -> String {
            let mut sql = format!("REFERENCES {}({})", reference.table, reference.column);
            if let Some(on_delete) = reference.on_delete {
                sql.push_str(&format!(" ON DELETE {on_delete}"));
            }
            if let Some(on_update) = reference.on_update {
                sql.push_str(&format!(" ON UPDATE {on_update}"));
            }
            sql
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
        let schema = <User as Table>::schema();

        assert_eq!(<User as Table>::name(), "users");
        assert_eq!(columns.id, "id");
        assert_eq!(columns.name, "name");
        assert_eq!(schema.name, "users");
        assert_eq!(schema.columns.len(), 2);
        assert!(schema.columns[0].primary_key);
        assert!(schema.columns[0].indexed);
        assert!(schema.columns[0].auto_increment);
        assert!(schema.columns[1].indexed);
        assert!(schema.columns[1].nullable);
        assert_eq!(schema.columns[1].default, Some("anonymous"));
        assert_eq!(schema.columns[1].db_type, Some("text"));
        assert_eq!(schema.indexes.len(), 3);
        assert_eq!(schema.indexes[2].name, Some("users_name_id_idx"));
        assert_eq!(schema.indexes[2].columns, &["name", "id"]);
        assert!(schema.indexes[2].unique);
    }

    #[test]
    fn derive_table_populates_foreign_key_metadata() {
        let schema = <Post as Table>::schema();
        let user_id = &schema.columns[1];
        let references = user_id.references.expect("user_id should reference users");

        assert!(user_id.indexed);
        assert_eq!(references.table, "users");
        assert_eq!(references.column, "id");
        assert_eq!(references.on_delete, Some("cascade"));
    }

    #[test]
    fn generator_creates_schema_sql() {
        let statements = TestGenerator.create_table(&<User as Table>::schema());

        assert_eq!(
            statements[0],
            "CREATE TABLE users (id text PRIMARY KEY AUTOINCREMENT NOT NULL, name text DEFAULT anonymous)"
        );
        assert_eq!(
            statements[3],
            "CREATE UNIQUE INDEX users_name_id_idx ON users (name, id)"
        );
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
