use crate::contexts::ApplicationContext;
use crate::error::Result;
use crate::handlers::SessionContext;
use crate::projects::{
    CreateProject, LoadVersion, ProjectDb, ProjectId, ProjectListOptions, ProjectVersionId,
    UpdateProject,
};
use crate::util::extractors::{ValidatedJson, ValidatedQuery};
use crate::util::IdResponse;
use actix_web::{web, FromRequest, HttpResponse, Responder};

pub(crate) fn init_project_routes<C>(cfg: &mut web::ServiceConfig)
where
    C: ApplicationContext,
    C::Session: FromRequest,
{
    cfg.service(web::resource("/projects").route(web::get().to(list_projects_handler::<C>)))
        .service(
            web::scope("/project")
                .service(web::resource("").route(web::post().to(create_project_handler::<C>)))
                .service(
                    web::resource("/{project}")
                        .route(web::get().to(load_project_latest_handler::<C>))
                        .route(web::patch().to(update_project_handler::<C>))
                        .route(web::delete().to(delete_project_handler::<C>)),
                )
                .service(
                    web::resource("/{project}/versions")
                        .route(web::get().to(project_versions_handler::<C>)),
                )
                .service(
                    web::resource("/{project}/{version}")
                        .route(web::get().to(load_project_version_handler::<C>)),
                ),
        );
}

