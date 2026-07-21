use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span, TokenStream as TokenStream2};

use crate::common::{
	bool_tokens, foreign_key_ident, generated_ident, is_attribute_start, literal_string,
	matches_ident, option_literal, parse_db_type, required_literal, to_pascal, to_snake_plural,
	MacroError,
};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
	match table_struct(input) {
		Ok(table) => table.expand(SourceMode::Table),
		Err(error) => error.into_compile_error(),
	}
}

/// Which kind of queryable relation the projection machinery is being expanded for. A table is
/// read-write; a view and a CTE are read-only (no insert/update surface). A table and a view also get
/// a [`QuerySource`](squealy::QuerySource) impl marking them as non-CTE `FROM` sources, while a CTE
/// gets its `QuerySource` impl (carrying its [`CteDef`](squealy::CteDef)) from the `CTE` derive itself.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceMode {
	Table,
	View,
	Cte,
}

impl SourceMode {
	/// Read-only relations (views and CTEs) omit the write-side impls.
	fn read_only(self) -> bool {
		matches!(self, SourceMode::View | SourceMode::Cte)
	}

	/// Tables and views get a no-CTE [`QuerySource`] impl here; a CTE supplies its own.
	fn emits_query_source(self) -> bool {
		matches!(self, SourceMode::Table | SourceMode::View)
	}
}

pub(crate) struct TableStruct {
	pub(crate) ident: Ident,
	visibility: TokenStream2,
	pub(crate) fields: Vec<Field>,
	indexes: Vec<IndexAttrs>,
	primary_key: Option<PrimaryKeyAttrs>,
	uniques: Vec<UniqueAttrs>,
	pub(crate) schema: Option<proc_macro2::TokenStream>,
}

pub(crate) struct Field {
	pub(crate) ident: Ident,
	pub(crate) value_ty: proc_macro2::TokenStream,
	attrs: FieldAttrs,
}

struct IndexAttrs {
	name: Option<String>,
	columns: Vec<Ident>,
	unique: bool,
	/// Raw tokens of a `where = |row| ...` partial-index predicate, lowered to an ANSI SQL string
	/// at model-build time. See [`crate`]'s `Table` derive docs for the supported expression subset.
	predicate: Option<proc_macro2::TokenStream>,
}

struct PrimaryKeyAttrs {
	name: Option<String>,
	columns: Vec<Ident>,
}

struct UniqueAttrs {
	name: Option<String>,
	columns: Vec<Ident>,
	/// Raw tokens of a `where = |row| ...` predicate. When present, the constraint is lowered to a
	/// partial unique index (`CREATE UNIQUE INDEX ... WHERE ...`) rather than a `UNIQUE` constraint.
	predicate: Option<proc_macro2::TokenStream>,
}

#[derive(Default)]
struct TableAttrs {
	indexes: Vec<IndexAttrs>,
	primary_key: Option<PrimaryKeyAttrs>,
	uniques: Vec<UniqueAttrs>,
	schema: Option<proc_macro2::TokenStream>,
}

#[derive(Default)]
struct FieldAttrs {
	column_name: Option<String>,
	primary_key: bool,
	index: bool,
	unique: bool,
	auto_increment: bool,
	generated: bool,
	insert: Option<bool>,
	update: Option<bool>,
	default: Option<DefaultAttrs>,
	db_type: Option<String>,
	check: Option<String>,
	references: Option<ForeignKeyAttrs>,
	/// Raw tokens of a `#[column(unique, where = |row| ...)]` partial-unique predicate. Requires
	/// `unique`; lowered to a partial unique index at model-build time.
	predicate: Option<proc_macro2::TokenStream>,
}

enum DefaultAttrs {
	Literal(DefaultLiteral),
	CurrentTimestamp,
	CurrentDate,
	CurrentTime,
	Raw(String),
}

enum DefaultLiteral {
	Null,
	Int(i128),
	UInt(u128),
	Float(f64),
	Text(String),
	Bool(bool),
}

#[derive(Default)]
struct ForeignKeyAttrs {
	table: Option<Ident>,
	column: Option<Ident>,
	on_delete: Option<String>,
	on_update: Option<String>,
}

