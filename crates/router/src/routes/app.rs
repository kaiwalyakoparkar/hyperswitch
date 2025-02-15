use std::{collections::HashMap, sync::Arc};

use actix_web::{web, Scope};
#[cfg(all(
    feature = "olap",
    any(feature = "v1", feature = "v2"),
    not(feature = "routing_v2")
))]
use api_models::routing::RoutingRetrieveQuery;
#[cfg(feature = "olap")]
use common_enums::TransactionType;
#[cfg(feature = "partial-auth")]
use common_utils::crypto::Blake3;
#[cfg(feature = "email")]
use external_services::email::{ses::AwsSes, EmailService};
use external_services::file_storage::FileStorageInterface;
use hyperswitch_interfaces::{
    encryption_interface::EncryptionManagementInterface,
    secrets_interface::secret_state::{RawSecret, SecuredSecret},
};
use router_env::tracing_actix_web::RequestId;
use scheduler::SchedulerInterface;
use storage_impl::{config::TenantConfig, redis::RedisStore, MockDb};
use tokio::sync::oneshot;

use self::settings::Tenant;
#[cfg(feature = "olap")]
use super::blocklist;
#[cfg(any(feature = "olap", feature = "oltp"))]
use super::currency;
#[cfg(feature = "dummy_connector")]
use super::dummy_connector::*;
#[cfg(any(feature = "olap", feature = "oltp"))]
use super::payment_methods::*;
#[cfg(feature = "payouts")]
use super::payout_link::*;
#[cfg(feature = "payouts")]
use super::payouts::*;
#[cfg(all(
    feature = "oltp",
    any(feature = "v1", feature = "v2"),
    not(feature = "customer_v2")
))]
use super::pm_auth;
#[cfg(feature = "oltp")]
use super::poll::retrieve_poll_status;
#[cfg(feature = "olap")]
use super::routing;
#[cfg(feature = "olap")]
use super::verification::{apple_pay_merchant_registration, retrieve_apple_pay_verified_domains};
#[cfg(feature = "olap")]
use super::{
    admin::*, api_keys::*, apple_pay_certificates_migration, connector_onboarding::*, disputes::*,
    files::*, gsm::*, payment_link::*, user::*, user_role::*, webhook_events::*,
};
use super::{cache::*, health::*};
#[cfg(any(feature = "olap", feature = "oltp"))]
use super::{configs::*, customers::*, mandates::*, payments::*, refunds::*};
#[cfg(feature = "oltp")]
use super::{ephemeral_key::*, webhooks::*};
#[cfg(feature = "olap")]
pub use crate::analytics::opensearch::OpenSearchClient;
#[cfg(feature = "olap")]
use crate::analytics::AnalyticsProvider;
#[cfg(feature = "partial-auth")]
use crate::errors::RouterResult;
#[cfg(all(feature = "frm", feature = "oltp"))]
use crate::routes::fraud_check as frm_routes;
#[cfg(all(feature = "recon", feature = "olap"))]
use crate::routes::recon as recon_routes;
pub use crate::{
    configs::settings,
    db::{CommonStorageInterface, GlobalStorageInterface, StorageImpl, StorageInterface},
    events::EventsHandler,
    routes::cards_info::card_iin_info,
    services::{get_cache_store, get_store},
};
use crate::{
    configs::{secrets_transformers, Settings},
    db::kafka_store::{KafkaStore, TenantID},
};

#[derive(Clone)]
pub struct ReqState {
    pub event_context: events::EventContext<crate::events::EventType, EventsHandler>,
}

#[derive(Clone)]
pub struct SessionState {
    pub store: Box<dyn StorageInterface>,
    /// Global store is used for global schema operations in tables like Users and Tenants
    pub global_store: Box<dyn GlobalStorageInterface>,
    pub conf: Arc<settings::Settings<RawSecret>>,
    pub api_client: Box<dyn crate::services::ApiClient>,
    pub event_handler: EventsHandler,
    #[cfg(feature = "email")]
    pub email_client: Arc<dyn EmailService>,
    #[cfg(feature = "olap")]
    pub pool: AnalyticsProvider,
    pub file_storage_client: Arc<dyn FileStorageInterface>,
    pub request_id: Option<RequestId>,
    pub base_url: String,
    pub tenant: Tenant,
    #[cfg(feature = "olap")]
    pub opensearch_client: Arc<OpenSearchClient>,
}
impl scheduler::SchedulerSessionState for SessionState {
    fn get_db(&self) -> Box<dyn SchedulerInterface> {
        self.store.get_scheduler_db()
    }
}
impl SessionState {
    pub fn get_req_state(&self) -> ReqState {
        ReqState {
            event_context: events::EventContext::new(self.event_handler.clone()),
        }
    }
}

pub trait SessionStateInfo {
    fn conf(&self) -> settings::Settings<RawSecret>;
    fn store(&self) -> Box<dyn StorageInterface>;
    fn event_handler(&self) -> EventsHandler;
    fn get_request_id(&self) -> Option<String>;
    fn add_request_id(&mut self, request_id: RequestId);
    #[cfg(feature = "partial-auth")]
    fn get_detached_auth(&self) -> RouterResult<(Blake3, &[u8])>;
    fn session_state(&self) -> SessionState;
}

impl SessionStateInfo for SessionState {
    fn store(&self) -> Box<dyn StorageInterface> {
        self.store.to_owned()
    }
    fn conf(&self) -> settings::Settings<RawSecret> {
        self.conf.as_ref().to_owned()
    }
    fn event_handler(&self) -> EventsHandler {
        self.event_handler.clone()
    }
    fn get_request_id(&self) -> Option<String> {
        self.api_client.get_request_id()
    }
    fn add_request_id(&mut self, request_id: RequestId) {
        self.api_client.add_request_id(request_id);
        self.store.add_request_id(request_id.to_string());
        self.request_id.replace(request_id);
    }

