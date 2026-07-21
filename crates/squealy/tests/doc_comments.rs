//! `///` doc comments on derived structs and their fields must not break the derive macros.
//!
//! Regression test for git-bug 630bed8: the hand-rolled field-attribute parser rejected any attribute
//! it did not recognize, so a `#[doc = "..."]` (the desugaring of `///`) on a field of a `#[derive(Table)]`
//! / `#[derive(View)]` struct was a compile error (`unsupported Table field attribute doc`). Doc comments
//! are documentation, not schema metadata — the derives must tolerate them. The test's value is that this
//! file compiles at all; the assertions just instantiate the derived items.

use squealy::*;

/// A user account.
///
/// A second doc line, to exercise multi-line docs.
#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
struct User<'scope, C: ColumnMode = ColumnExpr> {
	/// The primary key.
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	/// The user's display name.
	name: C::Type<'scope, String>,
	/// A doc comment placed *before* a column attribute.
	#[column(index)]
	active: C::Type<'scope, bool>,
}

/// Active users only.
#[allow(dead_code)]
#[derive(View)]
#[schema(Public)]
struct ActiveUser<'scope, C: ColumnMode = ColumnExpr> {
	/// The user id.
	id: C::Type<'scope, i32>,
	/// The user's display name.
	name: C::Type<'scope, String>,
}

impl<'scope, C: ColumnMode> ViewDefinition for ActiveUser<'scope, C> {
	fn definition(db: &'static ModelConn) -> impl ViewSelect<Row = <Self as SchemaView>::Row> {
		db.from::<User>()
			.where_(|user| user.active.equals(true))
			.project(|(user,)| (user.id, user.name))
	}
}

/// The public schema.
#[allow(dead_code)]
#[derive(Schema)]
struct Public {
	/// The users table.
	users: User<'static, ColumnName>,
	/// The active-users view.
	#[view]
	active_users: ActiveUser<'static, ColumnName>,
}

/// The application database.
#[allow(dead_code)]
#[derive(Database)]
struct AppDatabase {
	/// The public schema.
	public: Public,
}

/// A custom column type.
#[allow(dead_code)]
#[derive(ColumnType)]
#[column_type(db_type = "TEXT")]
struct Slug(String);

#[test]
fn doc_comments_do_not_break_the_derives() {
	// Compilation is the assertion; touch a couple of derived items so nothing is dead-code-eliminated
	// before its impl is checked.
	assert_eq!(<User<ColumnName> as SchemaTable>::name(), "users");
	let model = DatabaseModel::from_database::<AppDatabase>();
	assert_eq!(model.schemas[0].tables[0].name, "users");
	assert_eq!(model.schemas[0].views[0].name, "active_users");
}