impl TableStruct {
	/// Expands the projection/metadata machinery. For a read-only [`SourceMode`] (view/CTE) the
	/// write-side impls (`InsertableTable`/`UpdateableTable` and the write builder) are omitted: such
	/// a relation is a read-only `FROM` source, so it gets queryable projections without an
	/// insert/update surface.
	pub(crate) fn expand(&self, mode: SourceMode) -> TokenStream {
		let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
		let name = Literal::string(&to_snake_plural(&ident.to_string()));
		let fields = self
			.fields
			.iter()
			.map(|field| proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site()))
			.collect::<Vec<_>>();
		let field_literals = self
			.fields
			.iter()
			.map(|field| Literal::string(&field.column_name()))
			.collect::<Vec<_>>();
		let column_idents = self
			.fields
			.iter()
			.map(|field| generated_ident(&ident, &field.ident.to_string(), "Column"))
			.collect::<Vec<_>>();
		let exprs_ident = generated_ident(&ident, "exprs", "Projection");
		let rebound_exprs_ident = generated_ident(&ident, "exprs", "ReboundProjection");
		let nullable_exprs_ident = generated_ident(&ident, "nullable_exprs", "Projection");
		let nullable_rebound_exprs_ident =
			generated_ident(&ident, "nullable_exprs", "ReboundProjection");
		let row_shape_ident = generated_ident(&ident, "row", "Shape");
		let expr_kind_idents = self
			.fields
			.iter()
			.map(|field| {
				proc_macro2::Ident::new(
					&format!("{}{}", ident, to_pascal(&field.ident.to_string())),
					Span::call_site(),
				)
			})
			.collect::<Vec<_>>();
		// The whole-row projection's kinds as an `HList`, so a `SELECT DISTINCT … ORDER BY col` whose
		// projection is the bare row (`select(|(u,)| u)`) can check the ordering key against the row's
		// columns. Mirrors the per-column kinds of the exprs struct (`ColumnRef<K>` for each field).
		let row_kind_list = expr_kind_idents.iter().rev().fold(
			quote::quote! { ::squealy::HNil },
			|tail, kind| quote::quote! { ::squealy::HCons<#kind, #tail> },
		);
		// The *required* insert columns (insertable, no default) as a list of `RequiredCol<K,
		// Nullability>` — the set an `INSERT … SELECT`'s target columns must cover. Nullability is
		// resolved type-level via `ColumnNullability` (matching `insert_ready_bounds`, not a syntactic
		// `Option<…>` check), so the coverage check treats a column nullable through a type alias as
		// omittable. A defaulted column is always omittable, so it is excluded entirely.
		let required_insert_cols = self
			.fields
			.iter()
			.zip(expr_kind_idents.iter())
			.filter(|(field, _)| field.insertable() && field.attrs.default.is_none())
			.map(|(field, kind)| {
				let d = field.value_ty.clone();
				quote::quote! {
						::squealy::RequiredCol<#kind, <#d as ::squealy::ColumnNullability>::Nullability>
				}
			})
			.collect::<Vec<_>>();
		let required_insert_kind_list = required_insert_cols.iter().rev().fold(
			quote::quote! { ::squealy::HNil },
			|tail, item| quote::quote! { ::squealy::HCons<#item, #tail> },
		);
		// The *declared* field value type `D` (e.g. `Option<SystemTime>` or `SystemTime`). Nullability
		// is resolved from it at the type level via `ColumnNullability`.
		let field_value_tys = self
			.fields
			.iter()
			.map(|field| field.value_ty.clone())
			.collect::<Vec<_>>();
		// The inner (non-null) value type `<D as ColumnNullability>::Inner`. Used for the column's
		// `ExprKind::Value` (so predicate operands stay `T`), FK type assertions, and the nullable
		// (left-join) row decode.
		let field_inner_tys = self
			.fields
			.iter()
			.map(|field| {
				let d = field.value_ty.clone();
				quote::quote! { <#d as ::squealy::ColumnNullability>::Inner }
			})
			.collect::<Vec<_>>();
		// The column's type-level nullability marker (`NonNullableColumn` / `NullableColumn`).
		let field_nullability_tys = self
			.fields
			.iter()
			.map(|field| {
				let d = field.value_ty.clone();
				quote::quote! { <#d as ::squealy::ColumnNullability>::Nullability }
			})
			.collect::<Vec<_>>();
		// For each `references(Table::column)` foreign key, assert at compile time that the local
		// column's inner value type matches the referenced column's.
		let fk_type_assertions = self
			.fields
			.iter()
			.filter_map(|field| {
				let references = field.attrs.references.as_ref()?;
				let table = references.table.as_ref()?;
				let column = references.column.as_ref()?;
				let referenced_marker = proc_macro2::Ident::new(
					&format!("{}{}", table, to_pascal(&column.to_string())),
					Span::call_site(),
				);
				let d = field.value_ty.clone();
				Some(quote::quote! {
						const _: fn() = || {
								fn assert_foreign_key_column_type<A, B>()
								where
										A: ::squealy::SameValue<B>,
								{
								}
								assert_foreign_key_column_type::<
										<#d as ::squealy::ColumnNullability>::Inner,
										<#referenced_marker as ::squealy::ExprKind>::Value,
								>();
						};
				})
			})
			.collect::<Vec<_>>();
		// A primary-key or auto-increment column must be non-null. The macro-time `nullable()` check
		// catches a literal `Option<T>` with a friendly message, but a type *alias* to `Option<…>`
		// is token-invisible; this type-level assertion rejects it too, so the generated
		// `Column::nullable()` can never disagree with the declared key.
		let non_null_assertions = self
			.fields
			.iter()
			.filter(|field| field.attrs.primary_key || field.attrs.auto_increment)
			.map(|field| {
				let d = field.value_ty.clone();
				quote::quote! {
						const _: fn() = || {
								fn assert_non_null_column<T>()
								where
										T: ::squealy::ColumnNullability<Nullability = ::squealy::NonNullableColumn>,
								{
								}
								assert_non_null_column::<#d>();
						};
				}
			})
			.collect::<Vec<_>>();
		let field_indexes = self
			.fields
			.iter()
			.filter(|field| field.attrs.index)
			.map(|field| {
				(
					field,
					generated_ident(&ident, &field.ident.to_string(), "Index"),
				)
			})
			.collect::<Vec<_>>();
		let mut index_idents = field_indexes
			.iter()
			.map(|(_, ident)| ident.clone())
			.collect::<Vec<_>>();
		index_idents.extend(
			self
				.indexes
				.iter()
				.enumerate()
				.map(|(index, _)| generated_ident(&ident, &index.to_string(), "Index")),
		);
		// Partial-predicate (`where = |row| ...`) helper functions, accumulated across the
		// per-column unique markers, table-level `#[unique(...)]`, and table-level `#[index(...)]`
		// declarations. Each pushes a `fn() -> String` definition and yields the
		// `Option<fn() -> String>` reference spliced into the corresponding metadata accessor.
		let mut predicate_fn_defs: Vec<proc_macro2::TokenStream> = Vec::new();
		let field_unique_predicate_refs = self
			.fields
			.iter()
			.map(|field| match field.attrs.predicate.as_ref() {
				Some(closure) => predicate_fn_reference(
					&ident,
					&format!("{}_unique", field.ident),
					closure,
					&mut predicate_fn_defs,
				),
				None => quote::quote! { ::std::option::Option::None },
			})
			.collect::<Vec<_>>();
		let column_defs = self
			.fields
			.iter()
			.zip(column_idents.iter())
			.zip(field_unique_predicate_refs.iter())
			.map(|((field, ident), unique_predicate)| {
				field.column_definition_tokens(ident, unique_predicate)
			})
			.collect::<Vec<_>>();
		let foreign_key_defs = self
			.fields
			.iter()
			.zip(column_idents.iter())
			.filter_map(|(field, ident)| field.foreign_key_definition_tokens(ident))
			.collect::<Vec<_>>();
		let mut index_defs = field_indexes
			.iter()
			.map(|(field, ident)| field.index_definition_tokens(ident))
			.collect::<Vec<_>>();
		let field_index_count = index_defs.len();
		index_defs.extend(self.indexes.iter().enumerate().map(|(index, attrs)| {
			let predicate = match attrs.predicate.as_ref() {
				Some(closure) => predicate_fn_reference(
					&ident,
					&format!("index_{index}"),
					closure,
					&mut predicate_fn_defs,
				),
				None => quote::quote! { ::std::option::Option::None },
			};
			attrs.index_definition_tokens(
				&index_idents[field_index_count + index],
				&self.fields,
				&predicate,
			)
		}));
		let columns_static = generated_ident(&ident, "columns", "Static");
		let indexes_static = generated_ident(&ident, "indexes", "Static");
		let columns_len = Literal::usize_unsuffixed(column_idents.len());
		let indexes_len = Literal::usize_unsuffixed(index_idents.len());
		// When a table-level `#[primary_key(...)]` is declared, emit a static column-name list and
		// override `primary_key()` on both trait impls. Otherwise emit nothing and let the trait
		// defaults return `None`, leaving the per-column hoist in the model builder untouched.
		let primary_key_static = generated_ident(&ident, "primary_key", "Static");
		let (primary_key_static_def, table_primary_key_method, schema_primary_key_method) =
			match self.primary_key.as_ref() {
				Some(primary_key) => {
					let name = option_literal(primary_key.name.as_deref());
					let pk_columns = primary_key
						.columns
						.iter()
						.map(|column| {
							let column = column.to_string();
							let field = self
								.fields
								.iter()
								.find(|field| field.ident.to_string() == column)
								.expect("primary key fields are validated before code generation");
							Literal::string(&field.column_name())
						})
						.collect::<Vec<_>>();
					let pk_len = Literal::usize_unsuffixed(pk_columns.len());
					(
						quote::quote! {
								static #primary_key_static: [&'static str; #pk_len] = [#( #pk_columns, )*];
						},
						quote::quote! {
								fn primary_key(&self) -> Option<::squealy::TablePrimaryKey> {
										<Self as ::squealy::SchemaTable>::primary_key()
								}
						},
						quote::quote! {
								fn primary_key() -> Option<::squealy::TablePrimaryKey> {
										Some(::squealy::TablePrimaryKey {
												name: #name,
												columns: &#primary_key_static,
										})
								}
						},
					)
				}
				None => (quote::quote! {}, quote::quote! {}, quote::quote! {}),
			};
		// Table-level `#[unique(columns = [..])]` constraints. Each one gets a static column-name
		// list; the constraints themselves are gathered into a single static slice and surfaced
		// through `uniques()` on both trait impls. Tables without any declaration fall through to
		// the trait defaults (an empty slice).
		let unique_column_statics = self
			.uniques
			.iter()
			.enumerate()
			.map(|(unique, attrs)| {
				let columns_static = generated_ident(&ident, &format!("unique_{unique}"), "ColumnsStatic");
				let columns = attrs
					.columns
					.iter()
					.map(|column| {
						let column = column.to_string();
						let field = self
							.fields
							.iter()
							.find(|field| field.ident.to_string() == column)
							.expect("unique fields are validated before code generation");
						Literal::string(&field.column_name())
					})
					.collect::<Vec<_>>();
				let columns_len = Literal::usize_unsuffixed(columns.len());
				quote::quote! {
						static #columns_static: [&'static str; #columns_len] = [#( #columns, )*];
				}
			})
			.collect::<Vec<_>>();
		let unique_entries = self
			.uniques
			.iter()
			.enumerate()
			.map(|(unique, attrs)| {
				let columns_static = generated_ident(&ident, &format!("unique_{unique}"), "ColumnsStatic");
				let name = option_literal(attrs.name.as_deref());
				let predicate = match attrs.predicate.as_ref() {
					Some(closure) => predicate_fn_reference(
						&ident,
						&format!("unique_{unique}"),
						closure,
						&mut predicate_fn_defs,
					),
					None => quote::quote! { ::std::option::Option::None },
				};
				quote::quote! {
						::squealy::TableUnique {
								name: #name,
								columns: &#columns_static,
								predicate: #predicate,
						}
				}
			})
			.collect::<Vec<_>>();
		let uniques_static = generated_ident(&ident, "uniques", "Static");
		let uniques_len = Literal::usize_unsuffixed(unique_entries.len());
		let (uniques_static_def, table_uniques_method, schema_uniques_method) =
			if self.uniques.is_empty() {
				(quote::quote! {}, quote::quote! {}, quote::quote! {})
			} else {
				(
					quote::quote! {
							#( #unique_column_statics )*
							static #uniques_static: [::squealy::TableUnique; #uniques_len] =
									[#( #unique_entries, )*];
					},
					quote::quote! {
							fn uniques(&self) -> &'static [::squealy::TableUnique] {
									<Self as ::squealy::SchemaTable>::uniques()
							}
					},
					quote::quote! {
							fn uniques() -> &'static [::squealy::TableUnique] {
									&#uniques_static
							}
					},
				)
			};
		let schema = self
			.schema
			.clone()
			.unwrap_or_else(|| quote::quote! { ::squealy::DefaultSchema });
		let write_builder = self.write_builder_tokens(&ident, &expr_kind_idents);
		let visibility = &self.visibility;
		// A view/CTE is read-only: skip the write builder and the insertable/updateable markers.
		let (write_builder_defs, write_table_impl, write_marker_impls) = if mode.read_only() {
			(
				proc_macro2::TokenStream::new(),
				proc_macro2::TokenStream::new(),
				proc_macro2::TokenStream::new(),
			)
		} else {
			(
				write_builder.definitions,
				write_builder.table_impl,
				quote::quote! {
						impl<'scope> ::squealy::InsertableTable for #ident <'scope, ::squealy::ColumnExpr> {
								type RequiredInsertColumns = #required_insert_kind_list;
						}

						impl<'scope> ::squealy::UpdateableTable for #ident <'scope, ::squealy::ColumnExpr> {}
				},
			)
		};
		// A table or view is a non-CTE `FROM` source: mark it `QuerySource` so the query builder
		// accepts it (and contributes no `WITH` entry). A CTE's `QuerySource` impl is emitted by the
		// `CTE` derive instead, carrying its `CteDef`.
		let query_source_impl = if mode.emits_query_source() {
			quote::quote! {
					impl<'scope, C: ::squealy::ColumnMode> ::squealy::QuerySource for #ident <'scope, C>
					where
							#ident <'scope, C>: ::squealy::TableProjection,
					{}
			}
		} else {
			proc_macro2::TokenStream::new()
		};
		let insert_column_key_impls = self
			.fields
			.iter()
			.zip(expr_kind_idents.iter())
			.filter(|(field, _)| field.insertable())
			.map(|(_, expr_kind_ident)| {
				quote::quote! {
						impl ::squealy::InsertColumnKey for #expr_kind_ident {}
				}
			});
		let update_column_key_impls = self
			.fields
			.iter()
			.zip(expr_kind_idents.iter())
			.filter(|(field, _)| field.updateable())
			.map(|(_, expr_kind_ident)| {
				quote::quote! {
						impl ::squealy::UpdateColumnKey for #expr_kind_ident {}
				}
			});
		// A nullable column's kind is `NullableExpr`, which gates the `is_null` / `is_not_null`
		// predicate builders so they are only callable on columns that can actually be NULL.
		let nullable_expr_impls = self
			.fields
			.iter()
			.zip(expr_kind_idents.iter())
			.filter(|(field, _)| field.nullable())
			.map(|(_, expr_kind_ident)| {
				quote::quote! {
						impl ::squealy::NullableExpr for #expr_kind_ident {}
				}
			});

		quote::quote! {
            #(#fk_type_assertions)*
            #(#non_null_assertions)*
            #(#foreign_key_defs)*
            #(#column_defs)*
            #(#index_defs)*
            #(#predicate_fn_defs)*

            #(
                #[derive(Clone, Copy, Debug, PartialEq, Eq)]
                #visibility enum #expr_kind_idents {}

                impl ::squealy::ExprKind for #expr_kind_idents {
                    // Predicate operands use the inner (non-null) value type.
                    type Value = #field_inner_tys;
                }

                // A column is a non-null kind for LAG/LEAD purposes, so its window result becomes
                // `ScalarNullable` (nullable past the partition edge).
                impl ::squealy::IntoWindowNullable for #expr_kind_idents {
                    type Kind = ::squealy::ScalarNullable<#expr_kind_idents>;
                }

                // A nullable column as a searched-`CASE` branch makes the result nullable. The branch
                // value type is the column's inner (non-null) value type; nullability is taken from the
                // alias-transparent `ColumnNullability` path (not a syntactic `Option<…>` check).
                impl ::squealy::KindNullability for #expr_kind_idents {
                    type Value = #field_inner_tys;
                    type Nullable =
                        <#field_nullability_tys as ::squealy::ColumnCaseNull>::CaseNull;
                }

                impl ::squealy::ColumnKey for #expr_kind_idents {
                    type Table = #ident <'static, ::squealy::ColumnExpr>;
                    type Nullability = #field_nullability_tys;

                    const NAME: &'static str = #field_literals;
                }

                impl ::squealy::ProjectionShape for #expr_kind_idents {
                    type Exprs<'scope> = ::squealy::ColumnRef<'scope, #expr_kind_idents>;
                    type ReboundExprs<'scope> = ::squealy::Expr<'scope, #expr_kind_idents>;
                    // A single-column projection decodes as the declared type `D` (`Option<T>` when
                    // nullable; `Option<T>: Decode` handles a SQL NULL).
                    type Row = #field_value_tys;

                    fn exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::Exprs<'scope> {
                        ::squealy::ColumnRef::column(alias, #field_literals)
                    }