    #[cfg(feature = "partial-auth")]
    fn get_detached_auth(&self) -> RouterResult<(Blake3, &[u8])> {
        use error_stack::ResultExt;
        use hyperswitch_domain_models::errors::api_error_response as errors;
        use masking::prelude::PeekInterface as _;
        use router_env::logger;

        let output = CHECKSUM_KEY.get_or_try_init(|| {
            let conf = self.conf();
            let context = conf
                .api_keys
                .get_inner()
                .checksum_auth_context
                .peek()
                .clone();
            let key = conf.api_keys.get_inner().checksum_auth_key.peek();
            hex::decode(key).map(|key| {
                (
                    masking::StrongSecret::new(context),
                    masking::StrongSecret::new(key),
                )
            })
        });

        match output {
            Ok((context, key)) => Ok((Blake3::new(context.peek().clone()), key.peek())),
            Err(err) => {
                logger::error!("Failed to get checksum key");
                Err(err).change_context(errors::ApiErrorResponse::InternalServerError)
            }
        }
    }
    fn session_state(&self) -> SessionState {
        self.clone()
    }
}
#[derive(Clone)]
pub struct AppState {
    pub flow_name: String,
    pub global_store: Box<dyn GlobalStorageInterface>,
    pub stores: HashMap<String, Box<dyn StorageInterface>>,
    pub conf: Arc<settings::Settings<RawSecret>>,
    pub event_handler: EventsHandler,
    #[cfg(feature = "email")]
    pub email_client: Arc<dyn EmailService>,
    pub api_client: Box<dyn crate::services::ApiClient>,
    #[cfg(feature = "olap")]
    pub pools: HashMap<String, AnalyticsProvider>,
    #[cfg(feature = "olap")]
    pub opensearch_client: Arc<OpenSearchClient>,
    pub request_id: Option<RequestId>,
    pub file_storage_client: Arc<dyn FileStorageInterface>,
    pub encryption_client: Arc<dyn EncryptionManagementInterface>,
}
impl scheduler::SchedulerAppState for AppState {
    fn get_tenants(&self) -> Vec<String> {
        self.conf.multitenancy.get_tenant_names()
    }
}
pub trait AppStateInfo {
    fn conf(&self) -> settings::Settings<RawSecret>;
    fn event_handler(&self) -> EventsHandler;
    #[cfg(feature = "email")]
    fn email_client(&self) -> Arc<dyn EmailService>;
    fn add_request_id(&mut self, request_id: RequestId);
    fn add_flow_name(&mut self, flow_name: String);
    fn get_request_id(&self) -> Option<String>;
}

#[cfg(feature = "partial-auth")]
static CHECKSUM_KEY: once_cell::sync::OnceCell<(
    masking::StrongSecret<String>,
    masking::StrongSecret<Vec<u8>>,
)> = once_cell::sync::OnceCell::new();

impl AppStateInfo for AppState {
    fn conf(&self) -> settings::Settings<RawSecret> {
        self.conf.as_ref().to_owned()
    }
    #[cfg(feature = "email")]
    fn email_client(&self) -> Arc<dyn EmailService> {
        self.email_client.to_owned()
    }
    fn event_handler(&self) -> EventsHandler {
        self.event_handler.clone()
    }
    fn add_request_id(&mut self, request_id: RequestId) {
        self.api_client.add_request_id(request_id);
        self.request_id.replace(request_id);
    }

    fn add_flow_name(&mut self, flow_name: String) {
        self.api_client.add_flow_name(flow_name);
    }
    fn get_request_id(&self) -> Option<String> {
        self.api_client.get_request_id()
    }
}

impl AsRef<Self> for AppState {
    fn as_ref(&self) -> &Self {
        self
    }
}

#[cfg(feature = "email")]
pub async fn create_email_client(settings: &settings::Settings<RawSecret>) -> impl EmailService {
    match settings.email.active_email_client {
        external_services::email::AvailableEmailClients::SES => {
            AwsSes::create(&settings.email, settings.proxy.https_url.to_owned()).await
        }
    }
}

impl AppState {
    /// # Panics
    ///
    /// Panics if Store can't be created or JWE decryption fails
    pub async fn with_storage(
        conf: settings::Settings<SecuredSecret>,
        storage_impl: StorageImpl,
        shut_down_signal: oneshot::Sender<()>,
        api_client: Box<dyn crate::services::ApiClient>,
    ) -> Self {
        #[allow(clippy::expect_used)]
        let secret_management_client = conf
            .secrets_management
            .get_secret_management_client()
            .await
            .expect("Failed to create secret management client");

        let conf = Box::pin(secrets_transformers::fetch_raw_secrets(
            conf,
            &*secret_management_client,
        ))
        .await;

        #[allow(clippy::expect_used)]
        let encryption_client = conf
            .encryption_management
            .get_encryption_management_client()
            .await
            .expect("Failed to create encryption client");

        Box::pin(async move {
            let testable = storage_impl == StorageImpl::PostgresqlTest;
            #[allow(clippy::expect_used)]
            let event_handler = conf
                .events
                .get_event_handler()
                .await
                .expect("Failed to create event handler");

            #[allow(clippy::expect_used)]
            #[cfg(feature = "olap")]
            let opensearch_client = Arc::new(
                conf.opensearch
                    .get_opensearch_client()
                    .await
                    .expect("Failed to create opensearch client"),
            );

            #[cfg(feature = "olap")]
            let mut pools: HashMap<String, AnalyticsProvider> = HashMap::new();
            let mut stores = HashMap::new();
            #[allow(clippy::expect_used)]
            let cache_store = get_cache_store(&conf.clone(), shut_down_signal, testable)
                .await
                .expect("Failed to create store");
            let global_store: Box<dyn GlobalStorageInterface> = Self::get_store_interface(
                &storage_impl,
                &event_handler,
                &conf,
                &conf.multitenancy.global_tenant,
                Arc::clone(&cache_store),
                testable,
            )
            .await
            .get_global_storage_interface();
            for (tenant_name, tenant) in conf.clone().multitenancy.get_tenants() {
                let store: Box<dyn StorageInterface> = Self::get_store_interface(
                    &storage_impl,
                    &event_handler,
                    &conf,
                    tenant,
                    Arc::clone(&cache_store),
                    testable,
                )
                .await
                .get_storage_interface();
                stores.insert(tenant_name.clone(), store);
                #[cfg(feature = "olap")]
                let pool = AnalyticsProvider::from_conf(conf.analytics.get_inner(), tenant).await;
                #[cfg(feature = "olap")]
                pools.insert(tenant_name.clone(), pool);
            }

            #[cfg(feature = "email")]
            let email_client = Arc::new(create_email_client(&conf).await);

            let file_storage_client = conf.file_storage.get_file_storage_client().await;

            Self {
                flow_name: String::from("default"),
                stores,
                global_store,
                conf: Arc::new(conf),
                #[cfg(feature = "email")]
                email_client,
                api_client,
                event_handler,
                #[cfg(feature = "olap")]
                pools,
                #[cfg(feature = "olap")]
                opensearch_client,
                request_id: None,
                file_storage_client,
                encryption_client,
            }
        })
        .await
    }

    async fn get_store_interface(
        storage_impl: &StorageImpl,
        event_handler: &EventsHandler,
        conf: &Settings,
        tenant: &dyn TenantConfig,
        cache_store: Arc<RedisStore>,
        testable: bool,
    ) -> Box<dyn CommonStorageInterface> {
        match storage_impl {
            StorageImpl::Postgresql | StorageImpl::PostgresqlTest => match event_handler {
                EventsHandler::Kafka(kafka_client) => Box::new(
                    KafkaStore::new(
                        #[allow(clippy::expect_used)]
                        get_store(&conf.clone(), tenant, Arc::clone(&cache_store), testable)
                            .await
                            .expect("Failed to create store"),
                        kafka_client.clone(),
                        TenantID(tenant.get_schema().to_string()),
                        tenant,
                    )
                    .await,
                ),
                EventsHandler::Logs(_) => Box::new(
                    #[allow(clippy::expect_used)]
                    get_store(conf, tenant, Arc::clone(&cache_store), testable)
                        .await
                        .expect("Failed to create store"),
                ),
            },
            #[allow(clippy::expect_used)]
            StorageImpl::Mock => Box::new(
                MockDb::new(&conf.redis)
                    .await
                    .expect("Failed to create mock store"),
            ),
        }
    }

