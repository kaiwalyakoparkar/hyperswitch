use std::str::FromStr;

use api_models::{
    admin::{self as admin_types},
    enums as api_enums, routing as routing_types,
};
use base64::Engine;
use common_utils::{
    date_time,
    ext_traits::{AsyncExt, Encode, ValueExt},
    id_type, pii, type_name,
    types::keymanager::{self as km_types, KeyManagerState},
};
use diesel_models::configs;
#[cfg(all(any(feature = "v1", feature = "v2"), feature = "olap"))]
use diesel_models::organization::OrganizationBridge;
use error_stack::{report, FutureExt, ResultExt};
use masking::{ExposeInterface, PeekInterface, Secret};
use pm_auth::{connector::plaid::transformers::PlaidAuthType, types as pm_auth_types};
use regex::Regex;
use router_env::metrics::add_attributes;
use uuid::Uuid;

#[cfg(all(
    feature = "v2",
    feature = "routing_v2",
    feature = "business_profile_v2"
))]
use crate::core::routing;
#[cfg(any(feature = "v1", feature = "v2"))]
use crate::types::transformers::ForeignFrom;
use crate::{
    consts::{self, BASE64_ENGINE},
    core::{
        encryption::transfer_encryption_key,
        errors::{self, RouterResponse, RouterResult, StorageErrorExt},
        payment_methods::{cards, transformers},
        payments::helpers,
        pm_auth::helpers::PaymentAuthConnectorDataExt,
        routing::helpers as routing_helpers,
        utils as core_utils,
    },
    db::StorageInterface,
    routes::{metrics, SessionState},
    services::{self, api as service_api, authentication, pm_auth as payment_initiation_service},
    types::{
        self,
        api::{self, admin},
        domain::{
            self,
            types::{self as domain_types, AsyncLift},
        },
        storage::{self, enums::MerchantStorageScheme},
        transformers::{ForeignInto, ForeignTryFrom, ForeignTryInto},
    },
    utils,
};
const IBAN_MAX_LENGTH: usize = 34;
const BACS_SORT_CODE_LENGTH: usize = 6;
const BACS_MAX_ACCOUNT_NUMBER_LENGTH: usize = 8;

#[inline]
pub fn create_merchant_publishable_key() -> String {
    format!(
        "pk_{}_{}",
        router_env::env::prefix_for_env(),
        Uuid::new_v4().simple()
    )
}

pub async fn insert_merchant_configs(
    db: &dyn StorageInterface,
    merchant_id: &id_type::MerchantId,
) -> RouterResult<()> {
    db.insert_config(configs::ConfigNew {
        key: merchant_id.get_requires_cvv_key(),
        config: "true".to_string(),
    })
    .await
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Error while setting requires_cvv config")?;

    db.insert_config(configs::ConfigNew {
        key: merchant_id.get_merchant_fingerprint_secret_key(),
        config: utils::generate_id(consts::FINGERPRINT_SECRET_LENGTH, "fs"),
    })
    .await
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Error while inserting merchant fingerprint secret")?;

    Ok(())
}

#[cfg(feature = "olap")]
fn add_publishable_key_to_decision_service(
    state: &SessionState,
    merchant_account: &domain::MerchantAccount,
) {
    let state = state.clone();
    let publishable_key = merchant_account.publishable_key.clone();
    let merchant_id = merchant_account.get_id().clone();

    authentication::decision::spawn_tracked_job(
        async move {
            authentication::decision::add_publishable_key(
                &state,
                publishable_key.into(),
                merchant_id,
                None,
            )
            .await
        },
        authentication::decision::ADD,
    );
}

#[cfg(feature = "olap")]
pub async fn create_organization(
    state: SessionState,
    req: api::OrganizationRequest,
) -> RouterResponse<api::OrganizationResponse> {
    let db_organization = ForeignFrom::foreign_from(req);
    state
        .store
        .insert_organization(db_organization)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Error when creating organization")
        .map(ForeignFrom::foreign_from)
        .map(service_api::ApplicationResponse::Json)
}

#[cfg(feature = "olap")]
pub async fn update_organization(
    state: SessionState,
    org_id: api::OrganizationId,
    req: api::OrganizationRequest,
) -> RouterResponse<api::OrganizationResponse> {
    let organization_update = diesel_models::organization::OrganizationUpdate::Update {
        organization_name: req.organization_name,
        organization_details: req.organization_details,
        metadata: req.metadata,
    };
    state
        .store
        .update_organization_by_org_id(&org_id.organization_id, organization_update)
        .await
        .to_not_found_response(errors::ApiErrorResponse::GenericNotFoundError {
            message: "organization with the given id does not exist".to_string(),
        })
        .attach_printable(format!(
            "Failed to update organization with organization_id: {:?}",
            org_id.organization_id
        ))
        .map(ForeignFrom::foreign_from)
        .map(service_api::ApplicationResponse::Json)
}

#[cfg(feature = "olap")]
pub async fn get_organization(
    state: SessionState,
    org_id: api::OrganizationId,
) -> RouterResponse<api::OrganizationResponse> {
    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "merchant_account_v2"),
        feature = "olap"
    ))]
    {
        CreateOrValidateOrganization::new(Some(org_id.organization_id))
            .create_or_validate(state.store.as_ref())
            .await
            .map(ForeignFrom::foreign_from)
            .map(service_api::ApplicationResponse::Json)
    }
    #[cfg(all(feature = "v2", feature = "merchant_account_v2", feature = "olap"))]
    {
        CreateOrValidateOrganization::new(org_id.organization_id)
            .create_or_validate(state.store.as_ref())
            .await
            .map(ForeignFrom::foreign_from)
            .map(service_api::ApplicationResponse::Json)
    }
}

#[cfg(feature = "olap")]
pub async fn create_merchant_account(
    state: SessionState,
    req: api::MerchantAccountCreate,
) -> RouterResponse<api::MerchantAccountResponse> {
    #[cfg(feature = "keymanager_create")]
    use common_utils::{keymanager, types::keymanager::EncryptionTransferRequest};

    let db = state.store.as_ref();

    let key = services::generate_aes256_key()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Unable to generate aes 256 key")?;

    let master_key = db.get_master_key();

    let key_manager_state = &(&state).into();
    let merchant_id = req.get_merchant_reference_id();
    let identifier = km_types::Identifier::Merchant(merchant_id.clone());
    #[cfg(feature = "keymanager_create")]
    {
        keymanager::transfer_key_to_key_manager(
            key_manager_state,
            EncryptionTransferRequest {
                identifier: identifier.clone(),
                key: BASE64_ENGINE.encode(key),
            },
        )
        .await
        .change_context(errors::ApiErrorResponse::DuplicateMerchantAccount)
        .attach_printable("Failed to insert key to KeyManager")?;
    }

    let key_store = domain::MerchantKeyStore {
        merchant_id: merchant_id.clone(),
        key: domain_types::crypto_operation(
            key_manager_state,
            type_name!(domain::MerchantKeyStore),
            domain_types::CryptoOperation::Encrypt(key.to_vec().into()),
            identifier.clone(),
            master_key,
        )
        .await
        .and_then(|val| val.try_into_operation())
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to decrypt data from key store")?,
        created_at: date_time::now(),
    };

    let domain_merchant_account = req
        .create_domain_model_from_request(&state, key_store.clone(), &merchant_id)
        .await?;
    let key_manager_state = &(&state).into();
    db.insert_merchant_key_store(
        key_manager_state,
        key_store.clone(),
        &master_key.to_vec().into(),
    )
    .await
    .to_duplicate_response(errors::ApiErrorResponse::DuplicateMerchantAccount)?;

    let merchant_account = db
        .insert_merchant(key_manager_state, domain_merchant_account, &key_store)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::DuplicateMerchantAccount)?;

    add_publishable_key_to_decision_service(&state, &merchant_account);

    insert_merchant_configs(db, &merchant_id).await?;

    Ok(service_api::ApplicationResponse::Json(
        api::MerchantAccountResponse::foreign_try_from(merchant_account)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed while generating response")?,
    ))
}

#[cfg(feature = "olap")]
#[async_trait::async_trait]
trait MerchantAccountCreateBridge {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key: domain::MerchantKeyStore,
        identifier: &id_type::MerchantId,
    ) -> RouterResult<domain::MerchantAccount>;
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    feature = "olap",
    not(feature = "merchant_account_v2")
))]
#[async_trait::async_trait]
impl MerchantAccountCreateBridge for api::MerchantAccountCreate {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        identifier: &id_type::MerchantId,
    ) -> RouterResult<domain::MerchantAccount> {
        let db = &*state.store;
        let publishable_key = create_merchant_publishable_key();

        let primary_business_details = self.get_primary_details_as_value().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "primary_business_details",
            },
        )?;

        let webhook_details = self.webhook_details.clone().map(ForeignInto::foreign_into);

        let pm_collect_link_config = self.get_pm_link_config_as_value().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "pm_collect_link_config",
            },
        )?;

        let merchant_details = self.get_merchant_details_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "merchant_details",
            },
        )?;

        self.parse_routing_algorithm()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "routing_algorithm",
            })
            .attach_printable("Invalid routing algorithm given")?;

        let metadata = self.get_metadata_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "metadata",
            },
        )?;

        // Get the enable payment response hash as a boolean, where the default value is true
        let enable_payment_response_hash = self.get_enable_payment_response_hash();

        let payment_response_hash_key = self.get_payment_response_hash_key();

        let parent_merchant_id = get_parent_merchant(
            state,
            self.sub_merchants_enabled,
            self.parent_merchant_id.as_ref(),
            &key_store,
        )
        .await?;

        let organization = CreateOrValidateOrganization::new(self.organization_id)
            .create_or_validate(db)
            .await?;

        let key = key_store.key.clone().into_inner();
        let key_manager_state = state.into();

        let merchant_account = async {
            Ok::<_, error_stack::Report<common_utils::errors::CryptoError>>(
                domain::MerchantAccountSetter {
                    merchant_id: identifier.clone(),
                    merchant_name: self
                        .merchant_name
                        .async_lift(|inner| async {
                            domain_types::crypto_operation(
                                &key_manager_state,
                                type_name!(domain::MerchantAccount),
                                domain_types::CryptoOperation::EncryptOptional(inner),
                                km_types::Identifier::Merchant(key_store.merchant_id.clone()),
                                key.peek(),
                            )
                            .await
                            .and_then(|val| val.try_into_optionaloperation())
                        })
                        .await?,
                    merchant_details: merchant_details
                        .async_lift(|inner| async {
                            domain_types::crypto_operation(
                                &key_manager_state,
                                type_name!(domain::MerchantAccount),
                                domain_types::CryptoOperation::EncryptOptional(inner),
                                km_types::Identifier::Merchant(key_store.merchant_id.clone()),
                                key.peek(),
                            )
                            .await
                            .and_then(|val| val.try_into_optionaloperation())
                        })
                        .await?,
                    return_url: self.return_url.map(|a| a.to_string()),
                    webhook_details,
                    routing_algorithm: Some(serde_json::json!({
                        "algorithm_id": null,
                        "timestamp": 0
                    })),
                    sub_merchants_enabled: self.sub_merchants_enabled,
                    parent_merchant_id,
                    enable_payment_response_hash,
                    payment_response_hash_key,
                    redirect_to_merchant_with_http_post: self
                        .redirect_to_merchant_with_http_post
                        .unwrap_or_default(),
                    publishable_key,
                    locker_id: self.locker_id,
                    metadata,
                    storage_scheme: MerchantStorageScheme::PostgresOnly,
                    primary_business_details,
                    created_at: date_time::now(),
                    modified_at: date_time::now(),
                    intent_fulfillment_time: None,
                    frm_routing_algorithm: self.frm_routing_algorithm,
                    #[cfg(feature = "payouts")]
                    payout_routing_algorithm: self.payout_routing_algorithm,
                    #[cfg(not(feature = "payouts"))]
                    payout_routing_algorithm: None,
                    organization_id: organization.get_organization_id(),
                    is_recon_enabled: false,
                    default_profile: None,
                    recon_status: diesel_models::enums::ReconStatus::NotRequested,
                    payment_link_config: None,
                    pm_collect_link_config,
                    version: hyperswitch_domain_models::consts::API_VERSION,
                },
            )
        }
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)?;

        let mut domain_merchant_account = domain::MerchantAccount::from(merchant_account);

        CreateBusinessProfile::new(self.primary_business_details.clone())
            .create_business_profiles(state, &mut domain_merchant_account, &key_store)
            .await?;

        Ok(domain_merchant_account)
    }
}

#[cfg(feature = "olap")]
enum CreateOrValidateOrganization {
    /// Creates a new organization
    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "merchant_account_v2")
    ))]
    Create,
    /// Validates if this organization exists in the records
    Validate {
        organization_id: id_type::OrganizationId,
    },
}

#[cfg(feature = "olap")]
impl CreateOrValidateOrganization {
    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "merchant_account_v2"),
        feature = "olap"
    ))]
    /// Create an action to either create or validate the given organization_id
    /// If organization_id is passed, then validate if this organization exists
    /// If not passed, create a new organization
    fn new(organization_id: Option<id_type::OrganizationId>) -> Self {
        if let Some(organization_id) = organization_id {
            Self::Validate { organization_id }
        } else {
            Self::Create
        }
    }

    #[cfg(all(feature = "v2", feature = "merchant_account_v2", feature = "olap"))]
    /// Create an action to validate the provided organization_id
    fn new(organization_id: id_type::OrganizationId) -> Self {
        Self::Validate { organization_id }
    }

    #[cfg(feature = "olap")]
    /// Apply the action, whether to create the organization or validate the given organization_id
    async fn create_or_validate(
        &self,
        db: &dyn StorageInterface,
    ) -> RouterResult<diesel_models::organization::Organization> {
        match self {
            #[cfg(all(
                any(feature = "v1", feature = "v2"),
                not(feature = "merchant_account_v2")
            ))]
            Self::Create => {
                let new_organization = api_models::organization::OrganizationNew::new(None);
                let db_organization = ForeignFrom::foreign_from(new_organization);
                db.insert_organization(db_organization)
                    .await
                    .to_duplicate_response(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Error when creating organization")
            }
            Self::Validate { organization_id } => db
                .find_organization_by_org_id(organization_id)
                .await
                .to_not_found_response(errors::ApiErrorResponse::GenericNotFoundError {
                    message: "organization with the given id does not exist".to_string(),
                }),
        }
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    feature = "olap",
    not(feature = "merchant_account_v2")
))]
enum CreateBusinessProfile {
    /// Create business profiles from primary business details
    /// If there is only one business profile created, then set this profile as default
    CreateFromPrimaryBusinessDetails {
        primary_business_details: Vec<admin_types::PrimaryBusinessDetails>,
    },
    /// Create a default business profile, set this as default profile
    CreateDefaultBusinessProfile,
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    feature = "olap",
    not(feature = "merchant_account_v2")
))]
impl CreateBusinessProfile {
    /// Create a new business profile action from the given information
    /// If primary business details exist, then create business profiles from them
    /// If primary business details are empty, then create default business profile
    fn new(primary_business_details: Option<Vec<admin_types::PrimaryBusinessDetails>>) -> Self {
        match primary_business_details {
            Some(primary_business_details) if !primary_business_details.is_empty() => {
                Self::CreateFromPrimaryBusinessDetails {
                    primary_business_details,
                }
            }
            _ => Self::CreateDefaultBusinessProfile,
        }
    }

