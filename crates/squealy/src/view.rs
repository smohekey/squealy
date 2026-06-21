//! Canonical "model" backend and the view-body lowering it drives.
//!
//! A view body is a `SELECT` that must render into a backend-neutral [`ViewQueryModel`] with its
//! literals **inlined** (a view definition carries no bind parameters). The query builder only lets a
//! literal out through [`Encode`], and only lowers a query through a [`SelectSink`] bound to a
//! concrete [`Backend`]. So this module defines a render-only [`ModelBackend`]/[`ModelConn`] whose
//! `Encode` impls emit SQL-literal text, and a [`ModelSink`] that reuses the existing render helpers
//! to fill a structural [`ViewQueryModel`]. None of this ever executes — it exists purely to turn a
//! typed definition into the neutral model that backends render `CREATE VIEW` from.

use std::borrow::Cow;
use std::io::{self, Write};
use std::marker::PhantomData;

use crate::render::{
    Renderer, SqlWriter, write_column_value, write_expr_value, write_order_value,
    write_predicate_value,
};
use crate::{
    Backend, ColumnRef, Decode, Dialect, Encode, Expr, ExprFragment, ExprKind, InsertableTable,
    JoinItem, JoinKind, Order, ParamWriter, Predicate, PredicateKind, Projectable, ProjectionItem,
    ProjectionShape, ProjectionVisitor, QueryBuilder, RenderAst, RenderPredicateAst,
    RenderProjectable, RenderSelectAst, RowReader, SelectAst, SelectSink, Selected, SourceAlias,
    SourceRef, SqlType, Table, TableProjection, ViewQueryModel,
};

// ---------------------------------------------------------------------------
// Canonical backend
// ---------------------------------------------------------------------------

/// A render-only backend whose native parameter is the SQL-literal text of a value. It never connects
/// or executes; it exists so the shared renderer can inline literals into a view body.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelBackend;

/// The render-only connection that view definitions are built against. Constructing the query AST
/// requires a [`QueryBuilder`]; this one only provides the type machinery, never execution.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelConn;

/// The (unconstructed) error type for [`ModelBackend`]. Rendering a literal to text cannot fail.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelError;

/// Collects rendered SQL-literal text. [`ModelBackend::Param`] is the literal string itself.
pub struct ModelParamWriter<'params> {
    params: &'params mut Vec<String>,
}

impl ModelParamWriter<'_> {
    fn push_literal(&mut self, literal: String) {
        self.params.push(literal);
    }
}

impl ParamWriter for ModelParamWriter<'_> {
    type Backend = ModelBackend;

    fn write<T>(&mut self, value: &T) -> Result<(), ModelError>
    where
        T: Encode<ModelBackend>,
    {
        value.encode(self)
    }
}

/// The (unconstructed) row reader for [`ModelBackend`]; the model backend never reads rows.
pub struct ModelRowReader<'row>(PhantomData<&'row ()>, ModelNever);

/// An uninhabited type that makes [`ModelRowReader`] impossible to construct.
enum ModelNever {}

impl RowReader for ModelRowReader<'_> {
    type Backend = ModelBackend;

    fn read<T>(&mut self) -> Result<T, ModelError>
    where
        T: Decode<ModelBackend>,
    {
        match self.1 {}
    }
}

impl Backend for ModelBackend {
    type Error = ModelError;
    type RowReader<'row> = ModelRowReader<'row>;
    type ParamWriter<'param> = ModelParamWriter<'param>;
    type Param = String;

    fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_> {
        ModelParamWriter { params }
    }

    fn no_rows_error() -> Self::Error {
        ModelError
    }

    fn write_table(&self, _table: &(dyn Table + Sync), _writer: &mut impl Write) -> io::Result<()> {
        // The model backend renders views, not tables; table DDL goes through the real backends.
        unreachable!("ModelBackend does not render table DDL")
    }
}

// View bodies are dialect-neutral, so the model backend allows `full_join` (a full-join view is valid
// against PostgreSQL; deploying it to MySQL — which has no `FULL JOIN` — fails at DDL exec, as noted on
// `full_join`).
impl crate::SupportsFullJoin for ModelBackend {}

// ---------------------------------------------------------------------------
// Literal encoding: every value becomes its SQL-literal text
// ---------------------------------------------------------------------------

macro_rules! encode_display {
    ($($ty:ty),* $(,)?) => {
        $(
            impl Encode<ModelBackend> for $ty {
                fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
                    out.push_literal(self.to_string());
                    Ok(())
                }
            }
        )*
    };
}

