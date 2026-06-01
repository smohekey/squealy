use crate::{Expr, ForeignKey};

/// Controls how table fields are represented.
pub trait ColumnMode {
    type Type<'scope, U>;
}

/// Table fields are typed SQL expressions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnExpr {}

impl ColumnMode for ColumnExpr {
    type Type<'scope, U> = Expr<'scope, U>;
}

/// Table fields are database column names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnName {}

impl ColumnMode for ColumnName {
    type Type<'scope, U> = &'static str;
}

/// Table fields are plain Rust values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnValue {}

impl ColumnMode for ColumnValue {
    type Type<'scope, U> = U;
}

/// Table fields are nullable Rust values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ColumnNullableValue {}

impl ColumnMode for ColumnNullableValue {
    type Type<'scope, U> = Option<U>;
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

macro_rules! impl_column_type {
    ($($ty:ty => $kind:ident),* $(,)?) => {
        $(
            impl HasColumnType for $ty {
                const COLUMN_TYPE: ColumnType = ColumnType::$kind;
            }
        )*
    };
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
}

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
