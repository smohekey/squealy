use std::io::{self, Write};

use crate::{ProjectionShape, Query, Table};

/// Backend-specific SQL generation.
pub trait Backend: Sized {
    /// Generate backend-specific SQL for a query.
    fn write_query<Q>(&self, query: &Q, writer: &mut impl Write) -> io::Result<()>
    where
        Self: crate::Connection,
        Q: Query<Connection = Self>;

    /// Generate backend-specific SQL for a table.
    fn write_table(&self, table: &(dyn Table + Sync), writer: &mut impl Write) -> io::Result<()>;
}

/// A backend connection that constructs query objects tied to that backend.
pub trait Connection: Backend {
    type Query<Shape>: Query<Connection = Self, Shape = Shape>
    where
        Shape: ProjectionShape;

    fn query<Shape>(
        &self,
        f: impl for<'scope> FnOnce(
            &mut crate::Q<'scope, Self>,
        ) -> <Shape as ProjectionShape>::Exprs<'scope>,
    ) -> Self::Query<Shape>
    where
        Shape: ProjectionShape;
}
