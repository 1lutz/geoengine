use crate::contexts::SessionId;
use crate::handlers::ErrorResponse;
use actix_web::dev::ServiceResponse;
use actix_web::http::{header, Method};
use actix_web::test::TestRequest;
use actix_web_httpauth::headers::authorization::Bearer;
use std::collections::BTreeMap;
use std::future::Future;
use opentelemetry::sdk::metrics::registry::unique_instrument_meter_core;
use utoipa::openapi::path::Parameter;
use utoipa::openapi::{KnownFormat, OpenApi, PathItemType, Ref, RefOr, Schema, SchemaFormat, SchemaType};
use uuid::Uuid;

pub mod model;

fn get_schema_name_from_ref(reference: Ref) -> &str {
    &reference.ref_location[21..]
}

fn throw_if_invalid_ref(reference: Ref, schemas: &BTreeMap<String, RefOr<Schema>>) {
    assert!(
        schemas.contains_key(get_schema_name_from_ref(reference)),
        "Referenced the unknown schema `{}`",
        reference.ref_location
    );
}

fn can_resolve_schema(schema: RefOr<Schema>, schemas: &BTreeMap<String, RefOr<Schema>>) {
    match schema {
        RefOr::Ref(reference) => {
            throw_if_invalid_ref(reference, schemas);
        }
        RefOr::T(concrete) => match concrete {
            Schema::Array(arr) => {
                can_resolve_schema(*arr.items, &schemas);
            }
            Schema::Object(obj) => {
                for property in obj.properties.into_values() {
                    can_resolve_schema(property, schemas);
                }
                if let Some(additional_properties) = obj.additional_properties {
                    can_resolve_schema(*additional_properties, schemas);
                }
            }
            Schema::OneOf(oo) => {
                for item in oo.items {
                    can_resolve_schema(item, schemas);
                }
            }
            Schema::AllOf(ao) => {
                for item in ao.items {
                    can_resolve_schema(item, schemas);
                }
            }
            _ => panic!("Unknown schema type"),
        },
    }
}

pub fn can_resolve_api(api: OpenApi) {
    let schemas = api
        .components
        .expect("api has at least one component")
        .schemas;

    for path_item in api.paths.paths.into_values() {
        for operation in path_item.operations.into_values() {
            if let Some(request_body) = operation.request_body {
                for content in request_body.content.into_values() {
                    can_resolve_schema(content.schema, &schemas);
                }
            }

            if let Some(parameters) = operation.parameters {
                for parameter in parameters {
                    if let Some(schema) = parameter.schema {
                        can_resolve_schema(schema, &schemas);
                    }
                }
            }

            for response in operation.responses.responses.into_values() {
                match response {
                    RefOr::Ref(reference) => {
                        throw_if_invalid_ref(reference, &schemas);
                    }
                    RefOr::T(concrete) => {
                        for content in concrete.content.into_values() {
                            can_resolve_schema(content.schema, &schemas);
                        }
                    }
                }
            }
        }
    }
}

pub struct RunnableExample<'a, C, F, Fut>
where
    F: Fn(actix_web::test::TestRequest, C) -> Fut,
    Fut: Future<Output = ServiceResponse>,
{
    pub http_method: &'a PathItemType,
    pub uri: &'a str,
    pub parameters: &'a Option<Vec<Parameter>>,
    pub body: serde_json::Value,
    pub with_auth: bool,
    pub ctx: C,
    pub session_id: SessionId,
    pub send_test_request: &'a F,
}

