//! Regression test for git-bug 4d4ec99: the query builder must be usable inside a generic async
//! trait whose methods return `-> impl Future + Send` (the shape required when a `T: Store` is shared
//! across threads, e.g. an async-graphql resolver or a generic multithreaded service).
//!
//! This is a COMPILE test — the trait impl below type-checks but is never executed, so it needs no
//! live database. Before the fix, the `insert` / `update` / `delete` terminals returned
//! `impl Future` without `+ Send`, so the compiler tried to prove `Send` by leaking the auto-trait
//! through the lifetime-specific execution-query impls and failed with "implementation of
//! `ExecutableDeleteQuery` is not general enough" (rust-lang/rust#100013). `select` already worked.

// This is a type-check-only test: the `Store` impl is never executed, so its items read as dead.
#![allow(dead_code)]

use std::future::Future;

use squealy::*;
use squealy_postgresql::{PostgresConnection, PostgresError};

#[derive(Clone, Debug, PartialEq, Table)]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
    #[column(primary_key, auto_increment)]
    id: C::Type<'scope, i32>,
    tenant_id: C::Type<'scope, i32>,
    name: C::Type<'scope, String>,
}

/// Every method returns a `Send` future, so a `T: Store` can be driven across threads.
trait Store {
    fn fetch_one(&self, id: i32)
    -> impl Future<Output = Result<Option<i32>, PostgresError>> + Send;

    fn list(&self) -> impl Future<Output = Result<Vec<i32>, PostgresError>> + Send;

    fn create(&self, name: String) -> impl Future<Output = Result<u64, PostgresError>> + Send;

    fn rename(
        &self,
        id: i32,
        name: String,
    ) -> impl Future<Output = Result<u64, PostgresError>> + Send;

    fn remove(&self, a: i32, b: i32) -> impl Future<Output = Result<(), PostgresError>> + Send;
}

struct PgStore {
    conn: PostgresConnection,
}

impl Store for PgStore {
    async fn fetch_one(&self, id: i32) -> Result<Option<i32>, PostgresError> {
        // select -> fetch_optional
        self.conn
            .from::<Widget>()
            .where_(|w| w.id.equals(id))
            .select(|(w,)| w.id)
            .fetch_optional()
            .await
    }

    async fn list(&self) -> Result<Vec<i32>, PostgresError> {
        // select -> collect
        self.conn
            .from::<Widget>()
            .select(|(w,)| w.id)
            .collect()
            .await
    }

    async fn create(&self, name: String) -> Result<u64, PostgresError> {
        // insert
        self.conn
            .to::<Widget>()
            .tenant_id(1)
            .name(name)
            .insert()
            .await
    }

    async fn rename(&self, id: i32, name: String) -> Result<u64, PostgresError> {
        // update
        self.conn
            .to::<Widget>()
            .name(name)
            .where_(|w| w.id.equals(id))
            .update()
            .await
    }

    async fn remove(&self, a: i32, b: i32) -> Result<(), PostgresError> {
        // delete
        self.conn
            .from::<Widget>()
            .where_(|w| w.id.equals(a).and(w.tenant_id.equals(b)))
            .delete()
            .await?;
        Ok(())
    }
}

// The real-world trigger: a `Store` is used generically and shared across threads.
fn assert_store_is_send_sync<T: Store + Send + Sync>() {}

#[test]
fn query_builder_works_behind_send_async_trait() {
    assert_store_is_send_sync::<PgStore>();
}