    async fn create_business_profiles(
        &self,
        state: &SessionState,
        merchant_account: &mut domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<()> {
        match self {
            Self::CreateFromPrimaryBusinessDetails {
                primary_business_details,
            } => {
                let business_profiles = Self::create_business_profiles_for_each_business_details(
                    state,
                    merchant_account.clone(),
                    primary_business_details,
                    key_store,
                )
                .await?;

                // Update the default business profile in merchant account
                if business_profiles.len() == 1 {
                    merchant_account.default_profile = business_profiles
                        .first()
                        .map(|business_profile| business_profile.profile_id.clone())
                }
            }
            Self::CreateDefaultBusinessProfile => {
                let business_profile = self
                    .create_default_business_profile(state, merchant_account.clone(), key_store)
                    .await?;

                merchant_account.default_profile = Some(business_profile.profile_id);
            }
        }

        Ok(())
    }

    /// Create default business profile
    async fn create_default_business_profile(
        &self,
        state: &SessionState,
        merchant_account: domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfile> {
        let business_profile = create_and_insert_business_profile(
            state,
            api_models::admin::BusinessProfileCreate::default(),
            merchant_account.clone(),
            key_store,
        )
        .await?;

        Ok(business_profile)
    }

    /// Create business profile for each primary_business_details,
    /// If there is no default profile in merchant account and only one primary_business_detail
    /// is available, then create a default business profile.
    async fn create_business_profiles_for_each_business_details(
        state: &SessionState,
        merchant_account: domain::MerchantAccount,
        primary_business_details: &Vec<admin_types::PrimaryBusinessDetails>,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<Vec<domain::BusinessProfile>> {
        let mut business_profiles_vector = Vec::with_capacity(primary_business_details.len());

        // This must ideally be run in a transaction,
        // if there is an error in inserting some business profile, because of unique constraints
        // the whole query must be rolled back
        for business_profile in primary_business_details {
            let profile_name =
                format!("{}_{}", business_profile.country, business_profile.business);

            let business_profile_create_request = api_models::admin::BusinessProfileCreate {
                profile_name: Some(profile_name),
                ..Default::default()
            };

            create_and_insert_business_profile(
                state,
                business_profile_create_request,
                merchant_account.clone(),
                key_store,
            )
            .await
            .map_err(|business_profile_insert_error| {
                crate::logger::warn!(
                    "Business profile already exists {business_profile_insert_error:?}"
                );
            })
            .map(|business_profile| business_profiles_vector.push(business_profile))
            .ok();
        }

        Ok(business_profiles_vector)
    }
}

#[cfg(all(feature = "v2", feature = "merchant_account_v2", feature = "olap"))]
#[async_trait::async_trait]
impl MerchantAccountCreateBridge for api::MerchantAccountCreate {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        identifier: &id_type::MerchantId,
    ) -> RouterResult<domain::MerchantAccount> {
        let publishable_key = create_merchant_publishable_key();
        let db = &*state.store;

        let metadata = self.get_metadata_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "metadata",
            },
        )?;

        let merchant_details = self.get_merchant_details_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "merchant_details",
            },
        )?;

        let organization = CreateOrValidateOrganization::new(self.organization_id.clone())
            .create_or_validate(db)
            .await?;

        let key = key_store.key.into_inner();
        let id = identifier.to_owned();
        let key_manager_state = state.into();
        let identifier = km_types::Identifier::Merchant(id.clone());

        async {
            Ok::<_, error_stack::Report<common_utils::errors::CryptoError>>(
                domain::MerchantAccount::from(domain::MerchantAccountSetter {
                    id,
                    merchant_name: Some(
                        domain_types::crypto_operation(
                            &key_manager_state,
                            type_name!(domain::MerchantAccount),
                            domain_types::CryptoOperation::Encrypt(
                                self.merchant_name
                                    .map(|merchant_name| merchant_name.into_inner()),
                            ),
                            identifier.clone(),
                            key.peek(),
                        )
                        .await
                        .and_then(|val| val.try_into_operation())?,
                    ),
                    merchant_details: merchant_details
                        .async_lift(|inner| async {
                            domain_types::crypto_operation(
                                &key_manager_state,
                                type_name!(domain::MerchantAccount),
                                domain_types::CryptoOperation::EncryptOptional(inner),
                                identifier.clone(),
                                key.peek(),
                            )
                            .await
                            .and_then(|val| val.try_into_optionaloperation())
                        })
                        .await?,
                    publishable_key,
                    metadata,
                    storage_scheme: MerchantStorageScheme::PostgresOnly,
                    created_at: date_time::now(),
                    modified_at: date_time::now(),
                    organization_id: organization.get_organization_id(),
                    recon_status: diesel_models::enums::ReconStatus::NotRequested,
                }),
            )
        }
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("failed to encrypt merchant details")
    }
}

#[cfg(feature = "olap")]
pub async fn list_merchant_account(
    state: SessionState,
    req: api_models::admin::MerchantAccountListRequest,
) -> RouterResponse<Vec<api::MerchantAccountResponse>> {
    let merchant_accounts = state
        .store
        .list_merchant_accounts_by_organization_id(&(&state).into(), &req.organization_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_accounts = merchant_accounts
        .into_iter()
        .map(|merchant_account| {
            api::MerchantAccountResponse::foreign_try_from(merchant_account).change_context(
                errors::ApiErrorResponse::InvalidDataValue {
                    field_name: "merchant_account",
                },
            )
        })
        .collect::<Result<Vec<_>, _>>()?;

    Ok(services::ApplicationResponse::Json(merchant_accounts))
}

pub async fn get_merchant_account(
    state: SessionState,
    req: api::MerchantId,
    _profile_id: Option<String>,
) -> RouterResponse<api::MerchantAccountResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &req.merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, &req.merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    Ok(service_api::ApplicationResponse::Json(
        api::MerchantAccountResponse::foreign_try_from(merchant_account)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to construct response")?,
    ))
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_account_v2")
))]
/// For backwards compatibility, whenever new business labels are passed in
/// primary_business_details, create a business profile
pub async fn create_business_profile_from_business_labels(
    state: &SessionState,
    db: &dyn StorageInterface,
    key_store: &domain::MerchantKeyStore,
    merchant_id: &id_type::MerchantId,
    new_business_details: Vec<admin_types::PrimaryBusinessDetails>,
) -> RouterResult<()> {
    let key_manager_state = &state.into();
    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, merchant_id, key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let old_business_details = merchant_account
        .primary_business_details
        .clone()
        .parse_value::<Vec<admin_types::PrimaryBusinessDetails>>("PrimaryBusinessDetails")
        .change_context(errors::ApiErrorResponse::InvalidDataValue {
            field_name: "routing_algorithm",
        })
        .attach_printable("Invalid routing algorithm given")?;

    // find the diff between two vectors
    let business_profiles_to_create = new_business_details
        .into_iter()
        .filter(|business_details| !old_business_details.contains(business_details))
        .collect::<Vec<_>>();

    for business_profile in business_profiles_to_create {
        let profile_name = format!("{}_{}", business_profile.country, business_profile.business);

        let business_profile_create_request = admin_types::BusinessProfileCreate {
            profile_name: Some(profile_name),
            ..Default::default()
        };

        let business_profile_create_result = create_and_insert_business_profile(
            state,
            business_profile_create_request,
            merchant_account.clone(),
            key_store,
        )
        .await
        .map_err(|business_profile_insert_error| {
            // If there is any duplicate error, we need not take any action
            crate::logger::warn!(
                "Business profile already exists {business_profile_insert_error:?}"
            );
        });

        // If a business_profile is created, then unset the default profile
        if business_profile_create_result.is_ok() && merchant_account.default_profile.is_some() {
            let unset_default_profile = domain::MerchantAccountUpdate::UnsetDefaultProfile;
            db.update_merchant(
                key_manager_state,
                merchant_account.clone(),
                unset_default_profile,
                key_store,
            )
            .await
            .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
        }
    }

    Ok(())
}

#[cfg(any(feature = "v1", feature = "v2", feature = "olap"))]
#[async_trait::async_trait]
trait MerchantAccountUpdateBridge {
    async fn get_update_merchant_object(
        self,
        state: &SessionState,
        merchant_id: &id_type::MerchantId,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<storage::MerchantAccountUpdate>;
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_account_v2")
))]
#[async_trait::async_trait]
impl MerchantAccountUpdateBridge for api::MerchantAccountUpdate {
    async fn get_update_merchant_object(
        self,
        state: &SessionState,
        merchant_id: &id_type::MerchantId,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<storage::MerchantAccountUpdate> {
        use common_utils::ext_traits::ConfigExt;

        let key_manager_state = &state.into();
        let key = key_store.key.get_inner().peek();

        let db = state.store.as_ref();

        let primary_business_details = self.get_primary_details_as_value().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "primary_business_details",
            },
        )?;

        let pm_collect_link_config = self.get_pm_link_config_as_value().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "pm_collect_link_config",
            },
        )?;

        let merchant_details = self.get_merchant_details_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "merchant_details",
            },
        )?;

        self.parse_routing_algorithm().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "routing_algorithm",
            },
        )?;

        let webhook_details = self.webhook_details.map(ForeignInto::foreign_into);

        let parent_merchant_id = get_parent_merchant(
            state,
            self.sub_merchants_enabled,
            self.parent_merchant_id.as_ref(),
            key_store,
        )
        .await?;

        // This supports changing the business profile by passing in the profile_id
        let business_profile_id_update = if let Some(ref profile_id) = self.default_profile {
            if !profile_id.is_empty_after_trim() {
                // Validate whether profile_id passed in request is valid and is linked to the merchant
                core_utils::validate_and_get_business_profile(
                    state.store.as_ref(),
                    key_manager_state,
                    key_store,
                    Some(profile_id),
                    merchant_id,
                )
                .await?
                .map(|business_profile| Some(business_profile.profile_id))
            } else {
                // If empty, Update profile_id to None in the database
                Some(None)
            }
        } else {
            None
        };

        #[cfg(any(feature = "v1", feature = "v2"))]
        // In order to support backwards compatibility, if a business_labels are passed in the update
        // call, then create new business_profiles with the profile_name as business_label
        self.primary_business_details
            .clone()
            .async_map(|primary_business_details| async {
                let _ = create_business_profile_from_business_labels(
                    state,
                    db,
                    key_store,
                    merchant_id,
                    primary_business_details,
                )
                .await;
            })
            .await;

        let identifier = km_types::Identifier::Merchant(key_store.merchant_id.clone());
        Ok(storage::MerchantAccountUpdate::Update {
            merchant_name: self
                .merchant_name
                .map(Secret::new)
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        identifier.clone(),
                        key,
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt merchant name")?,
            merchant_details: merchant_details
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        identifier.clone(),
                        key,
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt merchant details")?,
            return_url: self.return_url.map(|a| a.to_string()),
            webhook_details,
            sub_merchants_enabled: self.sub_merchants_enabled,
            parent_merchant_id,
            enable_payment_response_hash: self.enable_payment_response_hash,
            payment_response_hash_key: self.payment_response_hash_key,
            redirect_to_merchant_with_http_post: self.redirect_to_merchant_with_http_post,
            locker_id: self.locker_id,
            metadata: self.metadata,
            publishable_key: None,
            primary_business_details,
            frm_routing_algorithm: self.frm_routing_algorithm,
            intent_fulfillment_time: None,
            #[cfg(feature = "payouts")]
            payout_routing_algorithm: self.payout_routing_algorithm,
            #[cfg(not(feature = "payouts"))]
            payout_routing_algorithm: None,
            default_profile: business_profile_id_update,
            payment_link_config: None,
            pm_collect_link_config,
            routing_algorithm: self.routing_algorithm,
        })
    }
}

#[cfg(all(any(feature = "v1", feature = "v2"), feature = "merchant_account_v2",))]
#[async_trait::async_trait]
impl MerchantAccountUpdateBridge for api::MerchantAccountUpdate {
    async fn get_update_merchant_object(
        self,
        state: &SessionState,
        _merchant_id: &id_type::MerchantId,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<storage::MerchantAccountUpdate> {
        let key_manager_state = &state.into();
        let key = key_store.key.get_inner().peek();

        let merchant_details = self.get_merchant_details_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "merchant_details",
            },
        )?;

        let metadata = self.get_metadata_as_secret().change_context(
            errors::ApiErrorResponse::InvalidDataValue {
                field_name: "metadata",
            },
        )?;

        let identifier = km_types::Identifier::Merchant(key_store.merchant_id.clone());
        Ok(storage::MerchantAccountUpdate::Update {
            merchant_name: self
                .merchant_name
                .map(Secret::new)
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        identifier.clone(),
                        key,
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt merchant name")?,
            merchant_details: merchant_details
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        identifier.clone(),
                        key,
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt merchant details")?,
            metadata,
            publishable_key: None,
        })
    }
}

pub async fn merchant_account_update(
    state: SessionState,
    merchant_id: &id_type::MerchantId,
    _profile_id: Option<String>,
    req: api::MerchantAccountUpdate,
) -> RouterResponse<api::MerchantAccountResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_account_storage_object = req
        .get_update_merchant_object(&state, merchant_id, &key_store)
        .await
        .attach_printable("Failed to create merchant account update object")?;

    let response = db
        .update_specific_fields_in_merchant(
            key_manager_state,
            merchant_id,
            merchant_account_storage_object,
            &key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    Ok(service_api::ApplicationResponse::Json(
        api::MerchantAccountResponse::foreign_try_from(response)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed while generating response")?,
    ))
}

pub async fn merchant_account_delete(
    state: SessionState,
    merchant_id: id_type::MerchantId,
) -> RouterResponse<api::MerchantAccountDeleteResponse> {
    let mut is_deleted = false;
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let merchant_key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &state.store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &merchant_key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let is_merchant_account_deleted = db
        .delete_merchant_account_by_merchant_id(&merchant_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
    if is_merchant_account_deleted {
        let is_merchant_key_store_deleted = db
            .delete_merchant_key_store_by_merchant_id(&merchant_id)
            .await
            .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
        is_deleted = is_merchant_account_deleted && is_merchant_key_store_deleted;
    }

    let state = state.clone();
    authentication::decision::spawn_tracked_job(
        async move {
            authentication::decision::revoke_api_key(
                &state,
                merchant_account.publishable_key.into(),
            )
            .await
        },
        authentication::decision::REVOKE,
    );

    match db
        .delete_config_by_key(merchant_id.get_requires_cvv_key().as_str())
        .await
    {
        Ok(_) => Ok::<_, errors::ApiErrorResponse>(()),
        Err(err) => {
            if err.current_context().is_db_not_found() {
                crate::logger::error!("requires_cvv config not found in db: {err:?}");
                Ok(())
            } else {
                Err(err
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable("Failed while deleting requires_cvv config"))?
            }
        }
    }
    .ok();

    let response = api::MerchantAccountDeleteResponse {
        merchant_id,
        deleted: is_deleted,
    };
    Ok(service_api::ApplicationResponse::Json(response))
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_account_v2")
))]
async fn get_parent_merchant(
    state: &SessionState,
    sub_merchants_enabled: Option<bool>,
    parent_merchant: Option<&id_type::MerchantId>,
    key_store: &domain::MerchantKeyStore,
) -> RouterResult<Option<id_type::MerchantId>> {
    Ok(match sub_merchants_enabled {
        Some(true) => {
            Some(
                parent_merchant.ok_or_else(|| {
                    report!(errors::ValidationError::MissingRequiredField {
                        field_name: "parent_merchant_id".to_string()
                    })
                    .change_context(errors::ApiErrorResponse::PreconditionFailed {
                        message: "If `sub_merchants_enabled` is `true`, then `parent_merchant_id` is mandatory".to_string(),
                    })
                })
                .map(|id| validate_merchant_id(state, id,key_store).change_context(
                    errors::ApiErrorResponse::InvalidDataValue { field_name: "parent_merchant_id" }
                ))?
                .await?
                .get_id().to_owned()
            )
        }
        _ => None,
    })
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_account_v2")
))]
async fn validate_merchant_id(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    key_store: &domain::MerchantKeyStore,
) -> RouterResult<domain::MerchantAccount> {
    let db = &*state.store;
    db.find_merchant_account_by_merchant_id(&state.into(), merchant_id, key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)
}