                    fn rebound_exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::ReboundExprs<'scope> {
                        ::squealy::Expr::column(alias, #field_literals)
                    }
                }
            )*

            #query_source_impl

            #(#insert_column_key_impls)*
            #(#update_column_key_impls)*
            #(#nullable_expr_impls)*

            #[doc(hidden)]
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #visibility struct #exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, #expr_kind_idents>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, #expr_kind_idents>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #row_shape_ident {
                #( pub #fields: #field_value_tys, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Copy, Debug, PartialEq, Eq)]
            #visibility struct #nullable_exprs_ident <'scope> {
                #( pub #fields: ::squealy::ColumnRef<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            #[doc(hidden)]
            #[derive(Clone, Debug, PartialEq)]
            #visibility struct #nullable_rebound_exprs_ident <'scope> {
                #( pub #fields: ::squealy::Expr<'scope, ::squealy::Nullable<#expr_kind_idents>>, )*
            }

            // Nullable-wrap this table's accumulated columns for a RIGHT/FULL JOIN base. The non-null
            // exprs map to the nullable struct (each column `K` -> `Nullable<K>`); the nullable struct
            // is idempotent so re-wrapping an already-outer-joined table does not nest `Nullable`.
            impl<'scope> ::squealy::IntoNullableExprs for #exprs_ident <'scope> {
                type Output = #nullable_exprs_ident <'scope>;

                fn into_nullable_exprs(self) -> Self::Output {
                    #nullable_exprs_ident {
                        #( #fields: self.#fields.into_nullable(), )*
                    }
                }
            }

            impl<'scope> ::squealy::IntoNullableExprs for #nullable_exprs_ident <'scope> {
                type Output = #nullable_exprs_ident <'scope>;

                fn into_nullable_exprs(self) -> Self::Output {
                    self
                }
            }

            #write_builder_defs

            static #columns_static: [&'static dyn ::squealy::Column; #columns_len] = [#( &#column_idents, )*];
            static #indexes_static: [&'static dyn ::squealy::Index; #indexes_len] = [#( &#index_idents, )*];
            #primary_key_static_def
            #uniques_static_def

            impl<'scope, C: ::squealy::ColumnMode> ::squealy::Table for #ident <'scope, C> {
                fn schema_name(&self) -> Option<&'static str> {
                    <Self as ::squealy::SchemaTable>::schema_name()
                }

                fn name(&self) -> &'static str {
                    <Self as ::squealy::SchemaTable>::name()
                }

                fn columns(&self) -> &'static [&'static dyn ::squealy::Column] {
                    <Self as ::squealy::SchemaTable>::columns()
                }

                fn indexes(&self) -> &'static [&'static dyn ::squealy::Index] {
                    <Self as ::squealy::SchemaTable>::indexes()
                }

                #table_primary_key_method
                #table_uniques_method
            }

            impl<'scope, C: ::squealy::ColumnMode> ::squealy::SchemaTable for #ident <'scope, C> {
                type Schema = #schema;

                type WithColumn<'next_scope, NextC: ::squealy::ColumnMode> = #ident <'next_scope, NextC>
                where
                    NextC: 'next_scope;

                type Exprs<'next_scope> = #exprs_ident <'next_scope>;

                type NullableExprs<'next_scope> = #nullable_exprs_ident <'next_scope>;

                fn name() -> &'static str {
                    #name
                }

                fn columns() -> &'static [&'static dyn ::squealy::Column] {
                    &#columns_static
                }

                fn indexes() -> &'static [&'static dyn ::squealy::Index] {
                    &#indexes_static
                }

                #schema_primary_key_method
                #schema_uniques_method

                fn column_names() -> Self::WithColumn<'static, ::squealy::ColumnName> {
                    #ident { #( #fields: #field_literals, )* }
                }

                fn column_exprs_from<'next_scope>(
                    alias: ::squealy::SourceAlias,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::Exprs<'next_scope> {
                    #exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }

                fn nullable_column_exprs_from<'next_scope>(
                    alias: ::squealy::SourceAlias,
                    columns: &Self::WithColumn<'static, ::squealy::ColumnName>,
                ) -> Self::NullableExprs<'next_scope> {
                    #nullable_exprs_ident { #( #fields: ::squealy::ColumnRef::column(alias, columns.#fields), )* }
                }
            }

            impl ::squealy::ProjectionShape for #row_shape_ident {
                type Exprs<'scope> = #exprs_ident <'scope>;
                type ReboundExprs<'scope> = #rebound_exprs_ident <'scope>;
                type Row = #row_shape_ident;

                fn exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::Exprs<'scope> {
                    <#ident <'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::column_exprs(alias)
                }

                fn rebound_exprs<'scope>(alias: ::squealy::SourceAlias) -> Self::ReboundExprs<'scope> {
                    ::squealy::Projectable::re_alias(
                        &<#ident <'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::column_exprs(alias),
                        alias,
                    )
                }
            }

            // The whole-row projection's kinds, for the `SELECT DISTINCT` + `ORDER BY` guard.
            impl ::squealy::IntoKindList for #row_shape_ident {
                type Kinds = #row_kind_list;
            }

            impl<Backend> ::squealy::Decode<Backend> for #ident <'static, ::squealy::ColumnValue>
            where
                Backend: ::squealy::Backend,
                #(#field_value_tys: ::squealy::Decode<Backend>,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#ident {
                        #(
                            #fields: ::squealy::RowReader::read::<#field_value_tys>(row)?,
                        )*
                    })
                }
            }

            impl<Backend> ::squealy::Decode<Backend> for #row_shape_ident
            where
                Backend: ::squealy::Backend,
                #(#field_value_tys: ::squealy::Decode<Backend>,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#row_shape_ident {
                        #(
                            #fields: ::squealy::RowReader::read::<#field_value_tys>(row)?,
                        )*
                    })
                }
            }

            // A LEFT JOIN makes every column nullable, so each field decodes as `Option<Inner>` via
            // the inner value type's `DecodeNullable` — a single `Option` layer whether the column was
            // already nullable (`D = Option<T>`, inner `T`) or not (`D = T`).
            impl<Backend> ::squealy::Decode<Backend> for #ident <'static, ::squealy::ColumnNullableValue>
            where
                Backend: ::squealy::Backend,
                #(#field_inner_tys: ::squealy::DecodeNullable<Backend>,)*
            {
                fn decode(
                    row: &mut <Backend as ::squealy::Backend>::RowReader<'_>,
                ) -> ::std::result::Result<Self, <Backend as ::squealy::Backend>::Error> {
                    Ok(#ident {
                        #(
                            #fields: <#field_inner_tys as ::squealy::DecodeNullable<Backend>>::decode_nullable(row)?,
                        )*
                    })
                }
            }

            // Generic over the expr `'scope` (not pinned to `'static`) so that, behind a generic
            // `async fn -> impl Future + Send` trait, the inferred table lifetime satisfies these
            // markers "for any lifetime" — mirroring `TableProjection`, which is why selects/deletes
            // already work there. Insertability does not depend on the scope lifetime. Omitted for
            // views, which are read-only.
            #write_marker_impls

            #write_table_impl

            impl<'scope> ::squealy::Projectable for #exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(
                            self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #exprs_ident <'scope> {
                type Shape = #row_shape_ident;
            }

            // A whole-table projection is all columns, i.e. all scalar — so it is a valid SELECT
            // list (alone or alongside other scalar projections) and a valid RETURNING projection.
            impl<'scope> ::squealy::ProjectionClass for #exprs_ident <'scope> {
                type Class = ::squealy::ScalarProjection;
            }
            impl<'scope> ::squealy::ProjectionClass for #rebound_exprs_ident <'scope> {
                type Class = ::squealy::ScalarProjection;
            }
            impl<'scope> ::squealy::ProjectionClass for #nullable_exprs_ident <'scope> {
                type Class = ::squealy::ScalarProjection;
            }
            impl<'scope> ::squealy::ProjectionClass for #nullable_rebound_exprs_ident <'scope> {
                type Class = ::squealy::ScalarProjection;
            }

            // A whole-table projection is all columns, which carry no runtime parameters.
            impl<'scope> ::squealy::ProjectionParams for #exprs_ident <'scope> {
                type Params = ::squealy::HNil;
            }
            impl<'scope> ::squealy::ProjectionParams for #rebound_exprs_ident <'scope> {
                type Params = ::squealy::HNil;
            }
            impl<'scope> ::squealy::ProjectionParams for #nullable_exprs_ident <'scope> {
                type Params = ::squealy::HNil;
            }
            impl<'scope> ::squealy::ProjectionParams for #nullable_rebound_exprs_ident <'scope> {
                type Params = ::squealy::HNil;
            }

            // A whole-table projection contains no window function, so it is valid in `RETURNING`.
            impl<'scope> ::squealy::ReturnableProjection for #exprs_ident <'scope> {}
            impl<'scope> ::squealy::ReturnableProjection for #rebound_exprs_ident <'scope> {}
            impl<'scope> ::squealy::ReturnableProjection for #nullable_exprs_ident <'scope> {}
            impl<'scope> ::squealy::ReturnableProjection for #nullable_rebound_exprs_ident <'scope> {}

            impl<'scope> ::squealy::Projectable for #rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #rebound_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(&self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(
                            &self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #rebound_exprs_ident <'scope> {
                type Shape = #row_shape_ident;
            }

            impl<'scope> ::squealy::Projectable for #nullable_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #nullable_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_column(
                            self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #nullable_exprs_ident <'scope> {
                type Shape = ::squealy::Maybe<#ident <'static, ::squealy::ColumnExpr>>;
            }

            impl<'scope> ::squealy::Projectable for #nullable_rebound_exprs_ident <'scope> {
                type Rebound<'next_scope> = #nullable_rebound_exprs_ident <'next_scope>;

                fn re_alias<'next_scope>(&self, alias: ::squealy::SourceAlias) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident { #( #fields: ::squealy::Expr::column(alias, #field_literals), )* }
                }

                fn re_alias_with_prefix<'next_scope>(
                    &self,
                    alias: ::squealy::SourceAlias,
                    prefix: &str,
                ) -> Self::Rebound<'next_scope> {
                    #nullable_rebound_exprs_ident {
                        #( #fields: ::squealy::Expr::column(alias, ::std::format!("{prefix}_{}", #field_literals)), )*
                    }
                }
            }

            impl<'scope, RenderBackend> ::squealy::RenderProjectable<RenderBackend> for #nullable_rebound_exprs_ident <'scope>
            where
                RenderBackend: ::squealy::Backend,
            {
                fn visit_projection<V>(&self, visitor: &mut V) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(&self.#fields, ::std::borrow::Cow::Borrowed(#field_literals))?;
                    )*
                    Ok(())
                }

                fn visit_projection_with_prefix<V>(
                    &self,
                    prefix: &str,
                    visitor: &mut V,
                ) -> ::std::result::Result<(), V::Error>
                where
                    V: ::squealy::ProjectionVisitor<Backend = RenderBackend>,
                {
                    #(
                        visitor.visit_expr(
                            &self.#fields,
                            ::std::borrow::Cow::Owned(::std::format!("{prefix}_{}", #field_literals)),
                        )?;
                    )*
                    Ok(())
                }
            }

            impl<'scope> ::squealy::ReturningProjection<'scope> for #nullable_rebound_exprs_ident <'scope> {
                type Shape = ::squealy::Maybe<#ident <'static, ::squealy::ColumnExpr>>;
            }
        }
        .into()
	}
}

struct BuilderExpansion {
	definitions: TokenStream2,
	table_impl: TokenStream2,
}

