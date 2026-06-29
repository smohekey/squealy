/// A fixed homogeneous list used by core builders.
pub trait FixedList<T> {
    fn len(&self) -> usize;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn try_for_each<E>(&self, f: impl FnMut(&T) -> Result<(), E>) -> Result<(), E>;
}

/// Append an item to a typed list, producing the widened list type.
pub trait TupleAppend<T>: FixedList<T> + Sized {
    type Output: FixedList<T>;

    fn append(self, value: T) -> Self::Output;
}

/// Concatenate two typed lists.
pub trait TupleConcat<T, Rhs>: FixedList<T> + Sized
where
    Rhs: FixedList<T>,
{
    type Output: FixedList<T>;

    fn concat(self, rhs: Rhs) -> Self::Output;
}

/// Map a typed homogeneous list to another typed homogeneous list of the same width.
pub trait MapFixedList<T, U>: FixedList<T> + Sized {
    type Output: FixedList<U>;

    fn map_list(self, f: impl FnMut(T) -> U) -> Self::Output;
}

/// Empty heterogeneous list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HNil;

/// Non-empty heterogeneous list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HCons<Head, Tail> {
    pub head: Head,
    pub tail: Tail,
}

/// A heterogeneous list whose width is known in the type system.
pub trait HList {
    const LEN: usize;
}

/// Marker for query ASTs that contain no runtime parameters.
///
/// Execution APIs require this marker, so queries containing `param::<T>()`
/// must be prepared before values are supplied.
pub trait NoRuntimeParams: HList {}

/// Append a value to the end of a heterogeneous list.
pub trait PushBack<T>: HList + Sized {
    type Output: HList;

    fn push_back(self, value: T) -> Self::Output;
}

/// Concatenate two heterogeneous lists.
pub trait HAppend<Rhs>: HList + Sized
where
    Rhs: HList,
{
    type Output: HList;
}

/// Convert a heterogeneous list to a same-order tuple.
pub trait ToTuple {
    type Tuple;

    fn to_tuple(self) -> Self::Tuple;
}

// === Type-level set membership ===
//
// Frunk-style, index-disambiguated membership over an `HList`: the `Index` (`Here`/`There<…>`) is an
// auxiliary type the compiler infers, so the `Contains` impls never overlap (no specialization needed).
// Used to enforce that a `SELECT DISTINCT` chain's `ORDER BY` keys all appear in the projection — see
// [`OrderKeysInProjection`](crate::OrderKeysInProjection).

/// Membership witness: the searched element is at the head of the list.
#[doc(hidden)]
pub struct Here;

/// Membership witness: the searched element is somewhere in the tail (`Index` away from there).
#[doc(hidden)]
pub struct There<Index>(core::marker::PhantomData<Index>);

/// `Self` contains type `T` (with the position witnessed by `Index`). Currently used only by the
/// `SELECT DISTINCT` + `ORDER BY` guard, so its unsatisfied-bound message is phrased for that case.
/// (`INSERT … SELECT` column coverage uses the parallel [`CoversColumn`] leaf so it gets its own
/// message — rustc surfaces the *deepest* failing trait, so the message must live on the leaf, not on
/// a wrapper like [`AllContained`].)
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`SELECT DISTINCT` requires every `ORDER BY` key to also be in the projection",
    note = "add the ordering column(s) to the projection, or drop `.distinct()`"
)]
pub trait Contains<T, Index> {}

impl<T, Tail> Contains<T, Here> for HCons<T, Tail> {}

// A key `K` is also satisfied by a projected `Nullable<K>`: a `RIGHT`/`FULL JOIN` nullable-wraps the
// base columns in the projection, but an `ORDER BY` key captured *before* the join is still the bare
// `K`. (No overlap with the bare-head impl above: `K = Nullable<K>` has no solution.)
impl<T, Tail> Contains<T, Here> for HCons<crate::Nullable<T>, Tail> {}

impl<T, Head, Tail, Index> Contains<T, There<Index>> for HCons<Head, Tail> where
    Tail: Contains<T, Index>
{
}

/// The target column list `Set` (an `HList` of column kinds) contains the required column `T` (position
/// witnessed by `Index`). A parallel of [`Contains`] dedicated to `INSERT … SELECT` required-column
/// coverage so its unsatisfied-bound message is phrased for that case — rustc surfaces the deepest
/// failing trait, so the message must live here on the leaf rather than on the [`RequiredCovered`]
/// wrapper.
#[doc(hidden)]
#[diagnostic::on_unimplemented(
    message = "`INSERT … SELECT` must list every required (non-null, no-default) column of the target table",
    note = "add the missing required column(s) to the `INSERT … SELECT` target column list"
)]
pub trait CoversColumn<T, Index> {}