    pub async fn new(
        conf: settings::Settings<SecuredSecret>,
        shut_down_signal: oneshot::Sender<()>,
        api_client: Box<dyn crate::services::ApiClient>,
    ) -> Self {
        Box::pin(Self::with_storage(
            conf,
            StorageImpl::Postgresql,
            shut_down_signal,
            api_client,
        ))
        .await
    }

    pub fn get_session_state<E, F>(self: Arc<Self>, tenant: &str, err: F) -> Result<SessionState, E>
    where
        F: FnOnce() -> E + Copy,
    {
        let tenant_conf = self.conf.multitenancy.get_tenant(tenant).ok_or_else(err)?;
        let mut event_handler = self.event_handler.clone();
        event_handler.add_tenant(tenant_conf);
        Ok(SessionState {
            store: self.stores.get(tenant).ok_or_else(err)?.clone(),
            global_store: self.global_store.clone(),
            conf: Arc::clone(&self.conf),
            api_client: self.api_client.clone(),
            event_handler,
            #[cfg(feature = "olap")]
            pool: self.pools.get(tenant).ok_or_else(err)?.clone(),
            file_storage_client: self.file_storage_client.clone(),
            request_id: self.request_id,
            base_url: tenant_conf.base_url.clone(),
            tenant: tenant_conf.clone(),
            #[cfg(feature = "email")]
            email_client: Arc::clone(&self.email_client),
            #[cfg(feature = "olap")]
            opensearch_client: Arc::clone(&self.opensearch_client),
        })
    }
}

pub struct Health;

impl Health {
    pub fn server(state: AppState) -> Scope {
        web::scope("health")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::get().to(health)))
            .service(web::resource("/ready").route(web::get().to(deep_health_check)))
    }
}

#[cfg(feature = "dummy_connector")]
pub struct DummyConnector;

#[cfg(feature = "dummy_connector")]
impl DummyConnector {
    pub fn server(state: AppState) -> Scope {
        let mut routes_with_restricted_access = web::scope("");
        #[cfg(not(feature = "external_access_dc"))]
        {
            routes_with_restricted_access =
                routes_with_restricted_access.guard(actix_web::guard::Host("localhost"));
        }
        routes_with_restricted_access = routes_with_restricted_access
            .service(web::resource("/payment").route(web::post().to(dummy_connector_payment)))
            .service(
                web::resource("/payments/{payment_id}")
                    .route(web::get().to(dummy_connector_payment_data)),
            )
            .service(
                web::resource("/{payment_id}/refund").route(web::post().to(dummy_connector_refund)),
            )
            .service(
                web::resource("/refunds/{refund_id}")
                    .route(web::get().to(dummy_connector_refund_data)),
            );
        web::scope("/dummy-connector")
            .app_data(web::Data::new(state))
            .service(
                web::resource("/authorize/{attempt_id}")
                    .route(web::get().to(dummy_connector_authorize_payment)),
            )
            .service(
                web::resource("/complete/{attempt_id}")
                    .route(web::get().to(dummy_connector_complete_payment)),
            )
            .service(routes_with_restricted_access)
    }
}

pub struct Payments;

#[cfg(all(
    any(feature = "olap", feature = "oltp"),
    feature = "v2",
    feature = "payment_methods_v2",
    feature = "payment_v2"
))]
impl Payments {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/v2/payments").app_data(web::Data::new(state));
        route = route.service(
            web::resource("/{payment_id}/saved_payment_methods")
                .route(web::get().to(list_customer_payment_method_for_payment)),
        );

        route
    }
}

#[cfg(all(
    any(feature = "olap", feature = "oltp"),
    any(feature = "v2", feature = "v1"),
    not(feature = "payment_methods_v2"),
    not(feature = "payment_v2")
))]
impl Payments {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/payments").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            route = route
                .service(
                    web::resource("/list")
                        .route(web::get().to(payments_list))
                        .route(web::post().to(payments_list_by_filter)),
                )
                .service(web::resource("/filter").route(web::post().to(get_filters_for_payments)))
                .service(web::resource("/v2/filter").route(web::get().to(get_payment_filters)))
                .service(web::resource("/aggregate").route(web::get().to(get_payments_aggregates)))
                .service(
                    web::resource("/{payment_id}/manual-update")
                        .route(web::put().to(payments_manual_update)),
                )
        }
        #[cfg(feature = "oltp")]
        {
            route = route
                .service(web::resource("").route(web::post().to(payments_create)))
                .service(
                    web::resource("/session_tokens")
                        .route(web::post().to(payments_connector_session)),
                )
                .service(
                    web::resource("/sync")
                        .route(web::post().to(payments_retrieve_with_gateway_creds)),
                )
                .service(
                    web::resource("/{payment_id}")
                        .route(web::get().to(payments_retrieve))
                        .route(web::post().to(payments_update)),
                )
                .service(
                    web::resource("/{payment_id}/confirm").route(web::post().to(payments_confirm)),
                )
                .service(
                    web::resource("/{payment_id}/cancel").route(web::post().to(payments_cancel)),
                )
                .service(
                    web::resource("/{payment_id}/capture").route(web::post().to(payments_capture)),
                )
                .service(
                    web::resource("/{payment_id}/approve")
                        .route(web::post().to(payments_approve)),
                )
                .service(
                    web::resource("/{payment_id}/reject")
                        .route(web::post().to(payments_reject)),
                )
                .service(
                    web::resource("/redirect/{payment_id}/{merchant_id}/{attempt_id}")
                        .route(web::get().to(payments_start)),
                )
                .service(
                    web::resource(
                        "/{payment_id}/{merchant_id}/redirect/response/{connector}/{creds_identifier}",
                    )
                    .route(web::get().to(payments_redirect_response_with_creds_identifier)),
                )
                .service(
                    web::resource("/{payment_id}/{merchant_id}/redirect/response/{connector}")
                        .route(web::get().to(payments_redirect_response))
                        .route(web::post().to(payments_redirect_response))
                )
                .service(
                    web::resource("/{payment_id}/{merchant_id}/redirect/complete/{connector}")
                        .route(web::get().to(payments_complete_authorize_redirect))
                        .route(web::post().to(payments_complete_authorize_redirect)),
                )
                .service(
                    web::resource("/{payment_id}/complete_authorize")
                        .route(web::post().to(payments_complete_authorize)),
                )
                .service(
                    web::resource("/{payment_id}/incremental_authorization").route(web::post().to(payments_incremental_authorization)),
                )
                .service(
                    web::resource("/{payment_id}/{merchant_id}/authorize/{connector}").route(web::post().to(post_3ds_payments_authorize)),
                )
                .service(
                    web::resource("/{payment_id}/3ds/authentication").route(web::post().to(payments_external_authentication)),
                )
                .service(
                    web::resource("/{payment_id}/extended_card_info").route(web::get().to(retrieve_extended_card_info)),
                )
        }
        route
    }
}