struct ConnectorAuthTypeAndMetadataValidation<'a> {
    connector_name: &'a api_models::enums::Connector,
    auth_type: &'a types::ConnectorAuthType,
    connector_meta_data: &'a Option<pii::SecretSerdeValue>,
}

impl<'a> ConnectorAuthTypeAndMetadataValidation<'a> {
    pub fn validate_auth_and_metadata_type(
        &self,
    ) -> Result<(), error_stack::Report<errors::ApiErrorResponse>> {
        let connector_auth_type_validation = ConnectorAuthTypeValidation {
            auth_type: self.auth_type,
        };
        connector_auth_type_validation.validate_connector_auth_type()?;
        self.validate_auth_and_metadata_type_with_connector()
            .map_err(|err| match *err.current_context() {
                errors::ConnectorError::InvalidConnectorName => {
                    err.change_context(errors::ApiErrorResponse::InvalidRequestData {
                        message: "The connector name is invalid".to_string(),
                    })
                }
                errors::ConnectorError::InvalidConnectorConfig { config: field_name } => err
                    .change_context(errors::ApiErrorResponse::InvalidRequestData {
                        message: format!("The {} is invalid", field_name),
                    }),
                errors::ConnectorError::FailedToObtainAuthType => {
                    err.change_context(errors::ApiErrorResponse::InvalidRequestData {
                        message: "The auth type is invalid for the connector".to_string(),
                    })
                }
                _ => err.change_context(errors::ApiErrorResponse::InvalidRequestData {
                    message: "The request body is invalid".to_string(),
                }),
            })
    }

