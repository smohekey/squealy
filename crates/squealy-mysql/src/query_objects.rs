// MySQL query objects. Included into `query.rs`, so it shares that module's imports.
//
// Mirrors the PostgreSQL backend's query objects, but rendering is delegated to the shared core
// renderer (`squealy::render`) with `MysqlDialect`, and execution goes through `MysqlRows` /
// `MysqlExecutor`. Prepared statements and `RETURNING` are intentionally not implemented for MySQL.

use crate::sql::MysqlDialect;

pub struct MysqlSelect<'conn, 'scope, Shape, Base, Projection, Conn = MysqlConnection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    connection: &'conn Conn,
    selected: Selected<'scope, Base, Shape, Projection>,
    _shape: PhantomData<Shape>,
}

pub struct MysqlInsert<'conn, S, Shape = (), Rows = HNil, Returning = (), Conn = MysqlConnection>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    connection: &'conn Conn,
    columns: Rows,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct MysqlDelete<'conn, S, Shape = (), Filters = HNil, Returning = (), Conn = MysqlConnection>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    connection: &'conn Conn,
    alias: SourceAlias,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct MysqlUpdate<
    'conn,
    S,
    Shape = (),
    Columns = HNil,
    Filters = HNil,
    Returning = (),
    Conn = MysqlConnection,
