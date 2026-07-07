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
    /// Returns a function that lowers the index's typed `where = |row| ...` attribute to a neutral
    /// [`ExprNode`](crate::ExprNode) (see [`build_ddl_predicate`](crate::build_ddl_predicate)), which
    /// each backend renders in its own dialect. It is a function rather than a value because the
    /// predicate is built from the table's column expressions; the model builder calls it once when
    /// constructing the schema model. Partial-index predicates are Postgres-only; other backends reject
    /// them.
    fn predicate(&self) -> Option<fn() -> crate::ExprNode> {
        None
    }
}