    fn validate_auth_and_metadata_type_with_connector(
        &self,
    ) -> Result<(), error_stack::Report<errors::ConnectorError>> {
        use crate::connector::*;

        match self.connector_name {
            api_enums::Connector::Adyenplatform => {
                adyenplatform::transformers::AdyenplatformAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            // api_enums::Connector::Payone => {payone::transformers::PayoneAuthType::try_from(val)?;Ok(())} Added as a template code for future usage
            #[cfg(feature = "dummy_connector")]
            api_enums::Connector::DummyConnector1
            | api_enums::Connector::DummyConnector2
            | api_enums::Connector::DummyConnector3
            | api_enums::Connector::DummyConnector4
            | api_enums::Connector::DummyConnector5
            | api_enums::Connector::DummyConnector6
            | api_enums::Connector::DummyConnector7 => {
                dummyconnector::transformers::DummyConnectorAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Aci => {
                aci::transformers::AciAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Adyen => {
                adyen::transformers::AdyenAuthType::try_from(self.auth_type)?;
                adyen::transformers::AdyenConnectorMetadataObject::try_from(
                    self.connector_meta_data,
                )?;
                Ok(())
            }
            api_enums::Connector::Airwallex => {
                airwallex::transformers::AirwallexAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Authorizedotnet => {
                authorizedotnet::transformers::AuthorizedotnetAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Bankofamerica => {
                bankofamerica::transformers::BankOfAmericaAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Billwerk => {
                billwerk::transformers::BillwerkAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Bitpay => {
                bitpay::transformers::BitpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Bambora => {
                bambora::transformers::BamboraAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Bamboraapac => {
                bamboraapac::transformers::BamboraapacAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Boku => {
                boku::transformers::BokuAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Bluesnap => {
                bluesnap::transformers::BluesnapAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Braintree => {
                braintree::transformers::BraintreeAuthType::try_from(self.auth_type)?;
                braintree::transformers::BraintreeMeta::try_from(self.connector_meta_data)?;
                Ok(())
            }
            api_enums::Connector::Cashtocode => {
                cashtocode::transformers::CashtocodeAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Checkout => {
                checkout::transformers::CheckoutAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Coinbase => {
                coinbase::transformers::CoinbaseAuthType::try_from(self.auth_type)?;
                coinbase::transformers::CoinbaseConnectorMeta::try_from(self.connector_meta_data)?;
                Ok(())
            }
            api_enums::Connector::Cryptopay => {
                cryptopay::transformers::CryptopayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Cybersource => {
                cybersource::transformers::CybersourceAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Datatrans => {
                datatrans::transformers::DatatransAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Dlocal => {
                dlocal::transformers::DlocalAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Ebanx => {
                ebanx::transformers::EbanxAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Fiserv => {
                fiserv::transformers::FiservAuthType::try_from(self.auth_type)?;
                fiserv::transformers::FiservSessionObject::try_from(self.connector_meta_data)?;
                Ok(())
            }
            api_enums::Connector::Forte => {
                forte::transformers::ForteAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Globalpay => {
                globalpay::transformers::GlobalpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Globepay => {
                globepay::transformers::GlobepayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Gocardless => {
                gocardless::transformers::GocardlessAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Gpayments => {
                gpayments::transformers::GpaymentsAuthType::try_from(self.auth_type)?;
                gpayments::transformers::GpaymentsMetaData::try_from(self.connector_meta_data)?;
                Ok(())
            }
            api_enums::Connector::Helcim => {
                helcim::transformers::HelcimAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Iatapay => {
                iatapay::transformers::IatapayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Itaubank => {
                itaubank::transformers::ItaubankAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Klarna => {
                klarna::transformers::KlarnaAuthType::try_from(self.auth_type)?;
                klarna::transformers::KlarnaConnectorMetadataObject::try_from(
                    self.connector_meta_data,
                )?;
                Ok(())
            }
            api_enums::Connector::Mifinity => {
                mifinity::transformers::MifinityAuthType::try_from(self.auth_type)?;
                mifinity::transformers::MifinityConnectorMetadataObject::try_from(
                    self.connector_meta_data,
                )?;
                Ok(())
            }
            api_enums::Connector::Mollie => {
                mollie::transformers::MollieAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Multisafepay => {
                multisafepay::transformers::MultisafepayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Netcetera => {
                netcetera::transformers::NetceteraAuthType::try_from(self.auth_type)?;
                netcetera::transformers::NetceteraMetaData::try_from(self.connector_meta_data)?;
                Ok(())
            }
            api_enums::Connector::Nexinets => {
                nexinets::transformers::NexinetsAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Nmi => {
                nmi::transformers::NmiAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Noon => {
                noon::transformers::NoonAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Nuvei => {
                nuvei::transformers::NuveiAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Opennode => {
                opennode::transformers::OpennodeAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Paybox => {
                paybox::transformers::PayboxAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Payme => {
                payme::transformers::PaymeAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Paypal => {
                paypal::transformers::PaypalAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Payone => {
                payone::transformers::PayoneAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Payu => {
                payu::transformers::PayuAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Placetopay => {
                placetopay::transformers::PlacetopayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Powertranz => {
                powertranz::transformers::PowertranzAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Prophetpay => {
                prophetpay::transformers::ProphetpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Rapyd => {
                rapyd::transformers::RapydAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Razorpay => {
                razorpay::transformers::RazorpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Shift4 => {
                shift4::transformers::Shift4AuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Square => {
                square::transformers::SquareAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Stax => {
                stax::transformers::StaxAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Stripe => {
                stripe::transformers::StripeAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Trustpay => {
                trustpay::transformers::TrustpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Tsys => {
                tsys::transformers::TsysAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Volt => {
                volt::transformers::VoltAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Wellsfargo => {
                wellsfargo::transformers::WellsfargoAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Wise => {
                wise::transformers::WiseAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Worldline => {
                worldline::transformers::WorldlineAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Worldpay => {
                worldpay::transformers::WorldpayAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Zen => {
                zen::transformers::ZenAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Zsl => {
                zsl::transformers::ZslAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Signifyd => {
                signifyd::transformers::SignifydAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Riskified => {
                riskified::transformers::RiskifiedAuthType::try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Plaid => {
                PlaidAuthType::foreign_try_from(self.auth_type)?;
                Ok(())
            }
            api_enums::Connector::Threedsecureio => {
                threedsecureio::transformers::ThreedsecureioAuthType::try_from(self.auth_type)?;
                Ok(())
            }
        }
    }
}

struct ConnectorAuthTypeValidation<'a> {
    auth_type: &'a types::ConnectorAuthType,
}

impl<'a> ConnectorAuthTypeValidation<'a> {
    fn validate_connector_auth_type(
        &self,
    ) -> Result<(), error_stack::Report<errors::ApiErrorResponse>> {
        let validate_non_empty_field = |field_value: &str, field_name: &str| {
            if field_value.trim().is_empty() {
                Err(errors::ApiErrorResponse::InvalidDataFormat {
                    field_name: format!("connector_account_details.{}", field_name),
                    expected_format: "a non empty String".to_string(),
                }
                .into())
            } else {
                Ok(())
            }
        };
        match self.auth_type {
            hyperswitch_domain_models::router_data::ConnectorAuthType::TemporaryAuth => Ok(()),
            hyperswitch_domain_models::router_data::ConnectorAuthType::HeaderKey { api_key } => {
                validate_non_empty_field(api_key.peek(), "api_key")
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::BodyKey {
                api_key,
                key1,
            } => {
                validate_non_empty_field(api_key.peek(), "api_key")?;
                validate_non_empty_field(key1.peek(), "key1")
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::SignatureKey {
                api_key,
                key1,
                api_secret,
            } => {
                validate_non_empty_field(api_key.peek(), "api_key")?;
                validate_non_empty_field(key1.peek(), "key1")?;
                validate_non_empty_field(api_secret.peek(), "api_secret")
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::MultiAuthKey {
                api_key,
                key1,
                api_secret,
                key2,
            } => {
                validate_non_empty_field(api_key.peek(), "api_key")?;
                validate_non_empty_field(key1.peek(), "key1")?;
                validate_non_empty_field(api_secret.peek(), "api_secret")?;
                validate_non_empty_field(key2.peek(), "key2")
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::CurrencyAuthKey {
                auth_key_map,
            } => {
                if auth_key_map.is_empty() {
                    Err(errors::ApiErrorResponse::InvalidDataFormat {
                        field_name: "connector_account_details.auth_key_map".to_string(),
                        expected_format: "a non empty map".to_string(),
                    }
                    .into())
                } else {
                    Ok(())
                }
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::CertificateAuth {
                certificate,
                private_key,
            } => {
                helpers::create_identity_from_certificate_and_key(
                    certificate.to_owned(),
                    private_key.to_owned(),
                )
                .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                    field_name:
                        "connector_account_details.certificate or connector_account_details.private_key"
                            .to_string(),
                    expected_format:
                        "a valid base64 encoded string of PEM encoded Certificate and Private Key"
                            .to_string(),
                })?;
                Ok(())
            }
            hyperswitch_domain_models::router_data::ConnectorAuthType::NoKey => Ok(()),
        }
    }
}

struct ConnectorStatusAndDisabledValidation<'a> {
    status: &'a Option<api_enums::ConnectorStatus>,
    disabled: &'a Option<bool>,
    auth: &'a types::ConnectorAuthType,
    current_status: &'a api_enums::ConnectorStatus,
}

impl<'a> ConnectorStatusAndDisabledValidation<'a> {
    fn validate_status_and_disabled(
        &self,
    ) -> RouterResult<(api_enums::ConnectorStatus, Option<bool>)> {
        let connector_status = match (self.status, self.auth) {
            (
                Some(common_enums::ConnectorStatus::Active),
                types::ConnectorAuthType::TemporaryAuth,
            ) => {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Connector status cannot be active when using TemporaryAuth"
                        .to_string(),
                }
                .into());
            }
            (Some(status), _) => status,
            (None, types::ConnectorAuthType::TemporaryAuth) => {
                &common_enums::ConnectorStatus::Inactive
            }
            (None, _) => self.current_status,
        };

        let disabled = match (self.disabled, connector_status) {
            (Some(false), common_enums::ConnectorStatus::Inactive) => {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Connector cannot be enabled when connector_status is inactive or when using TemporaryAuth"
                        .to_string(),
                }
                .into());
            }
            (Some(disabled), _) => Some(*disabled),
            (None, common_enums::ConnectorStatus::Inactive) => Some(true),
            (None, _) => None,
        };

        Ok((*connector_status, disabled))
    }
}

struct PaymentMethodsEnabled<'a> {
    payment_methods_enabled: &'a Option<Vec<api_models::admin::PaymentMethodsEnabled>>,
}

impl<'a> PaymentMethodsEnabled<'a> {
    fn get_payment_methods_enabled(&self) -> RouterResult<Option<Vec<pii::SecretSerdeValue>>> {
        let mut vec = Vec::new();
        let payment_methods_enabled = match self.payment_methods_enabled.clone() {
            Some(val) => {
                for pm in val.into_iter() {
                    let pm_value = pm
                        .encode_to_value()
                        .change_context(errors::ApiErrorResponse::InternalServerError)
                        .attach_printable(
                            "Failed while encoding to serde_json::Value, PaymentMethod",
                        )?;
                    vec.push(Secret::new(pm_value))
                }
                Some(vec)
            }
            None => None,
        };
        Ok(payment_methods_enabled)
    }
}

struct CertificateAndCertificateKey<'a> {
    certificate: &'a Secret<String>,
    certificate_key: &'a Secret<String>,
}

impl<'a> CertificateAndCertificateKey<'a> {
    pub fn create_identity_from_certificate_and_key(
        &self,
    ) -> Result<reqwest::Identity, error_stack::Report<errors::ApiClientError>> {
        let decoded_certificate = BASE64_ENGINE
            .decode(self.certificate.clone().expose())
            .change_context(errors::ApiClientError::CertificateDecodeFailed)?;

        let decoded_certificate_key = BASE64_ENGINE
            .decode(self.certificate_key.clone().expose())
            .change_context(errors::ApiClientError::CertificateDecodeFailed)?;

        let certificate = String::from_utf8(decoded_certificate)
            .change_context(errors::ApiClientError::CertificateDecodeFailed)?;

        let certificate_key = String::from_utf8(decoded_certificate_key)
            .change_context(errors::ApiClientError::CertificateDecodeFailed)?;

        reqwest::Identity::from_pkcs8_pem(certificate.as_bytes(), certificate_key.as_bytes())
            .change_context(errors::ApiClientError::CertificateDecodeFailed)
    }
}

struct ConnectorMetadata<'a> {
    connector_metadata: &'a Option<pii::SecretSerdeValue>,
}

impl<'a> ConnectorMetadata<'a> {
    fn validate_apple_pay_certificates_in_mca_metadata(&self) -> RouterResult<()> {
        self.connector_metadata
            .clone()
            .map(api_models::payments::ConnectorMetadata::from_value)
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                field_name: "metadata".to_string(),
                expected_format: "connector metadata".to_string(),
            })?
            .and_then(|metadata| metadata.get_apple_pay_certificates())
            .map(|(certificate, certificate_key)| {
                let certificate_and_certificate_key = CertificateAndCertificateKey {
                    certificate: &certificate,
                    certificate_key: &certificate_key,
                };
                certificate_and_certificate_key.create_identity_from_certificate_and_key()
            })
            .transpose()
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "certificate/certificate key",
            })?;
        Ok(())
    }
}

struct PMAuthConfigValidation<'a> {
    connector_type: &'a api_enums::ConnectorType,
    pm_auth_config: &'a Option<pii::SecretSerdeValue>,
    db: &'a dyn StorageInterface,
    merchant_id: &'a id_type::MerchantId,
    profile_id: &'a String,
    key_store: &'a domain::MerchantKeyStore,
    key_manager_state: &'a KeyManagerState,
}

impl<'a> PMAuthConfigValidation<'a> {
    async fn validate_pm_auth(&self, val: &pii::SecretSerdeValue) -> RouterResponse<()> {
        let config = serde_json::from_value::<api_models::pm_auth::PaymentMethodAuthConfig>(
            val.clone().expose(),
        )
        .change_context(errors::ApiErrorResponse::InvalidRequestData {
            message: "invalid data received for payment method auth config".to_string(),
        })
        .attach_printable("Failed to deserialize Payment Method Auth config")?;

        let all_mcas = self
            .db
            .find_merchant_connector_account_by_merchant_id_and_disabled_list(
                self.key_manager_state,
                self.merchant_id,
                true,
                self.key_store,
            )
            .await
            .change_context(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
                id: self.merchant_id.get_string_repr().to_owned(),
            })?;

        for conn_choice in config.enabled_payment_methods {
            let pm_auth_mca = all_mcas
                .clone()
                .into_iter()
                .find(|mca| mca.get_id() == conn_choice.mca_id)
                .ok_or(errors::ApiErrorResponse::GenericNotFoundError {
                    message: "payment method auth connector account not found".to_string(),
                })?;

            if &pm_auth_mca.profile_id != self.profile_id {
                return Err(errors::ApiErrorResponse::GenericNotFoundError {
                    message: "payment method auth profile_id differs from connector profile_id"
                        .to_string(),
                }
                .into());
            }
        }

        Ok(services::ApplicationResponse::StatusOk)
    }

    async fn validate_pm_auth_config(&self) -> RouterResult<()> {
        if self.connector_type != &api_enums::ConnectorType::PaymentMethodAuth {
            if let Some(val) = self.pm_auth_config.clone() {
                self.validate_pm_auth(&val).await?;
            }
        }
        Ok(())
    }
}

struct ConnectorTypeAndConnectorName<'a> {
    connector_type: &'a api_enums::ConnectorType,
    connector_name: &'a api_enums::Connector,
}

impl<'a> ConnectorTypeAndConnectorName<'a> {
    fn get_routable_connector(&self) -> RouterResult<Option<api_enums::RoutableConnectors>> {
        let mut routable_connector =
            api_enums::RoutableConnectors::from_str(&self.connector_name.to_string()).ok();

        let pm_auth_connector =
            api_enums::convert_pm_auth_connector(self.connector_name.to_string().as_str());
        let authentication_connector =
            api_enums::convert_authentication_connector(self.connector_name.to_string().as_str());

        if pm_auth_connector.is_some() {
            if self.connector_type != &api_enums::ConnectorType::PaymentMethodAuth
                && self.connector_type != &api_enums::ConnectorType::PaymentProcessor
            {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid connector type given".to_string(),
                }
                .into());
            }
        } else if authentication_connector.is_some() {
            if self.connector_type != &api_enums::ConnectorType::AuthenticationProcessor {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid connector type given".to_string(),
                }
                .into());
            }
        } else {
            let routable_connector_option = self
                .connector_name
                .to_string()
                .parse::<api_enums::RoutableConnectors>()
                .change_context(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid connector name given".to_string(),
                })?;
            routable_connector = Some(routable_connector_option);
        };
        Ok(routable_connector)
    }
}

struct MerchantDefaultConfigUpdate<'a> {
    routable_connector: &'a Option<api_enums::RoutableConnectors>,
    merchant_connector_id: &'a String,
    store: &'a dyn StorageInterface,
    merchant_id: &'a id_type::MerchantId,
    default_routing_config: &'a Vec<api_models::routing::RoutableConnectorChoice>,
    default_routing_config_for_profile: &'a Vec<api_models::routing::RoutableConnectorChoice>,
    profile_id: &'a String,
    transaction_type: &'a api_enums::TransactionType,
}

impl<'a> MerchantDefaultConfigUpdate<'a> {
    async fn update_merchant_default_config(&self) -> RouterResult<()> {
        let mut default_routing_config = self.default_routing_config.to_owned();
        let mut default_routing_config_for_profile =
            self.default_routing_config_for_profile.to_owned();
        if let Some(routable_connector_val) = self.routable_connector {
            let choice = routing_types::RoutableConnectorChoice {
                choice_kind: routing_types::RoutableChoiceKind::FullStruct,
                connector: *routable_connector_val,
                merchant_connector_id: Some(self.merchant_connector_id.clone()),
            };
            if !default_routing_config.contains(&choice) {
                default_routing_config.push(choice.clone());
                routing_helpers::update_merchant_default_config(
                    self.store,
                    self.merchant_id.get_string_repr(),
                    default_routing_config.clone(),
                    self.transaction_type,
                )
                .await?;
            }
            if !default_routing_config_for_profile.contains(&choice.clone()) {
                default_routing_config_for_profile.push(choice);
                routing_helpers::update_merchant_default_config(
                    self.store,
                    self.profile_id,
                    default_routing_config_for_profile.clone(),
                    self.transaction_type,
                )
                .await?;
            }
        }
        Ok(())
    }
}

#[cfg(any(feature = "v1", feature = "v2", feature = "olap"))]
#[async_trait::async_trait]
trait MerchantConnectorAccountUpdateBridge {
    async fn get_merchant_connector_account_from_id(
        self,
        db: &dyn StorageInterface,
        merchant_id: &id_type::MerchantId,
        merchant_connector_id: &str,
        key_store: &domain::MerchantKeyStore,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount>;

    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        mca: &domain::MerchantConnectorAccount,
        key_manager_state: &KeyManagerState,
        merchant_account: &domain::MerchantAccount,
    ) -> RouterResult<domain::MerchantConnectorAccountUpdate>;
}

#[cfg(all(
    feature = "v2",
    feature = "merchant_connector_account_v2",
    feature = "olap"
))]
#[async_trait::async_trait]
impl MerchantConnectorAccountUpdateBridge for api_models::admin::MerchantConnectorUpdate {
    async fn get_merchant_connector_account_from_id(
        self,
        db: &dyn StorageInterface,
        _merchant_id: &id_type::MerchantId,
        merchant_connector_id: &str,
        key_store: &domain::MerchantKeyStore,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount> {
        db.find_merchant_connector_account_by_id(
            key_manager_state,
            merchant_connector_id,
            key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)
    }

    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        mca: &domain::MerchantConnectorAccount,
        key_manager_state: &KeyManagerState,
        merchant_account: &domain::MerchantAccount,
    ) -> RouterResult<domain::MerchantConnectorAccountUpdate> {
        let payment_methods_enabled = PaymentMethodsEnabled {
            payment_methods_enabled: &self.payment_methods_enabled,
        };
        let payment_methods_enabled = payment_methods_enabled.get_payment_methods_enabled()?;

        let frm_configs = self.get_frm_config_as_secret();

        let auth = types::ConnectorAuthType::from_secret_value(
            self.connector_account_details
                .clone()
                .unwrap_or(mca.connector_account_details.clone().into_inner()),
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "connector_account_details".to_string(),
            expected_format: "auth_type and api_key".to_string(),
        })?;

        let metadata = self.metadata.clone().or(mca.metadata.clone());

        let connector_name = mca.connector_name.as_ref();
        let connector_enum = api_models::enums::Connector::from_str(connector_name)
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "connector",
            })
            .attach_printable_lazy(|| {
                format!("unable to parse connector name {connector_name:?}")
            })?;
        let connector_auth_type_and_metadata_validation = ConnectorAuthTypeAndMetadataValidation {
            connector_name: &connector_enum,
            auth_type: &auth,
            connector_meta_data: &metadata,
        };
        connector_auth_type_and_metadata_validation.validate_auth_and_metadata_type()?;
        let connector_status_and_disabled_validation = ConnectorStatusAndDisabledValidation {
            status: &self.status,
            disabled: &self.disabled,
            auth: &auth,
            current_status: &mca.status,
        };
        let (connector_status, disabled) =
            connector_status_and_disabled_validation.validate_status_and_disabled()?;

        let pm_auth_config_validation = PMAuthConfigValidation {
            connector_type: &self.connector_type,
            pm_auth_config: &self.pm_auth_config,
            db: state.store.as_ref(),
            merchant_id: merchant_account.get_id(),
            profile_id: &mca.profile_id.clone(),
            key_store: &key_store,
            key_manager_state,
        };

        pm_auth_config_validation.validate_pm_auth_config().await?;

        Ok(storage::MerchantConnectorAccountUpdate::Update {
            connector_type: Some(self.connector_type),
            connector_label: self.connector_label.clone(),
            connector_account_details: self
                .connector_account_details
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantConnectorAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        km_types::Identifier::Merchant(key_store.merchant_id.clone()),
                        key_store.key.get_inner().peek(),
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed while encrypting data")?,
            disabled,
            payment_methods_enabled,
            metadata: self.metadata,
            frm_configs,
            connector_webhook_details: match &self.connector_webhook_details {
                Some(connector_webhook_details) => connector_webhook_details
                    .encode_to_value()
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .map(Some)?
                    .map(Secret::new),
                None => None,
            },
            applepay_verified_domains: None,
            pm_auth_config: self.pm_auth_config,
            status: Some(connector_status),
            connector_wallets_details: helpers::get_encrypted_apple_pay_connector_wallets_details(
                state, &key_store, &metadata,
            )
            .await?,
        })
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2", feature = "olap"),
    not(feature = "merchant_connector_account_v2")
))]
#[async_trait::async_trait]
impl MerchantConnectorAccountUpdateBridge for api_models::admin::MerchantConnectorUpdate {
    async fn get_merchant_connector_account_from_id(
        self,
        db: &dyn StorageInterface,
        merchant_id: &id_type::MerchantId,
        merchant_connector_id: &str,
        key_store: &domain::MerchantKeyStore,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount> {
        db.find_by_merchant_connector_account_merchant_id_merchant_connector_id(
            key_manager_state,
            merchant_id,
            merchant_connector_id,
            key_store,
        )
        .await
        .to_not_found_response(
            errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
                id: merchant_connector_id.to_string(),
            },
        )
    }

    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        mca: &domain::MerchantConnectorAccount,
        key_manager_state: &KeyManagerState,
        merchant_account: &domain::MerchantAccount,
    ) -> RouterResult<domain::MerchantConnectorAccountUpdate> {
        let payment_methods_enabled = self.payment_methods_enabled.map(|pm_enabled| {
            pm_enabled
                .iter()
                .flat_map(Encode::encode_to_value)
                .map(Secret::new)
                .collect::<Vec<pii::SecretSerdeValue>>()
        });

        let frm_configs = get_frm_config_as_secret(self.frm_configs);

        let auth: types::ConnectorAuthType = self
            .connector_account_details
            .clone()
            .unwrap_or(mca.connector_account_details.clone().into_inner())
            .parse_value("ConnectorAuthType")
            .change_context(errors::ApiErrorResponse::InvalidDataFormat {
                field_name: "connector_account_details".to_string(),
                expected_format: "auth_type and api_key".to_string(),
            })?;
        let metadata = self.metadata.clone().or(mca.metadata.clone());

        let connector_name = mca.connector_name.as_ref();
        let connector_enum = api_models::enums::Connector::from_str(connector_name)
            .change_context(errors::ApiErrorResponse::InvalidDataValue {
                field_name: "connector",
            })
            .attach_printable_lazy(|| {
                format!("unable to parse connector name {connector_name:?}")
            })?;
        let connector_auth_type_and_metadata_validation = ConnectorAuthTypeAndMetadataValidation {
            connector_name: &connector_enum,
            auth_type: &auth,
            connector_meta_data: &metadata,
        };
        connector_auth_type_and_metadata_validation.validate_auth_and_metadata_type()?;
        let connector_status_and_disabled_validation = ConnectorStatusAndDisabledValidation {
            status: &self.status,
            disabled: &self.disabled,
            auth: &auth,
            current_status: &mca.status,
        };
        let (connector_status, disabled) =
            connector_status_and_disabled_validation.validate_status_and_disabled()?;

        if self.connector_type != api_enums::ConnectorType::PaymentMethodAuth {
            if let Some(val) = self.pm_auth_config.clone() {
                validate_pm_auth(
                    val,
                    state,
                    merchant_account.get_id(),
                    &key_store,
                    merchant_account.clone(),
                    &mca.profile_id,
                )
                .await?;
            }
        }

        Ok(storage::MerchantConnectorAccountUpdate::Update {
            connector_type: Some(self.connector_type),
            connector_name: None,
            merchant_connector_id: None,
            connector_label: self.connector_label.clone(),
            connector_account_details: self
                .connector_account_details
                .async_lift(|inner| async {
                    domain_types::crypto_operation(
                        key_manager_state,
                        type_name!(storage::MerchantConnectorAccount),
                        domain_types::CryptoOperation::EncryptOptional(inner),
                        km_types::Identifier::Merchant(key_store.merchant_id.clone()),
                        key_store.key.get_inner().peek(),
                    )
                    .await
                    .and_then(|val| val.try_into_optionaloperation())
                })
                .await
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed while encrypting data")?,
            test_mode: self.test_mode,
            disabled,
            payment_methods_enabled,
            metadata: self.metadata,
            frm_configs,
            connector_webhook_details: match &self.connector_webhook_details {
                Some(connector_webhook_details) => connector_webhook_details
                    .encode_to_value()
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .map(Some)?
                    .map(Secret::new),
                None => None,
            },
            applepay_verified_domains: None,
            pm_auth_config: self.pm_auth_config,
            status: Some(connector_status),
            connector_wallets_details: helpers::get_encrypted_apple_pay_connector_wallets_details(
                state, &key_store, &metadata,
            )
            .await?,
        })
    }
}

#[cfg(any(feature = "v1", feature = "v2", feature = "olap"))]
#[async_trait::async_trait]
trait MerchantConnectorAccountCreateBridge {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        business_profile: &domain::BusinessProfile,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount>;

    async fn validate_and_get_profile_id(
        self,
        merchant_account: &domain::MerchantAccount,
        db: &dyn StorageInterface,
        key_manager_state: &KeyManagerState,
        key_store: &domain::MerchantKeyStore,
        should_validate: bool,
    ) -> RouterResult<String>;
}

#[cfg(all(
    feature = "v2",
    feature = "merchant_connector_account_v2",
    feature = "olap",
    feature = "merchant_account_v2"
))]
#[async_trait::async_trait]
impl MerchantConnectorAccountCreateBridge for api::MerchantConnectorCreate {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        business_profile: &domain::BusinessProfile,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount> {
        // If connector label is not passed in the request, generate one
        let connector_label = self.get_connector_label(business_profile.profile_name.clone());
        let payment_methods_enabled = PaymentMethodsEnabled {
            payment_methods_enabled: &self.payment_methods_enabled,
        };
        let payment_methods_enabled = payment_methods_enabled.get_payment_methods_enabled()?;
        let frm_configs = self.get_frm_config_as_secret();
        // Validate Merchant api details and return error if not in correct format
        let auth = types::ConnectorAuthType::from_option_secret_value(
            self.connector_account_details.clone(),
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "connector_account_details".to_string(),
            expected_format: "auth_type and api_key".to_string(),
        })?;

        let connector_auth_type_and_metadata_validation = ConnectorAuthTypeAndMetadataValidation {
            connector_name: &self.connector_name,
            auth_type: &auth,
            connector_meta_data: &self.metadata,
        };
        connector_auth_type_and_metadata_validation.validate_auth_and_metadata_type()?;
        let connector_status_and_disabled_validation = ConnectorStatusAndDisabledValidation {
            status: &self.status,
            disabled: &self.disabled,
            auth: &auth,
            current_status: &api_enums::ConnectorStatus::Active,
        };
        let (connector_status, disabled) =
            connector_status_and_disabled_validation.validate_status_and_disabled()?;
        let identifier = km_types::Identifier::Merchant(business_profile.merchant_id.clone());
        let merchant_recipient_data = if let Some(data) = &self.additional_merchant_data {
            Some(
                process_open_banking_connectors(
                    state,
                    &business_profile.merchant_id,
                    &auth,
                    &self.connector_type,
                    &self.connector_name,
                    types::AdditionalMerchantData::foreign_from(data.clone()),
                )
                .await?,
            )
        } else {
            None
        }
        .map(|data| {
            serde_json::to_value(types::AdditionalMerchantData::OpenBankingRecipientData(
                data,
            ))
        })
        .transpose()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to get MerchantRecipientData")?;
        Ok(domain::MerchantConnectorAccount {
            merchant_id: business_profile.merchant_id.clone(),
            connector_type: self.connector_type,
            connector_name: self.connector_name.to_string(),
            connector_account_details: domain_types::crypto_operation(
                key_manager_state,
                type_name!(domain::MerchantConnectorAccount),
                domain_types::CryptoOperation::Encrypt(self.connector_account_details.ok_or(
                    errors::ApiErrorResponse::MissingRequiredField {
                        field_name: "connector_account_details",
                    },
                )?),
                identifier.clone(),
                key_store.key.peek(),
            )
            .await
            .and_then(|val| val.try_into_operation())
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt connector account details")?,
            payment_methods_enabled,
            disabled,
            metadata: self.metadata.clone(),
            frm_configs,
            connector_label: Some(connector_label.clone()),
            created_at: date_time::now(),
            modified_at: date_time::now(),
            id: common_utils::generate_time_ordered_id("mca"),
            connector_webhook_details: match self.connector_webhook_details {
                Some(connector_webhook_details) => {
                    connector_webhook_details.encode_to_value(
                    )
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable(format!("Failed to serialize api_models::admin::MerchantConnectorWebhookDetails for Merchant: {:?}", business_profile.merchant_id))
                    .map(Some)?
                    .map(Secret::new)
                }
                None => None,
            },
            profile_id: business_profile.profile_id.clone(),
            applepay_verified_domains: None,
            pm_auth_config: self.pm_auth_config.clone(),
            status: connector_status,
            connector_wallets_details: helpers::get_encrypted_apple_pay_connector_wallets_details(state, &key_store, &self.metadata).await?,
            additional_merchant_data: if let Some(mcd) =  merchant_recipient_data {
                Some(domain_types::crypto_operation(
                    key_manager_state,
                    type_name!(domain::MerchantConnectorAccount),
                    domain_types::CryptoOperation::Encrypt(Secret::new(mcd)),
                    identifier,
                    key_store.key.peek(),
                )
                .await
                .and_then(|val| val.try_into_operation())
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt additional_merchant_data")?)
            } else {
                None
            },
            version: hyperswitch_domain_models::consts::API_VERSION,
        })
    }

    async fn validate_and_get_profile_id(
        self,
        merchant_account: &domain::MerchantAccount,
        db: &dyn StorageInterface,
        key_manager_state: &KeyManagerState,
        key_store: &domain::MerchantKeyStore,
        should_validate: bool,
    ) -> RouterResult<String> {
        let profile_id = self.profile_id;
        // Check whether this business profile belongs to the merchant
        if should_validate {
            let _ = core_utils::validate_and_get_business_profile(
                db,
                key_manager_state,
                key_store,
                Some(&profile_id),
                merchant_account.get_id(),
            )
            .await?;
        }
        Ok(profile_id.clone())
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2", feature = "olap"),
    not(feature = "merchant_connector_account_v2"),
    not(feature = "merchant_account_v2")
))]
#[async_trait::async_trait]
impl MerchantConnectorAccountCreateBridge for api::MerchantConnectorCreate {
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: domain::MerchantKeyStore,
        business_profile: &domain::BusinessProfile,
        key_manager_state: &KeyManagerState,
    ) -> RouterResult<domain::MerchantConnectorAccount> {
        // If connector label is not passed in the request, generate one
        let connector_label = self
            .connector_label
            .clone()
            .or(core_utils::get_connector_label(
                self.business_country,
                self.business_label.as_ref(),
                self.business_sub_label.as_ref(),
                &self.connector_name.to_string(),
            ))
            .unwrap_or(format!(
                "{}_{}",
                self.connector_name, business_profile.profile_name
            ));
        let payment_methods_enabled = PaymentMethodsEnabled {
            payment_methods_enabled: &self.payment_methods_enabled,
        };
        let payment_methods_enabled = payment_methods_enabled.get_payment_methods_enabled()?;
        let frm_configs = self.get_frm_config_as_secret();
        // Validate Merchant api details and return error if not in correct format
        let auth = types::ConnectorAuthType::from_option_secret_value(
            self.connector_account_details.clone(),
        )
        .change_context(errors::ApiErrorResponse::InvalidDataFormat {
            field_name: "connector_account_details".to_string(),
            expected_format: "auth_type and api_key".to_string(),
        })?;

        let connector_auth_type_and_metadata_validation = ConnectorAuthTypeAndMetadataValidation {
            connector_name: &self.connector_name,
            auth_type: &auth,
            connector_meta_data: &self.metadata,
        };
        connector_auth_type_and_metadata_validation.validate_auth_and_metadata_type()?;
        let connector_status_and_disabled_validation = ConnectorStatusAndDisabledValidation {
            status: &self.status,
            disabled: &self.disabled,
            auth: &auth,
            current_status: &api_enums::ConnectorStatus::Active,
        };
        let (connector_status, disabled) =
            connector_status_and_disabled_validation.validate_status_and_disabled()?;
        let identifier = km_types::Identifier::Merchant(business_profile.merchant_id.clone());
        let merchant_recipient_data = if let Some(data) = &self.additional_merchant_data {
            Some(
                process_open_banking_connectors(
                    state,
                    &business_profile.merchant_id,
                    &auth,
                    &self.connector_type,
                    &self.connector_name,
                    types::AdditionalMerchantData::foreign_from(data.clone()),
                )
                .await?,
            )
        } else {
            None
        }
        .map(|data| {
            serde_json::to_value(types::AdditionalMerchantData::OpenBankingRecipientData(
                data,
            ))
        })
        .transpose()
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to get MerchantRecipientData")?;
        Ok(domain::MerchantConnectorAccount {
            merchant_id: business_profile.merchant_id.clone(),
            connector_type: self.connector_type,
            connector_name: self.connector_name.to_string(),
            merchant_connector_id: utils::generate_id(consts::ID_LENGTH, "mca"),
            connector_account_details: domain_types::crypto_operation(
                key_manager_state,
                type_name!(domain::MerchantConnectorAccount),
                domain_types::CryptoOperation::Encrypt(self.connector_account_details.ok_or(
                    errors::ApiErrorResponse::MissingRequiredField {
                        field_name: "connector_account_details",
                    },
                )?),
                identifier.clone(),
                key_store.key.peek(),
            )
            .await
            .and_then(|val| val.try_into_operation())
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt connector account details")?,
            payment_methods_enabled,
            disabled,
            metadata: self.metadata.clone(),
            frm_configs,
            connector_label: Some(connector_label.clone()),
            created_at: date_time::now(),
            modified_at: date_time::now(),
            connector_webhook_details: match self.connector_webhook_details {
                Some(connector_webhook_details) => {
                    connector_webhook_details.encode_to_value(
                    )
                    .change_context(errors::ApiErrorResponse::InternalServerError)
                    .attach_printable(format!("Failed to serialize api_models::admin::MerchantConnectorWebhookDetails for Merchant: {:?}", business_profile.merchant_id))
                    .map(Some)?
                    .map(Secret::new)
                }
                None => None,
            },
            profile_id: business_profile.profile_id.clone(),
            applepay_verified_domains: None,
            pm_auth_config: self.pm_auth_config.clone(),
            status: connector_status,
            connector_wallets_details: helpers::get_encrypted_apple_pay_connector_wallets_details(state, &key_store, &self.metadata).await?,
            test_mode: self.test_mode,
            business_country: self.business_country,
            business_label: self.business_label.clone(),
            business_sub_label: self.business_sub_label.clone(),
            additional_merchant_data: if let Some(mcd) =  merchant_recipient_data {
                Some(domain_types::crypto_operation(
                    key_manager_state,
                    type_name!(domain::MerchantConnectorAccount),
                    domain_types::CryptoOperation::Encrypt(Secret::new(mcd)),
                    identifier,
                    key_store.key.peek(),
                )
                .await
                .and_then(|val| val.try_into_operation())
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Unable to encrypt additional_merchant_data")?)
            } else {
                None
            },
            version: hyperswitch_domain_models::consts::API_VERSION,
        })
    }

    /// If profile_id is not passed, use default profile if available, or
    /// If business_details (business_country and business_label) are passed, get the business_profile
    /// or return a `MissingRequiredField` error
    async fn validate_and_get_profile_id(
        self,
        merchant_account: &domain::MerchantAccount,
        db: &dyn StorageInterface,
        key_manager_state: &KeyManagerState,
        key_store: &domain::MerchantKeyStore,
        should_validate: bool,
    ) -> RouterResult<String> {
        match self.profile_id.or(merchant_account.default_profile.clone()) {
            Some(profile_id) => {
                // Check whether this business profile belongs to the merchant
                if should_validate {
                    let _ = core_utils::validate_and_get_business_profile(
                        db,
                        key_manager_state,
                        key_store,
                        Some(&profile_id),
                        merchant_account.get_id(),
                    )
                    .await?;
                }
                Ok(profile_id.clone())
            }
            None => match self.business_country.zip(self.business_label) {
                Some((business_country, business_label)) => {
                    let profile_name = format!("{business_country}_{business_label}");
                    let business_profile = db
                        .find_business_profile_by_profile_name_merchant_id(
                            key_manager_state,
                            key_store,
                            &profile_name,
                            merchant_account.get_id(),
                        )
                        .await
                        .to_not_found_response(
                            errors::ApiErrorResponse::BusinessProfileNotFound { id: profile_name },
                        )?;

                    Ok(business_profile.profile_id)
                }
                _ => Err(report!(errors::ApiErrorResponse::MissingRequiredField {
                    field_name: "profile_id or business_country, business_label"
                })),
            },
        }
    }
}

pub async fn create_connector(
    state: SessionState,
    req: api::MerchantConnectorCreate,
    merchant_id: &id_type::MerchantId,
) -> RouterResponse<api_models::admin::MerchantConnectorResponse> {
    let store = state.store.as_ref();
    let key_manager_state = &(&state).into();
    #[cfg(feature = "dummy_connector")]
    req.connector_name
        .clone()
        .validate_dummy_connector_enabled(state.conf.dummy_connector.enabled)
        .change_context(errors::ApiErrorResponse::InvalidRequestData {
            message: "Invalid connector name".to_string(),
        })?;

    let key_store = store
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &state.store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let connector_metadata = ConnectorMetadata {
        connector_metadata: &req.metadata,
    };

    connector_metadata.validate_apple_pay_certificates_in_mca_metadata()?;

    let merchant_account = state
        .store
        .find_merchant_account_by_merchant_id(key_manager_state, merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "merchant_account_v2")
    ))]
    helpers::validate_business_details(
        req.business_country,
        req.business_label.as_ref(),
        &merchant_account,
    )?;

    let profile_id = req
        .clone()
        .validate_and_get_profile_id(
            &merchant_account,
            store,
            key_manager_state,
            &key_store,
            true,
        )
        .await?;

    let pm_auth_config_validation = PMAuthConfigValidation {
        connector_type: &req.connector_type,
        pm_auth_config: &req.pm_auth_config,
        db: store,
        merchant_id,
        profile_id: &profile_id.clone(),
        key_store: &key_store,
        key_manager_state,
    };
    pm_auth_config_validation.validate_pm_auth_config().await?;

    let business_profile = state
        .store
        .find_business_profile_by_profile_id(key_manager_state, &key_store, &profile_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_owned(),
        })?;

    let connector_type_and_connector_enum = ConnectorTypeAndConnectorName {
        connector_type: &req.connector_type,
        connector_name: &req.connector_name,
    };
    let routable_connector = connector_type_and_connector_enum.get_routable_connector()?;

    // The purpose of this merchant account update is just to update the
    // merchant account `modified_at` field for KGraph cache invalidation
    state
        .store
        .update_specific_fields_in_merchant(
            key_manager_state,
            merchant_id,
            storage::MerchantAccountUpdate::ModifiedAtUpdate,
            &key_store,
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("error updating the merchant account when creating payment connector")?;

    let merchant_connector_account = req
        .clone()
        .create_domain_model_from_request(
            &state,
            key_store.clone(),
            &business_profile,
            key_manager_state,
        )
        .await?;

    let transaction_type = req.get_transaction_type();

    let mut default_routing_config = routing_helpers::get_merchant_default_config(
        &*state.store,
        merchant_id.get_string_repr(),
        &transaction_type,
    )
    .await?;

    let mut default_routing_config_for_profile = routing_helpers::get_merchant_default_config(
        &*state.clone().store,
        &profile_id,
        &transaction_type,
    )
    .await?;

    let mca = state
        .store
        .insert_merchant_connector_account(
            key_manager_state,
            merchant_connector_account.clone(),
            &key_store,
        )
        .await
        .to_duplicate_response(
            errors::ApiErrorResponse::DuplicateMerchantConnectorAccount {
                profile_id: profile_id.clone(),
                connector_label: merchant_connector_account
                    .connector_label
                    .unwrap_or_default(),
            },
        )?;

    //update merchant default config
    let merchant_default_config_update = MerchantDefaultConfigUpdate {
        routable_connector: &routable_connector,
        merchant_connector_id: &mca.get_id(),
        store,
        merchant_id,
        default_routing_config: &mut default_routing_config,
        default_routing_config_for_profile: &mut default_routing_config_for_profile,
        profile_id: &profile_id,
        transaction_type: &transaction_type,
    };

    merchant_default_config_update
        .update_merchant_default_config()
        .await?;

    metrics::MCA_CREATE.add(
        &metrics::CONTEXT,
        1,
        &add_attributes([
            ("connector", req.connector_name.to_string()),
            ("merchant", merchant_id.get_string_repr().to_owned()),
        ]),
    );

    let mca_response = mca.foreign_try_into()?;
    Ok(service_api::ApplicationResponse::Json(mca_response))
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_connector_account_v2")
))]
async fn validate_pm_auth(
    val: pii::SecretSerdeValue,
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    key_store: &domain::MerchantKeyStore,
    merchant_account: domain::MerchantAccount,
    profile_id: &String,
) -> RouterResponse<()> {
    let config =
        serde_json::from_value::<api_models::pm_auth::PaymentMethodAuthConfig>(val.expose())
            .change_context(errors::ApiErrorResponse::InvalidRequestData {
                message: "invalid data received for payment method auth config".to_string(),
            })
            .attach_printable("Failed to deserialize Payment Method Auth config")?;

    let all_mcas = &*state
        .store
        .find_merchant_connector_account_by_merchant_id_and_disabled_list(
            &state.into(),
            merchant_id,
            true,
            key_store,
        )
        .await
        .change_context(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: merchant_account.get_id().get_string_repr().to_owned(),
        })?;

    for conn_choice in config.enabled_payment_methods {
        let pm_auth_mca = all_mcas
            .iter()
            .find(|mca| mca.get_id() == conn_choice.mca_id)
            .ok_or(errors::ApiErrorResponse::GenericNotFoundError {
                message: "payment method auth connector account not found".to_string(),
            })?;

        if &pm_auth_mca.profile_id != profile_id {
            return Err(errors::ApiErrorResponse::GenericNotFoundError {
                message: "payment method auth profile_id differs from connector profile_id"
                    .to_string(),
            }
            .into());
        }
    }

    Ok(services::ApplicationResponse::StatusOk)
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_connector_account_v2")
))]
pub async fn retrieve_connector(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    profile_id: Option<String>,
    merchant_connector_id: String,
) -> RouterResponse<api_models::admin::MerchantConnectorResponse> {
    let store = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = store
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let _merchant_account = store
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let mca = store
        .find_by_merchant_connector_account_merchant_id_merchant_connector_id(
            key_manager_state,
            &merchant_id,
            &merchant_connector_id,
            &key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: merchant_connector_id.clone(),
        })?;
    core_utils::validate_profile_id_from_auth_layer(profile_id, &mca)?;

    Ok(service_api::ApplicationResponse::Json(
        mca.foreign_try_into()?,
    ))
}

#[cfg(all(feature = "v2", feature = "merchant_connector_account_v2"))]
pub async fn retrieve_connector(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    id: String,
) -> RouterResponse<api_models::admin::MerchantConnectorResponse> {
    let store = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = store
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let mca = store
        .find_merchant_connector_account_by_id(key_manager_state, &id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: id.clone(),
        })?;

    // Validate if the merchant_id sent in the request is valid
    if mca.merchant_id != merchant_id {
        return Err(errors::ApiErrorResponse::InvalidRequestData {
            message: format!(
                "Invalid merchant_id {} provided for merchant_connector_account {}",
                merchant_id.get_string_repr(),
                id
            ),
        }
        .into());
    }

    Ok(service_api::ApplicationResponse::Json(
        mca.foreign_try_into()?,
    ))
}

pub async fn list_payment_connectors(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    profile_id_list: Option<Vec<String>>,
) -> RouterResponse<Vec<api_models::admin::MerchantConnectorListResponse>> {
    let store = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = store
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    // Validate merchant account
    store
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_connector_accounts = store
        .find_merchant_connector_account_by_merchant_id_and_disabled_list(
            key_manager_state,
            &merchant_id,
            true,
            &key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::InternalServerError)?;
    let merchant_connector_accounts = core_utils::filter_objects_based_on_profile_id_list(
        profile_id_list,
        merchant_connector_accounts,
    );
    let mut response = vec![];

    // The can be eliminated once [#79711](https://github.com/rust-lang/rust/issues/79711) is stabilized
    for mca in merchant_connector_accounts.into_iter() {
        response.push(mca.foreign_try_into()?);
    }

    Ok(service_api::ApplicationResponse::Json(response))
}

pub async fn update_connector(
    state: SessionState,
    merchant_id: &id_type::MerchantId,
    profile_id: Option<String>,
    merchant_connector_id: &str,
    req: api_models::admin::MerchantConnectorUpdate,
) -> RouterResponse<api_models::admin::MerchantConnectorResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let mca = req
        .clone()
        .get_merchant_connector_account_from_id(
            db,
            merchant_id,
            merchant_connector_id,
            &key_store,
            key_manager_state,
        )
        .await?;
    core_utils::validate_profile_id_from_auth_layer(profile_id, &mca)?;

    let payment_connector = req
        .clone()
        .create_domain_model_from_request(
            &state,
            key_store.clone(),
            &mca,
            key_manager_state,
            &merchant_account,
        )
        .await?;

    // Profile id should always be present
    let profile_id = mca.profile_id.clone();

    let request_connector_label = req.connector_label;

    let updated_mca = db
        .update_merchant_connector_account(
            key_manager_state,
            mca,
            payment_connector.into(),
            &key_store,
        )
        .await
        .change_context(
            errors::ApiErrorResponse::DuplicateMerchantConnectorAccount {
                profile_id,
                connector_label: request_connector_label.unwrap_or_default(),
            },
        )
        .attach_printable_lazy(|| {
            format!("Failed while updating MerchantConnectorAccount: id: {merchant_connector_id}")
        })?;

    let response = updated_mca.foreign_try_into()?;

    Ok(service_api::ApplicationResponse::Json(response))
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "merchant_connector_account_v2")
))]
pub async fn delete_connector(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    merchant_connector_id: String,
) -> RouterResponse<api::MerchantConnectorDeleteResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let _merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let _mca = db
        .find_by_merchant_connector_account_merchant_id_merchant_connector_id(
            key_manager_state,
            &merchant_id,
            &merchant_connector_id,
            &key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: merchant_connector_id.clone(),
        })?;

    let is_deleted = db
        .delete_merchant_connector_account_by_merchant_id_merchant_connector_id(
            &merchant_id,
            &merchant_connector_id,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: merchant_connector_id.clone(),
        })?;

    let response = api::MerchantConnectorDeleteResponse {
        merchant_id,
        merchant_connector_id,
        deleted: is_deleted,
    };
    Ok(service_api::ApplicationResponse::Json(response))
}

#[cfg(all(feature = "v2", feature = "merchant_connector_account_v2"))]
pub async fn delete_connector(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    id: String,
) -> RouterResponse<api::MerchantConnectorDeleteResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let mca = db
        .find_merchant_connector_account_by_id(key_manager_state, &id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: id.clone(),
        })?;

    // Validate if the merchant_id sent in the request is valid
    if mca.merchant_id != merchant_id {
        return Err(errors::ApiErrorResponse::InvalidRequestData {
            message: format!(
                "Invalid merchant_id {} provided for merchant_connector_account {}",
                merchant_id.get_string_repr(),
                id
            ),
        }
        .into());
    }

    let is_deleted = db
        .delete_merchant_connector_account_by_id(&id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantConnectorAccountNotFound {
            id: id.clone(),
        })?;

    let response = api::MerchantConnectorDeleteResponse {
        merchant_id,
        id,
        deleted: is_deleted,
    };
    Ok(service_api::ApplicationResponse::Json(response))
}

pub async fn kv_for_merchant(
    state: SessionState,
    merchant_id: id_type::MerchantId,
    enable: bool,
) -> RouterResponse<api_models::admin::ToggleKVResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    // check if the merchant account exists
    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let updated_merchant_account = match (enable, merchant_account.storage_scheme) {
        (true, MerchantStorageScheme::RedisKv) | (false, MerchantStorageScheme::PostgresOnly) => {
            Ok(merchant_account)
        }
        (true, MerchantStorageScheme::PostgresOnly) => {
            if state.conf.as_ref().is_kv_soft_kill_mode() {
                Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Kv cannot be enabled when application is in soft_kill_mode"
                        .to_owned(),
                })?
            }

            db.update_merchant(
                key_manager_state,
                merchant_account,
                storage::MerchantAccountUpdate::StorageSchemeUpdate {
                    storage_scheme: MerchantStorageScheme::RedisKv,
                },
                &key_store,
            )
            .await
        }
        (false, MerchantStorageScheme::RedisKv) => {
            db.update_merchant(
                key_manager_state,
                merchant_account,
                storage::MerchantAccountUpdate::StorageSchemeUpdate {
                    storage_scheme: MerchantStorageScheme::PostgresOnly,
                },
                &key_store,
            )
            .await
        }
    }
    .map_err(|error| {
        error
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("failed to switch merchant_storage_scheme")
    })?;
    let kv_status = matches!(
        updated_merchant_account.storage_scheme,
        MerchantStorageScheme::RedisKv
    );

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::ToggleKVResponse {
            merchant_id: updated_merchant_account.get_id().to_owned(),
            kv_enabled: kv_status,
        },
    ))
}

pub async fn toggle_kv_for_all_merchants(
    state: SessionState,
    enable: bool,
) -> RouterResponse<api_models::admin::ToggleAllKVResponse> {
    let db = state.store.as_ref();
    let storage_scheme = if enable {
        MerchantStorageScheme::RedisKv
    } else {
        MerchantStorageScheme::PostgresOnly
    };

    let total_update = db
        .update_all_merchant_account(storage::MerchantAccountUpdate::StorageSchemeUpdate {
            storage_scheme,
        })
        .await
        .map_err(|error| {
            error
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to switch merchant_storage_scheme for all merchants")
        })?;

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::ToggleAllKVResponse {
            total_updated: total_update,
            kv_enabled: enable,
        },
    ))
}

pub async fn check_merchant_account_kv_status(
    state: SessionState,
    merchant_id: id_type::MerchantId,
) -> RouterResponse<api_models::admin::ToggleKVResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    // check if the merchant account exists
    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, &merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    let kv_status = matches!(
        merchant_account.storage_scheme,
        MerchantStorageScheme::RedisKv
    );

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::ToggleKVResponse {
            merchant_id: merchant_account.get_id().to_owned(),
            kv_enabled: kv_status,
        },
    ))
}