impl TableStruct {
	fn write_builder_tokens(
		&self,
		table_ident: &proc_macro2::Ident,
		expr_kind_idents: &[proc_macro2::Ident],
	) -> BuilderExpansion {
		let visibility = &self.visibility;
		let builder_ident = generated_ident(table_ident, "write", "Builder");
		let fields = self
			.fields
			.iter()
			.enumerate()
			.filter(|(_, field)| field.insertable() || field.updateable())
			.collect::<Vec<_>>();
		let setters = fields
			.iter()
			.map(|(field_index, field)| {
				let field_ident = proc_macro2::Ident::new(&field.ident.to_string(), Span::call_site());
				let field_expr_kind = &expr_kind_idents[*field_index];
				let value_bound = if field.nullable() {
					quote::quote! { ::squealy::IntoNullableAssignmentValue<#field_expr_kind> }
				} else {
					quote::quote! { ::squealy::IntoAssignmentValue<#field_expr_kind> }
				};
				let assignment_value_ty = if field.nullable() {
					quote::quote! { <Value as ::squealy::IntoNullableAssignmentValue<#field_expr_kind>>::Value }
				} else {
					quote::quote! { <Value as ::squealy::IntoAssignmentValue<#field_expr_kind>>::Value }
				};
				let assignment_value = if field.nullable() {
					quote::quote! { ::squealy::IntoNullableAssignmentValue::into_nullable_assignment_value(value) }
				} else {
					quote::quote! { ::squealy::IntoAssignmentValue::into_assignment_value(value) }
				};
				let insert_marker = if field.insertable() {
					quote::quote! { ::squealy::WriteEnabled }
				} else {
					quote::quote! { ::squealy::WriteDisabled }
				};
				let update_marker = if field.updateable() {
					quote::quote! { ::squealy::WriteEnabled }
				} else {
					quote::quote! { ::squealy::WriteDisabled }
				};
				let assignment_ty = quote::quote! {
						::squealy::WriteAssignment<
								#field_expr_kind,
								#assignment_value_ty,
								#insert_marker,
								#update_marker,
						>
				};

				quote::quote! {
						impl<'conn, Conn, Assignments, Filters, FilterState>
								#builder_ident <'conn, Conn, Assignments, Filters, FilterState>
						where
								Conn: ::squealy::QueryBuilder + 'conn,
						{
								pub fn #field_ident<Value>(
										self,
										value: Value,
								) -> #builder_ident <
										'conn,
										Conn,
										<Assignments as ::squealy::PushBack<#assignment_ty>>::Output,
										Filters,
										FilterState,
								>
								where
										Value: #value_bound,
										Assignments: ::squealy::PushBack<#assignment_ty>,
								{
										let assignment_value = #assignment_value;
										#builder_ident {
												connection: self.connection,
												assignments: self.assignments.push_back(
														::squealy::WriteAssignment::<
																#field_expr_kind,
																#assignment_value_ty,
																#insert_marker,
																#update_marker,
														>::new(assignment_value)
												),
												filters: self.filters,
												_state: ::std::marker::PhantomData,
										}
								}
						}
				}
			})
			.collect::<Vec<_>>();
		let update_finalizers = if fields.iter().any(|(_, field)| field.updateable()) {
			quote::quote! {
					impl<'conn, Conn, Assignments, Filters>
							#builder_ident <'conn, Conn, Assignments, Filters, ::squealy::MutationFiltered>
					where
							Conn: ::squealy::QueryBuilder + 'conn,
							Assignments: ::squealy::WriteAssignments,
							<Assignments as ::squealy::WriteAssignments>::Update: ::squealy::NonEmptyUpdateAssignments + 'conn,
							Filters: ::squealy::PredicateNodes + 'conn,
					{
							pub fn update(
									self,
							) -> impl ::std::future::Future<
									Output = ::std::result::Result<u64, <<Conn as ::squealy::QueryBuilder>::Backend as ::squealy::Backend>::Error>,
							> + ::std::marker::Send + 'conn
							where
									Conn: ::squealy::Connection,
									<<Assignments as ::squealy::WriteAssignments>::Update as ::squealy::UpdateAssignments>::Params: ::squealy::NoRuntimeParams,
									Filters: ::squealy::PredicateNodes,
									<Filters as ::squealy::PredicateNodes>::Params: ::squealy::NoRuntimeParams,
									<Conn as ::squealy::QueryBuilder>::Update<
											'conn,
											#table_ident <'static, ::squealy::ColumnExpr>,
											(),
											<Assignments as ::squealy::WriteAssignments>::Update,
											Filters,
											(),
									>: ::squealy::ExecutableUpdateQuery<'conn, <Assignments as ::squealy::WriteAssignments>::Update, Filters, ()>
											// The future captures this query object, so require it `Send` (see `insert`).
											+ ::std::marker::Send,
							{
									let update_columns = ::squealy::WriteAssignments::into_update(self.assignments);
									let query = <<Conn as ::squealy::QueryBuilder>::Update<
											'conn,
											#table_ident <'static, ::squealy::ColumnExpr>,
											(),
											<Assignments as ::squealy::WriteAssignments>::Update,
											Filters,
											(),
									> as ::squealy::UpdateQuery<'conn, <Assignments as ::squealy::WriteAssignments>::Update, Filters, ()>>::build(
											self.connection,
											Self::ALIAS,
											update_columns,
											self.filters,
											(),
									);
									async move {
											::squealy::ExecutableUpdateQuery::execute(&query).await
									}
							}

							pub fn update_returning<P>(
									self,
									projection: impl ::std::ops::FnOnce(
											<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
									) -> P,
							) -> <Conn as ::squealy::QueryBuilder>::Update<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape, <Assignments as ::squealy::WriteAssignments>::Update, Filters, P>
							where
									P: ::squealy::ReturningProjection<'static> + ::squealy::Projectable + ::squealy::ProjectionClass<Class = ::squealy::ScalarProjection> + ::squealy::ReturnableProjection + ::squealy::ProjectionParams<Params = ::squealy::HNil>,
									<P::Shape as ::squealy::ProjectionShape>::Row: ::squealy::Decode<<Conn as ::squealy::QueryBuilder>::Backend>,
							{
									let update_columns = ::squealy::WriteAssignments::into_update(self.assignments);
									let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
									let projection = projection(table);
									<<Conn as ::squealy::QueryBuilder>::Update<
											'conn,
											#table_ident <'static, ::squealy::ColumnExpr>,
											<P as ::squealy::ReturningProjection<'static>>::Shape,
											<Assignments as ::squealy::WriteAssignments>::Update,
											Filters,
											P,
									> as ::squealy::UpdateQuery<'conn, <Assignments as ::squealy::WriteAssignments>::Update, Filters, P>>::build(
											self.connection,
											Self::ALIAS,
											update_columns,
											self.filters,
											projection,
									)
							}
					}
			}
		} else {
			quote::quote! {}
		};

		let definitions = quote::quote! {
				#[doc(hidden)]
				#[derive(Clone, Debug, PartialEq)]
				#visibility struct #builder_ident <
						'conn,
						Conn: ::squealy::QueryBuilder + 'conn,
						Assignments = ::squealy::HNil,
						Filters = ::squealy::HNil,
						FilterState = ::squealy::MutationUnfiltered,
				> {
						connection: &'conn Conn,
						assignments: Assignments,
						filters: Filters,
						_state: ::std::marker::PhantomData<FilterState>,
				}

				impl<'conn, Conn> #builder_ident <'conn, Conn>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
				{
						fn new(connection: &'conn Conn) -> Self {
								Self {
										connection,
										assignments: ::squealy::HNil,
										filters: ::squealy::HNil,
										_state: ::std::marker::PhantomData,
								}
						}
				}

				impl<'conn, Conn, Assignments, Filters, FilterState>
						#builder_ident <'conn, Conn, Assignments, Filters, FilterState>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
				{
						const ALIAS: ::squealy::SourceAlias = ::squealy::SourceAlias::new(0, 0);
				}

				impl<'conn, Conn, Assignments, Filters, FilterState>
						#builder_ident <'conn, Conn, Assignments, Filters, FilterState>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
				{
						pub fn where_<P, PredicateAst>(
								self,
								predicate: impl ::std::ops::FnOnce(
										&<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
								) -> ::squealy::Predicate<'static, P, PredicateAst>,
						) -> #builder_ident <'conn, Conn, Assignments, <Filters as ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>>::Output, ::squealy::MutationFiltered>
						where
								Filters: ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>,
								<Filters as ::squealy::PushBack<::squealy::Predicate<'static, P, PredicateAst>>>::Output: ::squealy::PredicateNodes,
								P: ::squealy::PredicateKind,
								// Aggregates are invalid in a `WHERE` clause (they belong in HAVING), so the
								// predicate must be aggregate-free.
								PredicateAst: ::squealy::PredicateAst + ::squealy::NonAggregatePredicate,
						{
								let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
								let predicate = predicate(&table);
								#builder_ident {
										connection: self.connection,
										assignments: self.assignments,
										filters: self.filters.push_back(predicate),
										_state: ::std::marker::PhantomData,
								}
						}
				}

				impl<'conn, Conn, Assignments, Filters, FilterState>
						#builder_ident <'conn, Conn, Assignments, Filters, FilterState>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
				{
						pub fn all(self) -> #builder_ident <'conn, Conn, Assignments, Filters, ::squealy::MutationFiltered> {
								#builder_ident {
										connection: self.connection,
										assignments: self.assignments,
										filters: self.filters,
										_state: ::std::marker::PhantomData,
								}
						}
				}

				#(#setters)*

				impl<'conn, Conn, Assignments, Filters, FilterState> #builder_ident <'conn, Conn, Assignments, Filters, FilterState>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
						Assignments: ::squealy::WriteAssignments + 'conn,
						<Assignments as ::squealy::WriteAssignments>::Insert: ::squealy::InsertAssignments + 'conn,
						<<Assignments as ::squealy::WriteAssignments>::Insert as ::squealy::InsertAssignments>::Params: ::squealy::HAppend<::squealy::HNil>,
				{
						pub fn insert<__SquealyCoverage>(
								self,
						) -> impl ::std::future::Future<
								Output = ::std::result::Result<u64, <<Conn as ::squealy::QueryBuilder>::Backend as ::squealy::Backend>::Error>,
						> + ::std::marker::Send + 'conn
						where
								Conn: ::squealy::Connection,
								<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::InsertableTable>::RequiredInsertColumns: ::squealy::RequiredCovered<<Assignments as ::squealy::WriteAssignments>::InsertKeys, __SquealyCoverage>,
								::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>: ::squealy::InsertRows,
								<::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil> as ::squealy::InsertRows>::Params: ::squealy::NoRuntimeParams,
								<Conn as ::squealy::QueryBuilder>::Insert<
										'conn,
										#table_ident <'static, ::squealy::ColumnExpr>,
										(),
										::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>,
										(),
								>: ::squealy::ExecutableInsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>, ()>
										// The returned future captures this query object, so it must be `Send` for the
										// future to be `Send` behind a generic `async fn -> impl Future + Send` trait.
										+ ::std::marker::Send,
						{
								let insert_columns = ::squealy::WriteAssignments::into_insert(self.assignments);
								let insert_rows = ::squealy::HCons {
										head: ::squealy::InsertRow::new(insert_columns),
										tail: ::squealy::HNil,
								};
								let query = <<Conn as ::squealy::QueryBuilder>::Insert<
										'conn,
										#table_ident <'static, ::squealy::ColumnExpr>,
										(),
										::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>,
										(),
								> as ::squealy::InsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>, ()>>::build(
										self.connection,
										insert_rows,
										(),
								);
								async move {
										::squealy::ExecutableInsertQuery::execute(&query).await
								}
						}

						pub fn insert_returning<P, __SquealyCoverage>(
								self,
								projection: impl ::std::ops::FnOnce(
										<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
								) -> P,
						) -> <Conn as ::squealy::QueryBuilder>::Insert<'conn, #table_ident <'static, ::squealy::ColumnExpr>, <P as ::squealy::ReturningProjection<'static>>::Shape, ::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>, P>
						where
								<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::InsertableTable>::RequiredInsertColumns: ::squealy::RequiredCovered<<Assignments as ::squealy::WriteAssignments>::InsertKeys, __SquealyCoverage>,
								P: ::squealy::ReturningProjection<'static> + ::squealy::Projectable + ::squealy::ProjectionClass<Class = ::squealy::ScalarProjection> + ::squealy::ReturnableProjection + ::squealy::ProjectionParams<Params = ::squealy::HNil>,
								<P::Shape as ::squealy::ProjectionShape>::Row: ::squealy::Decode<<Conn as ::squealy::QueryBuilder>::Backend>,
						{
								let insert_columns = ::squealy::WriteAssignments::into_insert(self.assignments);
								let insert_rows = ::squealy::HCons {
										head: ::squealy::InsertRow::new(insert_columns),
										tail: ::squealy::HNil,
								};
								let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
								let projection = projection(table);
								<<Conn as ::squealy::QueryBuilder>::Insert<
										'conn,
										#table_ident <'static, ::squealy::ColumnExpr>,
										<P as ::squealy::ReturningProjection<'static>>::Shape,
										::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>,
										P,
								> as ::squealy::InsertQuery<'conn, ::squealy::HCons<::squealy::InsertRow<<Assignments as ::squealy::WriteAssignments>::Insert>, ::squealy::HNil>, P>>::build(
										self.connection,
										insert_rows,
										projection,
								)
						}

						/// `INSERT … ON CONFLICT (<target>) …` — PostgreSQL upsert. `target` selects the
						/// conflict column(s); follow with `do_nothing()` / `do_update()`. Gated to backends
						/// that support `ON CONFLICT` (PostgreSQL).
						pub fn on_conflict<__SquealyTarget, __SquealyCoverage>(
								self,
								target: impl ::std::ops::FnOnce(
										<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
								) -> __SquealyTarget,
						) -> ::squealy::OnConflict<'conn, Conn, #table_ident <'static, ::squealy::ColumnExpr>, <Assignments as ::squealy::WriteAssignments>::Insert>
						where
								Conn: ::squealy::OnConflictQueryBuilder,
								__SquealyTarget: ::squealy::ConflictTarget,
								<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::InsertableTable>::RequiredInsertColumns: ::squealy::RequiredCovered<<Assignments as ::squealy::WriteAssignments>::InsertKeys, __SquealyCoverage>,
						{
								let insert_columns = ::squealy::WriteAssignments::into_insert(self.assignments);
								let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
								let target = ::squealy::ConflictTarget::column_names(target(table));
								::squealy::OnConflict::new(self.connection, insert_columns, target)
						}
				}

				// `insert_select` is only available on a *fresh* builder: its rows come entirely from the
				// source query, so any prior write-builder state would be silently dropped. The whole state
				// is pinned to its initial form — no assignments and no filter —
				// so e.g. `.name(..).insert_select(..)` or `.where_(..).insert_select(..)` is a compile
				// error. No insert-readiness bounds either (values come from the source, not setters).
				impl<'conn, Conn>
						#builder_ident <'conn, Conn, ::squealy::HNil, ::squealy::HNil, ::squealy::MutationUnfiltered>
				where
						Conn: ::squealy::QueryBuilder + 'conn,
				{
						/// `INSERT INTO t (columns) <select>` — insert the result of a query. `columns` selects
						/// the target columns; `source` is a select whose projected row type must match them.
						pub fn insert_select<'src_scope, __SquealyCoverage, __SquealyRowWitness, __SquealyCols, __SquealySource>(
								self,
								columns: impl ::std::ops::FnOnce(
										<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::Exprs<'static>,
								) -> __SquealyCols,
								source: __SquealySource,
						) -> <__SquealySource as ::squealy::IntoInsertSelect<'conn, 'src_scope, Conn>>::InsertSelectQuery<#table_ident <'static, ::squealy::ColumnExpr>, ()>
						where
								__SquealyCols: ::squealy::InsertSelectColumns<#table_ident <'static, ::squealy::ColumnExpr>> + ::squealy::ReturningProjection<'static>,
								<__SquealyCols as ::squealy::ReturningProjection<'static>>::Shape: ::squealy::IntoKindList,
								// The target columns must cover every required (insertable, non-null, no-default)
								// column, just like the setter-based insert (`__SquealyCoverage` are the inferred
								// per-required-column witnesses; nullable required columns are omittable).
								<#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::InsertableTable>::RequiredInsertColumns:
										::squealy::RequiredCovered<
												<<__SquealyCols as ::squealy::ReturningProjection<'static>>::Shape as ::squealy::IntoKindList>::Kinds,
												__SquealyCoverage,
										>,
								__SquealySource: ::squealy::IntoInsertSelect<'conn, 'src_scope, Conn>,
								// The source's row type must be assignable to the target columns' row type —
								// element-wise exact-or-widen (`T` into a nullable `Option<T>`), not exact equality.
								<__SquealySource as ::squealy::IntoInsertSelect<'conn, 'src_scope, Conn>>::Row:
										::squealy::InsertSelectRowCompatible<
												<<__SquealyCols as ::squealy::ReturningProjection<'static>>::Shape as ::squealy::ProjectionShape>::Row,
												__SquealyRowWitness,
										>,
						{
								let table = <#table_ident <'static, ::squealy::ColumnExpr> as ::squealy::ProjectionShape>::exprs(Self::ALIAS);
								let names = <__SquealyCols as ::squealy::InsertSelectColumns<#table_ident <'static, ::squealy::ColumnExpr>>>::column_names(columns(table));
								// The target list is fixed at the call site; reject a duplicate column (which the
								// database would reject) here rather than emitting invalid SQL.
								::squealy::assert_distinct_insert_select_columns(&names);
								// Build the insert on the *destination* builder's connection (`self.connection`); the
								// source provides only its SELECT arm.
								::squealy::IntoInsertSelect::into_insert_select::<#table_ident <'static, ::squealy::ColumnExpr>, ()>(source, self.connection, names, ())
						}
				}

				#update_finalizers
		};

		let table_impl = quote::quote! {
				impl<'scope> ::squealy::WriteableTable for #table_ident <'scope, ::squealy::ColumnExpr> {
						type WriteBuilder<'conn, Conn> = #builder_ident <'conn, Conn>
						where
								Conn: ::squealy::QueryBuilder + 'conn;

						fn write_builder<'conn, Conn>(
								connection: &'conn Conn,
						) -> Self::WriteBuilder<'conn, Conn>
						where
								Conn: ::squealy::QueryBuilder + 'conn,
						{
								#builder_ident::new(connection)
						}
				}
		};

		BuilderExpansion {
			definitions,
			table_impl,
		}
	}
}