encode_display!(
    i8, i16, i32, i64, i128, isize, u8, u16, u32, u64, u128, usize, f32, f64
);

impl Encode<ModelBackend> for bool {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        out.push_literal(if *self { "TRUE" } else { "FALSE" }.to_owned());
        Ok(())
    }
}

impl Encode<ModelBackend> for str {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        out.push_literal(format!("'{}'", self.replace('\'', "''")));
        Ok(())
    }
}

impl Encode<ModelBackend> for String {
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        self.as_str().encode(out)
    }
}

impl<T> Encode<ModelBackend> for Option<T>
where
    T: Encode<ModelBackend>,
{
    fn encode(&self, out: &mut ModelParamWriter<'_>) -> Result<(), ModelError> {
        match self {
            Some(value) => value.encode(out),
            None => {
                out.push_literal("NULL".to_owned());
                Ok(())
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Canonical dialect: ANSI-quoted identifiers, literals already inlined (no placeholders)
// ---------------------------------------------------------------------------

/// The canonical dialect used to render view fragments: ANSI double-quoted identifiers and no
/// placeholders (literals are inlined by [`ModelWriter`], so the placeholder is a no-op).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ModelDialect;

const MODEL_DIALECT: &dyn Dialect = &ModelDialect;

impl Dialect for ModelDialect {
    fn write_placeholder(&self, _index: usize, _writer: &mut dyn Write) -> io::Result<()> {
        // Literals are written inline by `ModelWriter::push_bind`; there is no placeholder to emit.
        Ok(())
    }

    fn write_quoted_ident(&self, ident: &str, writer: &mut dyn Write) -> io::Result<()> {
        write!(writer, "\"{}\"", ident.replace('"', "\"\""))
    }

    fn write_cast_type(&self, ty: &SqlType, writer: &mut dyn Write) -> io::Result<()> {
        // A neutral spelling; casts are rare in view bodies and the backends accept the standard names.
        let name = match ty {
            SqlType::Bool => "boolean",
            SqlType::F32 => "real",
            SqlType::F64 => "double precision",
            SqlType::String | SqlType::Text => "text",
            _ => "numeric",
        };
        writer.write_all(name.as_bytes())
    }

    fn integer_division_needs_float_cast(&self) -> bool {
        // Avoid injecting backend-specific float casts into portable fragments.
        false
    }
}

// ---------------------------------------------------------------------------
// Literal-inlining writer
// ---------------------------------------------------------------------------

/// A render target that inlines each literal as SQL text instead of recording a bind. Used to render
/// one expression fragment of a view body into memory.
struct ModelWriter {
    buf: Vec<u8>,
}

impl Write for ModelWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl SqlWriter<ModelBackend> for ModelWriter {
    fn push_bind<T>(&mut self, value: &T)
    where
        T: Encode<ModelBackend>,
    {
        let mut params = Vec::new();
        {
            let mut writer = ModelBackend::param_writer(&mut params);
            // `encode` is infallible for the model backend.
            let _ = value.encode(&mut writer);
        }
        for literal in params {
            self.buf.extend_from_slice(literal.as_bytes());
        }
    }

    fn push_runtime_bind(&mut self, _index: usize) {
        // A view body cannot carry runtime parameters; the builder never emits one here.
    }
}

/// Renders one expression fragment of a view body into canonical text.
fn fragment<F>(render: F) -> ExprFragment
where
    F: FnOnce(&mut ModelWriter, &mut Renderer) -> io::Result<()>,
{
    let mut writer = ModelWriter { buf: Vec::new() };
    let mut renderer = Renderer::new(MODEL_DIALECT);
    render(&mut writer, &mut renderer).expect("rendering a view fragment to memory cannot fail");
    ExprFragment(String::from_utf8(writer.buf).expect("the renderer only writes UTF-8"))
}

fn and(slot: &mut Option<ExprFragment>, fragment: ExprFragment) {
    *slot = Some(match slot.take() {
        Some(previous) => ExprFragment(format!("{} AND {}", previous.0, fragment.0)),
        None => fragment,
    });
}

fn source_ref<S>(alias: SourceAlias) -> SourceRef
where
    S: TableProjection,
{
    SourceRef {
        schema: <S as TableProjection>::schema_name().map(str::to_owned),
        name: <S as TableProjection>::name().to_owned(),
        alias: alias.to_string(),
    }
}

// ---------------------------------------------------------------------------
// The sink that captures a view's structural body
// ---------------------------------------------------------------------------

/// A [`SelectSink`] that records a query's structure into a [`ViewQueryModel`] instead of emitting a
/// flat `SELECT` string. Scalar expressions are rendered to canonical [`ExprFragment`] text.
#[derive(Default)]
pub(crate) struct ModelSink {
    query: ViewQueryModel,
}

impl SelectSink for ModelSink {
    type Error = io::Error;
    type Backend = ModelBackend;

    fn push_projection<Shape, P>(&mut self, projection: P) -> Result<(), Self::Error>
    where
        Shape: ProjectionShape,
        P: RenderProjectable<ModelBackend>,
    {
        projection.visit_projection(self)
    }

    fn push_table_source<S>(&mut self, alias: SourceAlias) -> Result<(), Self::Error>
    where
        S: TableProjection,
    {
        self.query.from = Some(source_ref::<S>(alias));
        Ok(())
    }

    fn push_inner_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = fragment(|writer, renderer| write_predicate_value(&on, writer, renderer));
        self.query.joins.push(JoinItem {
            kind: JoinKind::Inner,
            source: source_ref::<S>(alias),
            on,
        });
        Ok(())
    }

    fn push_left_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = fragment(|writer, renderer| write_predicate_value(&on, writer, renderer));
        self.query.joins.push(JoinItem {
            kind: JoinKind::Left,
            source: source_ref::<S>(alias),
            on,
        });
        Ok(())
    }

    fn push_right_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = fragment(|writer, renderer| write_predicate_value(&on, writer, renderer));
        self.query.joins.push(JoinItem {
            kind: JoinKind::Right,
            source: source_ref::<S>(alias),
            on,
        });
        Ok(())
    }

    fn push_full_join<S, P, Ast>(
        &mut self,
        alias: SourceAlias,
        on: Predicate<'_, P, Ast>,
    ) -> Result<(), Self::Error>
    where
        S: TableProjection,
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let on = fragment(|writer, renderer| write_predicate_value(&on, writer, renderer));
        self.query.joins.push(JoinItem {
            kind: JoinKind::Full,
            source: source_ref::<S>(alias),
            on,
        });
        Ok(())
    }

    fn push_filter<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let predicate =
            fragment(|writer, renderer| write_predicate_value(&predicate, writer, renderer));
        and(&mut self.query.filter, predicate);
        Ok(())
    }

    fn push_group<K, Ast>(&mut self, key: &Expr<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        let key = fragment(|writer, renderer| write_expr_value(key, writer, renderer));
        self.query.group_by.push(key);
        Ok(())
    }

    fn push_having<P, Ast>(&mut self, predicate: Predicate<'_, P, Ast>) -> Result<(), Self::Error>
    where
        P: PredicateKind,
        Ast: RenderPredicateAst<ModelBackend>,
    {
        let predicate =
            fragment(|writer, renderer| write_predicate_value(&predicate, writer, renderer));
        and(&mut self.query.having, predicate);
        Ok(())
    }

    fn push_order<K, Ast>(&mut self, order: Order<'_, K, Ast>) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        // `write_order_value` renders the expression plus `ASC`/`DESC`; the direction is baked into the
        // fragment, so the structural direction/nulls stay `None`.
        let expr = fragment(|writer, renderer| write_order_value(&order, writer, renderer));
        self.query.order_by.push(crate::OrderItem {
            expr,
            direction: None,
            nulls: None,
        });
        Ok(())
    }

    fn set_limit(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.query.limit = Some(rows);
        Ok(())
    }

    fn set_offset(&mut self, rows: usize) -> Result<(), Self::Error> {
        self.query.offset = Some(rows);
        Ok(())
    }

    fn set_distinct(&mut self) -> Result<(), Self::Error> {
        self.query.distinct = true;
        Ok(())
    }
}

