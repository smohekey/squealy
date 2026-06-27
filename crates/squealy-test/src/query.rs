use std::future::{Future, Ready, ready};
use std::marker::PhantomData;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures_core::Stream;

use squealy::{
    Backend, Decode, DeleteQuery, Encode, ExecutableDeleteQuery, ExecutableInsertQuery,
    ExecutableSelectQuery, ExecutableUpdateQuery, HAppend, HList, HNil, InsertQuery, InsertRows,
    InsertableTable, NoRuntimeParams, ParamWriter, PredicateNodes, PreparableDeleteQuery,
    PreparableInsertQuery, PreparableSelectQuery, PreparableUpdateQuery, PreparedMutationQuery,
    PreparedParamValues, PreparedSelectQuery, Projectable, ProjectionShape, QueryBuilder,
    RenderInsertRows, RenderPredicateNodes, RenderProjectable, RenderSelectAst,
    RenderUpdateAssignments, RowsAffected, SelectAst, SelectQuery, Selected, SetArm, SetLeaf,
    SetOperand, SetOperations, SetSelectModifiers, SetTail, SourceAlias, TableProjection,
    UpdateQuery, UpdateableTable,
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

/// Inspectable parameter captured by the test backend's [`TestParamWriter`].
///
/// This is the test backend's native param type (the mirror of `PostgresParam`). It is
/// the value tests assert on, replacing the old neutral `BindValue`. Integer widths are
/// canonicalized to `i128`/`u128` so equality ignores the source width, matching the old
/// `BindValue` comparison semantics.
#[derive(Clone, Debug)]
pub enum TestParam {
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(String),
    Bytes(Vec<u8>),
    Bool(bool),
    Null,
}

impl PartialEq for TestParam {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Int(left), Self::Int(right)) => left == right,
            (Self::UInt(left), Self::UInt(right)) => left == right,
            (Self::Float(left), Self::Float(right)) => left == right,
            (Self::Text(left), Self::Text(right)) => left == right,
            (Self::Bytes(left), Self::Bytes(right)) => left == right,
            (Self::Bool(left), Self::Bool(right)) => left == right,
            (Self::Null, Self::Null) => true,
            _ => false,
        }
    }
}

/// Encode-side mirror of [`TestRowReader`]: captures native [`TestParam`]s for inspection.
pub struct TestParamWriter<'param> {
    params: &'param mut Vec<TestParam>,
}

impl<'param> TestParamWriter<'param> {
    pub fn new(params: &'param mut Vec<TestParam>) -> Self {
        Self { params }
    }

    pub fn push(&mut self, param: TestParam) {
        self.params.push(param);
    }
}

impl ParamWriter for TestParamWriter<'_> {
    type Backend = TestBackend;

    fn write<T>(&mut self, value: &T) -> Result<(), TestError>
    where
        T: Encode<TestBackend>,
    {
        value.encode(self)
    }
}

macro_rules! impl_test_encode {
    ($($ty:ty => |$value:ident| $param:expr),* $(,)?) => {
        $(impl Encode<TestBackend> for $ty {
            fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
                let $value = self;
                out.push($param);
                Ok(())
            }
        })*
    };
}

impl_test_encode! {
    i8 => |v| TestParam::Int(i128::from(*v)),
    i16 => |v| TestParam::Int(i128::from(*v)),
    i32 => |v| TestParam::Int(i128::from(*v)),
    i64 => |v| TestParam::Int(i128::from(*v)),
    i128 => |v| TestParam::Int(*v),
    isize => |v| TestParam::Int(*v as i128),
    u8 => |v| TestParam::UInt(u128::from(*v)),
    u16 => |v| TestParam::UInt(u128::from(*v)),
    u32 => |v| TestParam::UInt(u128::from(*v)),
    u64 => |v| TestParam::UInt(u128::from(*v)),
    u128 => |v| TestParam::UInt(*v),
    usize => |v| TestParam::UInt(*v as u128),
    f32 => |v| TestParam::Float(f64::from(*v)),
    f64 => |v| TestParam::Float(*v),
    bool => |v| TestParam::Bool(*v),
}

impl Encode<TestBackend> for str {
    fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
        out.push(TestParam::Text(self.to_owned()));
        Ok(())
    }
}

impl Encode<TestBackend> for String {
    fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
        out.push(TestParam::Text(self.clone()));
        Ok(())
    }
}

impl Encode<TestBackend> for Vec<u8> {
    fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
        out.push(TestParam::Bytes(self.clone()));
        Ok(())
    }
}

// Fixed-size byte arrays bind like `Vec<u8>` so `[u8; N]` columns work with the test backend's
// query-rendering / parameter assertions (matching the PostgreSQL/MySQL `Encode` impls).
impl<const N: usize> Encode<TestBackend> for [u8; N] {
    fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
        out.push(TestParam::Bytes(self.to_vec()));
        Ok(())
    }
}