#[cfg(any(feature = "olap", feature = "oltp"))]
pub struct Forex;

#[cfg(any(feature = "olap", feature = "oltp"))]
impl Forex {
    pub fn server(state: AppState) -> Scope {
        web::scope("/forex")
            .app_data(web::Data::new(state.clone()))
            .app_data(web::Data::new(state.clone()))
            .service(web::resource("/rates").route(web::get().to(currency::retrieve_forex)))
            .service(
                web::resource("/convert_from_minor").route(web::get().to(currency::convert_forex)),
            )
    }
}

#[cfg(feature = "olap")]
pub struct Routing;

#[cfg(all(feature = "olap", feature = "v2", feature = "routing_v2"))]
impl Routing {
    pub fn server(state: AppState) -> Scope {
        web::scope("/v2/routing_algorithm")
            .app_data(web::Data::new(state.clone()))
            .service(
                web::resource("").route(web::post().to(|state, req, payload| {
                    routing::routing_create_config(state, req, payload, &TransactionType::Payment)
                })),
            )
            .service(
                web::resource("/{algorithm_id}")
                    .route(web::get().to(routing::routing_retrieve_config)),
            )
    }
}
#[cfg(all(
    feature = "olap",
    any(feature = "v1", feature = "v2"),
    not(feature = "routing_v2")
))]
impl Routing {
    pub fn server(state: AppState) -> Scope {
        #[allow(unused_mut)]
        let mut route = web::scope("/routing")
            .app_data(web::Data::new(state.clone()))
            .service(
                web::resource("/active").route(web::get().to(|state, req, query_params| {
                    routing::routing_retrieve_linked_config(
                        state,
                        req,
                        query_params,
                        &TransactionType::Payment,
                    )
                })),
            )
            .service(
                web::resource("")
                    .route(
                        web::get().to(|state, req, path: web::Query<RoutingRetrieveQuery>| {
                            routing::list_routing_configs(
                                state,
                                req,
                                path,
                                &TransactionType::Payment,
                            )
                        }),
                    )
                    .route(web::post().to(|state, req, payload| {
                        routing::routing_create_config(
                            state,
                            req,
                            payload,
                            &TransactionType::Payment,
                        )
                    })),
            )
            .service(
                web::resource("/default")
                    .route(web::get().to(|state, req| {
                        routing::routing_retrieve_default_config(
                            state,
                            req,
                            &TransactionType::Payment,
                        )
                    }))
                    .route(web::post().to(|state, req, payload| {
                        routing::routing_update_default_config(
                            state,
                            req,
                            payload,
                            &TransactionType::Payment,
                        )
                    })),
            )
            .service(
                web::resource("/deactivate").route(web::post().to(|state, req, payload| {
                    routing::routing_unlink_config(state, req, payload, &TransactionType::Payment)
                })),
            )
            .service(
                web::resource("/decision")
                    .route(web::put().to(routing::upsert_decision_manager_config))
                    .route(web::get().to(routing::retrieve_decision_manager_config))
                    .route(web::delete().to(routing::delete_decision_manager_config)),
            )
            .service(
                web::resource("/decision/surcharge")
                    .route(web::put().to(routing::upsert_surcharge_decision_manager_config))
                    .route(web::get().to(routing::retrieve_surcharge_decision_manager_config))
                    .route(web::delete().to(routing::delete_surcharge_decision_manager_config)),
            )
            .service(
                web::resource("/default/profile/{profile_id}").route(web::post().to(
                    |state, req, path, payload| {
                        routing::routing_update_default_config_for_profile(
                            state,
                            req,
                            path,
                            payload,
                            &TransactionType::Payment,
                        )
                    },
                )),
            )
            .service(
                web::resource("/default/profile").route(web::get().to(|state, req| {
                    routing::routing_retrieve_default_config_for_profiles(
                        state,
                        req,
                        &TransactionType::Payment,
                    )
                })),
            );

        #[cfg(feature = "payouts")]
        {
            route = route
                .service(
                    web::resource("/payouts")
                        .route(web::get().to(
                            |state, req, path: web::Query<RoutingRetrieveQuery>| {
                                routing::list_routing_configs(
                                    state,
                                    req,
                                    path,
                                    &TransactionType::Payout,
                                )
                            },
                        ))
                        .route(web::post().to(|state, req, payload| {
                            routing::routing_create_config(
                                state,
                                req,
                                payload,
                                &TransactionType::Payout,
                            )
                        })),
                )
                .service(web::resource("/payouts/active").route(web::get().to(
                    |state, req, query_params| {
                        routing::routing_retrieve_linked_config(
                            state,
                            req,
                            query_params,
                            &TransactionType::Payout,
                        )
                    },
                )))
                .service(
                    web::resource("/payouts/default")
                        .route(web::get().to(|state, req| {
                            routing::routing_retrieve_default_config(
                                state,
                                req,
                                &TransactionType::Payout,
                            )
                        }))
                        .route(web::post().to(|state, req, payload| {
                            routing::routing_update_default_config(
                                state,
                                req,
                                payload,
                                &TransactionType::Payout,
                            )
                        })),
                )
                .service(
                    web::resource("/payouts/{algorithm_id}/activate").route(web::post().to(
                        |state, req, path| {
                            routing::routing_link_config(state, req, path, &TransactionType::Payout)
                        },
                    )),
                )
                .service(web::resource("/payouts/deactivate").route(web::post().to(
                    |state, req, payload| {
                        routing::routing_unlink_config(
                            state,
                            req,
                            payload,
                            &TransactionType::Payout,
                        )
                    },
                )))
                .service(
                    web::resource("/payouts/default/profile/{profile_id}").route(web::post().to(
                        |state, req, path, payload| {
                            routing::routing_update_default_config_for_profile(
                                state,
                                req,
                                path,
                                payload,
                                &TransactionType::Payout,
                            )
                        },
                    )),
                )
                .service(
                    web::resource("/payouts/default/profile").route(web::get().to(|state, req| {
                        routing::routing_retrieve_default_config_for_profiles(
                            state,
                            req,
                            &TransactionType::Payout,
                        )
                    })),
                );
        }

        route = route
            .service(
                web::resource("/{algorithm_id}")
                    .route(web::get().to(routing::routing_retrieve_config)),
            )
            .service(
                web::resource("/{algorithm_id}/activate").route(web::post().to(
                    |state, req, path| {
                        routing::routing_link_config(state, req, path, &TransactionType::Payment)
                    },
                )),
            );
        route
    }
}

