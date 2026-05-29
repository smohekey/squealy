//! SQL ORM for Rust.

extern crate self as squealy;

mod expr;
mod query;
mod table;

pub use expr::Expr;
pub use query::{Q, Query, query};
pub use squealy_macros::Table;
pub use table::{Column, ColumnExpr, ColumnName, ColumnValue, Projectable, SelectColumn, Table};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Table)]
    struct User<'scope, Column: crate::Column = ColumnExpr> {
        id: Column::T<'scope, i32>,
        name: Column::T<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    struct Post<'scope, Column: crate::Column = ColumnExpr> {
        id: Column::T<'scope, i32>,
        user_id: Column::T<'scope, i32>,
        body: Column::T<'scope, String>,
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

        assert_eq!(<User as Table>::name(), "users");
        assert_eq!(columns.id, "id");
        assert_eq!(columns.name, "name");
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
