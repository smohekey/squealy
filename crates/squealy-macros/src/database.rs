use proc_macro::{Delimiter, Group, Ident, TokenStream, TokenTree};
use proc_macro2::{Literal, Span};

use crate::common::{compile_error, generated_ident, matches_ident, struct_fields};

pub(crate) fn derive(input: TokenStream) -> TokenStream {
	match database_struct(input) {
		Ok(database) => database.expand(),
		Err(message) => compile_error(&message),
	}
}

struct DatabaseStruct {
	ident: Ident,
	fields: Vec<DatabaseField>,
}

struct DatabaseField {
	ident: Ident,
	ty: proc_macro2::TokenStream,
}

impl DatabaseStruct {
	fn expand(&self) -> TokenStream {
		let ident = proc_macro2::Ident::new(&self.ident.to_string(), Span::call_site());
		let schema_idents = self
			.fields
			.iter()
			.map(|field| generated_ident(&ident, &field.ident.to_string(), "Schema"))
			.collect::<Vec<_>>();
		let schema_types = self
			.fields
			.iter()
			.map(|field| &field.ty)
			.collect::<Vec<_>>();
		let schema_defs = schema_idents
            .iter()
            .zip(schema_types.iter())
            .map(|(schema_ident, schema_type)| {
                quote::quote! {
                    struct #schema_ident;

                    impl ::squealy::DatabaseSchema for #schema_ident {
                        fn name(&self) -> Option<&'static str> {
                            <#schema_type as ::squealy::Schema>::name()
                        }

                        fn tables(&self) -> ::std::boxed::Box<dyn Iterator<Item = &'static (dyn ::squealy::Table + Sync)> + '_> {
                            ::std::boxed::Box::new(<#schema_type as ::squealy::Schema>::tables())
                        }

                        fn views(&self) -> ::std::boxed::Box<dyn Iterator<Item = &'static (dyn ::squealy::ViewDef + Sync)> + '_> {
                            ::std::boxed::Box::new(<#schema_type as ::squealy::Schema>::views())
                        }
                    }
                }
            })
            .collect::<Vec<_>>();
		let schemas_static = generated_ident(&ident, "schemas", "Static");
		let schemas_len = Literal::usize_unsuffixed(schema_idents.len());

		quote::quote! {
            #(#schema_defs)*

            static #schemas_static: [&'static (dyn ::squealy::DatabaseSchema + Sync); #schemas_len] = [#( &#schema_idents, )*];

            impl ::squealy::Database for #ident {
                fn schemas() -> impl Iterator<Item = &'static (dyn ::squealy::DatabaseSchema + Sync)> {
                    #schemas_static.into_iter()
                }
            }
        }
        .into()
	}
}

fn database_struct(input: TokenStream) -> Result<DatabaseStruct, String> {
	let tokens = input.into_iter().collect::<Vec<_>>();
	let struct_index = tokens
		.iter()
		.position(|token| matches_ident(token, "struct"))
		.ok_or_else(|| "Database can only be derived for structs".to_owned())?;

	let ident = tokens
		.get(struct_index + 1)
		.and_then(|token| match token {
			TokenTree::Ident(ident) => Some(ident.clone()),
			_ => None,
		})
		.ok_or_else(|| "Database derive could not find the struct name".to_owned())?;

	let body_index = tokens
		.iter()
		.position(
			|token| matches!(token, TokenTree::Group(group) if group.delimiter() == Delimiter::Brace),
		)
		.ok_or_else(|| "Database requires a named-field struct".to_owned())?;

	let fields = match &tokens[body_index] {
		TokenTree::Group(group) => database_fields(group)?,
		_ => unreachable!(),
	};

	Ok(DatabaseStruct { ident, fields })
}

fn database_fields(group: &Group) -> Result<Vec<DatabaseField>, String> {
	struct_fields(group, "database field").map(|fields| {
		fields
			.into_iter()
			.map(|field| DatabaseField {
				ident: field.ident,
				ty: field.ty,
			})
			.collect()
	})
}
