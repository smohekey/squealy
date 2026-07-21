use crate::DatabaseSchema;

/// A database that can contain schemas.
pub trait Database {
	fn schemas() -> impl Iterator<Item = &'static (dyn DatabaseSchema + Sync)> {
		[].into_iter()
	}
}