impl ProjectionVisitor for ModelSink {
    type Error = io::Error;
    type Backend = ModelBackend;

    fn visit_expr<K, Ast>(
        &mut self,
        expr: &Expr<'_, K, Ast>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind,
        Ast: RenderAst<ModelBackend>,
    {
        let expr = fragment(|writer, renderer| write_expr_value(expr, writer, renderer));
        self.query.projection.push(ProjectionItem {
            output_name: alias.into_owned(),
            expr,
        });
        Ok(())
    }

    fn visit_column<K>(
        &mut self,
        column: ColumnRef<'_, K>,
        alias: Cow<'static, str>,
    ) -> Result<(), Self::Error>
    where
        K: ExprKind,
    {
        let expr = fragment(|writer, renderer| write_column_value(column, writer, renderer));
        self.query.projection.push(ProjectionItem {
            output_name: alias.into_owned(),
            expr,
        });
        Ok(())
    }
}

/// Lowers a typed, projected query (built against [`ModelConn`]) into a neutral [`ViewQueryModel`].
#[doc(hidden)]
pub fn lower_view<'conn, 'scope, Base, Shape, Projection>(
    selected: &Selected<'scope, Base, Shape, Projection>,
) -> ViewQueryModel
where
    Base: RenderSelectAst<'conn, 'scope, ModelConn, ModelBackend>,
    Shape: ProjectionShape,
    Projection: RenderProjectable<ModelBackend>,
{
    let mut sink = ModelSink::default();
    selected
        .lower_into::<ModelConn, _>(&mut sink)
        .expect("rendering a view body to memory cannot fail");
    sink.query
}

// ---------------------------------------------------------------------------
// View definition surface
// ---------------------------------------------------------------------------

/// A projected query usable as a view body. Implemented by [`Selected`] when it is built against
/// [`ModelConn`]; [`Self::Row`] is the projection's decoded row type, which the compile-time check in
/// [`ViewDefinition`] pins to the view's declared columns.
pub trait ViewSelect {
    /// The decoded row type of the projection (the ordered column types).
    type Row;

