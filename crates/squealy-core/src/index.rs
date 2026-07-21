/// Database schema metadata for an index.
pub trait Index: Sync {
	fn name(&self) -> Option<&'static str> {
		None
	}

	fn columns(&self) -> &'static [&'static str];

	fn unique(&self) -> bool {
		false
	}

	/// An optional structural partial-index predicate.
	///
	/// Returns a function that lowers the index's typed `where = |row| ...` attribute to a neutral
	/// [`ExprNode`](crate::ExprNode) (see [`build_schema_predicate`](crate::build_schema_predicate)).
	/// It is a function rather than a value because the
	/// predicate is built from the table's column expressions; the model builder calls it once when
	/// constructing the owned metadata.
	fn predicate(&self) -> Option<fn() -> crate::ExprNode> {
		None
	}
}