impl<T> Encode<TestBackend> for Option<T>
where
    T: Encode<TestBackend>,
{
    fn encode(&self, out: &mut TestParamWriter<'_>) -> Result<(), TestError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push(TestParam::Null);
                Ok(())
            }
        }
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
impl_test_decode_no_rows!(Vec<u8>);

// The test backend never yields rows, so decode is a stub; const generics can't go through the macro.
impl<const N: usize> Decode<TestBackend> for [u8; N] {
    fn decode(_row: &mut <TestBackend as Backend>::RowReader<'_>) -> Result<Self, TestError> {
        Err(TestError::NoRows)
    }
}

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

pub struct TestInsert<'conn, S, Shape = (), Rows = HNil, Returning = ()>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
{
    connection: &'conn TestConnection,
    columns: Rows,
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

impl<'conn, S, Shape, Rows, Returning> TestInsert<'conn, S, Shape, Rows, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
{
    pub(crate) fn new(
        connection: &'conn TestConnection,
        columns: Rows,
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
        ParamValues: PreparedParamValues<Self::Params, TestBackend>,
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
        ParamValues: PreparedParamValues<Self::Params, TestBackend> + 'query,
    {
        self.connection.execute_insert()
    }

    fn fetch<'query, ParamValues>(&'query self, _params: ParamValues) -> Self::RowStream<'query>
    where
        ParamValues: PreparedParamValues<Self::Params, TestBackend>,
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

impl<'conn, S, Shape, Rows, Returning> InsertQuery<'conn, Rows, Returning>
    for TestInsert<'conn, S, Shape, Rows, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Rows: InsertRows,
    Returning: Projectable,
{
    type Builder = TestConnection;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(connection: &'conn Self::Builder, columns: Rows, returning: Returning) -> Self {
        Self::new(connection, columns, returning)
    }
}

impl<'conn, S, Shape, Rows, Returning> ExecutableInsertQuery<'conn, Rows, Returning>
    for TestInsert<'conn, S, Shape, Rows, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend>,
    Rows: InsertRows,
    Rows::Params: NoRuntimeParams,
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

impl<'conn, S, Shape, Rows, Returning> PreparableInsertQuery<'conn, Rows, Returning>
    for TestInsert<'conn, S, Shape, Rows, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<TestBackend> + Send,
    Rows: InsertRows,
    Rows::Params: HList,
    Returning: Projectable,
{
    type Params = Rows::Params;

    type Prepared<'prepared>
        = TestPreparedMutation<'prepared, Shape::Row, Rows::Params>
    where
        Self: 'prepared,
        'conn: 'prepared,
        Rows: 'prepared,
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
        Rows: 'prepared,
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
    Base: RenderSelectAst<'conn, 'scope, TestConnection, TestBackend>,
    Projection: RenderProjectable<TestBackend>,
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

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<crate::TestParam>) -> Result<(), TestError>
    where
        Base: RenderSelectAst<'conn, 'scope, TestConnection, TestBackend>,
        Projection: RenderProjectable<TestBackend>,
    {
        sql::write_selected_params::<TestConnection, Base, Shape, Projection>(
            &self.selected,
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<crate::TestParam>, TestError>
    where
        Base: RenderSelectAst<'conn, 'scope, TestConnection, TestBackend>,
        Projection: RenderProjectable<TestBackend>,
    {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

/// A set-operation query object (`(<left>) UNION (<right>) …`) over a [`SetArm`] tree.
pub struct TestSetSelect<'conn, 'scope, Tree> {
    connection: &'conn TestConnection,
    tree: Tree,
    tail: SetTail,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Tree> TestSetSelect<'conn, 'scope, Tree> {
    fn new(connection: &'conn TestConnection, tree: Tree) -> Self {
        Self {
            connection,
            tree,
            tail: SetTail::default(),
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, Tree> TestSetSelect<'conn, 'scope, Tree>
where
    Tree: squealy::render::RenderSetArm<'conn, 'scope, TestConnection, TestBackend>,
{
    /// Render this set query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        render_sql(|writer| self.write_sql(writer))
    }

    /// Stream SQL into caller-provided storage without allocating a SQL string.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        sql::write_set_into::<Tree, _>(&self.tree, &self.tail, writer)
    }

    /// Write bind parameters (left-to-right across the whole tree) into a native param vector.
    pub fn write_params(&self, params: &mut Vec<crate::TestParam>) -> Result<(), TestError> {
        sql::write_set_params::<Tree>(&self.tree, &self.tail, params)
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<crate::TestParam>, TestError> {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<'conn, 'scope, Tree> SetSelectModifiers<'scope> for TestSetSelect<'conn, 'scope, Tree>
where
    Tree: SetArm<'conn, 'scope, TestConnection>,
{
    type Shape = <Tree as SetArm<'conn, 'scope, TestConnection>>::Shape;

    fn set_tail_mut(&mut self) -> &mut SetTail {
        &mut self.tail
    }
}

impl<'conn, 'scope, Shape, Base, Projection> SetOperand<'conn, 'scope, TestConnection>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    type Row = Shape::Row;
    type Arm = SetLeaf<'conn, 'scope, TestConnection, Base, Shape, Projection>;

    fn into_set_parts(self) -> (&'conn TestConnection, Self::Arm) {
        (self.connection, SetLeaf::new(self.selected))
    }
}

impl<'conn, 'scope, Shape, Base, Projection> SetOperations<'conn, 'scope, TestConnection>
    for TestSelect<'conn, 'scope, Shape, Base, Projection>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, TestConnection>,
    Projection: Projectable,
{
    type SetSelect<Tree>
        = TestSetSelect<'conn, 'scope, Tree>
    where
        Tree: SetArm<'conn, 'scope, TestConnection>;

    fn make_set_select<Tree>(connection: &'conn TestConnection, tree: Tree) -> Self::SetSelect<Tree>
    where
        Tree: SetArm<'conn, 'scope, TestConnection>,
    {
        TestSetSelect::new(connection, tree)
    }
}

impl<'conn, 'scope, Tree> SetOperand<'conn, 'scope, TestConnection>
    for TestSetSelect<'conn, 'scope, Tree>
where
    Tree: SetArm<'conn, 'scope, TestConnection>,
{
    type Row = <Tree as SetArm<'conn, 'scope, TestConnection>>::Row;
    type Arm = Tree;

    fn into_set_parts(self) -> (&'conn TestConnection, Self::Arm) {
        (self.connection, self.tree)
    }
}

impl<'conn, 'scope, Tree> SetOperations<'conn, 'scope, TestConnection>
    for TestSetSelect<'conn, 'scope, Tree>
where
    Tree: SetArm<'conn, 'scope, TestConnection>,
{
    type SetSelect<NewTree>
        = TestSetSelect<'conn, 'scope, NewTree>
    where
        NewTree: SetArm<'conn, 'scope, TestConnection>;

    fn make_set_select<NewTree>(
        connection: &'conn TestConnection,
        tree: NewTree,
    ) -> Self::SetSelect<NewTree>
    where
        NewTree: SetArm<'conn, 'scope, TestConnection>,
    {
        TestSetSelect::new(connection, tree)
    }
}

impl<S, Shape, Rows, Returning> TestInsert<'_, S, Shape, Rows, Returning>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: RenderInsertRows<TestBackend>,
    Returning: RenderProjectable<TestBackend>,
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

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<crate::TestParam>) -> Result<(), TestError>
    where
        Rows: RenderInsertRows<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        sql::write_insert_params::<S, _, _>(&self.columns, &self.returning, params)
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<crate::TestParam>, TestError>
    where
        Rows: RenderInsertRows<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<S, Shape, Filters, Returning> TestDelete<'_, S, Shape, Filters, Returning>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: RenderPredicateNodes<TestBackend>,
    Returning: RenderProjectable<TestBackend>,
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

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<crate::TestParam>) -> Result<(), TestError>
    where
        Filters: RenderPredicateNodes<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        sql::write_delete_params::<S, _, _>(self.alias, &self.filters, &self.returning, params)
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<crate::TestParam>, TestError>
    where
        Filters: RenderPredicateNodes<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}

impl<S, Shape, Columns, Filters, Returning> TestUpdate<'_, S, Shape, Columns, Filters, Returning>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: RenderUpdateAssignments<TestBackend>,
    Filters: RenderPredicateNodes<TestBackend>,
    Returning: RenderProjectable<TestBackend>,
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

    /// Write bind parameters into a caller-provided native param vector.
    pub fn write_params(&self, params: &mut Vec<crate::TestParam>) -> Result<(), TestError>
    where
        Columns: RenderUpdateAssignments<TestBackend>,
        Filters: RenderPredicateNodes<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        sql::write_update_params::<S, _, _, _>(
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            params,
        )
    }

    /// Collect bind parameters into a newly allocated vector.
    ///
    /// Use [`Self::write_params`] to inspect parameters without allocating a vector.
    pub fn collect_params(&self) -> Result<Vec<crate::TestParam>, TestError>
    where
        Columns: RenderUpdateAssignments<TestBackend>,
        Filters: RenderPredicateNodes<TestBackend>,
        Returning: RenderProjectable<TestBackend>,
    {
        let mut params = Vec::new();
        self.write_params(&mut params)?;
        Ok(params)
    }
}