impl<'a, C, F, Fut> RunnableExample<'a, C, F, Fut>
where
    F: Fn(TestRequest, C) -> Fut,
    Fut: Future<Output = ServiceResponse>,
{
    fn get_actix_http_method(&self) -> Method {
        match self.http_method {
            PathItemType::Get => Method::GET,
            PathItemType::Post => Method::POST,
            PathItemType::Put => Method::PUT,
            PathItemType::Delete => Method::DELETE,
            PathItemType::Options => Method::OPTIONS,
            PathItemType::Head => Method::HEAD,
            PathItemType::Patch => Method::PATCH,
            PathItemType::Trace => Method::TRACE,
            PathItemType::Connect => Method::CONNECT,
        }
    }

    fn get_default_parameter_value(schema: &Schema) -> String {
        match schema {
            Schema::Object(obj) => {
                match obj.schema_type {
                    SchemaType::String => {
                        match obj.format {
                            Some(SchemaFormat::KnownFormat(format)) => {
                                match format {
                                    KnownFormat::Uuid => Uuid::new_v4().to_string(),
                                    _ => unimplemented!()
                                }
                            },
                            None => "asdf".to_string(),
                            _ => unimplemented!()
                        }
                    }
                    SchemaType::Integer | SchemaType::Number => "42".to_string(),
                    SchemaType::Boolean => "false".to_string,
                    _ => unimplemented!()
                }
            }
            _ => unimplemented!()
        }
    }

    fn resolve_schema(ref_or: RefOr<Schema>, schemas: &BTreeMap<String, RefOr<Schema>>) -> &Schema {
        match ref_or {
            RefOr::Ref(reference) => {
                let schema_name = get_schema_name_from_ref(reference);
                match schemas.get(schema_name).expect("presence of schema is checked in other test") {
                    RefOr::Ref(_) => panic!("recursive refs are not supported for parameters"),
                    RefOr::T(concrete) => concrete,
                }
            }
            RefOr::T(concrete) => &concrete
        }
    }

    fn insert_parameters(uri: &str, parameters: &Vec<Parameter>, schemas: &BTreeMap<String, RefOr<Schema>>, req: &mut TestRequest) {
        for parameter in parameters {
            let schema = Self::resolve_schema(parameter.schema.expect("utoipa adds schema everytime"));
            let value = Self::get_default_parameter_value(schema);
        }
    }

    pub fn build_request(&self) -> TestRequest {
        let http_method = self.get_actix_http_method();
        let mut req = TestRequest::default().method(http_method);

        if let Some(parameters) = self.parameters {
            Self::insert_parameters(self.uri, parameters, &mut req);
        } else {
            req = req.uri(self.uri);
        }
        if self.with_auth {
            req = req.append_header((
                header::AUTHORIZATION,
                Bearer::new(self.session_id.to_string()),
            ))
        }
        req.append_header((header::CONTENT_TYPE, "application/json"))
            .set_json(&self.body)
    }

    pub async fn run(self) -> ServiceResponse {
        let req = self.build_request();
        (self.send_test_request)(req, self.ctx).await
    }

    pub async fn check_for_deserialization_error(self) {
        let res = self.run().await;

        if res.status() == 400 {
            let method = res.request().head().method.to_string();
            let path = res.request().path().to_string();
            let body: ErrorResponse = actix_web::test::read_body_json(res).await;

            if body.error == "BodyDeserializeError" {
                panic!(
                    "The example at {} {} threw an BodyDeserializeError",
                    method, path
                );
            }
        }
    }
}