pub struct Customers;

#[cfg(all(
    feature = "v2",
    feature = "customer_v2",
    any(feature = "olap", feature = "oltp")
))]
impl Customers {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/v2/customers").app_data(web::Data::new(state));
        #[cfg(all(feature = "oltp", feature = "v2", feature = "customer_v2"))]
        {
            route = route
                .service(web::resource("").route(web::post().to(customers_create)))
                .service(web::resource("/{id}").route(web::put().to(customers_update)))
        }
        #[cfg(all(feature = "oltp", feature = "v2", feature = "payment_methods_v2"))]
        {
            route = route.service(
                web::resource("/{customer_id}/saved_payment_methods")
                    .route(web::get().to(list_customer_payment_method_api)),
            );
        }
        route
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "customer_v2"),
    not(feature = "payment_methods_v2"),
    any(feature = "olap", feature = "oltp")
))]
impl Customers {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/customers").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            route = route
                .service(
                    web::resource("/{customer_id}/mandates")
                        .route(web::get().to(get_customer_mandates)),
                )
                .service(web::resource("/list").route(web::get().to(customers_list)))
        }

        #[cfg(feature = "oltp")]
        {
            route = route
                .service(web::resource("").route(web::post().to(customers_create)))
                .service(
                    web::resource("/payment_methods")
                        .route(web::get().to(list_customer_payment_method_api_client)),
                )
                .service(
                    web::resource("/{customer_id}/payment_methods")
                        .route(web::get().to(list_customer_payment_method_api)),
                )
                .service(
                    web::resource("/{customer_id}/payment_methods/{payment_method_id}/default")
                        .route(web::post().to(default_payment_method_set_api)),
                )
                .service(
                    web::resource("/{customer_id}")
                        .route(web::get().to(customers_retrieve))
                        .route(web::post().to(customers_update))
                        .route(web::delete().to(customers_delete)),
                )
        }

        route
    }
}
pub struct Refunds;

#[cfg(any(feature = "olap", feature = "oltp"))]
impl Refunds {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/refunds").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            route = route
                .service(web::resource("/list").route(web::post().to(refunds_list)))
                .service(web::resource("/filter").route(web::post().to(refunds_filter_list)))
                .service(web::resource("/v2/filter").route(web::get().to(get_refunds_filters)))
                .service(
                    web::resource("/{id}/manual-update")
                        .route(web::put().to(refunds_manual_update)),
                );
        }
        #[cfg(feature = "oltp")]
        {
            route = route
                .service(web::resource("").route(web::post().to(refunds_create)))
                .service(web::resource("/sync").route(web::post().to(refunds_retrieve_with_body)))
                .service(
                    web::resource("/{id}")
                        .route(web::get().to(refunds_retrieve))
                        .route(web::post().to(refunds_update)),
                );
        }
        route
    }
}

#[cfg(feature = "payouts")]
pub struct Payouts;

#[cfg(feature = "payouts")]
impl Payouts {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/payouts").app_data(web::Data::new(state));
        route = route.service(web::resource("/create").route(web::post().to(payouts_create)));

        #[cfg(feature = "olap")]
        {
            route = route
                .service(
                    web::resource("/list")
                        .route(web::get().to(payouts_list))
                        .route(web::post().to(payouts_list_by_filter)),
                )
                .service(
                    web::resource("/filter").route(web::post().to(payouts_list_available_filters)),
                );
        }
        route = route
            .service(
                web::resource("/{payout_id}")
                    .route(web::get().to(payouts_retrieve))
                    .route(web::put().to(payouts_update)),
            )
            .service(web::resource("/{payout_id}/confirm").route(web::post().to(payouts_confirm)))
            .service(web::resource("/{payout_id}/cancel").route(web::post().to(payouts_cancel)))
            .service(web::resource("/{payout_id}/fulfill").route(web::post().to(payouts_fulfill)));
        route
    }
}

pub struct PaymentMethods;

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    any(feature = "olap", feature = "oltp"),
    not(feature = "customer_v2")
))]
impl PaymentMethods {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/payment_methods").app_data(web::Data::new(state));
        #[cfg(feature = "olap")]
        {
            route = route.service(
                web::resource("/filter")
                    .route(web::get().to(list_countries_currencies_for_connector_payment_method)),
            );
        }
        #[cfg(feature = "oltp")]
        {
            route = route
                .service(
                    web::resource("")
                        .route(web::post().to(create_payment_method_api))
                        .route(web::get().to(list_payment_method_api)), // TODO : added for sdk compatibility for now, need to deprecate this later
                )
                .service(
                    web::resource("/migrate").route(web::post().to(migrate_payment_method_api)),
                )
                .service(
                    web::resource("/migrate-batch").route(web::post().to(migrate_payment_methods)),
                )
                .service(
                    web::resource("/collect").route(web::post().to(initiate_pm_collect_link_flow)),
                )
                .service(
                    web::resource("/collect/{merchant_id}/{collect_id}")
                        .route(web::get().to(render_pm_collect_link)),
                )
                .service(
                    web::resource("/{payment_method_id}")
                        .route(web::get().to(payment_method_retrieve_api))
                        .route(web::delete().to(payment_method_delete_api)),
                )
                .service(
                    web::resource("/{payment_method_id}/update")
                        .route(web::post().to(payment_method_update_api)),
                )
                .service(
                    web::resource("/{payment_method_id}/save")
                        .route(web::post().to(save_payment_method_api)),
                )
                .service(
                    web::resource("/auth/link").route(web::post().to(pm_auth::link_token_create)),
                )
                .service(
                    web::resource("/auth/exchange").route(web::post().to(pm_auth::exchange_token)),
                )
        }
        route
    }
}

#[cfg(all(feature = "olap", feature = "recon"))]
pub struct Recon;

#[cfg(all(feature = "olap", feature = "recon"))]
impl Recon {
    pub fn server(state: AppState) -> Scope {
        web::scope("/recon")
            .app_data(web::Data::new(state))
            .service(
                web::resource("/update_merchant")
                    .route(web::post().to(recon_routes::update_merchant)),
            )
            .service(web::resource("/token").route(web::get().to(recon_routes::get_recon_token)))
            .service(
                web::resource("/request").route(web::post().to(recon_routes::request_for_recon)),
            )
            .service(web::resource("/verify_token").route(web::get().to(verify_recon_token)))
    }
}

#[cfg(feature = "olap")]
pub struct Blocklist;

