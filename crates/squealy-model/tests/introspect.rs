use std::convert::Infallible;
use std::future;

use squealy_model::{DatabaseModel, SchemaIntrospect, SchemaModel, introspect};

#[derive(Debug)]
struct FakeIntrospector {
    called: bool,
    model: DatabaseModel,
}

impl SchemaIntrospect for FakeIntrospector {
    type Error = Infallible;

    fn introspect_database(&mut self) -> impl Future<Output = Result<DatabaseModel, Self::Error>> {
        self.called = true;
        future::ready(Ok(self.model.clone()))
    }
}

#[tokio::test]
async fn introspect_delegates_to_connection() {
    let model = DatabaseModel {
        schemas: vec![SchemaModel {
            name: Some("app".to_owned()),
            views: Vec::new(),
            enums: Vec::new(),
            sequences: Vec::new(),
            domains: Vec::new(),
            tables: Vec::new(),
        }],
    };
    let mut connection = FakeIntrospector {
        called: false,
        model: model.clone(),
    };

    let actual = introspect(&mut connection).await.unwrap();

    assert!(connection.called);
    assert_eq!(actual, model);
}