pub fn get_frm_config_as_secret(
    frm_configs: Option<Vec<api_models::admin::FrmConfigs>>,
) -> Option<Vec<Secret<serde_json::Value>>> {
    match frm_configs.as_ref() {
        Some(frm_value) => {
            let configs_for_frm_value: Vec<Secret<serde_json::Value>> = frm_value
                .iter()
                .map(|config| {
                    config
                        .encode_to_value()
                        .change_context(errors::ApiErrorResponse::ConfigNotFound)
                        .map(Secret::new)
                })
                .collect::<Result<Vec<_>, _>>()
                .ok()?;
            Some(configs_for_frm_value)
        }
        None => None,
    }
}

#[cfg(all(
    any(feature = "v1", feature = "v2"),
    not(feature = "business_profile_v2")
))]
pub async fn create_and_insert_business_profile(
    state: &SessionState,
    request: api::BusinessProfileCreate,
    merchant_account: domain::MerchantAccount,
    key_store: &domain::MerchantKeyStore,
) -> RouterResult<domain::BusinessProfile> {
    let business_profile_new = admin::create_business_profile_from_merchant_account(
        state,
        merchant_account,
        request,
        key_store,
    )
    .await?;

    let profile_name = business_profile_new.profile_name.clone();

    state
        .store
        .insert_business_profile(&state.into(), key_store, business_profile_new)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::GenericDuplicateError {
            message: format!(
                "Business Profile with the profile_name {profile_name} already exists"
            ),
        })
        .attach_printable("Failed to insert Business profile because of duplication error")
}