#[cfg(feature = "olap")]
impl Blocklist {
    pub fn server(state: AppState) -> Scope {
        web::scope("/blocklist")
            .app_data(web::Data::new(state))
            .service(
                web::resource("")
                    .route(web::get().to(blocklist::list_blocked_payment_methods))
                    .route(web::post().to(blocklist::add_entry_to_blocklist))
                    .route(web::delete().to(blocklist::remove_entry_from_blocklist)),
            )
            .service(
                web::resource("/toggle").route(web::post().to(blocklist::toggle_blocklist_guard)),
            )
    }
}

#[cfg(feature = "olap")]
pub struct Organization;
#[cfg(feature = "olap")]
impl Organization {
    pub fn server(state: AppState) -> Scope {
        web::scope("/organization")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(organization_create)))
            .service(
                web::resource("/{id}")
                    .route(web::get().to(organization_retrieve))
                    .route(web::put().to(organization_update)),
            )
    }
}

pub struct MerchantAccount;

#[cfg(all(feature = "v2", feature = "olap", feature = "merchant_account_v2"))]
impl MerchantAccount {
    pub fn server(state: AppState) -> Scope {
        web::scope("/v2/accounts")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(merchant_account_create)))
            .service(
                web::resource("/{id}")
                    .route(web::get().to(retrieve_merchant_account))
                    .route(web::put().to(update_merchant_account)),
            )
    }
}

#[cfg(all(
    feature = "olap",
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_account_v2")
))]
impl MerchantAccount {
    pub fn server(state: AppState) -> Scope {
        web::scope("/accounts")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(merchant_account_create)))
            .service(web::resource("/list").route(web::get().to(merchant_account_list)))
            .service(
                web::resource("/{id}/kv")
                    .route(web::post().to(merchant_account_toggle_kv))
                    .route(web::get().to(merchant_account_kv_status)),
            )
            .service(
                web::resource("/transfer").route(web::post().to(merchant_account_transfer_keys)),
            )
            .service(web::resource("/kv").route(web::post().to(merchant_account_toggle_all_kv)))
            .service(
                web::resource("/{id}")
                    .route(web::get().to(retrieve_merchant_account))
                    .route(web::post().to(update_merchant_account))
                    .route(web::delete().to(delete_merchant_account)),
            )
    }
}

pub struct MerchantConnectorAccount;

#[cfg(all(
    any(feature = "olap", feature = "oltp"),
    feature = "v2",
    feature = "merchant_connector_account_v2"
))]
impl MerchantConnectorAccount {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/v2/connector_accounts").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            use super::admin::*;

            route = route
                .service(web::resource("").route(web::post().to(connector_create)))
                .service(
                    web::resource("/{id}")
                        .route(web::put().to(connector_update))
                        .route(web::get().to(connector_retrieve))
                        .route(web::delete().to(connector_delete)),
                );
        }
        route
    }
}

#[cfg(all(
    any(feature = "olap", feature = "oltp"),
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_connector_account_v2")
))]
impl MerchantConnectorAccount {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/account").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            use super::admin::*;

            route = route
                .service(
                    web::resource("/connectors/verify")
                        .route(web::post().to(super::verify_connector::payment_connector_verify)),
                )
                .service(
                    web::resource("/{merchant_id}/connectors")
                        .route(web::post().to(connector_create))
                        .route(web::get().to(payment_connector_list)),
                )
                .service(
                    web::resource("/{merchant_id}/connectors/{merchant_connector_id}")
                        .route(web::get().to(connector_retrieve))
                        .route(web::post().to(connector_update))
                        .route(web::delete().to(connector_delete)),
                );
        }
        #[cfg(feature = "oltp")]
        {
            route = route.service(
                web::resource("/payment_methods").route(web::get().to(list_payment_method_api)),
            );
        }
        route
    }
}

pub struct EphemeralKey;

#[cfg(feature = "oltp")]
impl EphemeralKey {
    pub fn server(config: AppState) -> Scope {
        web::scope("/ephemeral_keys")
            .app_data(web::Data::new(config))
            .service(web::resource("").route(web::post().to(ephemeral_key_create)))
            .service(web::resource("/{id}").route(web::delete().to(ephemeral_key_delete)))
    }
}

pub struct Mandates;

#[cfg(any(feature = "olap", feature = "oltp"))]
impl Mandates {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/mandates").app_data(web::Data::new(state));

        #[cfg(feature = "olap")]
        {
            route =
                route.service(web::resource("/list").route(web::get().to(retrieve_mandates_list)));
            route = route.service(web::resource("/{id}").route(web::get().to(get_mandate)));
        }
        #[cfg(feature = "oltp")]
        {
            route =
                route.service(web::resource("/revoke/{id}").route(web::post().to(revoke_mandate)));
        }
        route
    }
}

pub struct Webhooks;

#[cfg(feature = "oltp")]
impl Webhooks {
    pub fn server(config: AppState) -> Scope {
        use api_models::webhooks as webhook_type;

        #[allow(unused_mut)]
        let mut route = web::scope("/webhooks")
            .app_data(web::Data::new(config))
            .service(
                web::resource("/{merchant_id}/{connector_id_or_name}")
                    .route(
                        web::post().to(receive_incoming_webhook::<webhook_type::OutgoingWebhook>),
                    )
                    .route(web::get().to(receive_incoming_webhook::<webhook_type::OutgoingWebhook>))
                    .route(
                        web::put().to(receive_incoming_webhook::<webhook_type::OutgoingWebhook>),
                    ),
            );

        #[cfg(feature = "frm")]
        {
            route = route.service(
                web::resource("/frm_fulfillment")
                    .route(web::post().to(frm_routes::frm_fulfillment)),
            );
        }

        route
    }
}

pub struct Configs;

#[cfg(any(feature = "olap", feature = "oltp"))]
impl Configs {
    pub fn server(config: AppState) -> Scope {
        web::scope("/configs")
            .app_data(web::Data::new(config))
            .service(web::resource("/").route(web::post().to(config_key_create)))
            .service(
                web::resource("/{key}")
                    .route(web::get().to(config_key_retrieve))
                    .route(web::post().to(config_key_update))
                    .route(web::delete().to(config_key_delete)),
            )
    }
}

pub struct ApplePayCertificatesMigration;

#[cfg(feature = "olap")]
impl ApplePayCertificatesMigration {
    pub fn server(state: AppState) -> Scope {
        web::scope("/apple_pay_certificates_migration")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(
                web::post().to(apple_pay_certificates_migration::apple_pay_certificates_migration),
            ))
    }
}

pub struct Poll;

#[cfg(feature = "oltp")]
impl Poll {
    pub fn server(config: AppState) -> Scope {
        web::scope("/poll")
            .app_data(web::Data::new(config))
            .service(web::resource("/status/{poll_id}").route(web::get().to(retrieve_poll_status)))
    }
}

pub struct ApiKeys;

