use std::future::{Future, Ready, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    Backend, BindSink, BindValue, Decode, DeleteQuery, ExecutableDeleteQuery,
    ExecutableInsertQuery, ExecutableSelectQuery, ExecutableUpdateQuery, HAppend, HList, HNil,
    InsertQuery, InsertableTable, NoRuntimeParams, PredicateNodes, PreparableDeleteQuery,
    PreparableInsertQuery, PreparableSelectQuery, PreparableUpdateQuery, PreparedMutationQuery,
    PreparedParamValues, PreparedSelectQuery, Projectable, ProjectionShape, QueryBuilder,
    RowsAffected, SelectAst, SelectQuery, Selected, SourceAlias, TableProjection, UpdateQuery,
    UpdateableTable,
};

use crate::{TestBackend, TestConnection, TestError, sql};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EmptyRows<Row> {
    _row: PhantomData<Row>,
}

impl<Row> Default for EmptyRows<Row> {
    fn default() -> Self {
        Self { _row: PhantomData }
    }
}

impl<Row> Stream for EmptyRows<Row> {
    type Item = Result<Row, TestError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(None)
    }
}

impl<Row> RowsAffected for EmptyRows<Row> {
    fn rows_affected(&self) -> Option<u64> {
        Some(0)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRowReader<'row> {
    _row: PhantomData<&'row ()>,
}

impl squealy::RowReader for TestRowReader<'_> {
    type Backend = TestBackend;

    fn read<T>(&mut self) -> Result<T, TestError>
    where
        T: Decode<TestBackend>,
    {
        T::decode(self)
    }
}

macro_rules! impl_test_decode_no_rows {
    ($($ty:ty),* $(,)?) => {
        $(impl Decode<TestBackend> for $ty {
            fn decode(
                _row: &mut <TestBackend as Backend>::RowReader<'_>,
            ) -> Result<Self, TestError> {
                Err(TestError::NoRows)
            }
        })*
    };
}

impl_test_decode_no_rows!(i8, i16, i32, i64, i128, isize);
impl_test_decode_no_rows!(u8, u16, u32, u64, u128, usize);
impl_test_decode_no_rows!(f32, f64);
impl_test_decode_no_rows!(String, bool);

impl<T> Decode<TestBackend> for Option<T>
where
    T: Decode<TestBackend>,
{
    fn decode(_row: &mut <TestBackend as Backend>::RowReader<'_>) -> Result<Self, TestError> {
        Ok(None)
    }
}

impl TestConnection {
    pub(crate) fn fetch_select<Row>(&self) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_insert(&self) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_insert<Row>(&self) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_delete(&self) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_delete<Row>(&self) -> EmptyRows<Row> {
        empty_rows()
    }

    pub(crate) fn execute_update(&self) -> Ready<Result<u64, TestError>> {
        ok(0)
    }

    pub(crate) fn fetch_update<Row>(&self) -> EmptyRows<Row> {
        empty_rows()
    }
}

fn empty_rows<Row>() -> EmptyRows<Row> {
    EmptyRows::default()
}

fn ok<T>(value: T) -> Ready<Result<T, TestError>> {
    ready(Ok(value))
}

pub struct TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    connection: &'conn TestConnection,
    selected: Selected<'scope, Base, Shape, Projection>,
    built_from_selected: bool,
    _shape: PhantomData<Shape>,
}