impl<T, Tail> CoversColumn<T, Here> for HCons<T, Tail> {}

impl<T, Head, Tail, Index> CoversColumn<T, There<Index>> for HCons<Head, Tail> where
    Tail: CoversColumn<T, Index>
{
}

/// A required insert column: its kind `K` paired with its type-level nullability `N`
/// ([`NonNullableColumn`](crate::NonNullableColumn) / [`NullableColumn`](crate::NullableColumn)). The
/// `Table` derive lists one per insertable, no-default column, resolving `N` via `ColumnNullability`
/// (the same type-level path as the setter-based insert's readiness bounds — so a nullable column
/// declared through a type alias is still recognized as omittable). [`RequiredCovered`] then requires
/// only the non-null ones appear in an `INSERT … SELECT` target.
#[doc(hidden)]
pub struct RequiredCol<K, N>(core::marker::PhantomData<(K, N)>);

/// Witness that a required column was satisfied by being omittable (nullable) rather than by membership
/// in the target list.
#[doc(hidden)]
pub struct Omittable;

/// Every **non-null** required column in `Self` (an `HList` of [`RequiredCol`]) appears in the target
/// column list `Set`; nullable required columns are omittable and skipped. Mirrors the setter-based
/// insert's type-level nullability. `Indices` are the per-element witnesses (a membership index for a
/// covered non-null column, [`Omittable`] for a skipped nullable one), inferred by the compiler.
#[doc(hidden)]
pub trait RequiredCovered<Set, Indices> {}

impl<Set> RequiredCovered<Set, HNil> for HNil {}

// A non-null required column must be present in the target list.
impl<Set, K, Tail, Index, RestIndices> RequiredCovered<Set, HCons<Index, RestIndices>>
    for HCons<RequiredCol<K, crate::NonNullableColumn>, Tail>
where
    Set: CoversColumn<K, Index>,
    Tail: RequiredCovered<Set, RestIndices>,
{
}

// A nullable required column is omittable — skip it (no membership requirement). The concrete
// nullability marker in the head keeps this from overlapping the non-null impl above.
impl<Set, K, Tail, RestIndices> RequiredCovered<Set, HCons<Omittable, RestIndices>>
    for HCons<RequiredCol<K, crate::NullableColumn>, Tail>
where
    Tail: RequiredCovered<Set, RestIndices>,
{
}

/// Nullable-wrap an accumulated source-exprs list (each element a table's exprs struct) when building
/// a `RIGHT`/`FULL JOIN`, where the base columns become nullable. Per-table impls are generated by the
/// `Table` derive (mapping each `ColumnRef<K>` to `ColumnRef<Nullable<K>>`, and treating an
/// already-nullable exprs struct as idempotent); these `HList` impls thread it element-wise.
pub trait IntoNullableExprs {
    type Output;

    fn into_nullable_exprs(self) -> Self::Output;
}

impl IntoNullableExprs for HNil {
    type Output = HNil;

    fn into_nullable_exprs(self) -> Self::Output {
        self
    }
}

impl<Head, Tail> IntoNullableExprs for HCons<Head, Tail>
where
    Head: IntoNullableExprs,
    Tail: IntoNullableExprs,
{
    type Output = HCons<Head::Output, Tail::Output>;

    fn into_nullable_exprs(self) -> Self::Output {
        HCons {
            head: self.head.into_nullable_exprs(),
            tail: self.tail.into_nullable_exprs(),
        }
    }
}