impl IndexAttrs {
	fn index_definition_tokens(
		&self,
		index_ident: &proc_macro2::Ident,
		fields: &[Field],
		predicate: &proc_macro2::TokenStream,
	) -> proc_macro2::TokenStream {
		let name = option_literal(self.name.as_deref());
		let columns = self
			.columns
			.iter()
			.map(|column| {
				let column = column.to_string();
				let field = fields
					.iter()
					.find(|field| field.ident.to_string() == column)
					.expect("index fields should be validated before code generation");
				Literal::string(&field.column_name())
			})
			.collect::<Vec<_>>();
		let unique = bool_tokens(self.unique);

		quote::quote! {
				struct #index_ident;

				impl ::squealy::Index for #index_ident {
						fn name(&self) -> Option<&'static str> {
								#name
						}

						fn columns(&self) -> &'static [&'static str] {
								&[#( #columns, )*]
						}

						fn unique(&self) -> bool {
								#unique
						}

						fn predicate(&self) -> Option<fn() -> ::squealy::ExprNode> {
								#predicate
						}
				}
		}
	}
}

impl Field {
	pub(crate) fn column_name(&self) -> String {
		self
			.attrs
			.column_name
			.clone()
			.unwrap_or_else(|| self.ident.to_string())
	}

	fn insertable(&self) -> bool {
		self
			.attrs
			.insert
			.unwrap_or(!self.attrs.generated && !self.attrs.auto_increment)
	}

	fn updateable(&self) -> bool {
		self
			.attrs
			.update
			.unwrap_or(!self.attrs.generated && !self.attrs.auto_increment)
	}

	/// Macro-time nullability, by a literal `Option<…>` token check. Used only for the setter
	/// value-dispatch and the `is_null` gate; storage/decode/DDL nullability and insert-required-ness
	/// are resolved at the type level through `RequiredInsertColumns` coverage (alias-transparent).
	fn nullable(&self) -> bool {
		strip_option(&self.value_ty).is_some()
	}