/// Create a new project for the user.
#[utoipa::path(
    tag = "Projects",
    post,
    path = "/project",
    request_body = CreateProject,
    responses(
        (status = 200, response = crate::api::model::responses::IdResponse)
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn create_project_handler<C: ApplicationContext>(
    session: C::Session,
    app_ctx: web::Data<C>,
    create: ValidatedJson<CreateProject>,
) -> Result<impl Responder> {
    let create = create.into_inner();
    let id = app_ctx
        .session_context(session)
        .db()
        .create_project(create)
        .await?;
    Ok(web::Json(IdResponse::from(id)))
}

/// List all projects accessible to the user that match the selected criteria.
#[utoipa::path(
    tag = "Projects",
    get,
    path = "/projects",
    responses(
        (status = 200, description = "List of projects the user can access", body = [ProjectListing],
            example = json!([
                {
                    "id": "df4ad02e-0d61-4e29-90eb-dc1259c1f5b9",
                    "name": "Test",
                    "description": "Foo",
                    "layerNames": [],
                    "plotNames": [],
                    "changed": "2021-04-26T14:03:51.984537900Z"
                }
            ])
        )
    ),
    params(ProjectListOptions),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn list_projects_handler<C: ApplicationContext>(
    session: C::Session,
    app_ctx: web::Data<C>,
    options: ValidatedQuery<ProjectListOptions>,
) -> Result<impl Responder> {
    let options = options.into_inner();
    let listing = app_ctx
        .session_context(session)
        .db()
        .list_projects(options)
        .await?;
    Ok(web::Json(listing))
}

/// Updates a project.
/// This will create a new version.
#[utoipa::path(
    tag = "Projects",
    patch,
    path = "/project/{project}",
    request_body = UpdateProject,
    responses(
        (status = 200, description = "OK")
    ),
    params(
        ("project" = ProjectId, description = "Project id")
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn update_project_handler<C: ApplicationContext>(
    project: web::Path<ProjectId>,
    session: C::Session,
    app_ctx: web::Data<C>,
    update: ValidatedJson<UpdateProject>,
) -> Result<impl Responder> {
    let mut update = update.into_inner();
    update.id = project.into_inner(); // TODO: avoid passing project id in path AND body
    app_ctx
        .session_context(session)
        .db()
        .update_project(update)
        .await?;
    Ok(HttpResponse::Ok())
}

/// Deletes a project.
#[utoipa::path(
    tag = "Projects",
    delete,
    path = "/project/{project}",
    responses(
        (status = 200, description = "OK")
    ),
    params(
        ("project" = ProjectId, description = "Project id")
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn delete_project_handler<C: ApplicationContext>(
    project: web::Path<ProjectId>,
    session: C::Session,
    app_ctx: web::Data<C>,
) -> Result<impl Responder> {
    app_ctx
        .session_context(session)
        .db()
        .delete_project(*project)
        .await?;
    Ok(HttpResponse::Ok())
}

/// Retrieves details about the given version of a project.
#[utoipa::path(
    tag = "Projects",
    get,
    path = "/project/{project}/{version}",
    responses(
        (status = 200, description = "Project loaded from database", body = Project,
            example = json!({
                "id": "df4ad02e-0d61-4e29-90eb-dc1259c1f5b9",
                "version": {
                    "id": "8f4b8683-f92c-4129-a16f-818aeeee484e",
                    "changed": "2021-04-26T14:05:39.677390600Z",
                    "author": "5b4466d2-8bab-4ed8-a182-722af3c80958"
                },
                "name": "Test",
                "description": "Foo",
                "layers": [],
                "plots": [],
                "bounds": {
                    "spatialReference": "EPSG:4326",
                    "boundingBox": {
                        "lowerLeftCoordinate": {
                            "x": 0.0,
                            "y": 0.0
                        },
                        "upperRightCoordinate": {
                            "x": 1.0,
                            "y": 1.0
                        }
                    },
                    "timeInterval": {
                        "start": 0,
                        "end": 1
                    }
                },
                "timeStep": {
                    "granularity": "months",
                    "step": 1
                }
            })
        )
    ),
    params(
        ("project" = ProjectId, description = "Project id"),
        ("version" = ProjectVersionId, description = "Version id")
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn load_project_version_handler<C: ApplicationContext>(
    project: web::Path<(ProjectId, ProjectVersionId)>,
    session: C::Session,
    app_ctx: web::Data<C>,
) -> Result<impl Responder> {
    let project = project.into_inner();
    let id = app_ctx
        .session_context(session)
        .db()
        .load_project_version(project.0, LoadVersion::Version(project.1))
        .await?;
    Ok(web::Json(id))
}

/// Retrieves details about the latest version of a project.
#[utoipa::path(
    tag = "Projects",
    get,
    path = "/project/{project}",
    responses(
        (status = 200, description = "Project loaded from database", body = Project,
            example = json!({
                "id": "df4ad02e-0d61-4e29-90eb-dc1259c1f5b9",
                "version": {
                    "id": "8f4b8683-f92c-4129-a16f-818aeeee484e",
                    "changed": "2021-04-26T14:05:39.677390600Z",
                    "author": "5b4466d2-8bab-4ed8-a182-722af3c80958"
                },
                "name": "Test",
                "description": "Foo",
                "layers": [],
                "plots": [],
                "bounds": {
                    "spatialReference": "EPSG:4326",
                    "boundingBox": {
                        "lowerLeftCoordinate": {
                            "x": 0.0,
                            "y": 0.0
                        },
                        "upperRightCoordinate": {
                            "x": 1.0,
                            "y": 1.0
                        }
                    },
                    "timeInterval": {
                        "start": 0,
                        "end": 1
                    }
                },
                "timeStep": {
                    "granularity": "months",
                    "step": 1
                }
            })
        )
    ),
    params(
        ("project" = ProjectId, description = "Project id")
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn load_project_latest_handler<C: ApplicationContext>(
    project: web::Path<ProjectId>,
    session: C::Session,
    app_ctx: web::Data<C>,
) -> Result<impl Responder> {
    let id = app_ctx
        .session_context(session)
        .db()
        .load_project_version(project.into_inner(), LoadVersion::Latest)
        .await?;
    Ok(web::Json(id))
}

/// Lists all available versions of a project.
#[utoipa::path(
    tag = "Projects",
    get,
    path = "/project/{project}/versions",
    responses(
        (status = 200, description = "OK", body = [ProjectVersion],
            example = json!([
                {
                    "id": "8f4b8683-f92c-4129-a16f-818aeeee484e",
                    "changed": "2021-04-26T14:05:39.677390600Z",
                    "author": "5b4466d2-8bab-4ed8-a182-722af3c80958"
                },
                {
                    "id": "ced041c7-4b1d-4d13-b076-94596be6a36a",
                    "changed": "2021-04-26T14:13:10.901912700Z",
                    "author": "5b4466d2-8bab-4ed8-a182-722af3c80958"
                }
            ])
        )
    ),
    params(
        ("project" = ProjectId, description = "Project id")
    ),
    security(
        ("session_token" = [])
    )
)]
pub(crate) async fn project_versions_handler<C: ApplicationContext>(
    session: C::Session,
    app_ctx: web::Data<C>,
    project: web::Path<ProjectId>,
) -> Result<impl Responder> {
    let versions = app_ctx
        .session_context(session)
        .db()
        .list_project_versions(project.into_inner())
        .await?;
    Ok(web::Json(versions))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::model::datatypes::Colorizer;
    use crate::contexts::{Session, SimpleApplicationContext, SimpleSession};
    use crate::handlers::ErrorResponse;
    use crate::util::tests::{
        check_allowed_http_methods, create_project_helper, send_test_request,
        update_project_helper, with_temp_context,
    };
    use crate::util::Identifier;
    use crate::workflows::workflow::WorkflowId;
    use crate::{
        contexts::PostgresContext,
        projects::{
            LayerUpdate, LayerVisibility, Plot, PlotUpdate, Project, ProjectId, ProjectLayer,
            ProjectListing, RasterSymbology, STRectangle, Symbology, UpdateProject,
        },
    };
    use actix_web::dev::ServiceResponse;
    use actix_web::{http::header, http::Method, test};
    use actix_web_httpauth::headers::authorization::Bearer;

    use geoengine_datatypes::primitives::{TimeGranularity, TimeStep};
    use geoengine_datatypes::spatial_reference::SpatialReference;

    use serde_json::json;
    use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
    use tokio_postgres::{NoTls, Socket};

    async fn create_test_helper(
        app_ctx: PostgresContext<NoTls>,
        method: Method,
    ) -> ServiceResponse {
        let ctx = app_ctx.default_session_context().await.unwrap();

        let session_id = ctx.session().id();

        let create = CreateProject {
            name: "Test".to_string(),
            description: "Foo".to_string(),
            bounds: STRectangle::new(SpatialReference::epsg_4326(), 0., 0., 1., 1., 0, 1).unwrap(),
            time_step: Some(TimeStep {
                step: 1,
                granularity: TimeGranularity::Months,
            }),
        };

        let req = test::TestRequest::default()
            .method(method)
            .uri("/project")
            .append_header((header::CONTENT_LENGTH, 0))
            .append_header((header::AUTHORIZATION, Bearer::new(session_id.to_string())))
            .set_json(&create);
        send_test_request(req, app_ctx).await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create() {
        with_temp_context(move |app_ctx, _| async move {
            let res = create_test_helper(app_ctx, Method::POST).await;

            assert_eq!(res.status(), 200);

            let _project: IdResponse<ProjectId> = test::read_body_json(res).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_invalid_method() {
        with_temp_context(|app_ctx, _| async move {
            check_allowed_http_methods(
                |method| create_test_helper(app_ctx.clone(), method),
                &[Method::POST],
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_invalid_body() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session_id = ctx.session().id();

            let req = test::TestRequest::post()
                .uri("/project")
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session_id.to_string())))
                .set_payload("no json");
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                415,
                "UnsupportedMediaType",
                "Unsupported content type header.",
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_missing_fields() {
        with_temp_context(|app_ctx, _| async move {
        let ctx = app_ctx.default_session_context().await.unwrap();

        let session_id = ctx.session().id();

        let create = json!({
            "description": "Foo".to_string(),
            "bounds": STRectangle::new(SpatialReference::epsg_4326(), 0., 0., 1., 1., 0, 1).unwrap(),
        });

        let req = test::TestRequest::post()
            .uri("/project")
            .append_header((header::AUTHORIZATION, Bearer::new(session_id.to_string())))
            .set_json(&create);
        let res = send_test_request(req, app_ctx).await;

        ErrorResponse::assert(
            res,
            400,
            "BodyDeserializeError",
            "missing field `name` at line 1 column 195",
        )
        .await;

        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn create_missing_header() {
        with_temp_context(|app_ctx, _| async move {
        let create = json!({
            "description": "Foo".to_string(),
            "bounds": STRectangle::new(SpatialReference::epsg_4326(), 0., 0., 1., 1., 0, 1).unwrap(),
        });

        let req = test::TestRequest::post().uri("/project").set_json(&create);
        let res = send_test_request(req, app_ctx).await;

        ErrorResponse::assert(
            res,
            401,
            "Unauthorized",
            "Authorization error: Header with authorization token not provided.",
        )
        .await;

        })
        .await;
    }

    async fn list_test_helper(method: Method) -> ServiceResponse {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session();
            let _ = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::default()
                .method(method)
                .uri(&format!(
                    "/projects?{}",
                    &serde_urlencoded::to_string([
                        (
                            "permissions",
                            serde_json::json! {["Read", "Write", "Owner"]}
                                .to_string()
                                .as_str()
                        ),
                        // omitted ("filter", "None"),
                        ("order", "NameDesc"),
                        ("offset", "0"),
                        ("limit", "2"),
                    ])
                    .unwrap()
                ))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            send_test_request(req, app_ctx).await
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn list() {
        let res = list_test_helper(Method::GET).await;

        assert_eq!(res.status(), 200);

        let result: Vec<ProjectListing> = test::read_body_json(res).await;
        assert_eq!(result.len(), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn list_invalid_method() {
        check_allowed_http_methods(list_test_helper, &[Method::GET]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn list_missing_header() {
        with_temp_context(|app_ctx, _| async move {
            create_project_helper(&app_ctx).await;

            let req = test::TestRequest::get()
                .uri(&format!(
                    "/projects?{}",
                    &serde_urlencoded::to_string([
                        (
                            "permissions",
                            serde_json::json! {["Read", "Write", "Owner"]}
                                .to_string()
                                .as_str()
                        ),
                        // omitted ("filter", "None"),
                        ("order", "NameDesc"),
                        ("offset", "0"),
                        ("limit", "2"),
                    ])
                    .unwrap()
                ))
                .append_header((header::CONTENT_LENGTH, 0));
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                401,
                "Unauthorized",
                "Authorization error: Header with authorization token not provided.",
            )
            .await;
        })
        .await;
    }

    async fn load_test_helper(method: Method) -> ServiceResponse {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::default()
                .method(method)
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            send_test_request(req, app_ctx).await
        })
        .await
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load() {
        let res = load_test_helper(Method::GET).await;

        assert_eq!(res.status(), 200);

        let _project: Project = test::read_body_json(res).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load_invalid_method() {
        check_allowed_http_methods(
            load_test_helper,
            &[Method::GET, Method::PATCH, Method::DELETE],
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load_missing_header() {
        with_temp_context(|app_ctx, _| async move {
            let project = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::get()
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0));
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                401,
                "Unauthorized",
                "Authorization error: Header with authorization token not provided.",
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load_not_found() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session_id = ctx.session().id();

            let req = test::TestRequest::get()
                .uri("/project")
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session_id.to_string())));
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(res, 405, "MethodNotAllowed", "HTTP method not allowed.").await;
        })
        .await;
    }

    async fn update_test_helper(
        app_ctx: PostgresContext<NoTls>,
        method: Method,
    ) -> (SimpleSession, ProjectId, ServiceResponse) {
        let ctx = app_ctx.default_session_context().await.unwrap();

        let session = ctx.session().clone();
        let project = create_project_helper(&app_ctx).await;

        let update = update_project_helper(project);

        let req = test::TestRequest::default()
            .method(method)
            .uri(&format!("/project/{project}"))
            .append_header((header::CONTENT_LENGTH, 0))
            .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())))
            .set_json(&update);
        let res = send_test_request(req, app_ctx.clone()).await;

        (session, project, res)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn update() {
        with_temp_context(|app_ctx, _| async move {
            let (session, project, res) = update_test_helper(app_ctx.clone(), Method::PATCH).await;

            assert_eq!(res.status(), 200);

            let loaded = app_ctx
                .session_context(session)
                .db()
                .load_project(project)
                .await
                .unwrap();
            assert_eq!(loaded.name, "TestUpdate");
            assert_eq!(loaded.layers.len(), 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn update_invalid_body() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::patch()
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::CONTENT_TYPE, mime::APPLICATION_JSON))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())))
                .set_payload("no json");
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                400,
                "BodyDeserializeError",
                "expected ident at line 1 column 2",
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn update_missing_fields() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let update = json!({
                "name": "TestUpdate",
                "description": None::<String>,
                "layers": vec![LayerUpdate::UpdateOrInsert(ProjectLayer {
                    workflow: WorkflowId::new(),
                    name: "L1".to_string(),
                    visibility: Default::default(),
                    symbology: Symbology::Raster(RasterSymbology {
                        opacity: 1.0,
                        colorizer: Colorizer::Rgba,
                    })
                })],
                "bounds": None::<String>,
                "time_step": None::<String>,
            });

            let req = test::TestRequest::patch()
                .uri(&format!("/project/{project}"))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())))
                .set_json(&update);
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                400,
                "BodyDeserializeError",
                "missing field `id` at line 1 column 260",
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[allow(clippy::too_many_lines)]
    async fn update_layers() {
        async fn update_and_load_latest<Tls>(
            app_ctx: &PostgresContext<Tls>,
            session: &SimpleSession,
            project_id: ProjectId,
            update: UpdateProject,
        ) -> Vec<ProjectLayer>
        where
            Tls: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
            <Tls as MakeTlsConnect<Socket>>::Stream: Send + Sync,
            <Tls as MakeTlsConnect<Socket>>::TlsConnect: Send,
            <<Tls as MakeTlsConnect<Socket>>::TlsConnect as TlsConnect<Socket>>::Future: Send,
        {
            let req = test::TestRequest::patch()
                .uri(&format!("/project/{project_id}"))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())))
                .set_json(&update);
            let res = send_test_request(req, app_ctx.clone()).await;

            assert_eq!(res.status(), 200);

            let loaded = app_ctx
                .session_context(session.clone())
                .db()
                .load_project(project_id)
                .await
                .unwrap();

            loaded.layers
        }

        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let layer_1 = ProjectLayer {
                workflow: WorkflowId::new(),
                name: "L1".to_string(),
                visibility: LayerVisibility {
                    data: true,
                    legend: false,
                },
                symbology: Symbology::Raster(RasterSymbology {
                    opacity: 1.0,
                    colorizer: Colorizer::Rgba,
                }),
            };

            let layer_2 = ProjectLayer {
                workflow: WorkflowId::new(),
                name: "L2".to_string(),
                visibility: LayerVisibility {
                    data: false,
                    legend: true,
                },
                symbology: Symbology::Raster(RasterSymbology {
                    opacity: 1.0,
                    colorizer: Colorizer::Rgba,
                }),
            };

            // add first layer
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: Some(vec![LayerUpdate::UpdateOrInsert(layer_1.clone())]),
                        plots: None,
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![layer_1.clone()]
            );

            // add second layer
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: Some(vec![
                            LayerUpdate::None(Default::default()),
                            LayerUpdate::UpdateOrInsert(layer_2.clone())
                        ]),
                        plots: None,
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![layer_1.clone(), layer_2.clone()]
            );

            // remove first layer
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: Some(vec![
                            LayerUpdate::Delete(Default::default()),
                            LayerUpdate::None(Default::default()),
                        ]),
                        plots: None,
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![layer_2.clone()]
            );

            // clear layers
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: Some(vec![]),
                        plots: None,
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    #[allow(clippy::too_many_lines)]
    async fn update_plots() {
        async fn update_and_load_latest<Tls>(
            app_ctx: &PostgresContext<Tls>,
            session: &SimpleSession,
            project_id: ProjectId,
            update: UpdateProject,
        ) -> Vec<Plot>
        where
            Tls: MakeTlsConnect<Socket> + Clone + Send + Sync + 'static,
            <Tls as MakeTlsConnect<Socket>>::Stream: Send + Sync,
            <Tls as MakeTlsConnect<Socket>>::TlsConnect: Send,
            <<Tls as MakeTlsConnect<Socket>>::TlsConnect as TlsConnect<Socket>>::Future: Send,
        {
            let req = test::TestRequest::patch()
                .uri(&format!("/project/{project_id}"))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())))
                .set_json(&update);
            let res = send_test_request(req, app_ctx.clone()).await;

            assert_eq!(res.status(), 200);

            let loaded = app_ctx
                .session_context(session.clone())
                .db()
                .load_project(project_id)
                .await
                .unwrap();

            loaded.plots
        }

        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let plot_1 = Plot {
                workflow: WorkflowId::new(),
                name: "P1".to_string(),
            };

            let plot_2 = Plot {
                workflow: WorkflowId::new(),
                name: "P2".to_string(),
            };

            // add first plot
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: None,
                        plots: Some(vec![PlotUpdate::UpdateOrInsert(plot_1.clone())]),
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![plot_1.clone()]
            );

            // add second plot
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: None,
                        plots: Some(vec![
                            PlotUpdate::None(Default::default()),
                            PlotUpdate::UpdateOrInsert(plot_2.clone())
                        ]),
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![plot_1.clone(), plot_2.clone()]
            );

            // remove first plot
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: None,
                        plots: Some(vec![
                            PlotUpdate::Delete(Default::default()),
                            PlotUpdate::None(Default::default()),
                        ]),
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![plot_2.clone()]
            );

            // clear plots
            assert_eq!(
                update_and_load_latest(
                    &app_ctx,
                    &session,
                    project,
                    UpdateProject {
                        id: project,
                        name: None,
                        description: None,
                        layers: None,
                        plots: Some(vec![]),
                        bounds: None,
                        time_step: None,
                    }
                )
                .await,
                vec![]
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn delete() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::delete()
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            let res = send_test_request(req, app_ctx.clone()).await;

            assert_eq!(res.status(), 200);

            assert!(ctx.db().load_project(project).await.is_err());

            let req = test::TestRequest::delete()
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            let res = send_test_request(req, app_ctx).await;

            ErrorResponse::assert(
                res,
                400,
                "ProjectDeleteFailed",
                "Failed to delete the project.",
            )
            .await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load_version() {
        with_temp_context(|app_ctx, _| async move {
            let ctx = app_ctx.default_session_context().await.unwrap();

            let session = ctx.session().clone();
            let project = create_project_helper(&app_ctx).await;

            let db = ctx.db();

            db.update_project(update_project_helper(project))
                .await
                .unwrap();

            let req = test::TestRequest::get()
                .uri(&format!("/project/{project}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            let res = send_test_request(req, app_ctx.clone()).await;

            assert_eq!(res.status(), 200);

            let body: Project = test::read_body_json(res).await;

            assert_eq!(body.name, "TestUpdate");

            let versions = db.list_project_versions(project).await.unwrap();
            let version_id = versions.first().unwrap().id;

            let req = test::TestRequest::get()
                .uri(&format!("/project/{project}/{version_id}"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            let res = send_test_request(req, app_ctx).await;

            assert_eq!(res.status(), 200);

            let body: Project = test::read_body_json(res).await;
            assert_eq!(body.name, "TestUpdate");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn load_version_not_found() {
        with_temp_context(|app_ctx, _| async move {
            let session = app_ctx.default_session().await.unwrap();
            let project = create_project_helper(&app_ctx).await;

            let req = test::TestRequest::get()
                .uri(&format!(
                    "/project/{project}/00000000-0000-0000-0000-000000000000"
                ))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            let res = send_test_request(req, app_ctx).await;

            assert_eq!(res.status(), 400);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn list_versions() {
        with_temp_context(|app_ctx, _| async move {
            let session = app_ctx.default_session().await.unwrap();

            let project = create_project_helper(&app_ctx).await;

            let ctx = app_ctx.session_context(session.clone());

            let db = ctx.db();

            db.update_project(update_project_helper(project))
                .await
                .unwrap();

            let req = test::TestRequest::get()
                .uri(&format!("/project/{project}/versions"))
                .append_header((header::CONTENT_LENGTH, 0))
                .append_header((header::AUTHORIZATION, Bearer::new(session.id().to_string())));
            send_test_request(req, app_ctx).await;
        })
        .await;
    }
}