#[cfg(feature = "olap")]
impl ApiKeys {
    pub fn server(state: AppState) -> Scope {
        web::scope("/api_keys/{merchant_id}")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(api_key_create)))
            .service(web::resource("/list").route(web::get().to(api_key_list)))
            .service(
                web::resource("/{key_id}")
                    .route(web::get().to(api_key_retrieve))
                    .route(web::post().to(api_key_update))
                    .route(web::delete().to(api_key_revoke)),
            )
    }
}

pub struct Disputes;

#[cfg(feature = "olap")]
impl Disputes {
    pub fn server(state: AppState) -> Scope {
        web::scope("/disputes")
            .app_data(web::Data::new(state))
            .service(web::resource("/list").route(web::get().to(retrieve_disputes_list)))
            .service(web::resource("/accept/{dispute_id}").route(web::post().to(accept_dispute)))
            .service(
                web::resource("/evidence")
                    .route(web::post().to(submit_dispute_evidence))
                    .route(web::put().to(attach_dispute_evidence))
                    .route(web::delete().to(delete_dispute_evidence)),
            )
            .service(
                web::resource("/evidence/{dispute_id}")
                    .route(web::get().to(retrieve_dispute_evidence)),
            )
            .service(web::resource("/{dispute_id}").route(web::get().to(retrieve_dispute)))
    }
}

pub struct Cards;

impl Cards {
    pub fn server(state: AppState) -> Scope {
        web::scope("/cards")
            .app_data(web::Data::new(state))
            .service(web::resource("/{bin}").route(web::get().to(card_iin_info)))
    }
}

pub struct Files;

#[cfg(feature = "olap")]
impl Files {
    pub fn server(state: AppState) -> Scope {
        web::scope("/files")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(files_create)))
            .service(
                web::resource("/{file_id}")
                    .route(web::delete().to(files_delete))
                    .route(web::get().to(files_retrieve)),
            )
    }
}

pub struct Cache;

impl Cache {
    pub fn server(state: AppState) -> Scope {
        web::scope("/cache")
            .app_data(web::Data::new(state))
            .service(web::resource("/invalidate/{key}").route(web::post().to(invalidate)))
    }
}

pub struct PaymentLink;
#[cfg(feature = "olap")]
impl PaymentLink {
    pub fn server(state: AppState) -> Scope {
        web::scope("/payment_link")
            .app_data(web::Data::new(state))
            .service(web::resource("/list").route(web::post().to(payments_link_list)))
            .service(
                web::resource("/{payment_link_id}").route(web::get().to(payment_link_retrieve)),
            )
            .service(
                web::resource("{merchant_id}/{payment_id}")
                    .route(web::get().to(initiate_payment_link)),
            )
            .service(
                web::resource("s/{merchant_id}/{payment_id}")
                    .route(web::get().to(initiate_secure_payment_link)),
            )
            .service(
                web::resource("status/{merchant_id}/{payment_id}")
                    .route(web::get().to(payment_link_status)),
            )
    }
}

#[cfg(feature = "payouts")]
pub struct PayoutLink;

#[cfg(feature = "payouts")]
impl PayoutLink {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/payout_link").app_data(web::Data::new(state));
        route = route.service(
            web::resource("/{merchant_id}/{payout_id}").route(web::get().to(render_payout_link)),
        );
        route
    }
}

pub struct BusinessProfile;
#[cfg(all(
    feature = "olap",
    feature = "v2",
    feature = "routing_v2",
    feature = "business_profile_v2"
))]
impl BusinessProfile {
    pub fn server(state: AppState) -> Scope {
        web::scope("/v2/profiles")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(business_profile_create)))
            .service(
                web::scope("/{profile_id}")
                    .service(
                        web::resource("")
                            .route(web::get().to(business_profile_retrieve))
                            .route(web::put().to(business_profile_update)),
                    )
                    .service(
                        web::resource("/fallback_routing")
                            .route(web::get().to(routing::routing_retrieve_default_config))
                            .route(web::post().to(routing::routing_update_default_config)),
                    )
                    .service(
                        web::resource("/activate_routing_algorithm").route(web::patch().to(
                            |state, req, path, payload| {
                                routing::routing_link_config(
                                    state,
                                    req,
                                    path,
                                    payload,
                                    &TransactionType::Payment,
                                )
                            },
                        )),
                    )
                    .service(
                        web::resource("/deactivate_routing_algorithm").route(web::patch().to(
                            |state, req, path| {
                                routing::routing_unlink_config(
                                    state,
                                    req,
                                    path,
                                    &TransactionType::Payment,
                                )
                            },
                        )),
                    )
                    .service(web::resource("/routing_algorithm").route(web::get().to(
                        |state, req, query_params, path| {
                            routing::routing_retrieve_linked_config(
                                state,
                                req,
                                query_params,
                                path,
                                &TransactionType::Payment,
                            )
                        },
                    ))),
            )
    }
}
#[cfg(all(
    feature = "olap",
    any(feature = "v1", feature = "v2"),
    not(any(feature = "routing_v2", feature = "business_profile_v2"))
))]
impl BusinessProfile {
    pub fn server(state: AppState) -> Scope {
        web::scope("/account/{account_id}/business_profile")
            .app_data(web::Data::new(state))
            .service(
                web::resource("")
                    .route(web::post().to(business_profile_create))
                    .route(web::get().to(business_profiles_list)),
            )
            .service(
                web::scope("/{profile_id}")
                    .service(
                        web::resource("")
                            .route(web::get().to(business_profile_retrieve))
                            .route(web::post().to(business_profile_update))
                            .route(web::delete().to(business_profile_delete)),
                    )
                    .service(
                        web::resource("/toggle_extended_card_info")
                            .route(web::post().to(toggle_extended_card_info)),
                    )
                    .service(
                        web::resource("/toggle_connector_agnostic_mit")
                            .route(web::post().to(toggle_connector_agnostic_mit)),
                    ),
            )
    }
}

pub struct Gsm;

#[cfg(feature = "olap")]
impl Gsm {
    pub fn server(state: AppState) -> Scope {
        web::scope("/gsm")
            .app_data(web::Data::new(state))
            .service(web::resource("").route(web::post().to(create_gsm_rule)))
            .service(web::resource("/get").route(web::post().to(get_gsm_rule)))
            .service(web::resource("/update").route(web::post().to(update_gsm_rule)))
            .service(web::resource("/delete").route(web::post().to(delete_gsm_rule)))
    }
}

#[cfg(feature = "olap")]
pub struct Verify;

#[cfg(feature = "olap")]
impl Verify {
    pub fn server(state: AppState) -> Scope {
        web::scope("/verify")
            .app_data(web::Data::new(state))
            .service(
                web::resource("/apple_pay/{merchant_id}")
                    .route(web::post().to(apple_pay_merchant_registration)),
            )
            .service(
                web::resource("/applepay_verified_domains")
                    .route(web::get().to(retrieve_apple_pay_verified_domains)),
            )
    }
}

