/// Database schema metadata for a foreign-key reference.
pub trait ForeignKey: Sync {
	fn schema_name(&self) -> Option<&'static str> {
		None
	}

	fn table(&self) -> &'static str;

	fn column(&self) -> &'static str;

	/// The referential action for `ON DELETE`, emitted verbatim into DDL.
	///
	/// This string is rendered into the generated `REFERENCES ... ON DELETE
	/// <value>` clause without escaping or validation, so it must be a valid
	/// backend action keyword such as `cascade` or `set null`. It originates
	/// from a compile-time `references(..., on_delete = "...")` attribute, not
	/// runtime input.
	fn on_delete(&self) -> Option<&'static str> {
		None
	}

	/// The referential action for `ON UPDATE`, emitted verbatim into DDL.
	///
	/// See [`ForeignKey::on_delete`] for the escaping and trust caveats.
	fn on_update(&self) -> Option<&'static str> {
		None
	}
}