pub async fn can_run_examples<F1, Fut1, F2, Fut2, C>(api: OpenApi, ctx: F1, send_test_request: F2)
where
    F1: Fn() -> Fut1,
    Fut1: Future<Output = (C, SessionId)>,
    F2: Fn(actix_web::test::TestRequest, C) -> Fut2,
    Fut2: Future<Output = ServiceResponse>,
{
    for (uri, path_item) in api.paths.paths {
        for (http_method, operation) in path_item.operations {
            if let Some(request_body) = operation.request_body {
                let with_auth = operation.security.is_some();

                for content in request_body.content.into_values() {
                    if let Some(example) = content.example {
                        let (ctx, session_id) = ctx().await;
                        RunnableExample {
                            http_method: &http_method,
                            uri: uri.as_str(),
                            parameters: &operation.parameters,
                            body: example,
                            with_auth,
                            ctx: ctx,
                            session_id: session_id,
                            send_test_request: &send_test_request,
                        }
                        .check_for_deserialization_error()
                        .await;
                    } else {
                        for example in content.examples.into_values() {
                            match example {
                                RefOr::Ref(_reference) => {
                                    unimplemented!()
                                }
                                RefOr::T(concrete) => {
                                    if let Some(body) = concrete.value {
                                        let (ctx, session_id) = ctx().await;
                                        RunnableExample {
                                            http_method: &http_method,
                                            uri: uri.as_str(),
                                            parameters: &operation.parameters,
                                            body,
                                            with_auth,
                                            ctx: ctx,
                                            session_id: session_id,
                                            send_test_request: &send_test_request,
                                        }
                                        .check_for_deserialization_error()
                                        .await;
                                    } else {
                                        //skip external examples
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use utoipa::openapi::path::{OperationBuilder, ParameterBuilder, PathItemBuilder};
    use utoipa::openapi::request_body::RequestBodyBuilder;
    use utoipa::openapi::{
        AllOfBuilder, ArrayBuilder, ComponentsBuilder, ContentBuilder, ObjectBuilder, OneOfBuilder,
        OpenApiBuilder, PathItemType, PathsBuilder, ResponseBuilder,
    };

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn throws_because_of_invalid_ref() {
        throw_if_invalid_ref(Ref::from_schema_name("MissingSchema"), &BTreeMap::new());
    }

    #[test]
    fn finds_ref() {
        throw_if_invalid_ref(
            Ref::from_schema_name("ExistingSchema"),
            &BTreeMap::from([("ExistingSchema".to_string(), RefOr::T(Schema::default()))]),
        );
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detects_missing_array_ref() {
        can_resolve_schema(
            RefOr::T(
                ArrayBuilder::new()
                    .items(Ref::from_schema_name("MissingSchema"))
                    .into(),
            ),
            &BTreeMap::new(),
        );
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detects_missing_object_ref() {
        can_resolve_schema(
            RefOr::T(
                ObjectBuilder::new()
                    .property("Prop", Ref::from_schema_name("MissingSchema"))
                    .into(),
            ),
            &BTreeMap::new(),
        );
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detects_missing_oneof_ref() {
        can_resolve_schema(
            RefOr::T(
                OneOfBuilder::new()
                    .item(Ref::from_schema_name("MissingSchema"))
                    .into(),
            ),
            &BTreeMap::new(),
        );
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detects_missing_allof_ref() {
        can_resolve_schema(
            RefOr::T(
                AllOfBuilder::new()
                    .item(Ref::from_schema_name("MissingSchema"))
                    .into(),
            ),
            &BTreeMap::new(),
        );
    }

    #[test]
    fn successfull_api_validation() {
        let api: OpenApi = OpenApiBuilder::new()
            .paths(
                PathsBuilder::new().path(
                    "asdf",
                    PathItemBuilder::new()
                        .operation(
                            PathItemType::Post,
                            OperationBuilder::new()
                                .parameter(
                                    ParameterBuilder::new()
                                        .schema(Some(Ref::from_schema_name("Schema1"))),
                                )
                                .request_body(Some(
                                    RequestBodyBuilder::new()
                                        .content(
                                            "application/json",
                                            ContentBuilder::new()
                                                .schema(Ref::from_schema_name("Schema2"))
                                                .into(),
                                        )
                                        .into(),
                                ))
                                .response(
                                    "200",
                                    ResponseBuilder::new().content(
                                        "application/json",
                                        ContentBuilder::new()
                                            .schema(Ref::from_schema_name("Schema3"))
                                            .into(),
                                    ),
                                ),
                        )
                        .into(),
                ),
            )
            .components(Some(
                ComponentsBuilder::new()
                    .schemas_from_iter([
                        ("Schema1", Schema::default()),
                        ("Schema2", Schema::default()),
                        ("Schema3", Schema::default()),
                    ])
                    .into(),
            ))
            .into();
        can_resolve_api(api);
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detect_unresolvable_request_body() {
        let api: OpenApi = OpenApiBuilder::new()
            .paths(
                PathsBuilder::new().path(
                    "asdf",
                    PathItemBuilder::new()
                        .operation(
                            PathItemType::Post,
                            OperationBuilder::new().request_body(Some(
                                RequestBodyBuilder::new()
                                    .content(
                                        "application/json",
                                        ContentBuilder::new()
                                            .schema(Ref::from_schema_name("MissingSchema"))
                                            .into(),
                                    )
                                    .into(),
                            )),
                        )
                        .into(),
                ),
            )
            .components(Some(
                ComponentsBuilder::new()
                    .schemas_from_iter([
                        ("Schema1", Schema::default()),
                        ("Schema2", Schema::default()),
                        ("Schema3", Schema::default()),
                    ])
                    .into(),
            ))
            .into();
        can_resolve_api(api);
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detect_unresolvable_parameter() {
        let api: OpenApi = OpenApiBuilder::new()
            .paths(
                PathsBuilder::new().path(
                    "asdf",
                    PathItemBuilder::new()
                        .operation(
                            PathItemType::Post,
                            OperationBuilder::new().parameter(
                                ParameterBuilder::new()
                                    .schema(Some(Ref::from_schema_name("MissingSchema"))),
                            ),
                        )
                        .into(),
                ),
            )
            .components(Some(
                ComponentsBuilder::new()
                    .schemas_from_iter([
                        ("Schema1", Schema::default()),
                        ("Schema2", Schema::default()),
                        ("Schema3", Schema::default()),
                    ])
                    .into(),
            ))
            .into();
        can_resolve_api(api);
    }

    #[test]
    #[should_panic(expected = "MissingSchema")]
    fn detect_unresolvable_response() {
        let api: OpenApi = OpenApiBuilder::new()
            .paths(
                PathsBuilder::new().path(
                    "asdf",
                    PathItemBuilder::new()
                        .operation(
                            PathItemType::Post,
                            OperationBuilder::new().response(
                                "200",
                                ResponseBuilder::new().content(
                                    "application/json",
                                    ContentBuilder::new()
                                        .schema(Ref::from_schema_name("MissingSchema"))
                                        .into(),
                                ),
                            ),
                        )
                        .into(),
                ),
            )
            .components(Some(
                ComponentsBuilder::new()
                    .schemas_from_iter([
                        ("Schema1", Schema::default()),
                        ("Schema2", Schema::default()),
                        ("Schema3", Schema::default()),
                    ])
                    .into(),
            ))
            .into();
        can_resolve_api(api);
    }
}
