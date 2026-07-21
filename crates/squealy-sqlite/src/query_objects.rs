// SQLite query objects. Included into `query.rs`, so it shares that module's imports.
//
// Mirrors the MySQL backend's query objects, but this slice is DRIVER-FREE: only the build and render
// side is provided (`to_sql()` / `write_sql()` / `collect_params()`). Execution (`Executable*Query`,
// a connection type, prepared statements) lands in a later slice. Rendering is delegated to the shared
// core renderer (`squealy::render`) with `SqliteDialect`.

use crate::sql::SqliteDialect;

pub struct SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn = Sqlite>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    #[allow(dead_code)]
    connection: &'conn Conn,
    selected: Selected<'scope, Base, Shape, Projection>,
    _shape: PhantomData<Shape>,
}

pub struct SqliteInsert<'conn, S, Shape = (), Rows = HNil, Returning = (), Conn = Sqlite>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    #[allow(dead_code)]
    connection: &'conn Conn,
    columns: Rows,
    returning: Returning,
    // `Some` for an upsert (`INSERT … ON CONFLICT DO UPDATE/NOTHING`); `None` for a plain insert.
    conflict: Option<squealy::ConflictClause>,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct SqliteDelete<'conn, S, Shape = (), Filters = HNil, Returning = (), Conn = Sqlite>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    #[allow(dead_code)]
    connection: &'conn Conn,
    alias: SourceAlias,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

pub struct SqliteUpdate<
    'conn,
    S,
    Shape = (),
    Columns = HNil,
    Filters = HNil,
    Returning = (),
    Conn = Sqlite,
> where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    #[allow(dead_code)]
    connection: &'conn Conn,
    alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    returning: Returning,
    _table: PhantomData<S>,
    _shape: PhantomData<Shape>,
}

