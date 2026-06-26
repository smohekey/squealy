use crate::{Expr, ForeignKey};

/// Controls how table fields are represented. The field value type `U` is always a column value
/// type (it implements [`ColumnNullability`]), which lets the nullable-value mode peel a declared
/// `Option<T>` down to its inner `T` before re-wrapping, so a LEFT JOIN of an already-nullable column
/// yields a single `Option<T>` rather than `Option<Option<T>>`.
pub trait ColumnMode {
    type Type<'scope, U>
    where
        U: ColumnNullability;
}

/// Table fields are typed SQL expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnExpr {}

impl ColumnMode for ColumnExpr {
    type Type<'scope, U>
        = Expr<'scope, U>
    where
        U: ColumnNullability;
}

/// Table fields are database column names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnName {}

impl ColumnMode for ColumnName {
    type Type<'scope, U>
        = &'static str
    where
        U: ColumnNullability;
}

/// Table fields are plain Rust values: the declared type `U` (`Option<T>` for a nullable column).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnValue {}

impl ColumnMode for ColumnValue {
    type Type<'scope, U>
        = U
    where
        U: ColumnNullability;
}

/// Table fields are nullable Rust values (a LEFT JOIN makes every column nullable). Peels the
/// declared type to its inner `T` first, so an already-nullable `Option<T>` column stays a single
/// `Option<T>` here rather than `Option<Option<T>>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnNullableValue {}

impl ColumnMode for ColumnNullableValue {
    type Type<'scope, U>
        = Option<<U as ColumnNullability>::Inner>
    where
        U: ColumnNullability;
}