pub struct TestInsert<'conn, S, Shape = (), Columns = HNil, Returning = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Columns: squealy::InsertAssignments,
    Returning: Projectable,
{
    connection: &'conn TestConnection,
    columns: Columns,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct TestDelete<'conn, S, Shape = (), Filters = HNil, Returning = ()>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    connection: &'conn TestConnection,
    alias: SourceAlias,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct TestUpdate<'conn, S, Shape = (), Columns = HNil, Filters = HNil, Returning = ()>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    connection: &'conn TestConnection,
    alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct TestPreparedSelect<'conn, Row, ParamShape = HNil>
where
    ParamShape: HList,
{
    connection: &'conn TestConnection,
    _row: PhantomData<Row>,
    _params: PhantomData<ParamShape>,
}

pub struct TestPreparedMutation<'conn, Row, ParamShape = HNil>
where
    ParamShape: HList,
{
    connection: &'conn TestConnection,
    _row: PhantomData<Row>,
    _params: PhantomData<ParamShape>,
}

impl<'conn, Row, ParamShape> TestPreparedSelect<'conn, Row, ParamShape>
where
    ParamShape: HList,
{
    fn new(connection: &'conn TestConnection) -> Self {
        Self {
            connection,
            _row: PhantomData,
            _params: PhantomData,
        }
    }
}

impl<'conn, Row, ParamShape> TestPreparedMutation<'conn, Row, ParamShape>
where
    ParamShape: HList,
{
    fn new(connection: &'conn TestConnection) -> Self {
        Self {
            connection,
            _row: PhantomData,
            _params: PhantomData,
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection> TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    pub(crate) fn new_selected(
        connection: &'conn TestConnection,
        selected: Selected<'scope, Base, Shape, Projection>,
    ) -> Self {
        Self {
            connection,
            selected,
            built_from_selected: true,
            _shape: PhantomData,
        }
    }

    pub fn built_from_selected(&self) -> bool {
        self.built_from_selected
    }
}

impl<'conn, S, Shape, Columns, Returning> TestInsert<'conn, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Columns: squealy::InsertAssignments,
    Returning: Projectable,
{
    pub(crate) fn new(
        connection: &'conn TestConnection,
        columns: Columns,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            columns,
            returning,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }
}

impl<'conn, S, Shape, Filters, Returning> TestDelete<'conn, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    pub(crate) fn new(
        connection: &'conn TestConnection,
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
}

impl<'conn, S, Shape, Columns, Filters, Returning>
    TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    pub(crate) fn new(
        connection: &'conn TestConnection,
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
}

fn render_sql(write: impl FnOnce(&mut Vec<u8>) -> std::io::Result<()>) -> String {
    let mut sql = Vec::new();
    write(&mut sql).expect("render SQL");
    String::from_utf8(sql).expect("SQL should be valid UTF-8")
}

impl<'conn, 'scope, Shape, Base, Projection> SelectQuery<'conn, 'scope, Base, Projection>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    type Builder = TestConnection;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build_selected(
        connection: &'conn Self::Builder,
        selected: Selected<'scope, Base, Self::Shape, Projection>,
    ) -> Self {
        Self::new_selected(connection, selected)
    }
}

impl<'conn, 'scope, Shape, Base, Projection> ExecutableSelectQuery<'conn, 'scope, Base, Projection>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Base::Params: NoRuntimeParams,
    Projection: Projectable,
{
    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_select()
    }
}

impl<'conn, Row, ParamShape> PreparedSelectQuery<'conn>
    for TestPreparedSelect<'conn, Row, ParamShape>
where
    Row: Decode<TestBackend> + Send,
    ParamShape: HList,
{
    type Builder = TestConnection;
    type Params = ParamShape;
    type Row = Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn fetch<'query, ParamValues>(&'query self, _params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params>,
    {
        self.connection.fetch_select()
    }
}

impl<'conn, Row, ParamShape> PreparedMutationQuery<'conn>
    for TestPreparedMutation<'conn, Row, ParamShape>
where
    Row: Decode<TestBackend> + Send,
    ParamShape: HList,
{
    type Builder = TestConnection;
    type Params = ParamShape;
    type Row = Row;

    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn execute<'query, ParamValues>(
        &'query self,
        _params: ParamValues,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + 'query
    where
        'conn: 'query,
        ParamValues: PreparedParamValues<Self::Params> + 'query,
    {
        self.connection.execute_insert()
    }

    fn fetch<'query, ParamValues>(&'query self, _params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params>,
    {
        self.connection.fetch_insert()
    }
}

impl<'conn, 'scope, Shape, Base, Projection> PreparableSelectQuery<'conn, 'scope, Base, Projection>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend> + Send,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Base::Params: HList,
    Projection: Projectable,
{
    type Params = Base::Params;

    type Prepared<'prepared>
        = TestPreparedSelect<'prepared, Shape::Row, Base::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        'scope: 'prepared,
        Base: 'prepared,
        Projection: 'prepared,
    {
        ready(Ok(TestPreparedSelect::new(self.connection)))
    }
}

impl<'conn, S, Shape, Columns, Returning> InsertQuery<'conn, Columns, Returning>
    for TestInsert<'conn, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Columns: squealy::InsertAssignments,
    Returning: Projectable,
{
    type Builder = TestConnection;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(connection: &'conn Self::Builder, columns: Columns, returning: Returning) -> Self {
        Self::new(connection, columns, returning)
    }
}

impl<'conn, S, Shape, Columns, Returning> ExecutableInsertQuery<'conn, Columns, Returning>
    for TestInsert<'conn, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Columns: squealy::InsertAssignments,
    Columns::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_insert()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_insert()
    }
}