pub struct User;

#[cfg(feature = "olap")]
impl User {
    pub fn server(state: AppState) -> Scope {
        let mut route = web::scope("/user").app_data(web::Data::new(state));

        route = route
            .service(web::resource("").route(web::get().to(get_user_details)))
            .service(web::resource("/v2/signin").route(web::post().to(user_signin)))
            // signin/signup with sso using openidconnect
            .service(web::resource("/oidc").route(web::post().to(sso_sign)))
            .service(web::resource("/signout").route(web::post().to(signout)))
            .service(web::resource("/rotate_password").route(web::post().to(rotate_password)))
            .service(web::resource("/change_password").route(web::post().to(change_password)))
            .service(web::resource("/internal_signup").route(web::post().to(internal_user_signup)))
            .service(web::resource("/switch_merchant").route(web::post().to(switch_merchant_id)))
            .service(
                web::resource("/create_merchant")
                    .route(web::post().to(user_merchant_account_create)),
            )
            // TODO: Remove this endpoint once migration to /merchants/list is done
            .service(web::resource("/switch/list").route(web::get().to(list_merchants_for_user)))
            .service(web::resource("/merchants/list").route(web::get().to(list_merchants_for_user)))
            // The route is utilized to select an invitation from a list of merchants in an intermediate state
            .service(
                web::resource("/merchants_select/list")
                    .route(web::get().to(list_merchants_for_user)),
            )
            .service(web::resource("/permission_info").route(web::get().to(get_authorization_info)))
            .service(web::resource("/module/list").route(web::get().to(get_role_information)))
            .service(web::resource("/update").route(web::post().to(update_user_account_details)))
            .service(
                web::resource("/data")
                    .route(web::get().to(get_multiple_dashboard_metadata))
                    .route(web::post().to(set_dashboard_metadata)),
            );

        route = route.service(
            web::scope("/key")
                .service(web::resource("/transfer").route(web::post().to(transfer_user_key))),
        );

        // Two factor auth routes
        route = route.service(
            web::scope("/2fa")
                .service(web::resource("").route(web::get().to(check_two_factor_auth_status)))
                .service(
                    web::scope("/totp")
                        .service(web::resource("/begin").route(web::get().to(totp_begin)))
                        .service(web::resource("/reset").route(web::get().to(totp_reset)))
                        .service(
                            web::resource("/verify")
                                .route(web::post().to(totp_verify))
                                .route(web::put().to(totp_update)),
                        ),
                )
                .service(
                    web::scope("/recovery_code")
                        .service(
                            web::resource("/verify").route(web::post().to(verify_recovery_code)),
                        )
                        .service(
                            web::resource("/generate")
                                .route(web::get().to(generate_recovery_codes)),
                        ),
                )
                .service(
                    web::resource("/terminate").route(web::get().to(terminate_two_factor_auth)),
                ),
        );

        route = route.service(
            web::scope("/auth")
                .service(
                    web::resource("")
                        .route(web::post().to(create_user_authentication_method))
                        .route(web::put().to(update_user_authentication_method)),
                )
                .service(
                    web::resource("/list").route(web::get().to(list_user_authentication_methods)),
                )
                .service(web::resource("/url").route(web::get().to(get_sso_auth_url)))
                .service(web::resource("/select").route(web::post().to(terminate_auth_select))),
        );

        #[cfg(feature = "email")]
        {
            route = route
                .service(web::resource("/from_email").route(web::post().to(user_from_email)))
                .service(
                    web::resource("/connect_account").route(web::post().to(user_connect_account)),
                )
                .service(web::resource("/forgot_password").route(web::post().to(forgot_password)))
                .service(web::resource("/reset_password").route(web::post().to(reset_password)))
                .service(
                    web::resource("/signup_with_merchant_id")
                        .route(web::post().to(user_signup_with_merchant_id)),
                )
                .service(web::resource("/v2/verify_email").route(web::post().to(verify_email)))
                .service(
                    web::resource("/verify_email_request")
                        .route(web::post().to(verify_email_request)),
                )
                .service(web::resource("/user/resend_invite").route(web::post().to(resend_invite)))
                .service(
                    web::resource("/accept_invite_from_email")
                        .route(web::post().to(accept_invite_from_email)),
                );
        }
        #[cfg(not(feature = "email"))]
        {
            route = route.service(web::resource("/signup").route(web::post().to(user_signup)))
        }

        // User management
        route = route.service(
            web::scope("/user")
                .service(web::resource("").route(web::get().to(get_user_role_details)))
                .service(
                    web::resource("/list").route(web::get().to(list_users_for_merchant_account)),
                )
                .service(
                    web::resource("/invite_multiple").route(web::post().to(invite_multiple_user)),
                )
                .service(
                    web::resource("/invite/accept")
                        .route(web::post().to(merchant_select))
                        .route(web::put().to(accept_invitation)),
                )
                .service(web::resource("/update_role").route(web::post().to(update_user_role)))
                .service(web::resource("/delete").route(web::delete().to(delete_user_role))),
        );

        // Role information
        route = route.service(
            web::scope("/role")
                .service(
                    web::resource("")
                        .route(web::get().to(get_role_from_token))
                        .route(web::post().to(create_role)),
                )
                .service(web::resource("/list").route(web::get().to(list_all_roles)))
                .service(
                    web::resource("/{role_id}")
                        .route(web::get().to(get_role))
                        .route(web::put().to(update_role)),
                ),
        );

        #[cfg(feature = "dummy_connector")]
        {
            route = route.service(
                web::resource("/sample_data")
                    .route(web::post().to(generate_sample_data))
                    .route(web::delete().to(delete_sample_data)),
            )
        }
        route
    }
}

pub struct ConnectorOnboarding;

#[cfg(feature = "olap")]
impl ConnectorOnboarding {
    pub fn server(state: AppState) -> Scope {
        web::scope("/connector_onboarding")
            .app_data(web::Data::new(state))
            .service(web::resource("/action_url").route(web::post().to(get_action_url)))
            .service(web::resource("/sync").route(web::post().to(sync_onboarding_status)))
            .service(web::resource("/reset_tracking_id").route(web::post().to(reset_tracking_id)))
    }
}

#[cfg(feature = "olap")]
pub struct WebhookEvents;

#[cfg(feature = "olap")]
impl WebhookEvents {
    pub fn server(config: AppState) -> Scope {
        web::scope("/events/{merchant_id}")
            .app_data(web::Data::new(config))
            .service(web::resource("").route(web::get().to(list_initial_webhook_delivery_attempts)))
            .service(
                web::scope("/{event_id}")
                    .service(
                        web::resource("attempts")
                            .route(web::get().to(list_webhook_delivery_attempts)),
                    )
                    .service(
                        web::resource("retry")
                            .route(web::post().to(retry_webhook_delivery_attempt)),
                    ),
            )
    }
}