    /// Lower the query body into the neutral model.
    fn lower(&self) -> ViewQueryModel;
}

impl<'scope, Base, Shape, Projection> ViewSelect for Selected<'scope, Base, Shape, Projection>
where
    Shape: ProjectionShape,
    Projection: Projectable + RenderProjectable<ModelBackend>,
    Base: RenderSelectAst<'static, 'scope, ModelConn, ModelBackend>,
    // A view body has no bind parameters — every value is inlined as a literal. Requiring an empty
    // runtime-parameter shape rejects a definition that uses `param::<K>()` at compile time, rather
    // than silently dropping the placeholder and emitting invalid DDL.
    <Base as SelectAst<'static, 'scope, ModelConn>>::Params: crate::NoRuntimeParams,
{
    type Row = Shape::Row;

    fn lower(&self) -> ViewQueryModel {
        lower_view(self)
    }
}

/// A view's declared output schema: its name, namespace, and typed columns. `#[derive(View)]`
/// generates this from the struct fields; it is the view analogue of `SchemaTable`.
pub trait SchemaView {
    /// The declared output row type (the ordered column types), matched against the body's projection.
    type Row;

    fn schema_name() -> Option<&'static str>;

    fn view_name() -> &'static str;

    fn view_columns() -> Vec<crate::ViewColumnModel>;
}

/// The user-written body of a view: the `SELECT` that produces its declared columns. The metadata
/// comes from [`SchemaView`] (generated by `#[derive(View)]`), so the user writes only
/// [`definition`](Self::definition), against [`ModelConn`] using
/// [`project`](crate::SourceQuery::project).
///
/// The return type `impl ViewSelect<Row = <Self as SchemaView>::Row>` is the compile-time guarantee:
/// the body's projection must decode to the same row type as the declared columns, so a mismatch is a
/// type error.
pub trait ViewDefinition: SchemaView {
    /// The view body, built against the render-only connection. The `&'static` borrow keeps the
    /// resulting query fully owned (no caller-tied lifetime), which is all the model walker needs.
    fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row>;
}

/// Any [`ViewDefinition`] is an object-safe [`ViewDef`](crate::ViewDef) the model walker can consume.
impl<T> crate::ViewDef for T
where
    T: ViewDefinition + Sync,
{
    fn schema_name(&self) -> Option<&'static str> {
        <T as SchemaView>::schema_name()
    }

    fn name(&self) -> &'static str {
        <T as SchemaView>::view_name()
    }

    fn columns(&self) -> Vec<crate::ViewColumnModel> {
        <T as SchemaView>::view_columns()
    }

    fn definition_model(&self) -> ViewQueryModel {
        view_definition_model::<T>()
    }
}

