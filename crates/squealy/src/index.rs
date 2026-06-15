/// Database schema metadata for an index.
pub trait Index: Sync {
    fn name(&self) -> Option<&'static str> {
        None
    }

    fn columns(&self) -> &'static [&'static str];

    fn unique(&self) -> bool {
        false
    }

    /// An optional partial-index predicate, rendered into `CREATE INDEX ... WHERE <predicate>`.
    ///
    /// Returns a function that lowers the index's typed `where = |row| ...` attribute to a
    /// self-contained ANSI SQL string (see [`render_ddl_predicate`](crate::render_ddl_predicate)).
    /// It is a function rather than a `&'static str` because the predicate is built from the
    /// table's column expressions; the model builder calls it once when constructing the schema
    /// model. Partial-index predicates are Postgres-only; other backends reject them.
    fn predicate(&self) -> Option<fn() -> String> {
        None
    }
}