impl<'conn, 'scope, Shape, Base, Projection, Conn>
    SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
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

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Sqlite>,
        Projection: RenderProjectable<Sqlite>,
    {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this query, returning a render reject (a query shape SQLite cannot render, such as a
    /// scoped recursive CTE arm) as an error instead of panicking like [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Sqlite>,
        Projection: RenderProjectable<Sqlite>,
    {
        try_rendered_sql(|writer| self.write_sql(writer)).map_err(SqliteError::Render)
    }

    /// Streams SQL into caller-provided storage.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Sqlite>,
        Projection: RenderProjectable<Sqlite>,
    {
        render::write_selected_into::<Conn, Base, Shape, Projection, _>(
            &SqliteDialect,
            &self.selected,
            writer,
        )
    }

    /// Collects bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError>
    where
        Base: RenderSelectAst<'conn, 'scope, Conn, Sqlite>,
        Projection: RenderProjectable<Sqlite>,
    {
        let mut params = Vec::new();
        render::write_selected_params::<Conn, Base, Shape, Projection>(
            &SqliteDialect,
            &self.selected,
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> SqliteInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Rows: InsertRows,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    pub(crate) fn new(connection: &'conn Conn, columns: Rows, returning: Returning) -> Self {
        Self {
            connection,
            columns,
            returning,
            conflict: None,
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    pub(crate) fn new_upsert(
        connection: &'conn Conn,
        columns: Rows,
        returning: Returning,
        conflict: squealy::ConflictClause,
    ) -> Self {
        Self {
            connection,
            columns,
            returning,
            conflict: Some(conflict),
            _table: PhantomData,
            _shape: PhantomData,
        }
    }

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Rows: RenderInsertRows<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this query, returning a render reject (a query shape SQLite cannot render, such as a
    /// scoped recursive CTE arm) as an error instead of panicking like [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError>
    where
        Rows: RenderInsertRows<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        try_rendered_sql(|writer| {
            render::write_insert::<S, Sqlite, _, _>(
                &SqliteDialect,
                &self.columns,
                &self.returning,
                self.conflict.as_ref(),
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collects bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError>
    where
        Rows: RenderInsertRows<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        let mut params = Vec::new();
        render::write_insert_params::<S, Sqlite, _, _>(
            &SqliteDialect,
            &self.columns,
            &self.returning,
            self.conflict.as_ref(),
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn>
    SqliteDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
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

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this query, returning a render reject (a query shape SQLite cannot render, such as a
    /// scoped recursive CTE arm) as an error instead of panicking like [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError>
    where
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        try_rendered_sql(|writer| {
            render::write_delete::<S, Sqlite, _, _>(
                &SqliteDialect,
                self.alias,
                &self.filters,
                &self.returning,
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collects bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError>
    where
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        let mut params = Vec::new();
        render::write_delete_params::<S, Sqlite, _, _>(
            &SqliteDialect,
            self.alias,
            &self.filters,
            &self.returning,
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    SqliteUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Returning: Projectable,
    Conn: QueryBuilder<Backend = Sqlite>,
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

    /// Renders this query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String
    where
        Columns: RenderUpdateAssignments<Sqlite>,
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this query, returning a render reject (a query shape SQLite cannot render, such as a
    /// scoped recursive CTE arm) as an error instead of panicking like [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError>
    where
        Columns: RenderUpdateAssignments<Sqlite>,
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        try_rendered_sql(|writer| {
            render::write_update::<S, Sqlite, _, _, _>(
                &SqliteDialect,
                self.alias,
                &self.columns,
                &self.filters,
                &self.returning,
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collects bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError>
    where
        Columns: RenderUpdateAssignments<Sqlite>,
        Filters: RenderPredicateNodes<Sqlite>,
        Returning: RenderProjectable<Sqlite>,
    {
        let mut params = Vec::new();
        render::write_update_params::<S, Sqlite, _, _, _>(
            &SqliteDialect,
            self.alias,
            &self.columns,
            &self.filters,
            &self.returning,
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SelectQuery<'conn, 'scope, Base, Projection>
    for SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
    Shape::Row: Decode<Sqlite>,
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
    for SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    Conn: SqliteExecutor + 'conn,
    Shape::Row: Decode<Sqlite>,
    Base: RenderSelectAst<'conn, 'scope, Conn, Sqlite>,
    Base::Params: NoRuntimeParams,
    Projection: RenderProjectable<Sqlite>,
{
    type RowStream<'query>
        = SqliteRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn fetch(&self) -> Self::RowStream<'_> {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => SqliteRows::query(self.connection, sql, params),
            Err(error) => SqliteRows::error(error),
        }
    }
}

// ---------------------------------------------------------------------------
// Set operations
// ---------------------------------------------------------------------------

/// A set-operation query object (`(<left>) UNION (<right>) …`) over a [`SetArm`] tree.
pub struct SqliteSetSelect<'conn, 'scope, Tree, Conn = Sqlite>
where
    Conn: QueryBuilder<Backend = Sqlite>,
{
    #[allow(dead_code)]
    connection: &'conn Conn,
    tree: Tree,
    tail: SetTail,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, Tree, Conn> SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: QueryBuilder<Backend = Sqlite>,
{
    fn new(connection: &'conn Conn, tree: Tree) -> Self {
        Self {
            connection,
            tree,
            tail: SetTail::default(),
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, Tree, Conn> SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: QueryBuilder<Backend = Sqlite>,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Sqlite>,
{
    /// Renders this set query into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this set query, returning a render reject (a query shape SQLite cannot render, such as
    /// a scoped recursive CTE arm) as an error instead of panicking like [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError> {
        try_rendered_sql(|writer| self.write_sql(writer)).map_err(SqliteError::Render)
    }

    /// Streams SQL into caller-provided storage.
    pub fn write_sql(&self, writer: &mut impl std::io::Write) -> std::io::Result<()> {
        render::write_set_into::<Conn, Tree, _>(&SqliteDialect, &self.tree, &self.tail, writer)
    }

    /// Collects bind parameters (left-to-right across the whole tree) into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError> {
        let mut params = Vec::new();
        render::write_set_params::<Conn, Tree>(&SqliteDialect, &self.tree, &self.tail, &mut params)?;
        Ok(params)
    }
}

impl<'conn, 'scope, Tree, Conn> ExecutableSetSelectQuery<'conn>
    for SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Conn: SqliteExecutor + 'conn,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Sqlite>,
    <Tree as SetArm<'conn, 'scope, Conn>>::Row: Decode<Sqlite> + Send,
    // Executing inlines literals (no prepared path), so reject runtime params in any arm.
    <Tree as SetArm<'conn, 'scope, Conn>>::Params: NoRuntimeParams,
{
    type Builder = Conn;
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;

    type RowStream<'query>
        = SqliteRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn fetch(&self) -> Self::RowStream<'_> {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => SqliteRows::query(self.connection, sql, params),
            Err(error) => SqliteRows::error(error),
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SetOperand<'conn, 'scope, Conn>
    for SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // A row-locked select cannot be a set operand (a locking clause is invalid in a UNION/INTERSECT/
    // EXCEPT input). `SetOperations` requires `SetOperand`, so this also blocks the left operand.
    Base: SelectAst<'conn, 'scope, Conn, RowLockState = squealy::RowUnlocked>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Row = Shape::Row;
    type Arm = SetLeaf<'conn, 'scope, Conn, Base, Shape, Projection>;

    fn into_set_parts(self) -> (&'conn Conn, Self::Arm) {
        (self.connection, SetLeaf::new(self.selected))
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> IntoInsertSelect<'conn, 'scope, Conn>
    for SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // Any row-lock state — a locked single select renders `INSERT … SELECT … FOR UPDATE`. The lock ban
    // applies only to set-op operands, via their `SetOperand` impls.
    Base: SelectAst<'conn, 'scope, Conn>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Row = Shape::Row;

    type InsertSelectQuery<S, Returning>
        = SqliteInsertSelect<
            'conn,
            'scope,
            S,
            SetLeaf<'conn, 'scope, Conn, Base, Shape, Projection>,
            Returning,
            Conn,
        >
    where
        S: InsertableTable,
        Returning: Projectable;

    fn into_insert_select<S, Returning>(
        self,
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        returning: Returning,
    ) -> Self::InsertSelectQuery<S, Returning>
    where
        S: InsertableTable,
        Returning: Projectable,
    {
        SqliteInsertSelect::new(connection, columns, SetLeaf::new(self.selected), returning)
    }
}

// A set-op source (`select(...).union(...)`, etc.) inserts as `INSERT INTO t (cols) SELECT … UNION …`.
// Its `SetOperand::Arm` is a `SetGroup` carrying the set tree plus any trailing `ORDER BY`/`LIMIT`.
impl<'conn, 'scope, Tree, Conn> IntoInsertSelect<'conn, 'scope, Conn>
    for SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;

    type InsertSelectQuery<S, Returning>
        = SqliteInsertSelect<'conn, 'scope, S, squealy::SetGroup<Tree>, Returning, Conn>
    where
        S: InsertableTable,
        Returning: Projectable;

    fn into_insert_select<S, Returning>(
        self,
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        returning: Returning,
    ) -> Self::InsertSelectQuery<S, Returning>
    where
        S: InsertableTable,
        Returning: Projectable,
    {
        // Use the *destination* `connection`; the source contributes only its set arm (with its tail).
        let (_source_connection, arm) = self.into_set_parts();
        SqliteInsertSelect::new(connection, columns, arm, returning)
    }
}

/// `INSERT INTO t (columns) <select>` query object (SQLite).
pub struct SqliteInsertSelect<'conn, 'scope, S, Tree, Returning, Conn = Sqlite> {
    #[allow(dead_code)]
    connection: &'conn Conn,
    columns: Vec<&'static str>,
    source: Tree,
    returning: Returning,
    _table: PhantomData<S>,
    _scope: PhantomData<&'scope ()>,
}

impl<'conn, 'scope, S, Tree, Returning, Conn>
    SqliteInsertSelect<'conn, 'scope, S, Tree, Returning, Conn>
{
    fn new(
        connection: &'conn Conn,
        columns: Vec<&'static str>,
        source: Tree,
        returning: Returning,
    ) -> Self {
        Self {
            connection,
            columns,
            source,
            returning,
            _table: PhantomData,
            _scope: PhantomData,
        }
    }
}

impl<'conn, 'scope, S, Tree, Returning, Conn>
    SqliteInsertSelect<'conn, 'scope, S, Tree, Returning, Conn>
where
    S: InsertableTable,
    Tree: render::RenderSetArm<'conn, 'scope, Conn, Sqlite>,
    Returning: RenderProjectable<Sqlite>,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    /// Render this `INSERT … SELECT` into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this `INSERT … SELECT`, returning a render reject (a query shape SQLite cannot render,
    /// such as a scoped recursive CTE arm in the source) as an error instead of panicking like
    /// [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError> {
        try_rendered_sql(|writer| {
            render::write_insert_select::<S, Conn, _, _>(
                &SqliteDialect,
                &self.columns,
                &self.source,
                &self.returning,
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError> {
        let mut params = Vec::new();
        render::write_insert_select_params::<S, Conn, _, _>(
            &SqliteDialect,
            &self.columns,
            &self.source,
            &self.returning,
            &mut params,
        )?;
        Ok(params)
    }

    /// Execute the insert, returning the number of rows affected.
    pub fn insert(&self) -> impl Future<Output = Result<u64, SqliteError>> + Send + '_
    where
        Conn: SqliteExecutor,
        // One-shot execution collects only static binds, so the source must be free of runtime `param`s.
        <Tree as SetArm<'conn, 'scope, Conn>>::Params: NoRuntimeParams,
    {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

impl<'conn, 'scope, Shape, Base, Projection, Conn> SetOperations<'conn, 'scope, Conn>
    for SqliteSelect<'conn, 'scope, Shape, Base, Projection, Conn>
where
    Shape: ProjectionShape,
    // Matches the `SetOperand` supertrait bound: a row-locked select cannot start a set operation.
    Base: SelectAst<'conn, 'scope, Conn, RowLockState = squealy::RowUnlocked>,
    Projection: Projectable,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type SetSelect<Tree>
        = SqliteSetSelect<'conn, 'scope, Tree, Conn>
    where
        Tree: SetArm<'conn, 'scope, Conn>;

    fn make_set_select<Tree>(connection: &'conn Conn, tree: Tree) -> Self::SetSelect<Tree>
    where
        Tree: SetArm<'conn, 'scope, Conn>,
    {
        SqliteSetSelect::new(connection, tree)
    }
}

impl<'conn, 'scope, Tree, Conn> SetOperand<'conn, 'scope, Conn>
    for SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Row = <Tree as SetArm<'conn, 'scope, Conn>>::Row;
    type Arm = squealy::SetGroup<Tree>;

    fn into_set_parts(self) -> (&'conn Conn, Self::Arm) {
        (self.connection, squealy::SetGroup::new(self.tree, self.tail))
    }
}

impl<'conn, 'scope, Tree, Conn> SetOperations<'conn, 'scope, Conn>
    for SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type SetSelect<NewTree>
        = SqliteSetSelect<'conn, 'scope, NewTree, Conn>
    where
        NewTree: SetArm<'conn, 'scope, Conn>;

    fn make_set_select<NewTree>(connection: &'conn Conn, tree: NewTree) -> Self::SetSelect<NewTree>
    where
        NewTree: SetArm<'conn, 'scope, Conn>,
    {
        SqliteSetSelect::new(connection, tree)
    }
}

impl<'conn, 'scope, Tree, Conn> SetSelectModifiers<'scope>
    for SqliteSetSelect<'conn, 'scope, Tree, Conn>
where
    Tree: SetArm<'conn, 'scope, Conn>,
    Conn: QueryBuilder<Backend = Sqlite>,
{
    type Shape = <Tree as SetArm<'conn, 'scope, Conn>>::Shape;

    fn set_tail_mut(&mut self) -> &mut SetTail {
        &mut self.tail
    }
}

impl<'conn, S, Shape, Rows, Returning, Conn> InsertQuery<'conn, Rows, Returning>
    for SqliteInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
    Shape::Row: Decode<Sqlite>,
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
    for SqliteInsert<'conn, S, Shape, Rows, Returning, Conn>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Conn: SqliteExecutor + 'conn,
    Shape::Row: Decode<Sqlite>,
    Rows: RenderInsertRows<Sqlite>,
    Rows::Params: NoRuntimeParams,
    Returning: RenderProjectable<Sqlite>,
{
    type RowStream<'query>
        = SqliteRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => SqliteRows::query(self.connection, sql, params),
            Err(error) => SqliteRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Filters, Returning, Conn> DeleteQuery<'conn, Filters, Returning>
    for SqliteDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
    Shape::Row: Decode<Sqlite>,
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
    for SqliteDelete<'conn, S, Shape, Filters, Returning, Conn>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Conn: SqliteExecutor + 'conn,
    Shape::Row: Decode<Sqlite>,
    Filters: RenderPredicateNodes<Sqlite>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Sqlite>,
{
    type RowStream<'query>
        = SqliteRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => SqliteRows::query(self.connection, sql, params),
            Err(error) => SqliteRows::error(error),
        }
    }
}

impl<'conn, S, Shape, Columns, Filters, Returning, Conn>
    UpdateQuery<'conn, Columns, Filters, Returning>
    for SqliteUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
    Shape::Row: Decode<Sqlite>,
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
    for SqliteUpdate<'conn, S, Shape, Columns, Filters, Returning, Conn>
where
    S: UpdateableTable,
    Shape: ProjectionShape,
    Conn: SqliteExecutor + 'conn,
    Shape::Row: Decode<Sqlite>,
    Columns: RenderUpdateAssignments<Sqlite>,
    Columns::Params: NoRuntimeParams,
    Filters: RenderPredicateNodes<Sqlite>,
    Filters::Params: NoRuntimeParams,
    Returning: RenderProjectable<Sqlite>,
{
    type RowStream<'query>
        = SqliteRows<'query, Self::Row, Conn>
    where
        Self: 'query;

    fn execute(
        &self,
    ) -> impl Future<Output = Result<u64, <<Self::Builder as QueryBuilder>::Backend as Backend>::Error>>
    + Send
    + '_ {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }

    fn fetch(&self) -> Self::RowStream<'_> {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => SqliteRows::query(self.connection, sql, params),
            Err(error) => SqliteRows::error(error),
        }
    }
}

// --- UPDATE … FROM (correlated update from a second source) ---

/// Correlated `UPDATE … FROM` query object (SQLite).
pub struct SqliteUpdateFrom<'conn, S, O, Columns = HNil, Filters = HNil, Conn = Sqlite> {
    #[allow(dead_code)]
    connection: &'conn Conn,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    columns: Columns,
    filters: Filters,
    _table: PhantomData<(S, O)>,
}

impl<'conn, S, O, Columns, Filters, Conn> SqliteUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: RenderUpdateAssignments<Sqlite>,
    Filters: RenderPredicateNodes<Sqlite>,
{
    /// Render this correlated update into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this correlated update, returning a render reject (a query shape SQLite cannot render,
    /// such as a scoped recursive CTE arm in the source) as an error instead of panicking like
    /// [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError> {
        try_rendered_sql(|writer| {
            render::write_update_from::<S, O, Sqlite, _, _, _>(
                &SqliteDialect,
                self.target_alias,
                self.source_alias,
                &self.columns,
                &self.filters,
                &(),
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError> {
        let mut params = Vec::new();
        render::write_update_from_params::<S, O, Sqlite, _, _, _>(
            &SqliteDialect,
            self.target_alias,
            self.source_alias,
            &self.columns,
            &self.filters,
            &(),
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, O, Columns, Filters, Conn> UpdateFromQuery<'conn, S, O, Columns, Filters>
    for SqliteUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: UpdateAssignments,
    Filters: PredicateNodes,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Builder = Conn;

    fn build(
        connection: &'conn Conn,
        target_alias: SourceAlias,
        source_alias: SourceAlias,
        columns: Columns,
        filters: Filters,
    ) -> Self {
        Self {
            connection,
            target_alias,
            source_alias,
            columns,
            filters,
            _table: PhantomData,
        }
    }
}

impl<'conn, S, O, Columns, Filters, Conn> ExecutableUpdateFromQuery<'conn, S, O, Columns, Filters>
    for SqliteUpdateFrom<'conn, S, O, Columns, Filters, Conn>
where
    S: UpdateableTable,
    O: SchemaTable,
    Columns: RenderUpdateAssignments<Sqlite>,
    Columns::Params: NoRuntimeParams,
    Filters: RenderPredicateNodes<Sqlite>,
    Filters::Params: NoRuntimeParams,
    Conn: SqliteExecutor + 'conn,
{
    fn execute(&self) -> impl Future<Output = Result<u64, SqliteError>> + Send + '_ {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

// --- DELETE … USING (correlated delete from a second source) ---

/// Correlated `DELETE … USING` query object (SQLite).
pub struct SqliteDeleteUsing<'conn, S, O, Filters = HNil, Conn = Sqlite> {
    #[allow(dead_code)]
    connection: &'conn Conn,
    target_alias: SourceAlias,
    source_alias: SourceAlias,
    filters: Filters,
    _table: PhantomData<(S, O)>,
}

impl<'conn, S, O, Filters, Conn> SqliteDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection,
    O: TableProjection,
    Filters: RenderPredicateNodes<Sqlite>,
{
    /// Render this correlated delete into a newly allocated SQL string.
    pub fn to_sql(&self) -> String {
        self.try_to_sql().expect("render SQL")
    }

    /// Renders this correlated delete, returning a render reject (a query shape SQLite cannot render,
    /// such as a scoped recursive CTE arm in the source) as an error instead of panicking like
    /// [`to_sql`](Self::to_sql).
    pub fn try_to_sql(&self) -> Result<String, SqliteError> {
        try_rendered_sql(|writer| {
            render::write_delete_using::<S, O, Sqlite, _, _>(
                &SqliteDialect,
                self.target_alias,
                self.source_alias,
                &self.filters,
                &(),
                writer,
            )
        })
        .map_err(SqliteError::Render)
    }

    /// Collect bind parameters into a newly allocated vector.
    pub fn collect_params(&self) -> Result<Vec<SqliteValue>, SqliteError> {
        let mut params = Vec::new();
        render::write_delete_using_params::<S, O, Sqlite, _, _>(
            &SqliteDialect,
            self.target_alias,
            self.source_alias,
            &self.filters,
            &(),
            &mut params,
        )?;
        Ok(params)
    }
}

impl<'conn, S, O, Filters, Conn> DeleteUsingQuery<'conn, S, O, Filters>
    for SqliteDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection,
    O: TableProjection,
    Filters: PredicateNodes,
    Conn: QueryBuilder<Backend = Sqlite> + 'conn,
{
    type Builder = Conn;

    fn build(
        connection: &'conn Conn,
        target_alias: SourceAlias,
        source_alias: SourceAlias,
        filters: Filters,
    ) -> Self {
        Self {
            connection,
            target_alias,
            source_alias,
            filters,
            _table: PhantomData,
        }
    }
}

impl<'conn, S, O, Filters, Conn> ExecutableDeleteUsingQuery<'conn, S, O, Filters>
    for SqliteDeleteUsing<'conn, S, O, Filters, Conn>
where
    S: TableProjection + UpdateableTable,
    O: TableProjection,
    Filters: RenderPredicateNodes<Sqlite>,
    Filters::Params: NoRuntimeParams,
    Conn: SqliteExecutor + 'conn,
{
    fn execute(&self) -> impl Future<Output = Result<u64, SqliteError>> + Send + '_ {
        match self
            .try_to_sql()
            .and_then(|sql| self.collect_params().map(|params| (sql, params)))
        {
            Ok((sql, params)) => self.connection.run_execute(sql, params),
            Err(error) => execute_error(error),
        }
    }
}

// `QueryBuilder` is implemented for the `Sqlite` marker (so query objects can be built and rendered
// driver-free, e.g. in render tests) and for the two runtime executors, `SqliteConnection` and
// `SqliteTransaction`, which additionally satisfy `SqliteExecutor` so the `Executable*` impls fire.
macro_rules! impl_query_builder_for {
    ($ty:ty) => {
        impl QueryBuilder for $ty {
            type Backend = Sqlite;

            type Select<'conn, 'scope, Base, Shape, Projection>
                = SqliteSelect<'conn, 'scope, Shape, Base, Projection, Self>
            where
                Self: 'conn,
                Base: SelectAst<'conn, 'scope, Self> + 'conn,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Projection: Projectable;

            type Insert<'conn, S, Shape, Rows, Returning>
                = SqliteInsert<'conn, S, Shape, Rows, Returning, Self>
            where
                Self: 'conn,
                S: InsertableTable,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Rows: InsertRows,
                Returning: Projectable;

            type Update<'conn, S, Shape, Columns, Filters, Returning>
                = SqliteUpdate<'conn, S, Shape, Columns, Filters, Returning, Self>
            where
                Self: 'conn,
                S: UpdateableTable,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Columns: UpdateAssignments,
                Filters: PredicateNodes,
                Returning: Projectable;

            type Delete<'conn, S, Shape, Filters, Returning>
                = SqliteDelete<'conn, S, Shape, Filters, Returning, Self>
            where
                Self: 'conn,
                S: TableProjection,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Filters: PredicateNodes,
                Returning: Projectable;

            type UpdateFrom<'conn, S, O, Columns, Filters>
                = SqliteUpdateFrom<'conn, S, O, Columns, Filters, Self>
            where
                Self: 'conn,
                S: UpdateableTable,
                O: squealy::SchemaTable,
                Columns: UpdateAssignments,
                Filters: PredicateNodes;

            type DeleteUsing<'conn, S, O, Filters>
                = SqliteDeleteUsing<'conn, S, O, Filters, Self>
            where
                Self: 'conn,
                S: TableProjection,
                O: TableProjection,
                Filters: PredicateNodes;
        }
    };
}
impl_query_builder_for!(Sqlite);
impl_query_builder_for!(SqliteConnection);
impl_query_builder_for!(SqliteTransaction<'_>);

// Upsert (`INSERT … ON CONFLICT DO UPDATE/NOTHING`): the conflict clause is a runtime field on the
// existing `SqliteInsert` query object, so `build_upsert` just constructs it with the clause attached.
macro_rules! impl_on_conflict_query_builder_for {
    ($ty:ty) => {
        impl squealy::OnConflictQueryBuilder for $ty {
            fn build_upsert<'conn, S, Shape, Rows, Returning>(
                &'conn self,
                rows: Rows,
                returning: Returning,
                conflict: squealy::ConflictClause,
            ) -> Self::Insert<'conn, S, Shape, Rows, Returning>
            where
                Self: 'conn,
                S: InsertableTable,
                Shape: ProjectionShape,
                Shape::Row: Decode<Self::Backend>,
                Rows: InsertRows,
                Returning: Projectable,
            {
                SqliteInsert::new_upsert(self, rows, returning, conflict)
            }
        }
    };
}
impl_on_conflict_query_builder_for!(Sqlite);
impl_on_conflict_query_builder_for!(SqliteConnection);
impl_on_conflict_query_builder_for!(SqliteTransaction<'_>);

// SQLite (3.35+) supports `RETURNING` on INSERT/UPDATE/DELETE; the bundled library is well past that,
// so the `.returning(...)` builder methods (gated on `Backend: SupportsReturning`) are available.
impl squealy::SupportsReturning for Sqlite {}

impl Connection for Sqlite {}
impl Connection for SqliteConnection {}
impl Connection for SqliteTransaction<'_> {}
