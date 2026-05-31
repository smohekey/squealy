/// Database schema metadata for an index.
pub trait Index: Sync {
    fn name(&self) -> Option<&'static str> {
        None
    }

    fn columns(&self) -> &'static [&'static str];

    fn unique(&self) -> bool {
        false
    }
}