#[cfg(feature = "olap")]
#[async_trait::async_trait]
trait BusinessProfileCreateBridge {
    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "business_profile_v2")
    ))]
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        merchant_account: &domain::MerchantAccount,
        key: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfile>;

    #[cfg(all(feature = "v2", feature = "business_profile_v2"))]
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key: &domain::MerchantKeyStore,
        merchant_id: &id_type::MerchantId,
    ) -> RouterResult<domain::BusinessProfile>;
}

#[cfg(feature = "olap")]
#[async_trait::async_trait]
impl BusinessProfileCreateBridge for api::BusinessProfileCreate {
    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "business_profile_v2")
    ))]
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        merchant_account: &domain::MerchantAccount,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfile> {
        use common_utils::ext_traits::AsyncExt;

        if let Some(session_expiry) = &self.session_expiry {
            helpers::validate_session_expiry(session_expiry.to_owned())?;
        }

        if let Some(intent_fulfillment_expiry) = self.intent_fulfillment_time {
            helpers::validate_intent_fulfillment_expiry(intent_fulfillment_expiry)?;
        }

        if let Some(ref routing_algorithm) = self.routing_algorithm {
            let _: api_models::routing::RoutingAlgorithm = routing_algorithm
                .clone()
                .parse_value("RoutingAlgorithm")
                .change_context(errors::ApiErrorResponse::InvalidDataValue {
                    field_name: "routing_algorithm",
                })
                .attach_printable("Invalid routing algorithm given")?;
        }

        // Generate a unique profile id
        let profile_id = common_utils::generate_id_with_default_len("pro");
        let profile_name = self.profile_name.unwrap_or("default".to_string());

        let current_time = date_time::now();

        let webhook_details = self.webhook_details.map(ForeignInto::foreign_into);

        let payment_response_hash_key = self
            .payment_response_hash_key
            .or(merchant_account.payment_response_hash_key.clone())
            .unwrap_or(common_utils::crypto::generate_cryptographically_secure_random_string(64));

        let payment_link_config = self.payment_link_config.map(ForeignInto::foreign_into);
        let outgoing_webhook_custom_http_headers = self
            .outgoing_webhook_custom_http_headers
            .async_map(|headers| cards::create_encrypted_data(state, key_store, headers))
            .await
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt outgoing webhook custom HTTP headers")?;

        let payout_link_config = self
            .payout_link_config
            .map(|payout_conf| match payout_conf.config.validate() {
                Ok(_) => Ok(payout_conf.foreign_into()),
                Err(e) => Err(error_stack::report!(
                    errors::ApiErrorResponse::InvalidRequestData {
                        message: e.to_string()
                    }
                )),
            })
            .transpose()?;

        Ok(domain::BusinessProfile {
            profile_id,
            merchant_id: merchant_account.get_id().clone(),
            profile_name,
            created_at: current_time,
            modified_at: current_time,
            return_url: self
                .return_url
                .map(|return_url| return_url.to_string())
                .or(merchant_account.return_url.clone()),
            enable_payment_response_hash: self
                .enable_payment_response_hash
                .unwrap_or(merchant_account.enable_payment_response_hash),
            payment_response_hash_key: Some(payment_response_hash_key),
            redirect_to_merchant_with_http_post: self
                .redirect_to_merchant_with_http_post
                .unwrap_or(merchant_account.redirect_to_merchant_with_http_post),
            webhook_details: webhook_details.or(merchant_account.webhook_details.clone()),
            metadata: self.metadata,
            routing_algorithm: None,
            intent_fulfillment_time: self
                .intent_fulfillment_time
                .map(i64::from)
                .or(merchant_account.intent_fulfillment_time)
                .or(Some(common_utils::consts::DEFAULT_INTENT_FULFILLMENT_TIME)),
            frm_routing_algorithm: self
                .frm_routing_algorithm
                .or(merchant_account.frm_routing_algorithm.clone()),
            #[cfg(feature = "payouts")]
            payout_routing_algorithm: self
                .payout_routing_algorithm
                .or(merchant_account.payout_routing_algorithm.clone()),
            #[cfg(not(feature = "payouts"))]
            payout_routing_algorithm: None,
            is_recon_enabled: merchant_account.is_recon_enabled,
            applepay_verified_domains: self.applepay_verified_domains,
            payment_link_config,
            session_expiry: self
                .session_expiry
                .map(i64::from)
                .or(Some(common_utils::consts::DEFAULT_SESSION_EXPIRY)),
            authentication_connector_details: self
                .authentication_connector_details
                .map(ForeignInto::foreign_into),
            payout_link_config,
            is_connector_agnostic_mit_enabled: self.is_connector_agnostic_mit_enabled,
            is_extended_card_info_enabled: None,
            extended_card_info_config: None,
            use_billing_as_payment_method_billing: self
                .use_billing_as_payment_method_billing
                .or(Some(true)),
            collect_shipping_details_from_wallet_connector: self
                .collect_shipping_details_from_wallet_connector
                .or(Some(false)),
            collect_billing_details_from_wallet_connector: self
                .collect_billing_details_from_wallet_connector
                .or(Some(false)),
            outgoing_webhook_custom_http_headers: outgoing_webhook_custom_http_headers
                .map(Into::into),
        })
    }

    #[cfg(all(feature = "v2", feature = "business_profile_v2"))]
    async fn create_domain_model_from_request(
        self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
        merchant_id: &id_type::MerchantId,
    ) -> RouterResult<domain::BusinessProfile> {
        if let Some(session_expiry) = &self.session_expiry {
            helpers::validate_session_expiry(session_expiry.to_owned())?;
        }

        // Generate a unique profile id
        // TODO: the profile_id should be generated from the profile_name
        let profile_id = common_utils::generate_id_with_default_len("pro");
        let profile_name = self.profile_name;

        let current_time = date_time::now();

        let webhook_details = self.webhook_details.map(ForeignInto::foreign_into);

        let payment_response_hash_key = self
            .payment_response_hash_key
            .unwrap_or(common_utils::crypto::generate_cryptographically_secure_random_string(64));

        let payment_link_config = self.payment_link_config.map(ForeignInto::foreign_into);
        let outgoing_webhook_custom_http_headers = self
            .outgoing_webhook_custom_http_headers
            .async_map(|headers| cards::create_encrypted_data(state, key_store, headers))
            .await
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt outgoing webhook custom HTTP headers")?;

        let payout_link_config = self
            .payout_link_config
            .map(|payout_conf| match payout_conf.config.validate() {
                Ok(_) => Ok(payout_conf.foreign_into()),
                Err(e) => Err(error_stack::report!(
                    errors::ApiErrorResponse::InvalidRequestData {
                        message: e.to_string()
                    }
                )),
            })
            .transpose()?;

        Ok(domain::BusinessProfile {
            profile_id,
            merchant_id: merchant_id.clone(),
            profile_name,
            created_at: current_time,
            modified_at: current_time,
            return_url: self.return_url.map(|return_url| return_url.to_string()),
            enable_payment_response_hash: self.enable_payment_response_hash.unwrap_or(true),
            payment_response_hash_key: Some(payment_response_hash_key),
            redirect_to_merchant_with_http_post: self
                .redirect_to_merchant_with_http_post
                .unwrap_or(true),
            webhook_details,
            metadata: self.metadata,
            is_recon_enabled: false,
            applepay_verified_domains: self.applepay_verified_domains,
            payment_link_config,
            session_expiry: self
                .session_expiry
                .map(i64::from)
                .or(Some(common_utils::consts::DEFAULT_SESSION_EXPIRY)),
            authentication_connector_details: self
                .authentication_connector_details
                .map(ForeignInto::foreign_into),
            payout_link_config,
            is_connector_agnostic_mit_enabled: self.is_connector_agnostic_mit_enabled,
            is_extended_card_info_enabled: None,
            extended_card_info_config: None,
            use_billing_as_payment_method_billing: self
                .use_billing_as_payment_method_billing
                .or(Some(true)),
            collect_shipping_details_from_wallet_connector: self
                .collect_shipping_details_from_wallet_connector
                .or(Some(false)),
            collect_billing_details_from_wallet_connector: self
                .collect_billing_details_from_wallet_connector
                .or(Some(false)),
            outgoing_webhook_custom_http_headers: outgoing_webhook_custom_http_headers
                .map(Into::into),
            routing_algorithm_id: None,
            frm_routing_algorithm_id: None,
            payout_routing_algorithm_id: None,
            order_fulfillment_time: self
                .order_fulfillment_time
                .map(|order_fulfillment_time| order_fulfillment_time.into_inner())
                .or(Some(common_utils::consts::DEFAULT_ORDER_FULFILLMENT_TIME)),
            order_fulfillment_time_origin: self.order_fulfillment_time_origin,
            default_fallback_routing: None,
        })
    }
}