> where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    connection: &'conn Conn,
    alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    MysqlSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    fn new_selected(
        connection: &'conn Conn,
        selected: Selected<'scope, Base, Shape, Projection>,
    ) -> Self {
        Self {
            connection,
            selected,
            _shape: PhantomData,
        }
    }

    fn execution_parts(&self) -> Result<(String, Vec<Value>), MysqlError>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Mysql>,
        Projection: RenderProjectable<Mysql>,
    {
        let sql = rendered_sql(|writer| {
            render::write_selected_into::<Conn, Base, Shape, Projection, _>(
                &MysqlDialect,
                &self.selected,
                writer,
            )
        });
        let params = collect_mysql_params(0, |params| {
            render::write_selected_params::<Conn, Base, Shape, Projection>(
                &MysqlDialect,
                &self.selected,
                params,
            )
        })?;
        Ok((sql, params))
    }

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Mysql>,
        Projection: RenderProjectable<Mysql>,
    {
        rendered_sql(|writer| self.write_sql(writer))
    }

    /// Streams SQL into caller-provided storage.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Mysql>,
        Projection: RenderProjectable<Mysql>,
    {
        render::write_selected_into::<Conn, Base, Shape, Projection, _>(
            &MysqlDialect,
            &self.selected,
            writer,
        )
    }

    /// Collects bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<Value>, MysqlError>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Mysql>,
        Projection: RenderProjectable<Mysql>,
    {
        let mut params = Vec::new();
        render::write_selected_params::<Conn, Base, Shape, Projection>(
            &MysqlDialect,
            &self.selected,
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> MysqlInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    pub(crate) fn new(connection: &'conn Conn, columns: Rows, returning: Returning) -> Self {
        Self {
            connection,
            columns,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn execution_parts(&self) -> Result<(String, Vec<Value>), MysqlError>
    where
        Rows: RenderInsertRows<Mysql>,
        Returning: RenderProjectable<Mysql>,
    {
        let sql = rendered_sql(|writer| {
            render::write_insert::<S, Mysql, _, _>(
                &MysqlDialect,
                &self.columns,
                &self.returning,
                None,
                writer,
            )
        });
        let params =
            collect_mysql_params(self.columns.first_row_len() * self.columns.len(), |params| {
                render::write_insert_params::<S, Mysql, _, _>(
                    &MysqlDialect,
                    &self.columns,
                    &self.returning,
                    None,
                    params,
                )
            })?;
        Ok((sql, params))
    }

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Rows: RenderInsertRows<Mysql>,
        Returning: RenderProjectable<Mysql>,
    {
        rendered_sql(|writer| {
            render::write_insert::<S, Mysql, _, _>(
                &MysqlDialect,
                &self.columns,
                &self.returning,
                None,
                writer,
            )
        })
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> MysqlDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    pub(crate) fn new(
        connection: &'conn Conn,
        alias: SourceAlias,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            alias,
            filters,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn execution_parts(&self) -> Result<(String, Vec<Value>), MysqlError>
    where
        Filters: RenderPredicateNodes<Mysql>,
        Returning: RenderProjectable<Mysql>,
    {
        let sql = rendered_sql(|writer| {
            render::write_delete::<S, Mysql, _, _>(
                &MysqlDialect,
                self.alias,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_mysql_params(self.filters.len(), |params| {
            render::write_delete_params::<S, Mysql, _, _>(
                &MysqlDialect,
                self.alias,
                &self.filters,
                &self.returning,
                params,
            )
        })?;
        Ok((sql, params))
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    MysqlUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Mysql>,
{
    pub(crate) fn new(
        connection: &'conn Conn,
        alias: SourceAlias,
        columns: Columns,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            alias,
            columns,
            filters,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    fn execution_parts(&self) -> Result<(String, Vec<Value>), MysqlError>
    where
        Columns: RenderUpdateAssignments<Mysql>,
        Filters: RenderPredicateNodes<Mysql>,
        Returning: RenderProjectable<Mysql>,
    {
        let sql = rendered_sql(|writer| {
            render::write_update::<S, Mysql, _, _, _>(
                &MysqlDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                writer,
            )
        });
        let params = collect_mysql_params(self.columns.len() + self.filters.len(), |params| {
            render::write_update_params::<S, Mysql, _, _, _>(
                &MysqlDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                params,
            )
        })?;
        Ok((sql, params))
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SelectQuery<'conn, 'scope, Base, Projection>
    for MysqlSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Mysql> + 'conn,
    Shape::Row: Decode<Mysql>,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
{
    type Builder = Conn;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build_selected(
        connection: &'conn Self::Builder,
        selected: Selected<'scope, Base, Self::Shape, Projection>,
    ) -> Self {
        Self::new_selected(connection, selected)
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    ExecutableSelectQuery<'conn, 'scope, Base, Projection>
    for MysqlSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: MysqlExecutor + 'conn,
    Shape::Row: Decode<Mysql>,
    Base: RenderSelectAst<'conn, 'scope, Conn, Mysql>,
    Base::Params: NoRuntimeParams,
    Projection: RenderProjectable<Mysql>,
{
    type RowStream<'query>
        = MysqlRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => MysqlRows::query(self.connection, sql, params),
            Err(error) => MysqlRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> InsertQuery<'conn, Rows, Returning>
    for MysqlInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Mysql> + 'conn,
    Shape::Row: Decode<Mysql>,
    Rows: InsertRows,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(connection: &'conn Self::Builder, columns: Rows, returning: Returning) -> Self {
        Self::new(connection, columns, returning)
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> ExecutableInsertQuery<'conn, Rows, Returning>
    for MysqlInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: MysqlExecutor + 'conn,
    Shape::Row: Decode<Mysql>,
    Rows: RenderInsertRows<Mysql>,
    Rows::Params: NoRuntimeParams,
    Returning: RenderProjectable<Mysql>,
{
    type RowStream<'query>
        = MysqlRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => MysqlRows::query(self.connection, sql, params),
            Err(error) => MysqlRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> DeleteQuery<'conn, Filters, Returning>
    for MysqlDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Mysql> + 'conn,
    Shape::Row: Decode<Mysql>,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        connection: &'conn Self::Builder,
        alias: SourceAlias,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self::new(connection, alias, filters, returning)
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> ExecutableDeleteQuery<'conn, Filters, Returning>
    for MysqlDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: MysqlExecutor + 'conn,
    Shape::Row: Decode<Mysql>,
    Filters: RenderPredicateNodes<Mysql>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Mysql>,
{
    type RowStream<'query>
        = MysqlRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => MysqlRows::query(self.connection, sql, params),
            Err(error) => MysqlRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    UpdateQuery<'conn, Columns, Filters, Returning>
    for MysqlUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Mysql> + 'conn,
    Shape::Row: Decode<Mysql>,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = Conn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        connection: &'conn Self::Builder,
        alias: SourceAlias,
        columns: Columns,
        filters: Filters,
        returning: Returning,
    ) -> Self {
        Self::new(connection, alias, columns, filters, returning)
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    ExecutableUpdateQuery<'conn, Columns, Filters, Returning>
    for MysqlUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: MysqlExecutor + 'conn,
    Shape::Row: Decode<Mysql>,
    Columns: RenderUpdateAssignments<Mysql>,
    Columns::Params: NoRuntimeParams,
    Filters: RenderPredicateNodes<Mysql>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Mysql>,
{
    type RowStream<'query>
        = MysqlRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self.execution_parts() {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self.execution_parts() {
            Ok((sql, params)) => MysqlRows::query(self.connection, sql, params),
            Err(error) => MysqlRows::error(error),
        }
    }
}

macro_rules! impl_query_builder_for {
    ($ty:ty) => {
        impl QueryBuilder for $ty {
            type Backend = Mysql;

            type Select<'conn, 'scope, Base, Shape, Projection>
                = MysqlSelect<'conn, 'scope, Shape, Base, Projection, Self>
            where
                Self: 'conn,
                Base: SelectAst<'conn, 'scope, Self> + 'conn,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Projection: Projectable;

            type Insert<'conn, S, Shape, Rows, Returning>
                = MysqlInsert<'conn, S, Shape, Rows, Returning, Self>
            where
                Self: 'conn,
                S: InsertableTable,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Rows: InsertRows,
                Returning: Projectable;

            type Update<'conn, S, Shape, Columns, Filters, Returning>
                = MysqlUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
            where
                Self: 'conn,
                S: UpdateableTable,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Columns: UpdateAssignments,
                Filters: PredicateNodes,
                Returning: Projectable;

            type Delete<'conn, S, Shape, Filters, Returning>
                = MysqlDelete<'conn, S, Shape, Filters, Returning, Self>
            where
                Self: 'conn,
                S: TableProjection,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Filters: PredicateNodes,
                Returning: Projectable;
        }
    };
}

impl_query_builder_for!(Mysql);
impl_query_builder_for!(MysqlConnection);

impl Connection for MysqlConnection {}
