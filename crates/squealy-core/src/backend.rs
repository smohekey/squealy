use std::future::Future;
use std::io;

/// Backend-specific row cursor used while decoding a projected row.
pub trait RowReader: Sized {
	type Backend: Backend;

	fn read<T>(&mut self) -> Result<T, <Self::Backend as Backend>::Error>
	where
		T: Decode<Self::Backend>;
}

/// Decode a Rust value from a backend row reader.
pub trait Decode<B: Backend>: Sized {
	fn decode(row: &mut B::RowReader<'_>) -> Result<Self, B::Error>;
}

impl<B> Decode<B> for ()
where
	B: Backend,
{
	fn decode(_row: &mut B::RowReader<'_>) -> Result<Self, B::Error> {
		Ok(())
	}
}

/// Decode a nullable Rust value from a backend row reader.
///
/// Implementations return `None` when the next backend column is SQL `NULL`;
/// otherwise they decode and wrap the concrete value.
pub trait DecodeNullable<B: Backend>: Sized {
	fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error>;
}

macro_rules! impl_decode_nullable_via_option {
    ($($ty:ty),* $(,)?) => {
        $(impl<B> DecodeNullable<B> for $ty
        where
            B: Backend,
            Option<$ty>: Decode<B>,
        {
            fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error> {
                row.read::<Option<$ty>>()
            }
        })*
    };
}

impl_decode_nullable_via_option! {
		i8,
		i16,
		i32,
		i64,
		i128,
		isize,
		u8,
		u16,
		u32,
		u64,
		u128,
		usize,
		f32,
		f64,
		String,
		bool,
		Vec<u8>,
}

// Fixed-size byte arrays may appear in nullable / left-joined columns; const-generic mirror of the
// macro above. Resolves on backends that provide `Decode<B> for Option<[u8; N]>`.
impl<B, const N: usize> DecodeNullable<B> for [u8; N]
where
	B: Backend,
	Option<[u8; N]>: Decode<B>,
{
	fn decode_nullable(row: &mut B::RowReader<'_>) -> Result<Option<Self>, B::Error> {
		row.read::<Option<[u8; N]>>()
	}
}

// Native `uuid` column support: a `uuid::Uuid` field can appear in a nullable column or in a
// left-joined row, both of which require `DecodeNullable`. Resolves only on backends that provide
// `Decode<B> for Option<uuid::Uuid>` (the PostgreSQL backend does, behind its own `uuid` feature).
#[cfg(feature = "uuid")]
impl_decode_nullable_via_option! { uuid::Uuid }

// A `bytes::Bytes` column may be nullable or left-joined; resolves on backends that provide
// `Decode<B> for Option<bytes::Bytes>` (both backends do, behind their own `bytes` feature).
#[cfg(feature = "bytes")]
impl_decode_nullable_via_option! { bytes::Bytes }

// Native timestamp columns are commonly nullable (`deleted_at`, `expires_at`), which makes the
// table derive emit a `DecodeNullable` bound. Resolves only on backends that provide
// `Decode<B> for Option<T>` for the type, behind the same feature.
#[cfg(feature = "systemtime")]
impl_decode_nullable_via_option! { std::time::SystemTime }
#[cfg(feature = "time")]
impl_decode_nullable_via_option! { time::OffsetDateTime }
#[cfg(feature = "chrono")]
impl_decode_nullable_via_option! { chrono::DateTime<chrono::Utc> }

/// Backend-specific parameter cursor used while encoding bind values.
///
/// This is the encode-side mirror of [`RowReader`]: where a row reader pulls typed
/// values *out* of a backend row, a param writer pushes typed values *into* a backend
/// parameter list. Concrete writers may expose additional backend-private helpers (for
/// example, appending a typed `NULL`) used by that backend's own [`Encode`] impls.
pub trait ParamWriter: Sized {
	type Backend: Backend;