#[cfg(feature = "olap")]
pub async fn create_business_profile(
    state: SessionState,
    request: api::BusinessProfileCreate,
    merchant_id: &id_type::MerchantId,
) -> RouterResponse<api_models::admin::BusinessProfileResponse> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    // Get the merchant account, if few fields are not passed, then they will be inherited from
    // merchant account
    let merchant_account = db
        .find_merchant_account_by_merchant_id(key_manager_state, merchant_id, &key_store)
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;

    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "business_profile_v2")
    ))]
    let business_profile = request
        .create_domain_model_from_request(&state, &merchant_account, &key_store)
        .await?;

    #[cfg(all(feature = "v2", feature = "business_profile_v2"))]
    let business_profile = request
        .create_domain_model_from_request(&state, &key_store, merchant_account.get_id())
        .await?;

    let profile_id = business_profile.profile_id.clone();

    let business_profile = db
        .insert_business_profile(key_manager_state, &key_store, business_profile)
        .await
        .to_duplicate_response(errors::ApiErrorResponse::GenericDuplicateError {
            message: format!("Business Profile with the profile_id {profile_id} already exists"),
        })
        .attach_printable("Failed to insert Business profile because of duplication error")?;

    #[cfg(all(
        any(feature = "v1", feature = "v2"),
        not(feature = "business_profile_v2")
    ))]
    if merchant_account.default_profile.is_some() {
        let unset_default_profile = domain::MerchantAccountUpdate::UnsetDefaultProfile;
        db.update_merchant(
            key_manager_state,
            merchant_account,
            unset_default_profile,
            &key_store,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
    }

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::BusinessProfileResponse::foreign_try_from(business_profile)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to parse business profile details")?,
    ))
}

pub async fn list_business_profile(
    state: SessionState,
    merchant_id: id_type::MerchantId,
) -> RouterResponse<Vec<api_models::admin::BusinessProfileResponse>> {
    let db = state.store.as_ref();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            &(&state).into(),
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
    let profiles = db
        .list_business_profile_by_merchant_id(&(&state).into(), &key_store, &merchant_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::InternalServerError)?
        .clone();
    let mut business_profiles = Vec::new();
    for profile in profiles {
        let business_profile =
            api_models::admin::BusinessProfileResponse::foreign_try_from(profile)
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to parse business profile details")?;
        business_profiles.push(business_profile);
    }

    Ok(service_api::ApplicationResponse::Json(business_profiles))
}

pub async fn retrieve_business_profile(
    state: SessionState,
    profile_id: String,
    merchant_id: id_type::MerchantId,
) -> RouterResponse<api_models::admin::BusinessProfileResponse> {
    let db = state.store.as_ref();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            &(&state).into(),
            &merchant_id,
            &db.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)?;
    let business_profile = db
        .find_business_profile_by_profile_id(&(&state).into(), &key_store, &profile_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id,
        })?;

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::BusinessProfileResponse::foreign_try_from(business_profile)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to parse business profile details")?,
    ))
}

pub async fn delete_business_profile(
    state: SessionState,
    profile_id: String,
    merchant_id: &id_type::MerchantId,
) -> RouterResponse<bool> {
    let db = state.store.as_ref();
    let delete_result = db
        .delete_business_profile_by_profile_id_merchant_id(&profile_id, merchant_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id,
        })?;

    Ok(service_api::ApplicationResponse::Json(delete_result))
}

#[cfg(feature = "olap")]
#[async_trait::async_trait]
trait BusinessProfileUpdateBridge {
    async fn get_update_business_profile_object(
        self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfileUpdate>;
}

#[cfg(all(
    feature = "olap",
    any(feature = "v1", feature = "v2"),
    not(feature = "business_profile_v2")
))]
#[async_trait::async_trait]
impl BusinessProfileUpdateBridge for api::BusinessProfileUpdate {
    async fn get_update_business_profile_object(
        self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfileUpdate> {
        if let Some(session_expiry) = &self.session_expiry {
            helpers::validate_session_expiry(session_expiry.to_owned())?;
        }

        if let Some(intent_fulfillment_expiry) = self.intent_fulfillment_time {
            helpers::validate_intent_fulfillment_expiry(intent_fulfillment_expiry)?;
        }

        let webhook_details = self.webhook_details.map(ForeignInto::foreign_into);

        if let Some(ref routing_algorithm) = self.routing_algorithm {
            let _: api_models::routing::RoutingAlgorithm = routing_algorithm
                .clone()
                .parse_value("RoutingAlgorithm")
                .change_context(errors::ApiErrorResponse::InvalidDataValue {
                    field_name: "routing_algorithm",
                })
                .attach_printable("Invalid routing algorithm given")?;
        }

        let payment_link_config = self
            .payment_link_config
            .map(|payment_link_conf| match payment_link_conf.validate() {
                Ok(_) => Ok(payment_link_conf.foreign_into()),
                Err(e) => Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: e.to_string()
                })),
            })
            .transpose()?;

        let extended_card_info_config = self
            .extended_card_info_config
            .as_ref()
            .map(|config| {
                config.encode_to_value().change_context(
                    errors::ApiErrorResponse::InvalidDataValue {
                        field_name: "extended_card_info_config",
                    },
                )
            })
            .transpose()?
            .map(Secret::new);
        let outgoing_webhook_custom_http_headers = self
            .outgoing_webhook_custom_http_headers
            .async_map(|headers| cards::create_encrypted_data(state, key_store, headers))
            .await
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt outgoing webhook custom HTTP headers")?;

        let payout_link_config = self
            .payout_link_config
            .map(|payout_conf| match payout_conf.config.validate() {
                Ok(_) => Ok(payout_conf.foreign_into()),
                Err(e) => Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: e.to_string()
                })),
            })
            .transpose()?;

        Ok(domain::BusinessProfileUpdate::Update(Box::new(
            domain::BusinessProfileGeneralUpdate {
                profile_name: self.profile_name,
                return_url: self.return_url.map(|return_url| return_url.to_string()),
                enable_payment_response_hash: self.enable_payment_response_hash,
                payment_response_hash_key: self.payment_response_hash_key,
                redirect_to_merchant_with_http_post: self.redirect_to_merchant_with_http_post,
                webhook_details,
                metadata: self.metadata,
                routing_algorithm: self.routing_algorithm,
                intent_fulfillment_time: self.intent_fulfillment_time.map(i64::from),
                frm_routing_algorithm: self.frm_routing_algorithm,
                #[cfg(feature = "payouts")]
                payout_routing_algorithm: self.payout_routing_algorithm,
                #[cfg(not(feature = "payouts"))]
                payout_routing_algorithm: None,
                applepay_verified_domains: self.applepay_verified_domains,
                payment_link_config,
                session_expiry: self.session_expiry.map(i64::from),
                authentication_connector_details: self
                    .authentication_connector_details
                    .map(ForeignInto::foreign_into),
                payout_link_config,
                extended_card_info_config,
                use_billing_as_payment_method_billing: self.use_billing_as_payment_method_billing,
                collect_shipping_details_from_wallet_connector: self
                    .collect_shipping_details_from_wallet_connector,
                collect_billing_details_from_wallet_connector: self
                    .collect_billing_details_from_wallet_connector,
                is_connector_agnostic_mit_enabled: self.is_connector_agnostic_mit_enabled,
                outgoing_webhook_custom_http_headers: outgoing_webhook_custom_http_headers
                    .map(Into::into),
            },
        )))
    }
}

#[cfg(all(feature = "olap", feature = "v2", feature = "business_profile_v2"))]
#[async_trait::async_trait]
impl BusinessProfileUpdateBridge for api::BusinessProfileUpdate {
    async fn get_update_business_profile_object(
        self,
        state: &SessionState,
        key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<domain::BusinessProfileUpdate> {
        if let Some(session_expiry) = &self.session_expiry {
            helpers::validate_session_expiry(session_expiry.to_owned())?;
        }

        let webhook_details = self.webhook_details.map(ForeignInto::foreign_into);

        let payment_link_config = self
            .payment_link_config
            .map(|payment_link_conf| match payment_link_conf.validate() {
                Ok(_) => Ok(payment_link_conf.foreign_into()),
                Err(e) => Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: e.to_string()
                })),
            })
            .transpose()?;

        let extended_card_info_config = self
            .extended_card_info_config
            .as_ref()
            .map(|config| {
                config.encode_to_value().change_context(
                    errors::ApiErrorResponse::InvalidDataValue {
                        field_name: "extended_card_info_config",
                    },
                )
            })
            .transpose()?
            .map(Secret::new);
        let outgoing_webhook_custom_http_headers = self
            .outgoing_webhook_custom_http_headers
            .async_map(|headers| cards::create_encrypted_data(state, key_store, headers))
            .await
            .transpose()
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Unable to encrypt outgoing webhook custom HTTP headers")?;

        let payout_link_config = self
            .payout_link_config
            .map(|payout_conf| match payout_conf.config.validate() {
                Ok(_) => Ok(payout_conf.foreign_into()),
                Err(e) => Err(report!(errors::ApiErrorResponse::InvalidRequestData {
                    message: e.to_string()
                })),
            })
            .transpose()?;

        Ok(domain::BusinessProfileUpdate::Update(Box::new(
            domain::BusinessProfileGeneralUpdate {
                profile_name: self.profile_name,
                return_url: self.return_url.map(|return_url| return_url.to_string()),
                enable_payment_response_hash: self.enable_payment_response_hash,
                payment_response_hash_key: self.payment_response_hash_key,
                redirect_to_merchant_with_http_post: self.redirect_to_merchant_with_http_post,
                webhook_details,
                metadata: self.metadata,
                applepay_verified_domains: self.applepay_verified_domains,
                payment_link_config,
                session_expiry: self.session_expiry.map(i64::from),
                authentication_connector_details: self
                    .authentication_connector_details
                    .map(ForeignInto::foreign_into),
                payout_link_config,
                extended_card_info_config,
                use_billing_as_payment_method_billing: self.use_billing_as_payment_method_billing,
                collect_shipping_details_from_wallet_connector: self
                    .collect_shipping_details_from_wallet_connector,
                collect_billing_details_from_wallet_connector: self
                    .collect_billing_details_from_wallet_connector,
                is_connector_agnostic_mit_enabled: self.is_connector_agnostic_mit_enabled,
                outgoing_webhook_custom_http_headers: outgoing_webhook_custom_http_headers
                    .map(Into::into),
                order_fulfillment_time: self
                    .order_fulfillment_time
                    .map(|order_fulfillment_time| order_fulfillment_time.into_inner()),
                order_fulfillment_time_origin: self.order_fulfillment_time_origin,
            },
        )))
    }
}

#[cfg(feature = "olap")]
pub async fn update_business_profile(
    state: SessionState,
    profile_id: &str,
    merchant_id: &id_type::MerchantId,
    request: api::BusinessProfileUpdate,
) -> RouterResponse<api::BusinessProfileResponse> {
    let db = state.store.as_ref();
    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            &(&state).into(),
            merchant_id,
            &state.store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)
        .attach_printable("Error while fetching the key store by merchant_id")?;
    let key_manager_state = &(&state).into();

    let business_profile = db
        .find_business_profile_by_profile_id(key_manager_state, &key_store, profile_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_owned(),
        })?;

    if business_profile.merchant_id != *merchant_id {
        Err(errors::ApiErrorResponse::AccessForbidden {
            resource: profile_id.to_string(),
        })?
    }

    let business_profile_update = request
        .get_update_business_profile_object(&state, &key_store)
        .await?;

    let updated_business_profile = db
        .update_business_profile_by_profile_id(
            key_manager_state,
            &key_store,
            business_profile,
            business_profile_update,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_owned(),
        })?;

    Ok(service_api::ApplicationResponse::Json(
        api_models::admin::BusinessProfileResponse::foreign_try_from(updated_business_profile)
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Failed to parse business profile details")?,
    ))
}

#[cfg(all(
    feature = "v2",
    feature = "routing_v2",
    feature = "business_profile_v2"
))]
#[derive(Clone, Debug)]
pub struct BusinessProfileWrapper {
    pub profile: domain::BusinessProfile,
}

