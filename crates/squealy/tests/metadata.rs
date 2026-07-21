use squealy::*;

#[derive(Clone, Debug, PartialEq, Table)]
#[schema(Public)]
#[unique(name = "uq_widgets_tenant_slug", columns = [tenant_id, slug])]
#[index(name = "idx_widgets_slug", columns = [slug])]
struct Widget<'scope, C: ColumnMode = ColumnExpr> {
	#[column(primary_key, auto_increment)]
	id: C::Type<'scope, i32>,
	tenant_id: C::Type<'scope, i64>,
	#[column(default = value("new"), check = "slug <> ''")]
	slug: C::Type<'scope, String>,
	note: C::Type<'scope, Option<String>>,
}

#[allow(dead_code)]
#[derive(Schema)]
struct Public {
	widgets: Widget<'static, ColumnName>,
}

#[allow(dead_code)]
#[derive(Database)]
struct AppDatabase {
	public: Public,
}

#[test]
fn derives_export_owned_metadata_without_management_state() {
	let model = DatabaseModel::from_database::<AppDatabase>();
	assert_eq!(model.schemas.len(), 1);

	let schema = &model.schemas[0];
	assert_eq!(schema.name.as_deref(), Some("public"));
	assert!(schema.enums.is_empty());
	assert!(schema.sequences.is_empty());
	assert!(schema.domains.is_empty());

	let table = &schema.tables[0];
	assert_eq!(table.name, "widgets");
	assert_eq!(table.columns.len(), 4);
	assert_eq!(table.columns[0].ty, SqlType::I32);
	assert!(!table.columns[0].nullable);
	assert!(table.columns[0].identity.is_some());
	assert!(table.columns[3].nullable);
	assert_eq!(table.uniques[0].name, "uq_widgets_tenant_slug");
	assert_eq!(table.uniques[0].columns, ["tenant_id", "slug"]);
	assert_eq!(table.indexes[0].name, "idx_widgets_slug");
	assert!(table.exclusions.is_empty());

	let check = &table.checks[0].expression;
	assert!(matches!(check, ExprNode::Compare { .. }));
}

#[test]
fn complete_owned_metadata_collections_remain_public() {
	let mut schema = SchemaModel::default();
	schema.enums.push(EnumModel {
		name: "state".into(),
		labels: vec!["open".into(), "closed".into()],
	});
	schema.domains.push(DomainModel {
		name: "positive".into(),
		base_type: SqlType::I64,
		not_null: true,
		default: None,
		checks: Vec::new(),
	});
	schema.sequences.push(SequenceModel {
		name: "ticket_seq".into(),
		data_type: SequenceDataType::BigInt,
		start: 1,
		increment: 1,
		min: 1,
		max: i64::MAX,
		cache: 1,
		cycle: false,
		owned_by: None,
	});

	let model = DatabaseModel {
		schemas: vec![schema],
	};
	assert_eq!(model.schemas[0].enums[0].labels.len(), 2);
	assert_eq!(model.schemas[0].domains[0].base_type, SqlType::I64);
	assert_eq!(model.schemas[0].sequences[0].start, 1);
}
