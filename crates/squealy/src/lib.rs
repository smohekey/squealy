//! SQL ORM for Rust.

extern crate self as squealy;

mod expr;
mod generator;
mod query;
mod table;

pub use expr::Expr;
pub use generator::Generator;
pub use query::{Q, Query, query};
pub use squealy_macros::{Database, Schema, Table};
pub use table::{
    Column, ColumnExpr, ColumnMode, ColumnName, ColumnValue, Database, DatabaseSchema,
    DefaultSchema, ForeignKey, Index, Projectable, Schema, SchemaTable, SelectColumn, Table,
};

#[cfg(test)]
mod tests {
    use super::*;

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

    struct TestGenerator;

    impl Generator for TestGenerator {
        fn write_table<T: SchemaTable>(
            &self,
            writer: &mut impl std::io::Write,
        ) -> std::io::Result<()> {
            write!(
                writer,
                "CREATE TABLE {} (",
                <T as SchemaTable>::qualified_name()
            )?;
            for (index, column) in <T as SchemaTable>::columns().iter().enumerate() {
                if index > 0 {
                    writer.write_all(b", ")?;
                }
                write!(
                    writer,
                    "{} {}",
                    column.name(),
                    column.db_type().unwrap_or("text")
                )?;
                if column.primary_key() {
                    writer.write_all(b" PRIMARY KEY")?;
                }
                if column.auto_increment() {
                    writer.write_all(b" AUTOINCREMENT")?;
                }
                if !column.nullable() {
                    writer.write_all(b" NOT NULL")?;
                }
                if let Some(default) = column.default() {
                    write!(writer, " DEFAULT {default}")?;
                }
                if let Some(reference) = column.references() {
                    write!(
                        writer,
                        " REFERENCES {}{}({})",
                        reference
                            .schema_name()
                            .map(|schema| format!("{schema}."))
                            .unwrap_or_default(),
                        reference.table(),
                        reference.column()
                    )?;
                    if let Some(on_delete) = reference.on_delete() {
                        write!(writer, " ON DELETE {on_delete}")?;
                    }
                    if let Some(on_update) = reference.on_update() {
                        write!(writer, " ON UPDATE {on_update}")?;
                    }
                }
            }
            writer.write_all(b")")?;

            for index in <T as SchemaTable>::indexes() {
                let unique = if index.unique() { "UNIQUE " } else { "" };
                let name = index.name().unwrap_or("unnamed_idx");
                let columns = index.columns().join(", ");
                write!(
                    writer,
                    "\nCREATE {unique}INDEX {name} ON {} ({columns})",
                    <T as SchemaTable>::qualified_name()
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
        let columns = <User as SchemaTable>::column_names();
        let column_metadata = <User as SchemaTable>::columns();
        let indexes = <User as SchemaTable>::indexes();

        assert_eq!(<User as SchemaTable>::schema_name(), Some("public"));
        assert_eq!(<User as SchemaTable>::name(), "users");
        assert_eq!(<User as SchemaTable>::qualified_name(), "public.users");
        assert_eq!(<Post as SchemaTable>::schema_name(), Some("public"));
        assert_eq!(<Post as SchemaTable>::qualified_name(), "public.posts");
        assert_eq!(<Public as Schema>::name(), Some("public"));
        let schema_tables = <Public as Schema>::tables().collect::<Vec<_>>();
        assert_eq!(schema_tables.len(), 2);
        assert_eq!(schema_tables[0].qualified_name(), "public.users");
        assert_eq!(schema_tables[1].qualified_name(), "public.posts");
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
        let columns = <Post as SchemaTable>::columns();
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
    fn generator_creates_schema_sql() {
        let mut sql = Vec::new();
        TestGenerator.write_table::<User>(&mut sql).unwrap();
        let sql = String::from_utf8(sql).unwrap();

        assert!(sql.contains(
            "CREATE TABLE public.users (id text PRIMARY KEY AUTOINCREMENT NOT NULL, name text DEFAULT anonymous)"
        ));
        assert!(sql.contains("CREATE UNIQUE INDEX users_name_id_idx ON public.users (name, id)"));

        let mut sql = Vec::new();
        TestGenerator.write_table::<Post>(&mut sql).unwrap();
        let sql = String::from_utf8(sql).unwrap();

        assert!(sql.contains("REFERENCES public.users(id) ON DELETE cascade"));
    }

    #[test]
    fn each_selects_from_derived_table_metadata() {
        let users = Query::each::<User>();

        assert_eq!(
            users.to_sql(),
            r#"SELECT t0.id AS id, t0.name AS name FROM public.users AS t0"#
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
            r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM (SELECT t0.id AS id, t0.name AS name FROM public.users AS t0) AS q0_0 INNER JOIN LATERAL (SELECT q1_0.id AS id, q1_0.user_id AS user_id, q1_0.body AS body FROM (SELECT t0.id AS id, t0.user_id AS user_id, t0.body AS body FROM public.posts AS t0) AS q1_0 WHERE (q1_0.user_id = q0_0.id)) AS q0_1 ON TRUE"#
        );
    }
}