/// Lowers a [`ViewDefinition`] type's body into the neutral model without needing an instance. The
/// `#[derive(Schema)]`-generated `ViewDef` shims call this so a view registers from its type alone.
#[doc(hidden)]
pub fn view_definition_model<T>() -> ViewQueryModel
where
    T: ViewDefinition,
{
    static MODEL_CONN: ModelConn = ModelConn;
    T::definition(&MODEL_CONN).lower()
}

// ---------------------------------------------------------------------------
// Stub `QueryBuilder`: only the type machinery, never constructed
// ---------------------------------------------------------------------------

macro_rules! never_query {
    ($name:ident) => {
        #[doc(hidden)]
        pub struct $name<T: ?Sized>(PhantomData<T>, ModelNever);
    };
}

never_query!(ModelSelect);
never_query!(ModelInsert);
never_query!(ModelUpdate);
never_query!(ModelDelete);

impl<'builder, 'scope, Base, Shape, Projection>
    crate::SelectQuery<'builder, 'scope, Base, Projection>
    for ModelSelect<(&'builder (), &'scope (), Base, Shape, Projection)>
where
    Base: SelectAst<'builder, 'scope, ModelConn>,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Projection: Projectable,
{
    type Builder = ModelConn;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build_selected(
        _builder: &'builder ModelConn,
        _selected: Selected<'scope, Base, Shape, Projection>,
    ) -> Self {
        unreachable!("ModelConn never builds a select")
    }
}

impl<'builder, S, Shape, Rows, Returning> crate::InsertQuery<'builder, Rows, Returning>
    for ModelInsert<(&'builder (), S, Shape, Rows, Returning)>
where
    S: InsertableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Rows: crate::InsertRows,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(_builder: &'builder ModelConn, _rows: Rows, _returning: Returning) -> Self {
        unreachable!("ModelConn never builds an insert")
    }
}

impl<'builder, S, Shape, Columns, Filters, Returning>
    crate::UpdateQuery<'builder, Columns, Filters, Returning>
    for ModelUpdate<(&'builder (), S, Shape, Columns, Filters, Returning)>
where
    S: crate::UpdateableTable,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Columns: crate::UpdateAssignments,
    Filters: crate::PredicateNodes,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        _builder: &'builder ModelConn,
        _alias: SourceAlias,
        _columns: Columns,
        _filters: Filters,
        _returning: Returning,
    ) -> Self {
        unreachable!("ModelConn never builds an update")
    }
}

impl<'builder, S, Shape, Filters, Returning> crate::DeleteQuery<'builder, Filters, Returning>
    for ModelDelete<(&'builder (), S, Shape, Filters, Returning)>
where
    S: TableProjection,
    Shape: ProjectionShape,
    Shape::Row: Decode<ModelBackend> + Send,
    Filters: crate::PredicateNodes,
    Returning: Projectable,
{
    type Builder = ModelConn;
    type Table = S;
    type Shape = Shape;
    type Row = Shape::Row;

    fn build(
        _builder: &'builder ModelConn,
        _alias: SourceAlias,
        _filters: Filters,
        _returning: Returning,
    ) -> Self {
        unreachable!("ModelConn never builds a delete")
    }
}

impl QueryBuilder for ModelConn {
    type Backend = ModelBackend;

    type Select<'builder, 'scope, Base, Shape, Projection>
        = ModelSelect<(&'builder (), &'scope (), Base, Shape, Projection)>
    where
        Self: 'builder,
        Base: 'builder,
        Base: SelectAst<'builder, 'scope, Self>,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Projection: Projectable;

    type Insert<'builder, S, Shape, Rows, Returning>
        = ModelInsert<(&'builder (), S, Shape, Rows, Returning)>
    where
        Self: 'builder,
        S: InsertableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Rows: crate::InsertRows,
        Returning: Projectable;

    type Update<'builder, S, Shape, Columns, Filters, Returning>
        = ModelUpdate<(&'builder (), S, Shape, Columns, Filters, Returning)>
    where
        Self: 'builder,
        S: crate::UpdateableTable,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Columns: crate::UpdateAssignments,
        Filters: crate::PredicateNodes,
        Returning: Projectable;

    type Delete<'builder, S, Shape, Filters, Returning>
        = ModelDelete<(&'builder (), S, Shape, Filters, Returning)>
    where
        Self: 'builder,
        S: TableProjection,
        Shape: ProjectionShape,
        Shape::Row: Decode<Self::Backend>,
        Filters: crate::PredicateNodes,
        Returning: Projectable;
}