/// A backend-agnostic column default.
///
/// Most variants describe a structured value that backends render safely (for
/// example, [`ColumnDefault::Text`] is quoted and escaped). [`ColumnDefault::Raw`]
/// is the exception: its string is emitted verbatim into the generated `DEFAULT`
/// clause without escaping or validation. It comes from a compile-time
/// `default_raw = "..."` attribute, so it is the caller's responsibility to supply
/// a valid backend default expression.
#[derive(Clone, Debug, PartialEq)]
pub enum ColumnDefault {
    Null,
    Int(i128),
    UInt(u128),
    Float(f64),
    Text(&'static str),
    Bool(bool),
    CurrentTimestamp,
    CurrentDate,
    CurrentTime,
    /// A backend-specific default expression emitted verbatim, without escaping.
    Raw(&'static str),
}

/// Backend-agnostic column type metadata.
///
/// Variants describe the Rust-side value shape. Backend crates decide how those
/// shapes become concrete DDL types for their database. For example, PostgreSQL
/// can render [`ColumnType::I32`] as `integer`, while another backend may choose
/// a different native spelling.
///
/// [`ColumnType::Raw`] is the escape hatch for callers that need to name a
/// backend-specific type directly, such as `jsonb`, `varchar(64)`, or a database
/// domain. Raw type names are intentionally not interpreted by the core crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnType {
    I8,
    I16,
    I32,
    I64,
    I128,
    Isize,
    U8,
    U16,
    U32,
    U64,
    U128,
    Usize,
    F32,
    F64,
    String,
    Bool,
    // Structured types produced by parsing `#[column(db_type = "...")]` and (later) by introspection.
    Varchar(u32),
    Char(u32),
    Text,
    Decimal {
        precision: u32,
        scale: u32,
    },
    Date,
    Time {
        tz: bool,
    },
    Timestamp {
        tz: bool,
    },
    Uuid,
    Json,
    Jsonb,
    Bytes,
    /// A fixed-width binary column of `N` bytes (`[u8; N]`). PostgreSQL renders `bytea` with a
    /// generated `CHECK (octet_length(col) = N)`; MySQL renders `BINARY(N)`.
    FixedBytes(u32),
    Raw(&'static str),
}

/// Maps a Rust value type to backend-specific column DDL.
///
/// The [`Table`](crate::Table) derive uses this trait for each field unless the
/// field has an explicit `#[column(db_type = "...")]` override. Primitive Rust
/// types have built-in implementations. Domain newtypes can derive `ColumnType`
/// to use their single field's mapping while also gaining transparent bind
/// conversion, row decoding, and literal expression support.
///
/// If a table field's value type does not implement this trait, the table derive
/// fails to compile unless the field supplies a raw `db_type` override.
pub trait HasColumnType {
    const COLUMN_TYPE: ColumnType;
}

/// Resolves a column field's *declared* value type into its inner (non-null) value type and its
/// nullability, at the type level — so the `Table` derive never inspects `Option<…>` tokens and a
/// type alias to `Option<…>` resolves correctly. Implemented explicitly per value type (no blanket
/// over `T`, mirroring [`HasColumnType`]) plus once for `Option<T>`; the `Option<T>` impl requires a
/// non-null inner, so `Option<Option<T>>` does not resolve.
pub trait ColumnNullability {
    type Inner;
    type Nullability;
    const NULLABLE: bool;
}

/// Emits the non-null [`ColumnNullability`] impl for a value type. Re-exported for the `ColumnType`
/// derive and backend column types (e.g. `Json`), and for `db_type` column value types.
#[macro_export]
#[doc(hidden)]
macro_rules! impl_non_null_column {
    ($ty:ty) => {
        impl $crate::ColumnNullability for $ty {
            type Inner = $ty;
            type Nullability = $crate::NonNullableColumn;
            const NULLABLE: bool = false;
        }
    };
}

macro_rules! impl_column_type {
    ($($ty:ty => $kind:ident),* $(,)?) => {
        $(
            impl HasColumnType for $ty {
                const COLUMN_TYPE: ColumnType = ColumnType::$kind;
            }

            impl ColumnNullability for $ty {
                type Inner = $ty;
                type Nullability = crate::NonNullableColumn;
                const NULLABLE: bool = false;
            }
        )*
    };
}

/// `Option<T>` is the in-type spelling of a nullable column: its `COLUMN_TYPE` is the inner type's,
/// and it is nullable with inner `T`. The `ColumnNullability` bound requires a non-null inner, so
/// `Option<Option<T>>` does not resolve.
impl<T> HasColumnType for Option<T>
where
    T: HasColumnType,
{
    const COLUMN_TYPE: ColumnType = T::COLUMN_TYPE;
}

impl<T> ColumnNullability for Option<T>
where
    T: ColumnNullability<Nullability = crate::NonNullableColumn>,
{
    type Inner = T;
    type Nullability = crate::NullableColumn;
    const NULLABLE: bool = true;
}

impl_column_type! {
    i8 => I8,
    i16 => I16,
    i32 => I32,
    i64 => I64,
    i128 => I128,
    isize => Isize,
    u8 => U8,
    u16 => U16,
    u32 => U32,
    u64 => U64,
    u128 => U128,
    usize => Usize,
    f32 => F32,
    f64 => F64,
    String => String,
    bool => Bool,
    Vec<u8> => Bytes,
}

/// Fixed-size byte arrays map to a fixed-width binary column (`bytea` + a length CHECK on PostgreSQL,
/// `BINARY(N)` on MySQL). The matching `Encode`/`Decode` (which length-checks on read) lives in each
/// backend crate.
impl<const N: usize> HasColumnType for [u8; N] {
    const COLUMN_TYPE: ColumnType = ColumnType::FixedBytes(N as u32);
}
impl<const N: usize> ColumnNullability for [u8; N] {
    type Inner = [u8; N];
    type Nullability = crate::NonNullableColumn;
    const NULLABLE: bool = false;
}

/// A bare `uuid::Uuid` field maps to a `uuid` column, so no `#[column(db_type = "uuid")]`
/// override is needed. The matching `Encode`/`Decode` lives in each backend crate behind its own
/// `uuid` feature.
#[cfg(feature = "uuid")]
impl HasColumnType for uuid::Uuid {
    const COLUMN_TYPE: ColumnType = ColumnType::Uuid;
}
#[cfg(feature = "uuid")]
crate::impl_non_null_column!(uuid::Uuid);

/// Native timestamp columns. Each maps to a timezone-aware `timestamptz` column, so no
/// `#[column(db_type = "...")]` override is needed. The matching `Encode`/`Decode` lives in each
/// backend crate behind the same feature.
#[cfg(feature = "systemtime")]
impl HasColumnType for std::time::SystemTime {
    const COLUMN_TYPE: ColumnType = ColumnType::Timestamp { tz: true };
}
#[cfg(feature = "systemtime")]
crate::impl_non_null_column!(std::time::SystemTime);

#[cfg(feature = "time")]
impl HasColumnType for time::OffsetDateTime {
    const COLUMN_TYPE: ColumnType = ColumnType::Timestamp { tz: true };
}
#[cfg(feature = "time")]
crate::impl_non_null_column!(time::OffsetDateTime);

#[cfg(feature = "chrono")]
impl HasColumnType for chrono::DateTime<chrono::Utc> {
    const COLUMN_TYPE: ColumnType = ColumnType::Timestamp { tz: true };
}
#[cfg(feature = "chrono")]
crate::impl_non_null_column!(chrono::DateTime<chrono::Utc>);

/// Database schema metadata for a single column.
pub trait Column: Sync {
    fn name(&self) -> &'static str;

    fn primary_key(&self) -> bool {
        false
    }

    fn indexed(&self) -> bool {
        false
    }

    fn unique(&self) -> bool {
        false
    }

    /// An optional partial-index predicate for a `#[column(unique, where = |row| ...)]` marker.
    ///
    /// Only meaningful when [`unique`](Self::unique) is `true`. When present, the model builder
    /// emits a partial *unique index* (`CREATE UNIQUE INDEX ... WHERE <predicate>`) instead of a
    /// plain `UNIQUE` constraint. See [`Index::predicate`](crate::Index::predicate) for why this
    /// is a function rather than a `&'static str`.
    fn unique_predicate(&self) -> Option<fn() -> String> {
        None
    }

    fn nullable(&self) -> bool {
        false
    }

    fn auto_increment(&self) -> bool {
        false
    }

    fn generated(&self) -> bool {
        false
    }

    fn insertable(&self) -> bool {
        !self.generated() && !self.auto_increment()
    }

    fn updateable(&self) -> bool {
        !self.generated() && !self.auto_increment()
    }

    fn default(&self) -> Option<ColumnDefault> {
        None
    }

    /// Returns the logical column type used by backend DDL renderers.
    ///
    /// This is never optional: a column either gets its type from
    /// [`HasColumnType`] or from a raw `#[column(db_type = "...")]` override.
    fn column_type(&self) -> ColumnType;

    /// An optional `CHECK` constraint expression, emitted verbatim into DDL.
    ///
    /// The string is rendered into the generated `CHECK (...)` clause without
    /// escaping or validation, so it must be a valid backend boolean expression.
    /// It originates from a compile-time `check = "..."` attribute, not runtime
    /// input.
    fn check(&self) -> Option<&'static str> {
        None
    }

    fn references(&self) -> Option<&'static dyn ForeignKey> {
        None
    }
}