	fn write<T>(&mut self, value: &T) -> Result<(), <Self::Backend as Backend>::Error>
	where
		T: Encode<Self::Backend>;
}

/// Encode a Rust value into a backend parameter writer.
///
/// This is the mirror of [`Decode`]. Backends provide impls for the primitive types and
/// for `Option<T>` (nullability), exactly as they do for decoding; custom and native
/// types are added by implementing this trait for the backends that support them.
pub trait Encode<B: Backend> {
	fn encode(&self, out: &mut B::ParamWriter<'_>) -> Result<(), B::Error>;
}

impl<B> Encode<B> for ()
where
	B: Backend,
{
	fn encode(&self, _out: &mut B::ParamWriter<'_>) -> Result<(), B::Error> {
		Ok(())
	}
}

impl<B, T> Encode<B> for &T
where
	B: Backend,
	T: Encode<B> + ?Sized,
{
	fn encode(&self, out: &mut B::ParamWriter<'_>) -> Result<(), B::Error> {
		T::encode(self, out)
	}
}

/// Backend-specific query execution primitives.
pub trait Backend: Sized {
	type Error;

	type RowReader<'row>: RowReader<Backend = Self>;

	/// Encode-side mirror of [`RowReader`](Self::RowReader).
	type ParamWriter<'param>: ParamWriter<Backend = Self>;

	/// The backend's native bound-parameter representation (e.g. `PostgresParam`). A literal or
	/// runtime value is encoded into one of these via [`Encode`] before being handed to the driver.
	type Param;

	/// Construct a [`ParamWriter`](Self::ParamWriter) that appends encoded parameters to `params`.
	/// The shared renderer uses this to encode a single literal into [`Self::Param`].
	fn param_writer(params: &mut Vec<Self::Param>) -> Self::ParamWriter<'_>;

	fn no_rows_error() -> Self::Error;

	/// Construct the backend's error for a render reject — an [`io::Error`] the shared renderer
	/// returns when a query has no valid rendering for this dialect (e.g. a recursive CTE arm that
	/// carries its own `ORDER BY`/`LIMIT`/`OFFSET` targeting SQLite, whose grammar forbids the
	/// parenthesized arm that scoping requires). The runtime render collectors map that `io::Error`
	/// through this so a query render surfaces a returned error rather than panicking.
	fn render_error(error: io::Error) -> Self::Error;
}

/// Marker for backends whose dialect supports a `RETURNING` clause on data-modifying statements
/// (PostgreSQL). The `insert_returning`/`update_returning`/`delete_returning` builders require it, so
/// a backend that does not implement it (such as MySQL, which has no `RETURNING`) rejects those
/// queries at compile time rather than failing at runtime.
pub trait SupportsReturning: Backend {}

/// Marker for backends whose dialect supports `FULL [OUTER] JOIN` (PostgreSQL). The `full_join`
/// builder requires it, so a backend that does not implement it (such as MySQL, which has no
/// `FULL JOIN`) rejects `full_join` at compile time rather than emitting SQL the database can't parse.
/// `RIGHT JOIN` needs no marker — both backends support it.
pub trait SupportsFullJoin: Backend {}

/// Marker for backends whose dialect supports a query-level named `WINDOW` clause
/// (`SELECT … OVER w … WINDOW w AS (…)`) — every real backend (PostgreSQL, MySQL 8.0+, SQLite 3.25+).
/// The [`.window()`](crate::SourceQuery::window) builder requires it. It is deliberately *not*
/// implemented for the view-model backend (`ModelBackend`), so named windows in a view body are a
/// compile error: the view model does not yet carry window definitions (a query-only first cut).
pub trait SupportsNamedWindow: Backend {}

/// Marker for backends whose dialect supports `date_trunc(unit, ts)` (PostgreSQL). The `date_trunc`
/// expression's `RenderAst` requires it, so a backend that does not implement it (such as MySQL, which
/// has no `date_trunc`) rejects `date_trunc` at compile time. (`now` needs no marker — every backend
/// renders `CURRENT_TIMESTAMP`.)
pub trait SupportsDateTrunc: Backend {}

/// Marker for backends whose dialect supports `EXTRACT(<field> FROM <ts>)` (PostgreSQL and MySQL).
/// The `extract`/`extract_second` expressions' `RenderAst` requires it, so a backend that does not
/// implement it (SQLite, which has no `EXTRACT` syntax — it uses `strftime`) rejects `extract` at
/// compile time rather than rendering SQL that fails to prepare.
pub trait SupportsExtract: Backend {}

/// Opens a query connection from a backend-specific connection string.
pub trait Connect {
	type Connection: crate::Connection;
	type Error;

	fn connect(
		&self,
		url: &str,
	) -> impl Future<Output = Result<Self::Connection, Self::Error>> + Send;
}
