use std::collections::BTreeMap;

use bytes::Bytes;
use tower::BoxError;

#[path = "../common.rs"]
mod common;

const FILE_CONFIG: &str = include_str!("../fixtures/file_upload.router.yaml");
const FILE_CONFIG_LARGE_LIMITS: &str = include_str!("../fixtures/file_upload_large.router.yaml");

/// Create a valid handler for the [helper::FileUploadTestServer].
macro_rules! make_handler {
    ($handler:expr) => {
        ::axum::Router::new().route("/", ::axum::routing::post($handler))
    };
}

#[tokio::test(flavor = "multi_thread")]
async fn it_uploads_a_single_file() -> Result<(), BoxError> {
    const FILE: &str = "Hello, world!";
    const FILE_NAME: &str = "example.txt";

    // Construct the parts of the multipart request as defined by the schema
    let request = helper::create_request(
        vec![FILE_NAME],
        vec![tokio_stream::once(Ok(Bytes::from_static(FILE.as_bytes())))],
    );

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::echo_single_file))
        .request(request)
        .build()
        .run_test(|response| {
            // Make sure that it succeeded
            assert_eq!(response.errors, Vec::new());

            // Make sure that we get back the file
            let upload_response = response
                .data
                .expect("empty GraphQL response from subgraph")
                .get("file0")
                .cloned()
                .take()
                .expect("invalid response from subgraph");
            let upload: helper::Upload = serde_json_bytes::value::from_value(upload_response)
                .expect("invalid upload response from subgraph");

            assert_eq!(upload.filename, Some(FILE_NAME.into()));
            assert_eq!(upload.body, Some(FILE.into()));
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_uploads_multiple_files() -> Result<(), BoxError> {
    let files = BTreeMap::from([
        ("example.txt", "Hello, world!"),
        ("example.json", r#"{ "message": "Hello, world!" }"#),
        (
            "example.yaml",
            "
            message: |
                Hello, world!
        ",
        ),
        (
            "example.toml",
            "
            [message]
            Hello, world!
        ",
        ),
    ]);

    // Construct the parts of the multipart request as defined by the schema for multiple files
    let request = helper::create_request(
        files.keys().cloned().collect::<Vec<_>>(),
        files
            .values()
            .map(|contents| tokio_stream::once(Ok(bytes::Bytes::from_static(contents.as_bytes()))))
            .collect::<Vec<_>>(),
    );

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::echo_files))
        .request(request)
        .build()
        .run_test(move |response| {
            assert_eq!(response.errors, Vec::new());

            let upload_response = response.data.expect("empty GraphQL response from subgraph");
            let upload_response = upload_response.as_object().unwrap();

            for (index, (&name, &file)) in files.iter().enumerate() {
                let response = upload_response
                    .get(format!("file{index}").as_str())
                    .expect("missing file in response");
                let response: helper::Upload = serde_json_bytes::from_value(response.to_owned())
                    .expect("invalid upload response");

                assert_eq!(response.filename, Some(name.into()));
                assert_eq!(response.body, Some(file.into()));
            }
        })
        .await
}

// TODO: This test takes ~3 minutes to complete. Possible solutions:
// - Lower the amount of data sent
// - Don't check that all of the bytes match
// TODO: Can we measure memory usage from within the test and ensure that it doesn't blow up?
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn it_uploads_a_massive_file() -> Result<(), BoxError> {
    // Upload a stream of 10GB
    const ONE_MB: usize = 1 * 1024 * 1024;
    const TEN_GB: usize = 10 * 1024 * ONE_MB;
    const FILE_CHUNK: [u8; ONE_MB] = [0xAA; ONE_MB];
    const CHUNK_COUNT: usize = TEN_GB / ONE_MB;

    // Upload a file that is 1GB in size of 0xAA
    let file =
        tokio_stream::iter((0..CHUNK_COUNT).map(|_| Ok(bytes::Bytes::from_static(&FILE_CHUNK))));

    // Construct the parts of the multipart request as defined by the schema
    let request = helper::create_request(vec!["fat.payload.bin"], vec![file]);

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG_LARGE_LIMITS)
        .handler(make_handler!(helper::verify_stream).with_state((TEN_GB, 0xAA)))
        .request(request)
        .build()
        .run_test(|response| {
            // We just want to make sure that the file was processed correctly
            assert_eq!(response.errors, Vec::new());
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_fails_upload_without_file() -> Result<(), BoxError> {
    // Construct a request with no attached files
    let request = helper::create_request(vec!["missing.txt"], Vec::<tokio_stream::Once<_>>::new());

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::always_fail))
        .request(request)
        .build()
        .run_test(|response| {
            // We should get back an error from the supergraph
            assert_eq!(
                response.errors.len(),
                1,
                "expected only a supergraph error but got {}: {:?}",
                response.errors.len(),
                response
                    .errors
                    .into_iter()
                    .map(|err| err.message)
                    .collect::<Vec<_>>()
            );

            // TODO: Verify that the error is the correct kind when error handling exists for the plugin
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_fails_with_file_count_limits() -> Result<(), BoxError> {
    // Create a list of files that passes the limit set in the config (5)
    let files = (0..100).map(|index| index.to_string());

    // Construct the parts of the multipart request as defined by the schema for multiple files
    let request = helper::create_request(
        files.clone().collect::<Vec<_>>(),
        files
            .map(|_| tokio_stream::once(Ok(bytes::Bytes::new())))
            .collect::<Vec<_>>(),
    );

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::always_fail))
        .request(request)
        .build()
        .run_test(|response| {
            // We should get back an error from the supergraph
            assert_eq!(
                response.errors.len(),
                1,
                "expected only a supergraph error but got {}: {:?}",
                response.errors.len(),
                response
                    .errors
                    .into_iter()
                    .map(|err| err.message)
                    .collect::<Vec<_>>()
            );

            // TODO: Check that error is correct once we have concrete error handling in the plugin
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_fails_with_file_size_limit() -> Result<(), BoxError> {
    // Create a file that passes the limit set in the config (512KB)
    const ONE_MB: usize = 1 * 1024 * 1024;
    const FILE_CHUNK: [u8; ONE_MB] = [0xAA; ONE_MB];

    // Construct the parts of the multipart request as defined by the schema for multiple files
    let request = helper::create_request(
        vec!["fat.payload.bin"],
        vec![tokio_stream::once(Ok(bytes::Bytes::from_static(
            &FILE_CHUNK,
        )))],
    );

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::always_fail))
        .request(request)
        .build()
        .run_test(|response| {
            // We should get back an error from the supergraph
            assert_eq!(
                response.errors.len(),
                1,
                "expected only a supergraph error but got {}: {:?}",
                response.errors.len(),
                response
                    .errors
                    .into_iter()
                    .map(|err| err.message)
                    .collect::<Vec<_>>()
            );

            // TODO: Check that error is correct once we have concrete error handling in the plugin
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_fails_invalid_multipart_order() -> Result<(), BoxError> {
    use reqwest::multipart::{Form, Part};

    // Construct a manual multipart request out of order
    // Note: The order is wrong, but the parts follow the spec
    let request = Form::new()
        .part(
            "map",
            Part::text(serde_json::json!({
                "0": ["variables.file0"]
            }).to_string()),
        ).part(
            "operations",
            Part::text(serde_json::json!({
                "query": "mutation ($file0: Upload) { singleUpload(file: $file0) { filename } }",
                "variables": {
                    "file0": null,
                },
            }).to_string())
        ).part("0", Part::text("file contents").file_name("file0"));

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::always_fail))
        .request(request)
        .build()
        .run_test(|response| {
            // We should get back an error from the supergraph
            assert_eq!(
                response.errors.len(),
                1,
                "expected only a supergraph error but got {}: {:?}",
                response.errors.len(),
                response
                    .errors
                    .into_iter()
                    .map(|err| err.message)
                    .collect::<Vec<_>>()
            );

            // TODO: Check that error is correct once we have concrete error handling in the plugin
        })
        .await
}

#[tokio::test(flavor = "multi_thread")]
async fn it_fails_invalid_file_order() -> Result<(), BoxError> {
    use reqwest::multipart::{Form, Part};

    // Construct a manual multipart request with files out of order
    let request = Form::new()
        .part(
            "operations",
            Part::text(
                serde_json::json!({
                    "query": "mutation ($file0: Upload, $file1: Upload) {
                        file0: singleUpload(file: $file0) { filename }
                        file1: singleUpload(file: $file1) { filename }
                    }",
                    "variables": {
                        "file0": null,
                        "file1": null,
                    },
                })
                .to_string(),
            ),
        )
        .part(
            "map",
            Part::text(
                serde_json::json!({
                    "0": ["variables.file0"],
                    "1": ["variables.file1"],
                })
                .to_string(),
            ),
        )
        .part("1", Part::text("file1 contents").file_name("file1"))
        .part("0", Part::text("file0 contents").file_name("file0"));

    // Run the test
    helper::FileUploadTestServer::builder()
        .config(FILE_CONFIG)
        .handler(make_handler!(helper::always_fail))
        .request(request)
        .build()
        .run_test(|response| {
            // We should get back an error from the supergraph
            assert_eq!(
                response.errors.len(),
                1,
                "expected only a supergraph error but got {}: {:?}",
                response.errors.len(),
                response
                    .errors
                    .into_iter()
                    .map(|err| err.message)
                    .collect::<Vec<_>>()
            );

            // TODO: Add check that error is correct when error handling is added to the plugin
        })
        .await
}

mod helper {
    use std::collections::BTreeMap;
    use std::net::IpAddr;
    use std::net::Ipv4Addr;
    use std::net::SocketAddr;
    use std::path::PathBuf;

    use axum::extract::State;
    use axum::response::IntoResponse;
    use axum::BoxError;
    use axum::Json;
    use axum::Router;
    use buildstructor::buildstructor;
    use futures::StreamExt;
    use http::header::CONTENT_TYPE;
    use http::Request;
    use http::StatusCode;
    use hyper::Body;
    use itertools::Itertools;
    use multer::Multipart;
    use reqwest::multipart::Form;
    use reqwest::multipart::Part;
    use serde::de::DeserializeOwned;
    use serde::Deserialize;
    use serde::Serialize;
    use serde_json::json;
    use serde_json::Value;
    use thiserror::Error;
    use tokio::net::TcpListener;
    use tokio_stream::Stream;

    use super::common::IntegrationTest;

    /// A helper server for testing multipart uploads.
    ///
    /// Note: This is a shim until wiremock supports two needed features:
    /// - [Streaming of the body](https://github.com/LukeMathWalker/wiremock-rs/pull/133)
    /// - [Async handlers for responders](https://github.com/LukeMathWalker/wiremock-rs/issues/84)
    ///
    /// Another alternative is to treat the handler (a [Router]) as a tower service and just [tower::ServiceExt::oneshot] it,
    /// but since the integration test is running the router as a normal process, we don't have a nice way to
    /// do so without running the HTTP server.
    pub struct FileUploadTestServer {
        config: &'static str,
        handler: Router,
        request: Form,
    }

    #[buildstructor]
    impl FileUploadTestServer {
        /// Create a test server with the supplied config, handler and request.
        ///
        /// Prefer the builder so that tests are more descriptive.
        ///
        /// See [make_handler] and [create_request].
        #[builder]
        pub fn new(config: &'static str, handler: Router, request: Form) -> Self {
            Self {
                config,
                handler,
                request,
            }
        }

        /// Runs a test, using the provided validation_fn to ensure that the response matches
        /// what is expected.
        pub async fn run_test(
            self,
            validation_fn: impl Fn(apollo_router::graphql::Response),
        ) -> Result<(), BoxError> {
            // Bind to the first available port
            let addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 0);
            let bound = TcpListener::bind(addr).await.unwrap();
            let bound_url = bound.local_addr().unwrap();
            let bound_url = format!("http://{bound_url}/");

            // Set up the router with the custom subgraph handler above
            let mut router = IntegrationTest::builder()
                .config(self.config)
                .subgraph_override("uploads", bound_url)
                .supergraph(PathBuf::from_iter([
                    "tests",
                    "fixtures",
                    "file_upload_supergraph.graphql",
                ]))
                .build()
                .await;

            router.start().await;
            router.assert_started().await;

            // Have a way to shutdown the server once the test finishes
            let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

            // Start the server using the tcp listener randomly assigned above
            let server = axum::Server::from_tcp(bound.into_std().unwrap())
                .unwrap()
                .serve(self.handler.into_make_service())
                .with_graceful_shutdown(async {
                    shutdown_rx.await.ok();
                });

            // Spawn the server in the background, controlled by the shutdown signal
            tokio::spawn(server);

            // Make the request and pass it into the validator callback
            let (_span, response) = router.execute_multipart_request(self.request).await;
            let response = serde_json::from_slice(&response.bytes().await?)?;
            validation_fn(response);

            // Kill the server and finish up
            shutdown_tx.send(()).unwrap();
            Ok(())
        }
    }

    /// A valid response from the file upload GraphQL schema
    #[derive(Serialize, Deserialize)]
    pub struct Upload {
        pub filename: Option<String>,
        pub body: Option<String>,
    }

    #[derive(Serialize, Deserialize)]
    pub struct Operation {
        // TODO: Can we verify that this is a valid graphql query?
        query: String,
        variables: BTreeMap<String, Value>,
    }

    #[derive(Error, Debug)]
    pub enum FileUploadError {
        #[error("bad headers in request: {0}")]
        BadHeaders(String),

        #[error("required field is empty: {0}")]
        EmptyField(String),

        #[error("invalid data received in multipart message: {0}")]
        InvalidData(#[from] serde_json::Error),

        #[error("invalid multipart request: {0}")]
        InvalidMultipart(#[from] multer::Error),

        #[error("expected a file with name '{0}' but found nothing")]
        MissingFile(String),

        #[error("expected a set of mappings but found nothing")]
        MissingMapping,

        #[error("expected request to fail, but subgraph received data")]
        ShouldHaveFailed,

        #[error("stream ended prematurely, expected {0} bytes but found {1}")]
        StreamEnded(usize, usize),

        #[error("stream returned unexpected data: expected {0} but found {1}")]
        UnexpectedData(u8, u8),

        #[error("unexpected field: expected '{0}' but got '{1:?}'")]
        UnexpectedField(String, Option<String>),

        #[error("expected end of stream but found a file")]
        UnexpectedFile,

        #[error("mismatch between supplied variables and mappings: {0} != {1}")]
        VariableMismatch(usize, usize),
    }

    impl IntoResponse for FileUploadError {
        fn into_response(self) -> axum::response::Response {
            let error = apollo_router::graphql::Error::builder()
                .message(self.to_string().as_str())
                .extension_code("FILE_UPLOAD_ERROR") // Without this line, the error cannot be built...
                .build();
            let response = apollo_router::graphql::Response::builder()
                .error(error)
                .build();

            (StatusCode::BAD_REQUEST, Json(json!(response))).into_response()
        }
    }

    /// Creates a valid multipart request out of a list of files
    pub fn create_request(
        names: Vec<impl Into<String>>,
        files: Vec<impl Stream<Item = hyper::Result<bytes::Bytes>> + Send + 'static>,
    ) -> reqwest::multipart::Form {
        // Each of the below text fields is generated from the supplied list of files, so we need to construct
        // each specially in order to match the shape defined in the test schema.
        // TODO: Can we use the [graphql_client::GraphQLQuery] trait to construct this for us?

        // Operations needs to contain file upload mutations with each file specified as an argument, followed
        // by a list of variables that map the subsequent parts of the multipart stream to the mutation placeholders.
        let operations = Part::text(
            serde_json::json!({
                "query": format!(
                    "mutation ({args}) {{ {queries} }}",
                    args = names.iter().enumerate().map(|(index, _)| format!("$file{index}: Upload")).join(", "),
                    queries = names.iter().enumerate().map(|(index, _)| format!("file{index}: singleUpload(file: $file{index}) {{ filename body }}")).join(" "),
                ),
                "variables": names.iter().enumerate().map(|(index, _)| (format!("file{index}"), serde_json::Value::Null)).collect::<BTreeMap<String, serde_json::Value>>(),
            })
            .to_string(),
        )
        .file_name("operations.graphql");

        // The mappings match the field names of the multipart stream to the graphql variables of the query
        let mappings = Part::text(
            serde_json::json!(names
                .iter()
                .enumerate()
                .map(|(index, _)| (index.to_string(), vec![format!("variables.file{index}")]))
                .collect::<BTreeMap<String, Vec<String>>>())
            .to_string(),
        )
        .file_name("mappings.json");

        // The rest of the request are the file streams
        let mut request = reqwest::multipart::Form::new()
            .part("operations", operations)
            .part("map", mappings);
        for (index, (file_name, file)) in names.into_iter().zip(files).enumerate() {
            let file_name: String = file_name.into();
            let part = Part::stream(hyper::Body::wrap_stream(file)).file_name(file_name);

            request = request.part(index.to_string(), part);
        }

        request
    }

    /// Handler that echos back the contents of the files that it receives
    pub async fn echo_single_file(
        mut request: Request<Body>,
    ) -> Result<Json<Value>, FileUploadError> {
        let (_, map, mut multipart) = decode_request(&mut request).await?;

        // Assert that we only have 1 file
        if map.len() > 1 {
            return Err(FileUploadError::UnexpectedFile);
        }

        let field_name: String = map
            .into_keys()
            .take(1)
            .next()
            .ok_or(FileUploadError::MissingMapping)?;

        // Extract the single expected file
        let upload = {
            let f = multipart
                .next_field()
                .await?
                .ok_or(FileUploadError::MissingFile(field_name.clone()))?;

            let file_name = f.file_name().unwrap_or(&field_name).to_string();
            let body = f.bytes().await?;

            Upload {
                filename: Some(file_name),
                body: Some(String::from_utf8_lossy(&body).to_string()),
            }
        };

        Ok(Json(json!({
            "data": {
                "file0": upload,
            }
        })))
    }

    /// Handler that echos back the contents of the files that it receives
    pub async fn echo_files(mut request: Request<Body>) -> Result<Json<Value>, FileUploadError> {
        let (operation, map, mut multipart) = decode_request(&mut request).await?;

        // Make sure that we have some mappings
        if map.is_empty() {
            return Err(FileUploadError::MissingMapping);
        }

        // Make sure that we have an equal number of mappings and variables
        if map.len() != operation.variables.len() {
            return Err(FileUploadError::VariableMismatch(
                map.len(),
                operation.variables.len(),
            ));
        }

        // Extract all of the files
        let mut files = BTreeMap::new();
        for (file_mapping, var_mapping) in map.into_iter() {
            let f = multipart
                .next_field()
                .await?
                .ok_or(FileUploadError::MissingFile(file_mapping.clone()))?;

            let field_name = f
                .name()
                .and_then(|name| (name == file_mapping).then_some(name))
                .ok_or(FileUploadError::UnexpectedField(
                    file_mapping,
                    f.name().map(String::from),
                ))?;
            let file_name = f.file_name().unwrap_or(field_name).to_string();
            let body = f.bytes().await?;

            // TODO: This is a bit hard-coded, but it should be enough for testing the whole plugin stack
            // The shape of the variables list for tests should always be ["variables.<NAME_OF_FILE>"]
            let var_name = var_mapping.get(0).ok_or(FileUploadError::MissingMapping)?;
            let var_name = var_name.split(".").skip(1).next().unwrap().to_string();

            files.insert(
                var_name,
                Upload {
                    filename: Some(file_name),
                    body: Some(String::from_utf8_lossy(&body).to_string()),
                },
            );
        }

        Ok(Json(json!({
            "data": files
        })))
    }

    /// A handler that always fails. Useful for tests that should not reach the subgraph at all.
    pub async fn always_fail(mut request: Request<Body>) -> Result<Json<Value>, FileUploadError> {
        // Consume the stream
        while request.body_mut().next().await.is_some() {}

        // Signal a failure
        Err(FileUploadError::ShouldHaveFailed)
    }

    /// Verifies that a file stream is present and goes to completion
    ///
    /// Note: Make sure to use a router with state (Expected stream length, expected value).
    pub async fn verify_stream(
        State((expected_length, byte_value)): State<(usize, u8)>,
        mut request: Request<Body>,
    ) -> Result<Json<Value>, FileUploadError> {
        let (_, _, mut multipart) = decode_request(&mut request).await?;

        let mut file = multipart
            .next_field()
            .await?
            .ok_or(FileUploadError::MissingFile("verification stream".into()))?;

        let mut count = 0;
        while let Some(chunk) = file.chunk().await? {
            // Keep track of how many bytes we've seen
            count += chunk.len();

            // Make sure that the bytes match what is expected
            let unexpected = match chunk.into_iter().all_equal_value() {
                Ok(value) => (value != byte_value).then_some(value),
                Err(Some((lhs, rhs))) => {
                    if lhs != byte_value {
                        Some(lhs)
                    } else {
                        Some(rhs)
                    }
                }
                Err(None) => None,
            };
            if let Some(unexpected_byte) = unexpected {
                return Err(FileUploadError::UnexpectedData(byte_value, unexpected_byte));
            }
        }

        // Make sure we've read the expected amount of bytes
        if count != expected_length {
            return Err(FileUploadError::StreamEnded(expected_length, count));
        }

        // A successful response means that the stream was valid
        Ok(Json(json!({
            "data": {
                "file0": Upload {
                    filename: Some("streamed".into()),
                    body: Some("successfully verified".into()),
                }
            }
        })))
    }

    /// Extract a field from a mulitpart request and validate it
    async fn extract_field<'short, 'a: 'short, T: DeserializeOwned>(
        mp: &'short mut Multipart<'a>,
        field_name: &str,
    ) -> Result<T, FileUploadError> {
        let field = mp
            .next_field()
            .await?
            .ok_or(FileUploadError::EmptyField(field_name.into()))?;

        // Verify that the field is named as expected
        if field.name() != Some(field_name) {
            return Err(FileUploadError::UnexpectedField(
                field_name.into(),
                field.name().map(String::from),
            ));
        }

        // Deserialize the response
        let bytes = field.bytes().await?;
        let result = serde_json::from_slice::<T>(&bytes)?;

        Ok(result)
    }

    /// Decodes a raw request into a GraphQL file upload multipart message.
    ///
    /// Note: This performs validation checks as well.
    /// Note: The order of the mapping must correspond with the order in the request, so
    /// we use a [BTreeMap] here to keep the order when traversing the list of files.
    async fn decode_request<'a>(
        request: &'a mut Request<Body>,
    ) -> Result<(Operation, BTreeMap<String, Vec<String>>, Multipart<'a>), FileUploadError> {
        let content_type = request
            .headers()
            .get(CONTENT_TYPE)
            .ok_or(FileUploadError::BadHeaders("missing content_type".into()))?;

        let boundary = multer::parse_boundary(content_type.to_str().map_err(|e| {
            FileUploadError::BadHeaders(format!("could not parse multipart boundary: {e}"))
        })?)?;

        let mut multipart = Multipart::new(request.body_mut(), boundary);

        // Extract the operations
        // TODO: Should we be streaming here?
        let operations: Operation = extract_field(&mut multipart, "operations").await?;
        let map: BTreeMap<String, Vec<String>> = extract_field(&mut multipart, "map").await?;

        Ok((operations, map, multipart))
    }
}