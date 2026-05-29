use crate::Table;

/// A database schema namespace that can contain tables.
pub trait Schema {
    fn name() -> Option<&'static str>;

    fn tables() -> impl Iterator<Item = &'static (dyn Table + Sync)> {
        [].into_iter()
    }
}

/// Object-safe schema metadata exposed through database membership.
pub trait DatabaseSchema: Sync {
    fn name(&self) -> Option<&'static str>;

    fn tables(&self) -> Box<dyn Iterator<Item = &'static (dyn Table + Sync)> + '_>;
}

/// The default schema namespace for backends that do not need explicit qualification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefaultSchema {}

impl Schema for DefaultSchema {
    fn name() -> Option<&'static str> {
        None
    }
}
