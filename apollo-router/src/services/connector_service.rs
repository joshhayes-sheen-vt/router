//! Tower fetcher for fetch node execution.
use std::collections::HashMap;
use std::sync::Arc;
use std::task::Poll;

use apollo_compiler::validation::Valid;
use apollo_federation::sources::connect::Connector;
use futures::future::BoxFuture;
use indexmap::IndexMap;
use tower::BoxError;
use tower::ServiceExt;

use super::connect::BoxService;
use super::http::HttpClientServiceFactory;
use super::http::HttpRequest;
use super::new_service::ServiceFactory;
use crate::plugins::connectors::handle_responses::handle_responses;
use crate::plugins::connectors::make_requests::make_requests;
use crate::plugins::connectors::plugin::ConnectorContext;
use crate::plugins::subscription::SubscriptionConfig;
use crate::services::ConnectRequest;
use crate::services::ConnectResponse;
use crate::spec::Schema;

#[derive(Clone)]
pub(crate) struct ConnectorService {
    pub(crate) http_service_factory: Arc<IndexMap<String, HttpClientServiceFactory>>,
    pub(crate) schema: Arc<Schema>,
    pub(crate) _subgraph_schemas: Arc<HashMap<String, Arc<Valid<apollo_compiler::Schema>>>>,
    pub(crate) _subscription_config: Option<SubscriptionConfig>,
    pub(crate) connectors_by_service_name: Arc<IndexMap<Arc<str>, Connector>>,
}

impl tower::Service<ConnectRequest> for ConnectorService {
    type Response = ConnectResponse;
    type Error = BoxError;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn poll_ready(&mut self, _cx: &mut std::task::Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: ConnectRequest) -> Self::Future {
        let connector = self
            .connectors_by_service_name
            .get(&request.service_name)
            .cloned();

        let http_client_factory = self
            .http_service_factory
            .get(&request.service_name.to_string())
            .cloned();

        let schema = self.schema.supergraph_schema().clone();

        Box::pin(async move {
            let Some(connector) = connector else {
                return Err("no connector found".into());
            };

            let Some(http_client_factory) = http_client_factory else {
                return Err("no http client found".into());
            };

            execute(&http_client_factory, request, &connector, &schema).await
        })
    }
}

async fn execute(
    http_client_factory: &HttpClientServiceFactory,
    request: ConnectRequest,
    connector: &Connector,
    schema: &Valid<apollo_compiler::Schema>,
) -> Result<ConnectResponse, BoxError> {
    let context = request.context.clone();
    let context2 = context.clone();
    let original_subgraph_name = connector.id.subgraph_name.to_string();

    let mut debug = context
        .extensions()
        .with_lock(|mut lock| lock.remove::<ConnectorContext>());

    let requests = make_requests(request, connector, &mut debug).map_err(BoxError::from)?;

    let tasks = requests.into_iter().map(move |(req, key)| {
        let context = context.clone();
        let original_subgraph_name = original_subgraph_name.clone();
        async move {
            let context = context.clone();

            let client = http_client_factory.create(&original_subgraph_name);
            let req = HttpRequest {
                http_request: req,
                context,
            };
            let res = client.oneshot(req).await?;
            let mut res = res.http_response;
            let extensions = res.extensions_mut();
            extensions.insert(key);

            Ok::<_, BoxError>(res)
        }
    });

    let responses = futures::future::try_join_all(tasks)
        .await
        .map_err(BoxError::from)?;

    let result = handle_responses(responses, connector, &mut debug, schema)
        .await
        .map_err(BoxError::from);

    if let Some(debug) = debug {
        context2
            .extensions()
            .with_lock(|mut lock| lock.insert::<ConnectorContext>(debug));
    }

    result
}

#[derive(Clone)]
pub(crate) struct ConnectorServiceFactory {
    pub(crate) schema: Arc<Schema>,
    pub(crate) subgraph_schemas: Arc<HashMap<String, Arc<Valid<apollo_compiler::Schema>>>>,
    pub(crate) http_service_factory: Arc<IndexMap<String, HttpClientServiceFactory>>,
    pub(crate) subscription_config: Option<SubscriptionConfig>,
    pub(crate) connectors_by_service_name: Arc<IndexMap<Arc<str>, Connector>>,
}

impl ConnectorServiceFactory {
    pub(crate) fn new(
        schema: Arc<Schema>,
        subgraph_schemas: Arc<HashMap<String, Arc<Valid<apollo_compiler::Schema>>>>,
        http_service_factory: Arc<IndexMap<String, HttpClientServiceFactory>>,
        subscription_config: Option<SubscriptionConfig>,
        connectors_by_service_name: Arc<IndexMap<Arc<str>, Connector>>,
    ) -> Self {
        Self {
            http_service_factory,
            subgraph_schemas,
            schema,
            subscription_config,
            connectors_by_service_name,
        }
    }

    #[cfg(test)]
    pub(crate) fn empty(schema: Arc<Schema>) -> Self {
        Self {
            http_service_factory: Arc::new(Default::default()),
            subgraph_schemas: Default::default(),
            subscription_config: Default::default(),
            connectors_by_service_name: Default::default(),
            schema,
        }
    }
}

impl ServiceFactory<ConnectRequest> for ConnectorServiceFactory {
    type Service = BoxService;

    fn create(&self) -> Self::Service {
        ConnectorService {
            http_service_factory: self.http_service_factory.clone(),
            schema: self.schema.clone(),
            _subgraph_schemas: self.subgraph_schemas.clone(),
            _subscription_config: self.subscription_config.clone(),
            connectors_by_service_name: self.connectors_by_service_name.clone(),
        }
        .boxed()
    }
}