/// Runtime values supplied when executing a prepared statement.
///
/// Parameterized by the backend `B` (mirroring how the row type is `Decode<B>`) so that
/// each supplied value can be encoded via [`Encode<B>`](crate::Encode) directly into the
/// backend's [`ParamWriter`](crate::ParamWriter), with no neutral intermediate.
pub trait PreparedParamValues<Shape, B>
where
    Shape: HList,
    B: crate::Backend,
{
    fn write_params(&self, writer: &mut B::ParamWriter<'_>) -> Result<(), B::Error>;

    fn write_param_at(
        &self,
        index: usize,
        writer: &mut B::ParamWriter<'_>,
    ) -> Result<bool, B::Error>;
}

/// Coerces a value supplied to a prepared statement into its runtime-parameter shape
/// entry type, which the backend then encodes via [`Encode`](crate::Encode).
///
/// This carries the type-level check that a supplied value is compatible with the
/// query's declared parameter type (e.g. `&str` for a `String` parameter), and performs
/// any owning conversion that requires. The resulting value is encoded by the backend.
pub trait IntoPreparedParam<T> {
    fn into_prepared_param(self) -> T;
}

/// Reflexive identity: any value can be supplied for a parameter of its own type.
///
/// This is what keeps the open codec usable for prepared *runtime* parameters, not just inline
/// literals: a custom type that implements [`Encode<B>`](crate::Encode) — for example a derived
/// newtype over `uuid::Uuid`, or a backend's `Json<T>` wrapper — can be passed to `param::<T>()`
/// with no extra impl. The convenience borrow conversions below (`&str` → `String`, etc.) cover the
/// non-reflexive cases and do not overlap, since their `Self` and target types differ.
impl<T> IntoPreparedParam<T> for T {
    fn into_prepared_param(self) -> T {
        self
    }
}

impl IntoPreparedParam<String> for &str {
    fn into_prepared_param(self) -> String {
        self.to_owned()
    }
}

impl IntoPreparedParam<String> for &String {
    fn into_prepared_param(self) -> String {
        self.clone()
    }
}

impl IntoPreparedParam<Option<String>> for Option<&str> {
    fn into_prepared_param(self) -> Option<String> {
        self.map(str::to_owned)
    }
}

impl IntoPreparedParam<Option<String>> for Option<&String> {
    fn into_prepared_param(self) -> Option<String> {
        self.cloned()
    }
}

impl<T> FixedList<T> for () {
    fn len(&self) -> usize {
        0
    }

    fn try_for_each<E>(&self, _f: impl FnMut(&T) -> Result<(), E>) -> Result<(), E> {
        Ok(())
    }
}

impl<T> TupleAppend<T> for () {
    type Output = (T,);

    fn append(self, value: T) -> Self::Output {
        (value,)
    }
}

impl<T, Rhs> TupleConcat<T, Rhs> for ()
where
    Rhs: FixedList<T>,
{
    type Output = Rhs;

    fn concat(self, rhs: Rhs) -> Self::Output {
        rhs
    }
}

impl<T, U> MapFixedList<T, U> for () {
    type Output = ();

    fn map_list(self, _f: impl FnMut(T) -> U) -> Self::Output {}
}

impl HList for HNil {
    const LEN: usize = 0;
}

impl NoRuntimeParams for HNil {}

impl<Head, Tail> HList for HCons<Head, Tail>
where
    Tail: HList,
{
    const LEN: usize = Tail::LEN + 1;
}

impl<T> PushBack<T> for HNil {
    type Output = HCons<T, HNil>;

    fn push_back(self, value: T) -> Self::Output {
        HCons {
            head: value,
            tail: HNil,
        }
    }
}

impl<Rhs> HAppend<Rhs> for HNil
where
    Rhs: HList,
{
    type Output = Rhs;
}

impl<Head, Tail, Rhs> HAppend<Rhs> for HCons<Head, Tail>
where
    Tail: HAppend<Rhs>,
    Rhs: HList,
{
    type Output = HCons<Head, <Tail as HAppend<Rhs>>::Output>;
}

impl<Head, Tail, T> PushBack<T> for HCons<Head, Tail>
where
    Tail: PushBack<T>,
{
    type Output = HCons<Head, <Tail as PushBack<T>>::Output>;

    fn push_back(self, value: T) -> Self::Output {
        HCons {
            head: self.head,
            tail: self.tail.push_back(value),
        }
    }
}

impl ToTuple for HNil {
    type Tuple = ();

    fn to_tuple(self) -> Self::Tuple {}
}

impl<B> PreparedParamValues<HNil, B> for ()
where
    B: crate::Backend,
{
    fn write_params(&self, _writer: &mut B::ParamWriter<'_>) -> Result<(), B::Error> {
        Ok(())
    }

    fn write_param_at(
        &self,
        _index: usize,
        _writer: &mut B::ParamWriter<'_>,
    ) -> Result<bool, B::Error> {
        Ok(false)
    }
}

squealy_macros::tuple_fixed_lists!(32);
squealy_macros::hlist_tuples!(32);
squealy_macros::prepared_param_values!(32);
