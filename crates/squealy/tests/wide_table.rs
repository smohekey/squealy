use squealy::*;
use squealy_test::TestConnection;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(WideSchema)]
struct WideRow<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	required_name: C::Type<'scope, String>,
	optional01: C::Type<'scope, Option<i32>>,
	optional02: C::Type<'scope, Option<i32>>,
	optional03: C::Type<'scope, Option<i32>>,
	optional04: C::Type<'scope, Option<i32>>,
	optional05: C::Type<'scope, Option<i32>>,
	optional06: C::Type<'scope, Option<i32>>,
	optional07: C::Type<'scope, Option<i32>>,
	optional08: C::Type<'scope, Option<i32>>,
	optional09: C::Type<'scope, Option<i32>>,
	optional10: C::Type<'scope, Option<i32>>,
	optional11: C::Type<'scope, Option<i32>>,
	optional12: C::Type<'scope, Option<i32>>,
	optional13: C::Type<'scope, Option<i32>>,
	#[column(default = value(1))]
	default01: C::Type<'scope, i32>,
	#[column(default = value(2))]
	default02: C::Type<'scope, i32>,
	#[column(default = value(3))]
	default03: C::Type<'scope, i32>,
	#[column(default = value(4))]
	default04: C::Type<'scope, i32>,
	#[column(default = value(5))]
	default05: C::Type<'scope, i32>,
	#[column(default = value(6))]
	default06: C::Type<'scope, i32>,
	#[column(default = value(7))]
	default07: C::Type<'scope, i32>,
	#[column(default = value(8))]
	default08: C::Type<'scope, i32>,
	#[column(default = value(9))]
	default09: C::Type<'scope, i32>,
	#[column(default = value(10))]
	default10: C::Type<'scope, i32>,
	#[column(default = value(11))]
	default11: C::Type<'scope, i32>,
	#[column(default = value(12))]
	default12: C::Type<'scope, i32>,
	#[column(default = value(13))]
	default13: C::Type<'scope, i32>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct WideSchema {
	wide_rows: WideRow<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct WideDatabase {
	wide: WideSchema,
}

#[derive(Clone, Debug, PartialEq, Table)]
struct OperationSpecific<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	common: C::Type<'scope, String>,
	#[column(update = false)]
	insert_only: C::Type<'scope, String>,
	#[column(insert = false)]
	update_only: C::Type<'scope, String>,
}

#[test]
fn derives_and_builds_wide_insert_and_update_chains() {
	let model = DatabaseModel::from_database::<WideDatabase>();
	assert_eq!(model.schemas[0].tables[0].columns.len(), 28);

	let _insert = TestConnection
		.to::<WideRow>()
		.required_name("wide")
		.optional01(Some(1))
		.insert();

	let _update = TestConnection
		.to::<WideRow>()
		.required_name("renamed")
		.where_(|row| row.id.equals(1))
		.update();
}

#[test]
fn one_assignment_list_projects_operation_specific_columns() {
	let insert = TestConnection
		.to::<OperationSpecific>()
		.common("common")
		.insert_only("insert")
		.update_only("ignored")
		.insert_returning(|row| row.id);
	assert_eq!(
		insert.to_sql(),
		"INSERT INTO operation_specifics (common, insert_only) VALUES (?, ?) RETURNING q0_0.id AS id"
	);

	let update = TestConnection
		.to::<OperationSpecific>()
		.common("common")
		.insert_only("ignored")
		.update_only("update")
		.all()
		.update_returning(|row| row.id);
	assert_eq!(
		update.to_sql(),
		"UPDATE operation_specifics AS q0_0 SET common = ?, update_only = ? RETURNING q0_0.id AS id"
	);
}
