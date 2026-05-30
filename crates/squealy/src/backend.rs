use std::io::{self, Write};

use crate::{ProjectionShape, SelectQuery, Table};

/// Backend-specific DDL generation.
pub trait Backend: Sized {
    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}

/// A backend connection that constructs select objects tied to that backend.
pub trait Connection: Sized {
    type Error;

    type Select<'conn, Shape>: SelectQuery<'conn, Connection = Self, Shape = Shape>
    where
        Self: 'conn,
        Shape: ProjectionShape;

    fn select<Shape>(
        &self,
        f: impl for<'scope> FnOnce(
            &mut crate::SelectBuilder<'_, 'scope, Self>,
        ) -> <Shape as ProjectionShape>::Exprs<'scope>,
    ) -> Self::Select<'_, Shape>
    where
        Shape: ProjectionShape;
}