impl<'conn, S, Shape, Columns, Returning> PreparableInsertQuery<'conn, Columns, Returning>
    for TestInsert<'conn, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend> + Send,
    Columns: squealy::InsertAssignments,
    Columns::Params: HList,
    Returning: Projectable,
{
    type Params = Columns::Params;

    type Prepared<'prepared>
        = TestPreparedMutation<'prepared, Shape::Row, Columns::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        Columns: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Columns: 'prepared,
        Returning: 'prepared,
    {
        ready(Ok(TestPreparedMutation::new(self.connection)))
    }
}

impl<'conn, S, Shape, Filters, Returning> DeleteQuery<'conn, Filters, Returning>
    for TestDelete<'conn, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = TestConnection;
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

impl<'conn, S, Shape, Filters, Returning> ExecutableDeleteQuery<'conn, Filters, Returning>
    for TestDelete<'conn, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_delete()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_delete()
    }
}

impl<'conn, S, Shape, Filters, Returning> PreparableDeleteQuery<'conn, Filters, Returning>
    for TestDelete<'conn, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend> + Send,
    Filters: PredicateNodes,
    Filters::Params: HList,
    Returning: Projectable,
{
    type Params = Filters::Params;

    type Prepared<'prepared>
        = TestPreparedMutation<'prepared, Shape::Row, Filters::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared,
    {
        ready(Ok(TestPreparedMutation::new(self.connection)))
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning> UpdateQuery<'conn, Columns, Filters, Returning>
    for TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    type Builder = TestConnection;
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

impl<'conn, S, Shape, Columns, Filters, Returning>
    ExecutableUpdateQuery<'conn, Columns, Filters, Returning>
    for TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Columns: squealy::UpdateAssignments,
    Columns::Params: NoRuntimeParams,
    Filters: PredicateNodes,
    Filters::Params: NoRuntimeParams,
    Returning: Projectable,
{
    type RowStream<'query>
        = EmptyRows<Self::Row>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<
        Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>,
    > + Send
    + '_ {
        self.connection.execute_update()
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        self.connection.fetch_update()
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning>
    PreparableUpdateQuery<'conn, Columns, Filters, Returning>
    for TestUpdate<'conn, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend> + Send,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Columns::Params: HAppend<Filters::Params>,
    <Columns::Params as HAppend<Filters::Params>>::Output: HList,
    Returning: Projectable,
{
    type Params = <Columns::Params as HAppend<Filters::Params>>::Output;

    type Prepared<'prepared>
        = TestPreparedMutation<
        'prepared,
        Shape::Row,
        <Columns::Params as HAppend<Filters::Params>>::Output,
    >
    where
        Self: 'prepared,
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared;

    fn prepare<'prepared>(
        &'prepared self,
    ) -> impl Future<
        Output = Result<
            Self::Prepared<'prepared>,
            <<Self::Builder as QueryBuilder>::Backend as Backend>::Error,
        >,
    > + 'prepared
    where
        'conn: 'prepared,
        Columns: 'prepared,
        Filters: 'prepared,
        Returning: 'prepared,
    {
        ready(Ok(TestPreparedMutation::new(self.connection)))
    }
}

impl<'conn, 'scope, Shape, Base, Projection> TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        render_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_selected_into::<TestConnection, Base, Shape, Projection, _>(
            &self.selected,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        sql::write_selected_params::<TestConnection, Base, Shape, Projection, _>(
            &self.selected,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Columns, Returning> TestInsert<'_, S, Shape, Columns, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Columns: squealy::InsertAssignments,
    Returning: Projectable,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        render_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_insert::<S, _, _>(&self.columns, &self.returning, writer)
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        sql::write_insert_params::<S, _, _, _>(&self.columns, &self.returning, sink)
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Filters, Returning> TestDelete<'_, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        render_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_delete::<S, _, _>(self.alias, &self.filters, &self.returning, writer)
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        sql::write_delete_params::<S, _, _, _>(self.alias, &self.filters, &self.returning, sink)
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}

impl<S, Shape, Columns, Filters, Returning> TestUpdate<'_, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: squealy::UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
{
    /// Render this query into a newly allocated SQL string.
    ///
    /// Use [`Self::write_sql`] to stream SQL into caller-provided storage instead.
    pub fn to_sql(&self) -> String {
        render_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_update::<S, _, _, _>(
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            writer,
        )
    }

    /// Write bind parameters into a caller-provided sink.
    pub fn write_params<Sink>(&self, sink: &mut Sink) -> Result<(), Sink::Error>
    where
        Sink: BindSink,
    {
        sql::write_update_params::<S, _, _, _, _>(
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            sink,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Vec<BindValue> {
        let mut params = Vec::new();
        self.write_params(&mut params)
            .unwrap_or_else(|error| match error {});
        params
    }
}
