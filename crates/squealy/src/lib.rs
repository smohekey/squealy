//! SQL ORM for Rust.

extern crate self as squealy;

mod expr;
mod query;
mod table;

pub use expr::Expr;
pub use query::{Q, Query, query};
pub use squealy_macros::Table;
pub use table::{
    ExprMode, NameMode, Projectable, SelectColumn, Table, TableMode, TableSchema, ValueMode,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Clone, Debug, PartialEq, Table)]
    struct User<'scope, Mode: TableMode = ExprMode> {
        id: Mode::T<'scope, i32>,
        name: Mode::T<'scope, String>,
    }

    #[derive(Clone, Debug, PartialEq, Table)]
    struct Post<'scope, Mode: TableMode = ExprMode> {
        id: Mode::T<'scope, i32>,
        user_id: Mode::T<'scope, i32>,
        body: Mode::T<'scope, String>,
    }

    const USERS: TableSchema<User<'static, NameMode>> = TableSchema {
        name: "users",
        columns: User {
            id: "id",
            name: "name",
        },
    };

    const POSTS: TableSchema<Post<'static, NameMode>> = TableSchema {
        name: "posts",
        columns: Post {
            id: "id",
            user_id: "user_id",
            body: "body",
        },
    };

    fn posts_of_user(user_id: Expr<'static, i32>) -> Query<Post<'static, ExprMode>> {
        query(|q| {
            let post = q.q(Query::each(&POSTS));
            q.where_(post.user_id.clone().equals(user_id));
            post
        })
    }

    #[test]
    fn each_selects_from_schema() {
        let users = Query::each(&USERS);

        assert_eq!(
            users.to_sql(),
            r#"SELECT t0.id AS id, t0.name AS name FROM users AS t0"#
        );
    }

    #[test]
    fn query_composes_subqueries_with_lateral_joins() {
        let users_and_posts = query(|q| {
            let user = q.q(Query::each(&USERS));
            let post = q.q(posts_of_user(user.id.clone()));
            (user, post)
        });

        assert_eq!(
            users_and_posts.to_sql(),
            r#"SELECT q0_0.id AS left_id, q0_0.name AS left_name, q0_1.id AS right_id, q0_1.user_id AS right_user_id, q0_1.body AS right_body FROM (SELECT t0.id AS id, t0.name AS name FROM users AS t0) AS q0_0 INNER JOIN LATERAL (SELECT q1_0.id AS id, q1_0.user_id AS user_id, q1_0.body AS body FROM (SELECT t0.id AS id, t0.user_id AS user_id, t0.body AS body FROM posts AS t0) AS q1_0 WHERE (q1_0.user_id = q0_0.id)) AS q0_1 ON TRUE"#
        );
    }
}
