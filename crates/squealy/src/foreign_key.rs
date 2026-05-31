/// Database schema metadata for a foreign-key reference.
pub trait ForeignKey: Sync {
    fn schema_name(&self) -> Option<&'static str> {
        None
    }

    fn table(&self) -> &'static str;

    fn column(&self) -> &'static str;

    fn on_delete(&self) -> Option<&'static str> {
        None
    }

    fn on_update(&self) -> Option<&'static str> {
        None
    }
}