#[cfg(all(
    feature = "v2",
    feature = "routing_v2",
    feature = "business_profile_v2"
))]
impl BusinessProfileWrapper {
    pub fn new(profile: domain::BusinessProfile) -> Self {
        Self { profile }
    }
    fn get_routing_config_cache_key(self) -> storage_impl::redis::cache::CacheKind<'static> {
        let merchant_id = self.profile.merchant_id.clone();

        let profile_id = self.profile.profile_id.clone();

        storage_impl::redis::cache::CacheKind::Routing(
            format!(
                "routing_config_{}_{profile_id}",
                merchant_id.get_string_repr()
            )
            .into(),
        )
    }

    pub async fn update_business_profile_and_invalidate_routing_config_for_active_algorithm_id_update(
        self,
        db: &dyn StorageInterface,
        key_manager_state: &KeyManagerState,
        merchant_key_store: &domain::MerchantKeyStore,
        algorithm_id: String,
        transaction_type: &storage::enums::TransactionType,
    ) -> RouterResult<()> {
        let routing_cache_key = self.clone().get_routing_config_cache_key();

        let (routing_algorithm_id, payout_routing_algorithm_id) = match transaction_type {
            storage::enums::TransactionType::Payment => (Some(algorithm_id), None),
            #[cfg(feature = "payouts")]
            storage::enums::TransactionType::Payout => (None, Some(algorithm_id)),
        };

        let business_profile_update = domain::BusinessProfileUpdate::RoutingAlgorithmUpdate {
            routing_algorithm_id,
            payout_routing_algorithm_id,
        };

        let profile = self.profile;

        db.update_business_profile_by_profile_id(
            key_manager_state,
            merchant_key_store,
            profile,
            business_profile_update,
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to update routing algorithm ref in business profile")?;

        storage_impl::redis::cache::publish_into_redact_channel(
            db.get_cache_store().as_ref(),
            [routing_cache_key],
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to invalidate routing cache")?;
        Ok(())
    }

    pub fn get_profile_id_and_routing_algorithm_id<F>(
        &self,
        transaction_data: &routing::TransactionData<'_, F>,
    ) -> (Option<String>, Option<String>)
    where
        F: Send + Clone,
    {
        match transaction_data {
            routing::TransactionData::Payment(payment_data) => (
                payment_data.payment_intent.profile_id.clone(),
                self.profile.routing_algorithm_id.clone(),
            ),
            #[cfg(feature = "payouts")]
            routing::TransactionData::Payout(payout_data) => (
                Some(payout_data.payout_attempt.profile_id.clone()),
                self.profile.payout_routing_algorithm_id.clone(),
            ),
        }
    }
    pub fn get_default_fallback_list_of_connector_under_profile(
        &self,
    ) -> RouterResult<Vec<routing_types::RoutableConnectorChoice>> {
        use common_utils::ext_traits::OptionExt;
        use masking::ExposeOptionInterface;

        self.profile
            .default_fallback_routing
            .clone()
            .expose_option()
            .parse_value::<Vec<routing_types::RoutableConnectorChoice>>(
                "Vec<RoutableConnectorChoice>",
            )
            .change_context(errors::ApiErrorResponse::InternalServerError)
            .attach_printable("Merchant default config has invalid structure")
    }
    pub fn get_default_routing_configs_from_profile(
        &self,
    ) -> RouterResult<routing_types::ProfileDefaultRoutingConfig> {
        let profile_id = self.profile.profile_id.clone();
        let connectors = self.get_default_fallback_list_of_connector_under_profile()?;

        Ok(routing_types::ProfileDefaultRoutingConfig {
            profile_id,
            connectors,
        })
    }

    pub async fn update_default_routing_for_profile(
        self,
        db: &dyn StorageInterface,
        updated_config: &Vec<routing_types::RoutableConnectorChoice>,
        key_manager_state: &KeyManagerState,
        merchant_key_store: &domain::MerchantKeyStore,
    ) -> RouterResult<()> {
        let default_fallback_routing = Secret::from(
            updated_config
                .encode_to_value()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("Failed to convert routing ref to value")?,
        );
        let business_profile_update = domain::BusinessProfileUpdate::DefaultRoutingFallbackUpdate {
            default_fallback_routing: Some(default_fallback_routing),
        };

        db.update_business_profile_by_profile_id(
            key_manager_state,
            merchant_key_store,
            self.profile,
            business_profile_update,
        )
        .await
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to update routing algorithm ref in business profile")?;
        Ok(())
    }
}

pub async fn extended_card_info_toggle(
    state: SessionState,
    merchant_id: &id_type::MerchantId,
    profile_id: &str,
    ext_card_info_choice: admin_types::ExtendedCardInfoChoice,
) -> RouterResponse<admin_types::ExtendedCardInfoChoice> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();

    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &state.store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)
        .attach_printable("Error while fetching the key store by merchant_id")?;

    let business_profile = db
        .find_business_profile_by_profile_id(key_manager_state, &key_store, profile_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_string(),
        })?;

    if business_profile.is_extended_card_info_enabled.is_none()
        || business_profile
            .is_extended_card_info_enabled
            .is_some_and(|existing_config| existing_config != ext_card_info_choice.enabled)
    {
        let business_profile_update = domain::BusinessProfileUpdate::ExtendedCardInfoUpdate {
            is_extended_card_info_enabled: Some(ext_card_info_choice.enabled),
        };

        db.update_business_profile_by_profile_id(
            key_manager_state,
            &key_store,
            business_profile,
            business_profile_update,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_owned(),
        })?;
    }

    Ok(service_api::ApplicationResponse::Json(ext_card_info_choice))
}

pub async fn connector_agnostic_mit_toggle(
    state: SessionState,
    merchant_id: &id_type::MerchantId,
    profile_id: &str,
    connector_agnostic_mit_choice: admin_types::ConnectorAgnosticMitChoice,
) -> RouterResponse<admin_types::ConnectorAgnosticMitChoice> {
    let db = state.store.as_ref();
    let key_manager_state = &(&state).into();

    let key_store = db
        .get_merchant_key_store_by_merchant_id(
            key_manager_state,
            merchant_id,
            &state.store.get_master_key().to_vec().into(),
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::MerchantAccountNotFound)
        .attach_printable("Error while fetching the key store by merchant_id")?;

    let business_profile = db
        .find_business_profile_by_profile_id(key_manager_state, &key_store, profile_id)
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_string(),
        })?;

    if business_profile.merchant_id != *merchant_id {
        Err(errors::ApiErrorResponse::AccessForbidden {
            resource: profile_id.to_string(),
        })?
    }

    if business_profile.is_connector_agnostic_mit_enabled
        != Some(connector_agnostic_mit_choice.enabled)
    {
        let business_profile_update = domain::BusinessProfileUpdate::ConnectorAgnosticMitUpdate {
            is_connector_agnostic_mit_enabled: Some(connector_agnostic_mit_choice.enabled),
        };

        db.update_business_profile_by_profile_id(
            key_manager_state,
            &key_store,
            business_profile,
            business_profile_update,
        )
        .await
        .to_not_found_response(errors::ApiErrorResponse::BusinessProfileNotFound {
            id: profile_id.to_owned(),
        })?;
    }

    Ok(service_api::ApplicationResponse::Json(
        connector_agnostic_mit_choice,
    ))
}

pub async fn transfer_key_store_to_key_manager(
    state: SessionState,
    req: admin_types::MerchantKeyTransferRequest,
) -> RouterResponse<admin_types::TransferKeyResponse> {
    let resp = transfer_encryption_key(&state, req).await?;

    Ok(service_api::ApplicationResponse::Json(
        admin_types::TransferKeyResponse {
            total_transferred: resp,
        },
    ))
}

async fn process_open_banking_connectors(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    auth: &types::ConnectorAuthType,
    connector_type: &api_enums::ConnectorType,
    connector: &api_enums::Connector,
    additional_merchant_data: types::AdditionalMerchantData,
) -> RouterResult<types::MerchantRecipientData> {
    let new_merchant_data = match additional_merchant_data {
        types::AdditionalMerchantData::OpenBankingRecipientData(merchant_data) => {
            if connector_type != &api_enums::ConnectorType::PaymentProcessor {
                return Err(errors::ApiErrorResponse::InvalidConnectorConfiguration {
                    config:
                        "OpenBanking connector for Payment Initiation should be a payment processor"
                            .to_string(),
                }
                .into());
            }
            match &merchant_data {
                types::MerchantRecipientData::AccountData(acc_data) => {
                    validate_bank_account_data(acc_data)?;

                    let connector_name = api_enums::Connector::to_string(connector);

                    let recipient_creation_not_supported = state
                        .conf
                        .locker_based_open_banking_connectors
                        .connector_list
                        .contains(connector_name.as_str());

                    let recipient_id = if recipient_creation_not_supported {
                        locker_recipient_create_call(state, merchant_id, acc_data).await
                    } else {
                        connector_recipient_create_call(
                            state,
                            merchant_id,
                            connector_name,
                            auth,
                            acc_data,
                        )
                        .await
                    }
                    .attach_printable("failed to get recipient_id")?;

                    let conn_recipient_id = if recipient_creation_not_supported {
                        Some(types::RecipientIdType::LockerId(Secret::new(recipient_id)))
                    } else {
                        Some(types::RecipientIdType::ConnectorId(Secret::new(
                            recipient_id,
                        )))
                    };

                    let account_data = match &acc_data {
                        types::MerchantAccountData::Iban { iban, name, .. } => {
                            types::MerchantAccountData::Iban {
                                iban: iban.clone(),
                                name: name.clone(),
                                connector_recipient_id: conn_recipient_id.clone(),
                            }
                        }
                        types::MerchantAccountData::Bacs {
                            account_number,
                            sort_code,
                            name,
                            ..
                        } => types::MerchantAccountData::Bacs {
                            account_number: account_number.clone(),
                            sort_code: sort_code.clone(),
                            name: name.clone(),
                            connector_recipient_id: conn_recipient_id.clone(),
                        },
                    };

                    types::MerchantRecipientData::AccountData(account_data)
                }
                _ => merchant_data.clone(),
            }
        }
    };

    Ok(new_merchant_data)
}

fn validate_bank_account_data(data: &types::MerchantAccountData) -> RouterResult<()> {
    match data {
        types::MerchantAccountData::Iban { iban, .. } => {
            // IBAN check algorithm
            if iban.peek().len() > IBAN_MAX_LENGTH {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "IBAN length must be up to 34 characters".to_string(),
                }
                .into());
            }
            let pattern = Regex::new(r"^[A-Z0-9]*$")
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("failed to create regex pattern")?;

            let mut iban = iban.peek().to_string();

            if !pattern.is_match(iban.as_str()) {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "IBAN data must be alphanumeric".to_string(),
                }
                .into());
            }

            // MOD check
            let first_4 = iban.chars().take(4).collect::<String>();
            iban.push_str(first_4.as_str());
            let len = iban.len();

            let rearranged_iban = iban
                .chars()
                .rev()
                .take(len - 4)
                .collect::<String>()
                .chars()
                .rev()
                .collect::<String>();

            let mut result = String::new();

            rearranged_iban.chars().for_each(|c| {
                if c.is_ascii_uppercase() {
                    let digit = (u32::from(c) - u32::from('A')) + 10;
                    result.push_str(&format!("{:02}", digit));
                } else {
                    result.push(c);
                }
            });

            let num = result
                .parse::<u128>()
                .change_context(errors::ApiErrorResponse::InternalServerError)
                .attach_printable("failed to validate IBAN")?;

            if num % 97 != 1 {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid IBAN".to_string(),
                }
                .into());
            }

            Ok(())
        }
        types::MerchantAccountData::Bacs {
            account_number,
            sort_code,
            ..
        } => {
            if account_number.peek().len() > BACS_MAX_ACCOUNT_NUMBER_LENGTH
                || sort_code.peek().len() != BACS_SORT_CODE_LENGTH
            {
                return Err(errors::ApiErrorResponse::InvalidRequestData {
                    message: "Invalid BACS numbers".to_string(),
                }
                .into());
            }

            Ok(())
        }
    }
}

async fn connector_recipient_create_call(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    connector_name: String,
    auth: &types::ConnectorAuthType,
    data: &types::MerchantAccountData,
) -> RouterResult<String> {
    let connector = pm_auth_types::api::PaymentAuthConnectorData::get_connector_by_name(
        connector_name.as_str(),
    )?;

    let auth = pm_auth_types::ConnectorAuthType::foreign_try_from(auth.clone())
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed while converting ConnectorAuthType")?;

    let connector_integration: pm_auth_types::api::BoxedConnectorIntegration<
        '_,
        pm_auth_types::api::auth_service::RecipientCreate,
        pm_auth_types::RecipientCreateRequest,
        pm_auth_types::RecipientCreateResponse,
    > = connector.connector.get_connector_integration();

    let req = match data {
        types::MerchantAccountData::Iban { iban, name, .. } => {
            pm_auth_types::RecipientCreateRequest {
                name: name.clone(),
                account_data: pm_auth_types::RecipientAccountData::Iban(iban.clone()),
                address: None,
            }
        }
        types::MerchantAccountData::Bacs {
            account_number,
            sort_code,
            name,
            ..
        } => pm_auth_types::RecipientCreateRequest {
            name: name.clone(),
            account_data: pm_auth_types::RecipientAccountData::Bacs {
                sort_code: sort_code.clone(),
                account_number: account_number.clone(),
            },
            address: None,
        },
    };

    let router_data = pm_auth_types::RecipientCreateRouterData {
        flow: std::marker::PhantomData,
        merchant_id: Some(merchant_id.to_owned()),
        connector: Some(connector_name),
        request: req,
        response: Err(pm_auth_types::ErrorResponse {
            status_code: http::StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
            code: consts::NO_ERROR_CODE.to_string(),
            message: consts::UNSUPPORTED_ERROR_MESSAGE.to_string(),
            reason: None,
        }),
        connector_http_status_code: None,
        connector_auth_type: auth,
    };

    let resp = payment_initiation_service::execute_connector_processing_step(
        state,
        connector_integration,
        &router_data,
        &connector.connector_name,
    )
    .await
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Failed while calling recipient create connector api")?;

    let recipient_create_resp =
        resp.response
            .map_err(|err| errors::ApiErrorResponse::ExternalConnectorError {
                code: err.code,
                message: err.message,
                connector: connector.connector_name.to_string(),
                status_code: err.status_code,
                reason: err.reason,
            })?;

    let recipient_id = recipient_create_resp.recipient_id;

    Ok(recipient_id)
}

async fn locker_recipient_create_call(
    state: &SessionState,
    merchant_id: &id_type::MerchantId,
    data: &types::MerchantAccountData,
) -> RouterResult<String> {
    let enc_data = serde_json::to_string(data)
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to convert to MerchantAccountData json to String")?;

    let merchant_id_string = merchant_id.get_string_repr().to_owned();

    let cust_id = id_type::CustomerId::try_from(std::borrow::Cow::from(merchant_id_string))
        .change_context(errors::ApiErrorResponse::InternalServerError)
        .attach_printable("Failed to convert to CustomerId")?;

    let payload = transformers::StoreLockerReq::LockerGeneric(transformers::StoreGenericReq {
        merchant_id: merchant_id.to_owned(),
        merchant_customer_id: cust_id.clone(),
        enc_data,
        ttl: state.conf.locker.ttl_for_storage_in_secs,
    });

    let store_resp = cards::call_to_locker_hs(
        state,
        &payload,
        &cust_id,
        api_enums::LockerChoice::HyperswitchCardVault,
    )
    .await
    .change_context(errors::ApiErrorResponse::InternalServerError)
    .attach_printable("Failed to encrypt merchant bank account data")?;

    Ok(store_resp.card_reference)
}