	fn default_tokens(&self) -> proc_macro2::TokenStream {
		let Some(default) = self.attrs.default.as_ref() else {
			return quote::quote! { None };
		};

		let default = match default {
			DefaultAttrs::Literal(DefaultLiteral::Null) => {
				quote::quote! { ::squealy::ColumnDefault::Null }
			}
			DefaultAttrs::Literal(DefaultLiteral::Int(value)) => {
				quote::quote! { ::squealy::ColumnDefault::Int(#value) }
			}
			DefaultAttrs::Literal(DefaultLiteral::UInt(value)) => {
				quote::quote! { ::squealy::ColumnDefault::UInt(#value) }
			}
			DefaultAttrs::Literal(DefaultLiteral::Float(value)) => {
				quote::quote! { ::squealy::ColumnDefault::Float(#value) }
			}
			DefaultAttrs::Literal(DefaultLiteral::Text(value)) => {
				let value = Literal::string(value);
				quote::quote! { ::squealy::ColumnDefault::Text(#value) }
			}
			DefaultAttrs::Literal(DefaultLiteral::Bool(value)) => {
				let value = bool_tokens(*value);
				quote::quote! { ::squealy::ColumnDefault::Bool(#value) }
			}
			DefaultAttrs::CurrentTimestamp => {
				quote::quote! { ::squealy::ColumnDefault::CurrentTimestamp }
			}
			DefaultAttrs::CurrentDate => {
				quote::quote! { ::squealy::ColumnDefault::CurrentDate }
			}
			DefaultAttrs::CurrentTime => {
				quote::quote! { ::squealy::ColumnDefault::CurrentTime }
			}
			DefaultAttrs::Raw(value) => {
				let value = Literal::string(value);
				quote::quote! { ::squealy::ColumnDefault::Raw(#value) }
			}
		};

		quote::quote! { Some(#default) }
	}

	fn column_definition_tokens(
		&self,
		column_ident: &proc_macro2::Ident,
		unique_predicate: &proc_macro2::TokenStream,
	) -> proc_macro2::TokenStream {
		let name = Literal::string(&self.column_name());
		let primary_key = bool_tokens(self.attrs.primary_key);
		let indexed = bool_tokens(self.attrs.index);
		let unique = bool_tokens(self.attrs.unique);
		let auto_increment = bool_tokens(self.attrs.auto_increment);
		let generated = bool_tokens(self.attrs.generated);
		let insertable = bool_tokens(self.insertable());
		let updateable = bool_tokens(self.updateable());
		let default = self.default_tokens();
		let value_ty = &self.value_ty;
		let column_type = if let Some(db_type) = self.attrs.db_type.as_deref() {
			parse_db_type(db_type)
		} else {
			quote::quote! { <#value_ty as ::squealy::HasColumnType>::COLUMN_TYPE }
		};
		let check = crate::expr_tokens::check_option_tokens(self.attrs.check.as_deref());
		let references = self.references_tokens(column_ident);

		quote::quote! {
				struct #column_ident;

				impl ::squealy::Column for #column_ident {
						fn name(&self) -> &'static str { #name }
						fn primary_key(&self) -> bool { #primary_key }
						fn indexed(&self) -> bool { #indexed }
						fn unique(&self) -> bool { #unique }
						fn unique_predicate(&self) -> Option<fn() -> ::squealy::ExprNode> { #unique_predicate }
						fn nullable(&self) -> bool { <#value_ty as ::squealy::ColumnNullability>::NULLABLE }
						fn auto_increment(&self) -> bool { #auto_increment }
						fn generated(&self) -> bool { #generated }
						fn insertable(&self) -> bool { #insertable }
						fn updateable(&self) -> bool { #updateable }
						fn default(&self) -> Option<::squealy::ColumnDefault> { #default }
						fn column_type(&self) -> ::squealy::ColumnType { #column_type }
						fn check(&self) -> Option<::squealy::ExprNode> { #check }
						fn references(&self) -> Option<&'static dyn ::squealy::ForeignKey> { #references }
				}
		}
	}

	fn index_definition_tokens(&self, index_ident: &proc_macro2::Ident) -> proc_macro2::TokenStream {
		let name = option_literal(None);
		let column_name = Literal::string(&self.column_name());
		let unique = bool_tokens(self.attrs.unique);

		quote::quote! {
				struct #index_ident;

				impl ::squealy::Index for #index_ident {
						fn name(&self) -> Option<&'static str> {
								#name
						}

						fn columns(&self) -> &'static [&'static str] {
								&[#column_name]
						}

						fn unique(&self) -> bool {
								#unique
						}
				}
		}
	}

	fn foreign_key_definition_tokens(
		&self,
		column_ident: &proc_macro2::Ident,
	) -> Option<proc_macro2::TokenStream> {
		let references = self.attrs.references.as_ref()?;
		let foreign_key_ident = foreign_key_ident(column_ident);
		let table = proc_macro2::Ident::new(
			&references
				.table
				.as_ref()
				.expect("foreign keys should have a table before code generation")
				.to_string(),
			Span::call_site(),
		);
		let column = proc_macro2::Ident::new(
			&references
				.column
				.as_ref()
				.expect("foreign keys should have a column before code generation")
				.to_string(),
			Span::call_site(),
		);
		let on_delete = option_literal(references.on_delete.as_deref());
		let on_update = option_literal(references.on_update.as_deref());

		Some(quote::quote! {
				struct #foreign_key_ident;

				impl ::squealy::ForeignKey for #foreign_key_ident {
						fn schema_name(&self) -> Option<&'static str> {
								<#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::schema_name()
						}

						fn table(&self) -> &'static str {
								<#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::name()
						}

						fn column(&self) -> &'static str {
								<#table <'static, ::squealy::ColumnName> as ::squealy::SchemaTable>::column_names().#column
						}
						fn on_delete(&self) -> Option<&'static str> { #on_delete }
						fn on_update(&self) -> Option<&'static str> { #on_update }
				}
		})
	}

	fn references_tokens(&self, column_ident: &proc_macro2::Ident) -> proc_macro2::TokenStream {
		if self.attrs.references.is_none() {
			return quote::quote! { None };
		}

		let foreign_key_ident = foreign_key_ident(column_ident);
		quote::quote! { Some(&#foreign_key_ident) }
	}
}

pub(crate) fn table_struct(input: TokenStream) -> Result<TableStruct, MacroError> {
	let tokens = input.into_iter().collect::<Vec<_>>();
	let struct_index = tokens
		.iter()
		.position(|token| matches_ident(token, "struct"))
		.ok_or_else(|| "Table can only be derived for structs".to_owned())?;

	let ident = tokens
		.get(struct_index + 1)
		.and_then(|token| match token {
			TokenTree::Ident(ident) => Some(ident.clone()),
			_ => None,
		})
		.ok_or_else(|| "Table derive could not find the struct name".to_owned())?;

	let body_index = tokens
		.iter()
		.position(
			|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace),
		)
		.ok_or_else(|| "Table requires a named-field struct".to_owned())?;

	if !has_scope_and_mode(&tokens[struct_index + 2..body_index]) {
		return Err(MacroError::spanned(
			"Table currently requires structs shaped like `Type<'scope, C: ColumnMode = ColumnExpr>`",
			ident.span().into(),
		));
	}

	let fields = match &tokens[body_index] {
		TokenTree::Group(group) => named_fields(group)?,
		_ => unreachable!(),
	};
	let table_attrs = table_attributes(&tokens[..struct_index])?;
	validate_index_columns(&table_attrs.indexes, &fields)?;
	validate_primary_key(table_attrs.primary_key.as_ref(), &fields)?;
	validate_unique_columns(&table_attrs.uniques, &fields)?;
	validate_field_attrs(&fields)?;

	Ok(TableStruct {
		ident,
		visibility: struct_visibility(&tokens, struct_index),
		fields,
		indexes: table_attrs.indexes,
		primary_key: table_attrs.primary_key,
		uniques: table_attrs.uniques,
		schema: table_attrs.schema,
	})
}

/// Validates a table-level `#[primary_key(columns = [..])]` against the struct's fields:
/// every referenced column must exist and be non-nullable, and it must not be combined with
/// any per-column `#[column(primary_key)]` marker (which is the single-column form).
fn validate_primary_key(
	primary_key: Option<&PrimaryKeyAttrs>,
	fields: &[Field],
) -> Result<(), String> {
	let Some(primary_key) = primary_key else {
		return Ok(());
	};

	if fields.iter().any(|field| field.attrs.primary_key) {
		return Err(
			"a table cannot combine `#[primary_key(...)]` with per-column `#[column(primary_key)]`; \
             use one form"
				.to_owned(),
		);
	}

	for column in &primary_key.columns {
		let column = column.to_string();
		let Some(field) = fields
			.iter()
			.find(|field| field.ident.to_string() == column)
		else {
			return Err(format!("primary key references unknown field `{column}`"));
		};
		if field.nullable() {
			return Err(format!(
				"primary key column `{column}` cannot be `Option<_>` (nullable)"
			));
		}
	}

	Ok(())
}

/// Structurally verifies the generic parameter list contains a `'scope` lifetime
/// and a `C` column-mode type parameter, the shape every table field relies on
/// (`C::Type<'scope, Value>`). This replaces a brittle stringified-token match.
fn has_scope_and_mode(generic_tokens: &[TokenTree]) -> bool {
	let Some(start) = generic_tokens
		.iter()
		.position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == '<'))
	else {
		return false;
	};

	let mut depth = 0usize;
	let mut has_scope = false;
	let mut has_mode = false;

	for (offset, token) in generic_tokens[start..].iter().enumerate() {
		match token {
			TokenTree::Punct(punct) if punct.as_char() == '<' => depth += 1,
			TokenTree::Punct(punct) if punct.as_char() == '>' => {
				depth -= 1;
				if depth == 0 {
					break;
				}
			}
			// A lifetime is a `'` punct joined to the following identifier.
			TokenTree::Punct(punct) if punct.as_char() == '\'' && depth == 1 => {
				if let Some(TokenTree::Ident(name)) = generic_tokens.get(start + offset + 1)
					&& name.to_string() == "scope"
				{
					has_scope = true;
				}
			}
			TokenTree::Ident(ident) if depth == 1 && ident.to_string() == "C" => has_mode = true,
			_ => {}
		}
	}

	has_scope && has_mode
}

fn struct_visibility(tokens: &[TokenTree], struct_index: usize) -> TokenStream2 {
	if struct_index >= 1 && matches_ident(&tokens[struct_index - 1], "pub") {
		let mut visibility = TokenStream::new();
		visibility.extend([tokens[struct_index - 1].clone()]);
		return TokenStream2::from(visibility);
	}

	if struct_index >= 2
		&& matches_ident(&tokens[struct_index - 2], "pub")
		&& matches!(
				&tokens[struct_index - 1],
				TokenTree::Group(group) if group.delimiter() == Delimiter::Parenthesis
		) {
		let mut visibility = TokenStream::new();
		visibility.extend([
			tokens[struct_index - 2].clone(),
			tokens[struct_index - 1].clone(),
		]);
		return TokenStream2::from(visibility);
	}

	TokenStream2::new()
}

fn table_attributes(tokens: &[TokenTree]) -> Result<TableAttrs, String> {
	let mut attrs = TableAttrs::default();
	let mut iter = tokens.iter();

	while let Some(token) = iter.next() {
		if !is_attribute_start(token) {
			continue;
		}

		let Some(TokenTree::Group(attr)) = iter.next() else {
			return Err("Table attribute is missing its bracketed body".to_owned());
		};
		apply_table_attribute(attr, &mut attrs)?;
	}

	Ok(attrs)
}

fn apply_table_attribute(group: &Group, attrs: &mut TableAttrs) -> Result<(), String> {
	if group.delimiter() != Delimiter::Bracket {
		return Err("Table attributes must use square brackets".to_owned());
	}

	let mut tokens = group.stream().into_iter();
	let Some(TokenTree::Ident(attr_name)) = tokens.next() else {
		return Ok(());
	};

	match attr_name.to_string().as_str() {
		"index" => {
			let Some(TokenTree::Group(meta)) = tokens.next() else {
				return Err("table-level #[index(...)] requires metadata inside parentheses".to_owned());
			};
			attrs
				.indexes
				.push(parse_index(meta.stream().into_iter().collect::<Vec<_>>())?);
		}
		"primary_key" => {
			let Some(TokenTree::Group(meta)) = tokens.next() else {
				return Err(
					"table-level #[primary_key(...)] requires metadata inside parentheses".to_owned(),
				);
			};
			if attrs.primary_key.is_some() {
				return Err("a table may declare at most one #[primary_key(...)]".to_owned());
			}
			attrs.primary_key = Some(parse_primary_key(
				meta.stream().into_iter().collect::<Vec<_>>(),
			)?);
		}
		"unique" => {
			let Some(TokenTree::Group(meta)) = tokens.next() else {
				return Err("table-level #[unique(...)] requires metadata inside parentheses".to_owned());
			};
			attrs
				.uniques
				.push(parse_unique(meta.stream().into_iter().collect::<Vec<_>>())?);
		}
		"schema" => {
			let Some(TokenTree::Group(schema)) = tokens.next() else {
				return Err("table-level #[schema(...)] requires a schema type".to_owned());
			};
			let schema_tokens = schema.stream();
			if schema_tokens.is_empty() {
				return Err("table-level #[schema(...)] requires a schema type".to_owned());
			}
			attrs.schema = Some(proc_macro2::TokenStream::from(schema_tokens));
		}
		_ => {}
	}

	Ok(())
}

fn parse_index(tokens: Vec<TokenTree>) -> Result<IndexAttrs, String> {
	let mut index = 0;
	let mut attrs = IndexAttrs {
		name: None,
		columns: Vec::new(),
		unique: false,
		predicate: None,
	};

	while index < tokens.len() {
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
			index += 1;
		}

		let Some(TokenTree::Ident(name)) = tokens.get(index) else {
			break;
		};
		let name = name.to_string();
		index += 1;

		match name.as_str() {
			"unique" => attrs.unique = true,
			"where" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("index option `where` requires a `= |row| ...` predicate".to_owned());
				}
				index += 1;
				attrs.predicate = Some(collect_predicate_tokens(&tokens, &mut index, "index")?);
			}
			"name" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("index option `name` requires a string value".to_owned());
				}
				index += 1;
				attrs.name = Some(match tokens.get(index) {
					Some(TokenTree::Literal(literal)) => literal_string(literal),
					Some(token) => token.to_string(),
					None => return Err("index option `name` is missing a value".to_owned()),
				});
				index += 1;
			}
			"columns" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("index option `columns` requires a bracketed field list".to_owned());
				}
				index += 1;
				let Some(TokenTree::Group(columns)) = tokens.get(index) else {
					return Err("index option `columns` requires a bracketed field list".to_owned());
				};
				if columns.delimiter() != Delimiter::Bracket {
					return Err("index option `columns` requires square brackets".to_owned());
				}
				attrs.columns = parse_index_columns(columns)?;
				index += 1;
			}
			_ => return Err(format!("unsupported index option `{name}`")),
		}
	}

	if attrs.columns.is_empty() {
		return Err("table-level indexes require at least one column".to_owned());
	}

	Ok(attrs)
}

fn parse_primary_key(tokens: Vec<TokenTree>) -> Result<PrimaryKeyAttrs, String> {
	let mut index = 0;
	let mut attrs = PrimaryKeyAttrs {
		name: None,
		columns: Vec::new(),
	};

	while index < tokens.len() {
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
			index += 1;
		}

		let Some(TokenTree::Ident(name)) = tokens.get(index) else {
			break;
		};
		let name = name.to_string();
		index += 1;

		match name.as_str() {
			"name" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("primary key option `name` requires a string value".to_owned());
				}
				index += 1;
				attrs.name = Some(match tokens.get(index) {
					Some(TokenTree::Literal(literal)) => literal_string(literal),
					Some(token) => token.to_string(),
					None => return Err("primary key option `name` is missing a value".to_owned()),
				});
				index += 1;
			}
			"columns" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("primary key option `columns` requires a bracketed field list".to_owned());
				}
				index += 1;
				let Some(TokenTree::Group(columns)) = tokens.get(index) else {
					return Err("primary key option `columns` requires a bracketed field list".to_owned());
				};
				if columns.delimiter() != Delimiter::Bracket {
					return Err("primary key option `columns` requires square brackets".to_owned());
				}
				attrs.columns = parse_index_columns(columns)?;
				index += 1;
			}
			_ => return Err(format!("unsupported primary key option `{name}`")),
		}
	}

	if attrs.columns.is_empty() {
		return Err("table-level #[primary_key(...)] requires at least one column".to_owned());
	}

	Ok(attrs)
}

fn parse_unique(tokens: Vec<TokenTree>) -> Result<UniqueAttrs, String> {
	let mut index = 0;
	let mut attrs = UniqueAttrs {
		name: None,
		columns: Vec::new(),
		predicate: None,
	};

	while index < tokens.len() {
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
			index += 1;
		}

		let Some(TokenTree::Ident(name)) = tokens.get(index) else {
			break;
		};
		let name = name.to_string();
		index += 1;

		match name.as_str() {
			"name" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("unique option `name` requires a string value".to_owned());
				}
				index += 1;
				attrs.name = Some(match tokens.get(index) {
					Some(TokenTree::Literal(literal)) => literal_string(literal),
					Some(token) => token.to_string(),
					None => return Err("unique option `name` is missing a value".to_owned()),
				});
				index += 1;
			}
			"where" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("unique option `where` requires a `= |row| ...` predicate".to_owned());
				}
				index += 1;
				attrs.predicate = Some(collect_predicate_tokens(&tokens, &mut index, "unique")?);
			}
			"columns" => {
				if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
					return Err("unique option `columns` requires a bracketed field list".to_owned());
				}
				index += 1;
				let Some(TokenTree::Group(columns)) = tokens.get(index) else {
					return Err("unique option `columns` requires a bracketed field list".to_owned());
				};
				if columns.delimiter() != Delimiter::Bracket {
					return Err("unique option `columns` requires square brackets".to_owned());
				}
				attrs.columns = parse_index_columns(columns)?;
				index += 1;
			}
			_ => return Err(format!("unsupported unique option `{name}`")),
		}
	}

	if attrs.columns.is_empty() {
		return Err("table-level #[unique(...)] requires at least one column".to_owned());
	}

	Ok(attrs)
}

fn parse_index_columns(group: &Group) -> Result<Vec<Ident>, String> {
	let mut columns = Vec::new();
	for token in group.stream() {
		match token {
			TokenTree::Ident(ident) => columns.push(ident),
			TokenTree::Punct(punct) if punct.as_char() == ',' => {}
			_ => return Err("index columns must be field identifiers".to_owned()),
		}
	}
	Ok(columns)
}

/// Collects the raw tokens of a `where = <expr>` partial-index predicate from a table-level
/// attribute, advancing `index` past them. The expression runs to the next top-level `,` (option
/// separator); commas inside the predicate are nested in `(...)`/`[...]` groups, so a top-level
/// comma always ends the option. Returns the tokens as a `proc_macro2` stream for `quote!`.
fn collect_predicate_tokens(
	tokens: &[TokenTree],
	index: &mut usize,
	context: &str,
) -> Result<proc_macro2::TokenStream, String> {
	let mut predicate = Vec::new();
	while *index < tokens.len()
		&& !matches!(tokens.get(*index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',')
	{
		predicate.push(tokens[*index].clone());
		*index += 1;
	}
	if predicate.is_empty() {
		return Err(format!(
			"{context} option `where` requires a `= |row| ...` predicate"
		));
	}
	Ok(token_trees_to_stream(&predicate))
}

/// Re-tokenizes a slice of `proc_macro` token trees as a `proc_macro2` stream so it can be spliced
/// into a `quote!` body (the predicate expression is emitted back out verbatim).
fn token_trees_to_stream(tokens: &[TokenTree]) -> proc_macro2::TokenStream {
	tokens.iter().cloned().collect::<TokenStream>().into()
}

/// Emits a `fn() -> ExprNode` that lowers a `where = |row| ...` predicate to a neutral structural
/// [`ExprNode`], and returns the `Option<fn() -> ExprNode>` expression that references it (for a
/// `TableUnique`, `Column::unique_predicate`, or `Index::predicate`). The function definition is pushed
/// onto `defs`; the predicate closure is applied to the table's column expressions and lowered by
/// `squealy::build_schema_predicate`, whose marker bound only accepts the single-table, parameter-free
/// subset (`IS NULL` / comparisons of columns and literals), so an unsupported predicate
/// fails to compile here.
fn predicate_fn_reference(
	table_ident: &proc_macro2::Ident,
	tag: &str,
	closure: &proc_macro2::TokenStream,
	defs: &mut Vec<proc_macro2::TokenStream>,
) -> proc_macro2::TokenStream {
	let fn_ident = generated_ident(table_ident, tag, "Predicate");
	defs.push(quote::quote! {
			#[allow(non_snake_case)]
			fn #fn_ident() -> ::squealy::ExprNode {
					// Pass the user's `|row| ...` closure to a helper whose parameter type is fixed to the
					// table's column-expression struct. This gives the closure an *expected* signature, so
					// its `row` parameter is inferred (a bare immediately-applied closure cannot be — the
					// field access in the body needs the type known up front).
					fn build<__P>(
							predicate: impl ::std::ops::FnOnce(
									<#table_ident<'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::Exprs<'static>,
							) -> __P,
					) -> __P {
							predicate(
									<#table_ident<'static, ::squealy::ColumnExpr> as ::squealy::SchemaTable>::column_exprs(
											::squealy::SourceAlias::new(0, 0),
									),
							)
					}
					::squealy::build_schema_predicate(&build(#closure))
			}
	});
	quote::quote! { ::std::option::Option::Some(#fn_ident as fn() -> ::squealy::ExprNode) }
}

/// Rejects column attribute combinations that are mutually contradictory and would
/// otherwise only surface as a database error at DDL time.
fn validate_field_attrs(fields: &[Field]) -> Result<(), MacroError> {
	for field in fields {
		let attrs = &field.attrs;
		let name = field.ident.to_string();
		let nullable = field.nullable();
		let at_field = |message: String| MacroError::spanned(message, field.ident.span().into());

		if attrs.primary_key && nullable {
			return Err(at_field(format!(
				"column `{name}` cannot be both `primary_key` and `Option<_>` (nullable)"
			)));
		}
		if attrs.auto_increment && nullable {
			return Err(at_field(format!(
				"column `{name}` cannot be both `auto_increment` and `Option<_>` (nullable)"
			)));
		}
		if attrs.auto_increment && attrs.default.is_some() {
			return Err(at_field(format!(
				"column `{name}` cannot combine `auto_increment` with a default"
			)));
		}
		if attrs.generated && attrs.default.is_some() {
			return Err(at_field(format!(
				"column `{name}` cannot combine `generated` with a default"
			)));
		}
		if attrs.generated && attrs.auto_increment {
			return Err(at_field(format!(
				"column `{name}` cannot be both `generated` and `auto_increment`"
			)));
		}
		if attrs.predicate.is_some() && !attrs.unique {
			return Err(at_field(format!(
				"column `{name}` has a `where = ...` predicate but is not `unique`; a partial \
                 predicate is only meaningful on a unique column"
			)));
		}
	}

	Ok(())
}

fn validate_index_columns(indexes: &[IndexAttrs], fields: &[Field]) -> Result<(), String> {
	for index in indexes {
		for column in &index.columns {
			let column = column.to_string();
			if !fields.iter().any(|field| field.ident.to_string() == column) {
				return Err(format!("index references unknown field `{column}`"));
			}
		}
	}

	Ok(())
}

fn validate_unique_columns(uniques: &[UniqueAttrs], fields: &[Field]) -> Result<(), String> {
	for unique in uniques {
		for column in &unique.columns {
			let column = column.to_string();
			if !fields.iter().any(|field| field.ident.to_string() == column) {
				return Err(format!(
					"unique constraint references unknown field `{column}`"
				));
			}
		}
	}

	Ok(())
}

fn named_fields(group: &Group) -> Result<Vec<Field>, String> {
	let mut fields = Vec::new();
	let mut pending_attrs = FieldAttrs::default();
	let tokens = group.stream().into_iter().collect::<Vec<_>>();
	let mut index = 0;

	while index < tokens.len() {
		let token = tokens[index].clone();
		if is_attribute_start(&token) {
			let Some(TokenTree::Group(attr)) = tokens.get(index + 1) else {
				return Err("Table field attribute is missing its bracketed body".to_owned());
			};
			apply_attribute(attr, &mut pending_attrs)?;
			index += 2;
			continue;
		}

		let TokenTree::Ident(ident) = token else {
			index += 1;
			continue;
		};

		if !matches!(
				tokens.get(index + 1),
				Some(TokenTree::Punct(punct))
						if punct.as_char() == ':' && punct.spacing() == proc_macro::Spacing::Alone
		) {
			index += 1;
			continue;
		}

		index += 2;
		let mut type_tokens = Vec::new();
		let mut angle_depth = 0usize;

		while index < tokens.len() {
			match &tokens[index] {
				TokenTree::Punct(punct) if punct.as_char() == '<' => {
					angle_depth += 1;
					type_tokens.push(tokens[index].clone());
				}
				TokenTree::Punct(punct) if punct.as_char() == '>' => {
					angle_depth = angle_depth.saturating_sub(1);
					type_tokens.push(tokens[index].clone());
				}
				TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 0 => break,
				token => type_tokens.push(token.clone()),
			}
			index += 1;
		}

		let value_ty = column_type_value(&type_tokens)?;
		// Reject `Option<Option<_>>` here with a stable message (the trait-level rejection via
		// `ColumnNullability` produces a rustc-version-sensitive error).
		if let Some(inner) = strip_option(&value_ty)
			&& strip_option(&inner).is_some()
		{
			return Err(
				"a column type may not be `Option<Option<_>>` (a column is at most nullable once)"
					.to_owned(),
			);
		}

		fields.push(Field {
			ident,
			value_ty,
			attrs: std::mem::take(&mut pending_attrs),
		});
		index += 1;
	}

	if fields.is_empty() {
		Err("Table requires at least one named field".to_owned())
	} else {
		Ok(fields)
	}
}

/// If a declared column value type is a literal `Option<Inner>` (bare or via a `std`/`core` path),
/// returns its inner tokens. Token-based, so a type *alias* to `Option<…>` is not recognized — used
/// only for the `is_null` gate and setter value-dispatch (storage/decode/DDL nullability is fully
/// type-level via `ColumnNullability`).
fn strip_option(value_ty: &proc_macro2::TokenStream) -> Option<proc_macro2::TokenStream> {
	let tokens: Vec<proc_macro2::TokenTree> = value_ty.clone().into_iter().collect();
	let open = tokens
		.iter()
		.position(|token| matches!(token, proc_macro2::TokenTree::Punct(p) if p.as_char() == '<'))?;
	let path: String = tokens[..open]
		.iter()
		.map(|token| token.to_string())
		.collect::<String>()
		.replace(char::is_whitespace, "");
	let is_option = matches!(
		path.as_str(),
		"Option"
			| "::Option"
			| "std::option::Option"
			| "::std::option::Option"
			| "core::option::Option"
			| "::core::option::Option"
	);
	if !is_option {
		return None;
	}
	let close = tokens.len().checked_sub(1)?;
	if !matches!(&tokens[close], proc_macro2::TokenTree::Punct(p) if p.as_char() == '>') {
		return None;
	}
	let inner: proc_macro2::TokenStream = tokens[open + 1..close].iter().cloned().collect();
	if inner.is_empty() {
		return None;
	}
	Some(inner)
}

fn column_type_value(type_tokens: &[TokenTree]) -> Result<proc_macro2::TokenStream, String> {
	let mut angle_depth = 0usize;
	let mut seen_first_argument = false;
	let mut value_tokens = Vec::new();

	for token in type_tokens {
		match token {
			TokenTree::Punct(punct) if punct.as_char() == '<' => {
				angle_depth += 1;
				if seen_first_argument {
					value_tokens.push(token.clone());
				}
			}
			TokenTree::Punct(punct) if punct.as_char() == '>' => {
				if seen_first_argument && angle_depth > 1 {
					value_tokens.push(token.clone());
				}
				angle_depth = angle_depth.saturating_sub(1);
			}
			TokenTree::Punct(punct) if punct.as_char() == ',' && angle_depth == 1 => {
				seen_first_argument = true;
			}
			token if seen_first_argument => value_tokens.push(token.clone()),
			_ => {}
		}
	}

	if value_tokens.is_empty() {
		return Err("Table fields must use `C::Type<'scope, Value>`".to_owned());
	}

	Ok(proc_macro2::TokenStream::from(TokenStream::from_iter(
		value_tokens,
	)))
}

fn apply_attribute(group: &Group, attrs: &mut FieldAttrs) -> Result<(), String> {
	if group.delimiter() != Delimiter::Bracket {
		return Err("Table field attributes must use square brackets".to_owned());
	}

	let mut tokens = group.stream().into_iter();
	let Some(TokenTree::Ident(attr_name)) = tokens.next() else {
		return Ok(());
	};
	let attr_name = attr_name.to_string();

	// A `///` doc comment desugars to `#[doc = "..."]`; it is documentation, not schema metadata, so
	// ignore it rather than rejecting it as an unknown field attribute. Without this a doc comment on a
	// field is a compile error.
	if crate::common::is_ignored_attribute(&attr_name) {
		return Ok(());
	}

	let rest = tokens.collect::<Vec<_>>();

	if attr_name == "column" {
		let Some(TokenTree::Group(meta)) = rest.first() else {
			return Err("#[column(...)] requires metadata inside parentheses".to_owned());
		};
		parse_meta_items(meta.stream().into_iter().collect::<Vec<_>>(), attrs)
	} else if matches!(rest.first(), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
		parse_meta_item(&attr_name, &rest[1..], attrs)
	} else if let Some(TokenTree::Group(meta)) = rest.first() {
		parse_meta_item(&attr_name, &[TokenTree::Group(meta.clone())], attrs)
	} else {
		parse_meta_item(&attr_name, &rest, attrs)
	}
}

fn parse_meta_items(tokens: Vec<TokenTree>, attrs: &mut FieldAttrs) -> Result<(), String> {
	let mut index = 0;
	while index < tokens.len() {
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
			index += 1;
		}

		let Some(TokenTree::Ident(name)) = tokens.get(index) else {
			break;
		};
		let name = name.to_string();
		index += 1;

		let mut value_tokens = Vec::new();
		if matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
			index += 1;
			while index < tokens.len()
				&& !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',')
			{
				value_tokens.push(tokens[index].clone());
				index += 1;
			}
		} else if let Some(TokenTree::Group(group)) = tokens.get(index) {
			value_tokens.push(TokenTree::Group(group.clone()));
			index += 1;
		}

		parse_meta_item(&name, &value_tokens, attrs)?;
	}

	Ok(())
}

fn parse_meta_item(
	name: &str,
	value_tokens: &[TokenTree],
	attrs: &mut FieldAttrs,
) -> Result<(), String> {
	match name {
		"primary_key" => attrs.primary_key = true,
		"index" => attrs.index = true,
		"unique" => attrs.unique = true,
		"nullable" | "not_null" => {
			return Err(format!(
                "`#[column({name})]` was removed: declare nullability in the column type instead \
                 (`C::Type<'scope, Option<T>>` for a nullable column, `C::Type<'scope, T>` otherwise)"
            ));
		}
		"auto_increment" => attrs.auto_increment = true,
		"generated" => attrs.generated = true,
		"insert" => attrs.insert = Some(required_bool(name, value_tokens)?),
		"update" => attrs.update = Some(required_bool(name, value_tokens)?),
		"default" => attrs.default = Some(parse_default(value_tokens)?),
		"default_raw" => attrs.default = Some(DefaultAttrs::Raw(required_literal(name, value_tokens)?)),
		"db_type" => attrs.db_type = Some(required_literal(name, value_tokens)?),
		"check" => attrs.check = Some(required_literal(name, value_tokens)?),
		"where" => {
			if value_tokens.is_empty() {
				return Err("column option `where` requires a `= |row| ...` predicate".to_owned());
			}
			attrs.predicate = Some(token_trees_to_stream(value_tokens));
		}
		"column_name" | "name" => attrs.column_name = Some(required_literal(name, value_tokens)?),
		"references" => attrs.references = Some(parse_references(value_tokens)?),
		_ => return Err(format!("unsupported Table field attribute `{name}`")),
	}

	Ok(())
}

fn parse_default(value_tokens: &[TokenTree]) -> Result<DefaultAttrs, String> {
	let Some(token) = value_tokens.first() else {
		return Err(
            "attribute `default` requires `value(...)`, `current_timestamp`, `current_date`, or `current_time`"
                .to_owned(),
        );
	};

	if let TokenTree::Ident(ident) = token {
		return match ident.to_string().as_str() {
            "value" => {
                let Some(TokenTree::Group(group)) = value_tokens.get(1) else {
                    return Err("attribute `default = value(...)` requires a value".to_owned());
                };
                if group.delimiter() != Delimiter::Parenthesis {
                    return Err("attribute `default = value(...)` requires parentheses".to_owned());
                }
                Ok(DefaultAttrs::Literal(parse_default_literal(
                    &group.stream().into_iter().collect::<Vec<_>>(),
                )?))
            }
            "current_timestamp" => Ok(DefaultAttrs::CurrentTimestamp),
            "current_date" => Ok(DefaultAttrs::CurrentDate),
            "current_time" => Ok(DefaultAttrs::CurrentTime),
            _ => Err(format!(
                "unsupported default `{ident}`; use `value(...)`, `current_timestamp`, `current_date`, or `current_time`"
            )),
        };
	}

	Err("attribute `default` uses portable defaults; use `default = value(...)` for literals or `default_raw = \"...\"` for backend-specific SQL".to_owned())
}

fn parse_default_literal(value_tokens: &[TokenTree]) -> Result<DefaultLiteral, String> {
	let Some(token) = value_tokens.first() else {
		return Err("attribute `default = value(...)` requires a literal value".to_owned());
	};

	if let TokenTree::Ident(ident) = token {
		return match ident.to_string().as_str() {
			"null" => Ok(DefaultLiteral::Null),
			"true" => Ok(DefaultLiteral::Bool(true)),
			"false" => Ok(DefaultLiteral::Bool(false)),
			_ => Err(format!("unsupported default literal `{ident}`")),
		};
	}

	let mut negative = false;
	let token = if matches!(token, TokenTree::Punct(punct) if punct.as_char() == '-') {
		negative = true;
		value_tokens
			.get(1)
			.ok_or_else(|| "negative default value requires a numeric literal".to_owned())?
	} else {
		token
	};

	let TokenTree::Literal(literal_token) = token else {
		return Err("attribute `default = value(...)` requires a literal value".to_owned());
	};

	let literal = literal_token.to_string();
	if literal.starts_with('"') {
		if negative {
			return Err("string default values cannot be negative".to_owned());
		}
		return Ok(DefaultLiteral::Text(literal_string(literal_token)));
	}

	let literal = literal
		.trim_end_matches("i8")
		.trim_end_matches("i16")
		.trim_end_matches("i32")
		.trim_end_matches("i64")
		.trim_end_matches("i128")
		.trim_end_matches("isize")
		.trim_end_matches("u8")
		.trim_end_matches("u16")
		.trim_end_matches("u32")
		.trim_end_matches("u64")
		.trim_end_matches("u128")
		.trim_end_matches("usize")
		.trim_end_matches("f32")
		.trim_end_matches("f64");

	if literal.contains('.') {
		let mut value = literal
			.parse::<f64>()
			.map_err(|_| "float default value could not be parsed".to_owned())?;
		if negative {
			value = -value;
		}
		return Ok(DefaultLiteral::Float(value));
	}

	if negative {
		let value = literal
			.parse::<i128>()
			.map_err(|_| "integer default value could not be parsed".to_owned())?;
		Ok(DefaultLiteral::Int(-value))
	} else {
		if let Ok(value) = literal.parse::<i128>() {
			Ok(DefaultLiteral::Int(value))
		} else {
			let value = literal
				.parse::<u128>()
				.map_err(|_| "integer default value could not be parsed".to_owned())?;
			Ok(DefaultLiteral::UInt(value))
		}
	}
}

fn required_bool(name: &str, value_tokens: &[TokenTree]) -> Result<bool, String> {
	let Some(token) = value_tokens.first() else {
		return Err(format!("attribute `{name}` requires a boolean value"));
	};

	match token.to_string().as_str() {
		"true" => Ok(true),
		"false" => Ok(false),
		_ => Err(format!("attribute `{name}` requires a boolean value")),
	}
}

fn parse_references(value_tokens: &[TokenTree]) -> Result<ForeignKeyAttrs, String> {
	let Some(TokenTree::Group(group)) = value_tokens.first() else {
		return Err("references requires metadata like references(User::id)".to_owned());
	};

	let mut references = ForeignKeyAttrs::default();
	let tokens = group.stream().into_iter().collect::<Vec<_>>();
	let mut index = parse_reference_target(&tokens, &mut references)?;

	while index < tokens.len() {
		while matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == ',') {
			index += 1;
		}

		let Some(TokenTree::Ident(name)) = tokens.get(index) else {
			break;
		};
		let name = name.to_string();
		index += 1;

		if !matches!(tokens.get(index), Some(TokenTree::Punct(punct)) if punct.as_char() == '=') {
			return Err(format!(
				"references option `{name}` requires a string value"
			));
		}
		index += 1;

		let value = match tokens.get(index) {
			Some(TokenTree::Literal(literal)) => literal_string(literal),
			Some(token) => token.to_string(),
			None => return Err(format!("references option `{name}` is missing a value")),
		};
		index += 1;

		match name.as_str() {
			"on_delete" => references.on_delete = Some(value),
			"on_update" => references.on_update = Some(value),
			_ => return Err(format!("unsupported references option `{name}`")),
		}
	}

	if references.table.is_none() || references.column.is_none() {
		return Err("references requires a table field path like `User::id`".to_owned());
	}

	Ok(references)
}

fn parse_reference_target(
	tokens: &[TokenTree],
	references: &mut ForeignKeyAttrs,
) -> Result<usize, String> {
	let target_end = tokens
		.iter()
		.position(|token| matches!(token, TokenTree::Punct(punct) if punct.as_char() == ','))
		.unwrap_or(tokens.len());
	let target = &tokens[..target_end];

	let [TokenTree::Ident(table), TokenTree::Punct(first_colon), TokenTree::Punct(second_colon), TokenTree::Ident(column)] =
		target
	else {
		return Err("references requires a table field path like `User::id`".to_owned());
	};

	if first_colon.as_char() != ':' || second_colon.as_char() != ':' {
		return Err("references requires a table field path like `User::id`".to_owned());
	}

	references.table = Some(table.clone());
	references.column = Some(column.clone());

	Ok(target_end)
}
