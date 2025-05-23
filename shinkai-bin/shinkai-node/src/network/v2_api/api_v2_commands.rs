use rusqlite::params;
use serde_json::{json, Value};
use shinkai_sqlite::regex_pattern_manager::RegexPattern;
use shinkai_tools_primitives::tools::agent_tool_wrapper::AgentToolWrapper;
use shinkai_tools_primitives::tools::shinkai_tool::ShinkaiTool;
use std::fs::File;
use std::io::{Read, Write};
use std::{env, sync::Arc};
use tokio::fs;
use zip::ZipArchive;
use zip::{write::FileOptions, ZipWriter};

use async_channel::Sender;
use ed25519_dalek::{SigningKey, VerifyingKey};
use reqwest::StatusCode;

use shinkai_embedding::{embedding_generator::RemoteEmbeddingGenerator, model_type::EmbeddingModelType};
use shinkai_http_api::{
    api_v1::api_v1_handlers::APIUseRegistrationCodeSuccessResponse,
    api_v2::api_v2_handlers_general::InitialRegistrationRequest,
    node_api_router::{APIError, GetPublicKeysResponse},
};
use shinkai_message_primitives::{
    schemas::ws_types::WSUpdateHandler,
    shinkai_message::shinkai_message_schemas::JobCreationInfo,
    shinkai_utils::{job_scope::MinimalJobScope, shinkai_time::ShinkaiStringTime},
};
use shinkai_message_primitives::{
    schemas::{
        identity::{Identity, IdentityType, RegistrationCode},
        inbox_name::InboxName,
        llm_providers::{agent::Agent, serialized_llm_provider::SerializedLLMProvider},
        shinkai_name::ShinkaiName,
        tool_router_key::ToolRouterKey,
    },
    shinkai_message::{
        shinkai_message::{MessageBody, MessageData, ShinkaiMessage},
        shinkai_message_schemas::{
            APIAddOllamaModels, IdentityPermissions, JobMessage, MessageSchemaType, V2ChatMessage,
        },
    },
    shinkai_utils::{
        encryption::{encryption_public_key_to_string, EncryptionMethod},
        shinkai_message_builder::ShinkaiMessageBuilder,
        signatures::signature_public_key_to_string,
    },
};
use shinkai_sqlite::SqliteManager;
use tokio::sync::Mutex;
use x25519_dalek::PublicKey as EncryptionPublicKey;

use crate::llm_provider::providers::shinkai_backend::check_quota;
use crate::network::node_shareable_logic::ZipFileContents;
use crate::utils::environment::NodeEnvironment;
use shinkai_message_primitives::schemas::llm_providers::shinkai_backend::QuotaResponse;

use crate::managers::galxe_quests::{compute_quests, generate_proof};
use crate::managers::tool_router::ToolRouter;
use crate::{
    llm_provider::{job_manager::JobManager, llm_stopper::LLMStopper},
    managers::{identity_manager::IdentityManagerTrait, IdentityManager},
    network::{node_error::NodeError, node_shareable_logic::download_zip_file, Node},
    tools::tool_generation,
    utils::update_global_identity::update_global_identity_name,
};

use shinkai_message_primitives::schemas::shinkai_preferences::ShinkaiInternalComms;
use std::collections::HashMap;
use std::time::Instant;
use tokio::time::Duration;
use x25519_dalek::StaticSecret as EncryptionStaticKey;

#[cfg(debug_assertions)]
fn check_bearer_token(api_key: &str, bearer: &str) -> Result<(), ()> {
    if api_key == bearer || bearer == "debug" {
        return Ok(());
    } else {
        return Err(());
    }
}

#[cfg(not(debug_assertions))]
fn check_bearer_token(api_key: &str, bearer: &str) -> Result<(), ()> {
    if api_key == bearer {
        return Ok(());
    } else {
        return Err(());
    }
}

impl Node {
    pub async fn validate_bearer_token<T>(
        bearer: &str,
        db: Arc<SqliteManager>,
        res: &Sender<Result<T, APIError>>,
    ) -> Result<(), ()> {
        // Compare bearer token to the environment variable API_V2_KEY
        let api_key = match env::var("API_V2_KEY") {
            Ok(api_key) => api_key,
            Err(_) => {
                // If the environment variable is not set, read from the database
                match db.read_api_v2_key() {
                    Ok(Some(api_key)) => api_key,
                    Ok(None) | Err(_) => {
                        let api_error = APIError {
                            code: StatusCode::UNAUTHORIZED.as_u16(),
                            error: "Unauthorized".to_string(),
                            message: "Invalid bearer token".to_string(),
                        };
                        let _ = res.send(Err(api_error)).await;
                        return Err(());
                    }
                }
            }
        };

        let result = check_bearer_token(&api_key, bearer);
        match result {
            Ok(_) => Ok(()),
            Err(_) => {
                let api_error = APIError {
                    code: StatusCode::UNAUTHORIZED.as_u16(),
                    error: "Unauthorized".to_string(),
                    message: "Invalid bearer token".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                Err(())
            }
        }
    }

    pub async fn get_bearer_token<T>(
        db: Arc<SqliteManager>,
        res: &Sender<Result<T, APIError>>,
    ) -> Result<String, NodeError> {
        let api_key = match env::var("API_V2_KEY") {
            Ok(api_key) => api_key,
            Err(_) => {
                // If the environment variable is not set, read from the database
                match db.read_api_v2_key() {
                    Ok(Some(api_key)) => api_key,
                    Ok(None) | Err(_) => {
                        let api_error = APIError {
                            code: StatusCode::UNAUTHORIZED.as_u16(),
                            error: "Unauthorized".to_string(),
                            message: "Invalid bearer token".to_string(),
                        };
                        let _ = res.send(Err(api_error)).await;
                        return Err(NodeError {
                            message: "Invalid bearer token".to_string(),
                        });
                    }
                }
            }
        };
        Ok(api_key)
    }

    pub fn convert_shinkai_message_to_v2_chat_message(
        shinkai_message: ShinkaiMessage,
    ) -> Result<V2ChatMessage, NodeError> {
        let internal_metadata = match &shinkai_message.body {
            MessageBody::Unencrypted(body) => Ok(&body.internal_metadata),
            _ => Err(NodeError {
                message: "Missing internal metadata".to_string(),
            }),
        }?;

        let message_data = match &shinkai_message.body {
            MessageBody::Unencrypted(body) => match &body.message_data {
                MessageData::Unencrypted(data) => Ok(data),
                _ => Err(NodeError {
                    message: "Missing message data".to_string(),
                }),
            },
            _ => Err(NodeError {
                message: "Missing message data".to_string(),
            }),
        }?;

        let external_metadata = shinkai_message.external_metadata;

        let job_message: JobMessage =
            serde_json::from_str(&message_data.message_raw_content).map_err(|e| NodeError {
                message: format!("Failed to parse job message content: {}", e),
            })?;

        let node_api_data = internal_metadata.node_api_data.clone().ok_or(NodeError {
            message: "Missing node API data".to_string(),
        })?;

        Ok(V2ChatMessage {
            job_message,
            sender: external_metadata.sender,
            sender_subidentity: internal_metadata.sender_subidentity.clone(),
            receiver: external_metadata.recipient,
            receiver_subidentity: internal_metadata.recipient_subidentity.clone(),
            node_api_data,
            inbox: internal_metadata.inbox.clone(),
        })
    }

    pub fn convert_shinkai_messages_to_v2_chat_messages(
        shinkai_messages: Vec<Vec<ShinkaiMessage>>,
    ) -> Result<Vec<Vec<V2ChatMessage>>, NodeError> {
        shinkai_messages
            .into_iter()
            .map(|message_group| {
                message_group
                    .into_iter()
                    .map(Self::convert_shinkai_message_to_v2_chat_message)
                    .collect::<Result<Vec<V2ChatMessage>, NodeError>>()
            })
            .collect::<Result<Vec<Vec<V2ChatMessage>>, NodeError>>()
    }

    pub fn api_v2_create_shinkai_message(
        sender: ShinkaiName,
        receiver: ShinkaiName,
        payload: &str,
        schema: MessageSchemaType,
        node_encryption_sk: EncryptionStaticKey,
        node_signing_sk: SigningKey,
        receiver_public_key: EncryptionPublicKey,
        job: Option<String>,
    ) -> Result<ShinkaiMessage, &'static str> {
        let timestamp = ShinkaiStringTime::generate_time_now();
        let sender_subidentity = sender.get_fullname_string_without_node_name().unwrap_or_default();
        let receiver_subidentity = receiver.get_fullname_string_without_node_name().unwrap_or_default();

        let inbox_name = job
            .map(|job_id| {
                InboxName::get_job_inbox_name_from_params(job_id)
                    .map(|inbox| inbox.to_string())
                    .unwrap_or_else(|_| "".to_string())
            })
            .unwrap_or_else(|| "".to_string());

        ShinkaiMessageBuilder::new(node_encryption_sk, node_signing_sk, receiver_public_key)
            .message_raw_content(payload.to_string())
            .body_encryption(EncryptionMethod::None)
            .message_schema_type(schema)
            .internal_metadata_with_inbox(
                sender_subidentity.to_string(),
                receiver_subidentity.to_string(),
                inbox_name,
                EncryptionMethod::None,
                None,
            )
            .external_metadata_with_schedule(receiver.node_name.to_string(), sender.node_name.to_string(), timestamp)
            .build()
    }

    pub async fn v2_send_public_keys(
        identity_public_key: VerifyingKey,
        encryption_public_key: EncryptionPublicKey,
        res: Sender<Result<GetPublicKeysResponse, APIError>>,
    ) -> Result<(), NodeError> {
        let public_keys_response = GetPublicKeysResponse {
            signature_public_key: signature_public_key_to_string(identity_public_key),
            encryption_public_key: encryption_public_key_to_string(encryption_public_key),
        };

        if let Err(_) = res.send(Ok(public_keys_response)).await {
            let api_error = APIError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "Internal Server Error",
                "Failed to send public keys",
            );
            let _ = res.send(Err(api_error)).await;
        }

        Ok(())
    }

    pub async fn v2_handle_initial_registration(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        node_name: ShinkaiName,
        payload: InitialRegistrationRequest,
        public_https_certificate: Option<String>,
        res: Sender<Result<APIUseRegistrationCodeSuccessResponse, APIError>>,

        first_device_needs_registration_code: bool,
        embedding_generator: Arc<RemoteEmbeddingGenerator>,
        job_manager: Arc<Mutex<JobManager>>,
        encryption_public_key: EncryptionPublicKey,
        identity_public_key: VerifyingKey,
        identity_secret_key: SigningKey,
        initial_llm_providers: Vec<SerializedLLMProvider>,
        ws_manager: Option<Arc<Mutex<dyn WSUpdateHandler + Send>>>,
        supported_embedding_models: Arc<Mutex<Vec<EmbeddingModelType>>>,
    ) {
        let registration_code = RegistrationCode {
            code: "".to_string(),
            registration_name: "main".to_string(),
            profile_identity_pk: payload.profile_identity_pk.clone(),
            profile_encryption_pk: payload.profile_encryption_pk.clone(),
            device_identity_pk: payload.profile_identity_pk,
            device_encryption_pk: payload.profile_encryption_pk,
            identity_type: IdentityType::Device,
            permission_type: IdentityPermissions::Admin,
        };

        match Self::handle_registration_code_usage(
            db,
            node_name,
            first_device_needs_registration_code,
            embedding_generator,
            identity_manager,
            job_manager,
            encryption_public_key,
            identity_public_key,
            identity_secret_key,
            initial_llm_providers,
            registration_code,
            ws_manager,
            supported_embedding_models,
            public_https_certificate,
            res.clone(),
        )
        .await
        {
            Ok(_) => {}
            Err(err) => {
                let error = APIError {
                    code: 500,
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to handle registration code usage: {}", err),
                };
                let _ = res.send(Err(error)).await;
            }
        }
    }

    pub async fn v2_api_get_storage_location(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }
        let node_storage_path: Option<String> = match env::var("NODE_STORAGE_PATH").ok() {
            Some(val) => Some(val),
            None => Some("storage".to_string()),
        };
        let base_path = tokio::fs::canonicalize(node_storage_path.as_ref().unwrap())
            .await
            .unwrap();
        let _ = res.send(Ok(base_path.to_string_lossy().to_string())).await;

        Ok(())
    }

    pub async fn v2_api_get_default_embedding_model(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Get the default embedding model from the database
        match db.get_default_embedding_model() {
            Ok(model) => {
                let _ = res.send(Ok(model.to_string())).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to get default embedding model: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_get_supported_embedding_models(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<Vec<String>, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Get the supported embedding models from the database
        match db.get_supported_embedding_models() {
            Ok(models) => {
                let model_names: Vec<String> = models.into_iter().map(|model| model.to_string()).collect();
                let _ = res.send(Ok(model_names)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to get supported embedding models: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_update_default_embedding_model(
        db: Arc<SqliteManager>,
        bearer: String,
        model_name: String,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Convert the string to EmbeddingModelType
        let new_default_model = match EmbeddingModelType::from_string(&model_name) {
            Ok(model) => model,
            Err(_) => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Invalid embedding model provided".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Update the default embedding model in the database
        match db.update_default_embedding_model(new_default_model) {
            Ok(_) => {
                let _ = res
                    .send(Ok("Default embedding model updated successfully".to_string()))
                    .await;
                Ok(())
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to update default embedding model: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_add_llm_provider(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        job_manager: Option<Arc<Mutex<JobManager>>>,
        identity_secret_key: SigningKey,
        bearer: String,
        agent: SerializedLLMProvider,
        ws_manager: Option<Arc<Mutex<dyn WSUpdateHandler + Send>>>,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let job_manager = match job_manager {
            Some(manager) => manager,
            None => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: "JobManager is required".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        let profile = match identity_manager.lock().await.get_main_identity() {
            Some(Identity::Standard(std_identity)) => std_identity.clone().full_identity_name,
            _ => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Wrong identity type. Expected Standard identity.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        match Self::internal_add_llm_provider(
            db.clone(),
            identity_manager.clone(),
            job_manager,
            identity_secret_key.clone(),
            agent,
            &profile,
            ws_manager,
        )
        .await
        {
            Ok(_) => {
                let _ = res.send(Ok("Agent added successfully".to_string())).await;
                Ok(())
            }
            Err(err) => {
                // Check if the error message indicates a unique constraint violation
                let api_error = if err.to_string().contains("UNIQUE constraint failed") {
                    APIError {
                        code: StatusCode::CONFLICT.as_u16(),
                        error: "Conflict".to_string(),
                        message: "An LLM provider with this ID already exists".to_string(),
                    }
                } else {
                    APIError {
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        error: "Internal Server Error".to_string(),
                        message: format!("{}", err),
                    }
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_remove_llm_provider(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        bearer: String,
        llm_provider_id: String,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let requester_name = match identity_manager.lock().await.get_main_identity() {
            Some(Identity::Standard(std_identity)) => std_identity.clone().full_identity_name,
            _ => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Wrong identity type. Expected Standard identity.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        let mut identity_manager = identity_manager.lock().await;
        match db.remove_llm_provider(&llm_provider_id, &requester_name) {
            Ok(_) => match identity_manager.remove_agent_subidentity(&llm_provider_id).await {
                Ok(_) => {
                    let _ = res.send(Ok("Agent removed successfully".to_string())).await;
                    Ok(())
                }
                Err(err) => {
                    let api_error = APIError {
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        error: "Internal Server Error".to_string(),
                        message: format!("Failed to remove agent from identity manager: {}", err),
                    };
                    let _ = res.send(Err(api_error)).await;
                    Ok(())
                }
            },
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to remove agent: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_modify_llm_provider(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        bearer: String,
        agent: SerializedLLMProvider,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let requester_name = match identity_manager.lock().await.get_main_identity() {
            Some(Identity::Standard(std_identity)) => std_identity.clone().full_identity_name,
            _ => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Wrong identity type. Expected Standard identity.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        match db.update_llm_provider(agent.clone(), &requester_name) {
            Ok(_) => {
                let mut identity_manager = identity_manager.lock().await;
                match identity_manager.modify_llm_provider_subidentity(agent).await {
                    Ok(_) => {
                        let _ = res.send(Ok("Agent modified successfully".to_string())).await;
                        Ok(())
                    }
                    Err(err) => {
                        let api_error = APIError {
                            code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            error: "Internal Server Error".to_string(),
                            message: format!("Failed to update agent in identity manager: {}", err),
                        };
                        let _ = res.send(Err(api_error)).await;
                        Ok(())
                    }
                }
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to update agent: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_check_shinkai_backend_quota(
        db: Arc<SqliteManager>,
        model_type: String,
        bearer: String,
        res: Sender<Result<QuotaResponse, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        match check_quota(db, model_type).await {
            Ok(quota_response) => {
                let _ = res.send(Ok(quota_response)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to fetch quota: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_change_nodes_name(
        bearer: String,
        db: Arc<SqliteManager>,
        secret_file_path: &str,
        identity_manager: Arc<Mutex<IdentityManager>>,
        encryption_public_key: EncryptionPublicKey,
        identity_public_key: VerifyingKey,
        new_name: String,
        res: Sender<Result<(), APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Validate the new node name
        let new_node_name = match ShinkaiName::from_node_name(new_name) {
            Ok(name) => name,
            Err(_) => {
                let _ = res
                    .send(Err(APIError {
                        code: StatusCode::BAD_REQUEST.as_u16(),
                        error: "Bad Request".to_string(),
                        message: "Invalid node name".to_string(),
                    }))
                    .await;
                return Ok(());
            }
        };

        {
            // Check if the new node name exists in the blockchain and the keys match
            let identity_manager = identity_manager.lock().await;
            match identity_manager
                .external_profile_to_global_identity(new_node_name.get_node_name_string().as_str(), Some(true))
                .await
            {
                Ok(standard_identity) => {
                    if standard_identity.node_encryption_public_key != encryption_public_key
                        || standard_identity.node_signature_public_key != identity_public_key
                    {
                        let _ = res
                            .send(Err(APIError {
                                code: StatusCode::FORBIDDEN.as_u16(),
                                error: "Forbidden".to_string(),
                                message: "The keys do not match with the current node".to_string(),
                            }))
                            .await;
                        return Ok(());
                    }
                }
                Err(_) => {
                    let _ = res
                        .send(Err(APIError {
                            code: StatusCode::NOT_FOUND.as_u16(),
                            error: "Not Found".to_string(),
                            message: "The new node name does not exist in the blockchain".to_string(),
                        }))
                        .await;
                    return Ok(());
                }
            }
        }

        // Write to .secret file
        match update_global_identity_name(secret_file_path, new_node_name.get_node_name_string().as_str()) {
            Ok(_) => {
                eprintln!("Node name changed successfully. Restarting server...");
                let _ = res.send(Ok(())).await;
                tokio::time::sleep(tokio::time::Duration::from_secs(1)).await;
                panic!("Node name changed successfully. Restarting server...");
            }
            Err(err) => {
                let _ = res
                    .send(Err(APIError {
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        error: "Internal Server Error".to_string(),
                        message: format!("{}", err),
                    }))
                    .await;
            }
        };
        Ok(())
    }

    pub async fn v2_api_is_pristine(
        bearer: String,
        db: Arc<SqliteManager>,
        res: Sender<Result<bool, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let has_any_profile = db.has_any_profile().unwrap_or(false);
        let _ = res.send(Ok(!has_any_profile)).await;
        Ok(())
    }

    pub async fn v2_api_health_check(
        db: Arc<SqliteManager>,
        public_https_certificate: Option<String>,
        res: Sender<Result<serde_json::Value, APIError>>,
    ) -> Result<(), NodeError> {
        let public_https_certificate = match public_https_certificate {
            Some(cert) => cert,
            None => "".to_string(),
        };

        let version = env!("CARGO_PKG_VERSION");

        // Check if the version is 0.9.0
        let lancedb_exists = {
            // DB Path Env Vars
            let node_storage_path: String = env::var("NODE_STORAGE_PATH").unwrap_or_else(|_| "storage".to_string());

            // Try to open the folder main_db and search for lancedb
            let main_db_path = std::path::Path::new(&node_storage_path).join("main_db");

            if let Ok(entries) = std::fs::read_dir(&main_db_path) {
                entries.filter_map(Result::ok).any(|entry| {
                    let entry_path = entry.path();
                    if entry_path.is_dir() {
                        if entry_path.to_str().map_or(false, |s| s.contains("lancedb")) {
                            return true;
                        }
                        // Check one more level deep
                        if let Ok(sub_entries) = std::fs::read_dir(&entry_path) {
                            return sub_entries.filter_map(Result::ok).any(|sub_entry| {
                                let sub_entry_path = sub_entry.path();
                                sub_entry_path.is_dir()
                                    && sub_entry_path.to_str().map_or(false, |s| s.contains("lance"))
                            });
                        }
                    }
                    false
                })
            } else {
                false
            }
        };

        let (_current_version, needs_global_reset) = match db.get_version() {
            Ok(version) => version,
            Err(_err) => {
                let _ = res
                    .send(Err(APIError {
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        error: "Internal Server Error".to_string(),
                        message: format!("Failed to get version in table"),
                    }))
                    .await;
                return Ok(());
            }
        };
        let docker_status = match shinkai_tools_runner::tools::container_utils::is_docker_available() {
            shinkai_tools_runner::tools::container_utils::DockerStatus::NotInstalled => "not-installed",
            shinkai_tools_runner::tools::container_utils::DockerStatus::NotRunning => "not-running",
            shinkai_tools_runner::tools::container_utils::DockerStatus::Running => "running",
        };

        let _ = res
            .send(Ok(serde_json::json!({
                "is_pristine": !db.has_any_profile().unwrap_or(false),
                "public_https_certificate": public_https_certificate,
                "version": version,
                "update_requires_reset": needs_global_reset || lancedb_exists,
                "docker_status": docker_status,
            })))
            .await;
        Ok(())
    }

    pub async fn v2_api_scan_ollama_models(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<Vec<serde_json::Value>, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        match Self::internal_scan_ollama_models().await {
            Ok(response) => {
                let _ = res.send(Ok(response)).await;
                Ok(())
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("{}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_add_ollama_models(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        job_manager: Option<Arc<Mutex<JobManager>>>,
        identity_secret_key: SigningKey,
        bearer: String,
        payload: APIAddOllamaModels,
        ws_manager: Option<Arc<Mutex<dyn WSUpdateHandler + Send>>>,
        res: Sender<Result<(), APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let requester_name = match identity_manager.lock().await.get_main_identity() {
            Some(Identity::Standard(std_identity)) => std_identity.clone().full_identity_name,
            _ => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Wrong identity type. Expected Standard identity.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        let job_manager = match job_manager {
            Some(manager) => manager,
            None => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: "JobManager is required".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        match Node::internal_add_ollama_models(
            db,
            identity_manager,
            job_manager,
            identity_secret_key,
            payload.models,
            requester_name,
            ws_manager,
        )
        .await
        {
            Ok(_) => {
                let _ = res.send(Ok::<(), APIError>(())).await;
                Ok(())
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to add model: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    pub async fn v2_api_stop_llm(
        db: Arc<SqliteManager>,
        stopper: Arc<LLMStopper>,
        bearer: String,
        inbox_name: String,
        job_manager: Option<Arc<Mutex<JobManager>>>,
        res: Sender<Result<(), APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Check if the inbox_name starts with "jobid_"
        let inbox_name = if inbox_name.starts_with("jobid_") {
            match InboxName::get_job_inbox_name_from_params(inbox_name.clone()) {
                Ok(name) => name,
                Err(_) => {
                    let _ = res
                        .send(Err(APIError {
                            code: StatusCode::BAD_REQUEST.as_u16(),
                            error: "Bad Request".to_string(),
                            message: "Invalid job ID format".to_string(),
                        }))
                        .await;
                    return Ok(());
                }
            }
        } else {
            match InboxName::new(inbox_name.clone()) {
                Ok(name) => name,
                Err(_) => {
                    let _ = res
                        .send(Err(APIError {
                            code: StatusCode::BAD_REQUEST.as_u16(),
                            error: "Bad Request".to_string(),
                            message: "Invalid inbox name format".to_string(),
                        }))
                        .await;
                    return Ok(());
                }
            }
        };

        // Search in job manager and fill the job as well
        if let Some(job_manager) = job_manager {
            if let Some(job_id) = inbox_name.get_job_id() {
                // Get both queue managers
                let job_queue_manager_normal = job_manager.lock().await.job_queue_manager_normal.clone();
                let job_queue_manager_immediate = job_manager.lock().await.job_queue_manager_immediate.clone();

                // First try to dequeue from immediate queue
                let dequeue_result_immediate = job_queue_manager_immediate.lock().await.dequeue(&job_id).await;
                if let Ok(Some(_)) = dequeue_result_immediate {
                    // Job was successfully dequeued from immediate queue
                } else {
                    // If not found in immediate queue, try normal queue
                    let dequeue_result_normal = job_queue_manager_normal.lock().await.dequeue(&job_id).await;
                    if let Ok(Some(_)) = dequeue_result_normal {
                        // Job was successfully dequeued from normal queue
                    } else {
                        eprintln!("Job {} not found in either queue", job_id);
                    }
                }
            }
        }

        // Stop the LLM
        stopper.stop(&inbox_name.get_value());

        let _ = res.send(Ok(())).await;
        Ok(())
    }

    pub async fn v2_api_add_agent(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        bearer: String,
        agent: Agent,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Retrieve the profile name from the identity manager
        let requester_name = match identity_manager.lock().await.get_main_identity() {
            Some(Identity::Standard(std_identity)) => std_identity.clone().full_identity_name,
            _ => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Wrong identity type. Expected Standard identity.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Construct the expected full identity name
        let expected_full_identity_name = ShinkaiName::new(format!(
            "{}/main/agent/{}",
            requester_name.get_node_name_string(),
            agent.agent_id.to_lowercase()
        ))
        .unwrap();

        // Check if the full identity name matches
        if agent.full_identity_name != expected_full_identity_name {
            let api_error = APIError {
                code: StatusCode::BAD_REQUEST.as_u16(),
                error: "Bad Request".to_string(),
                message: "Invalid full identity name.".to_string(),
            };
            let _ = res.send(Err(api_error)).await;
            return Ok(());
        }
        // TODO: validate tools
        // TODO: validate knowledge

        // Check if the llm_provider_id exists
        let llm_provider_exists = {
            let exists = match db.get_llm_provider(&agent.llm_provider_id, &requester_name) {
                Ok(Some(_)) => true,
                _ => false,
            };
            exists
        };

        if llm_provider_exists {
            // Check if the agent_id already exists
            let agent_exists = {
                let exists = match db.get_agent(&agent.agent_id) {
                    Ok(Some(_)) => true,
                    _ => false,
                };
                exists
            };

            if agent_exists {
                let api_error = APIError {
                    code: StatusCode::CONFLICT.as_u16(),
                    error: "Conflict".to_string(),
                    message: "agent_id already exists".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
            } else {
                // Add the agent to the database
                match db.add_agent(agent.clone(), &requester_name) {
                    Ok(_) => {
                        // Create and add Agent tool wrapper
                        let node_name = requester_name.get_node_name_string();
                        let agent_tool_wrapper = AgentToolWrapper::new(
                            agent.agent_id.clone(),
                            agent.name.clone(),
                            agent.ui_description.clone(),
                            node_name,
                            None,
                        );

                        let shinkai_tool = ShinkaiTool::Agent(agent_tool_wrapper, true);

                        // Add agent tool to database
                        if let Err(err) = db.add_tool(shinkai_tool).await {
                            eprintln!("Warning: Failed to add agent tool: {}", err);
                        }

                        let _ = res.send(Ok("Agent added successfully".to_string())).await;
                    }
                    Err(err) => {
                        let api_error = APIError {
                            code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            error: "Internal Server Error".to_string(),
                            message: format!("Failed to add agent: {}", err),
                        };
                        let _ = res.send(Err(api_error)).await;
                    }
                }
            }
        } else {
            let api_error = APIError {
                code: StatusCode::NOT_FOUND.as_u16(),
                error: "Not Found".to_string(),
                message: "llm_provider_id does not exist".to_string(),
            };
            let _ = res.send(Err(api_error)).await;
        }

        Ok(())
    }

    pub async fn v2_api_remove_agent(
        db: Arc<SqliteManager>,
        bearer: String,
        node_name: ShinkaiName,
        agent_id: String,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Get the agent's node name
        let node_name_string = node_name.get_node_name_string();

        // Remove the agent from the database
        match db.remove_agent(&agent_id) {
            Ok(_) => {
                // Remove the agent tool
                let tool_router_key = ToolRouterKey::new(
                    "local".to_string(),
                    node_name_string.to_string(),
                    agent_id.clone(),
                    None,
                )
                .to_string_with_version();
                if let Err(err) = db.remove_tool(&tool_router_key, None) {
                    eprintln!("Warning: Failed to remove agent tool: {}", err);
                }

                let _ = res.send(Ok("Agent removed successfully".to_string())).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to remove agent: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_update_agent(
        db: Arc<SqliteManager>,
        bearer: String,
        partial_agent: serde_json::Value,
        res: Sender<Result<Agent, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Extract agent_id from partial_agent
        let agent_id = match partial_agent.get("agent_id").and_then(|id| id.as_str()) {
            Some(id) => id.to_string(),
            None => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "agent_id is missing in the request".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Retrieve the existing agent from the database
        let existing_agent = match db.get_agent(&agent_id) {
            Ok(Some(agent)) => agent,
            Ok(None) => {
                let api_error = APIError {
                    code: StatusCode::NOT_FOUND.as_u16(),
                    error: "Not Found".to_string(),
                    message: "Agent not found".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Database error: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Construct the full identity name
        let full_identity_name = match ShinkaiName::new(format!(
            "{}/main/agent/{}",
            existing_agent.full_identity_name.get_node_name_string(),
            agent_id
        )) {
            Ok(name) => name,
            Err(_) => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: "Failed to construct full identity name.".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Manually merge fields from partial_agent with existing_agent
        let updated_agent = Agent {
            name: partial_agent
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing_agent.name)
                .to_string(),
            agent_id: existing_agent.agent_id.clone(), // Keep the original agent_id
            llm_provider_id: partial_agent
                .get("llm_provider_id")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing_agent.llm_provider_id)
                .to_string(),
            // TODO: decide if we keep this
            // instructions: partial_agent
            //     .get("instructions")
            //     .and_then(|v| v.as_str())
            //     .unwrap_or(&existing_agent.instructions)
            //     .to_string(),
            ui_description: partial_agent
                .get("ui_description")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing_agent.ui_description)
                .to_string(),
            knowledge: partial_agent
                .get("knowledge")
                .and_then(|v| v.as_array())
                .map_or(existing_agent.knowledge.clone(), |v| {
                    v.iter().filter_map(|s| s.as_str().map(String::from)).collect()
                }),
            scope: partial_agent
                .get("scope")
                .map(|v| serde_json::from_value::<MinimalJobScope>(v.clone()).unwrap_or(existing_agent.scope.clone()))
                .unwrap_or(existing_agent.scope.clone()),
            storage_path: partial_agent
                .get("storage_path")
                .and_then(|v| v.as_str())
                .unwrap_or(&existing_agent.storage_path)
                .to_string(),
            tools: partial_agent
                .get("tools")
                .and_then(|v| v.as_array())
                .map_or(existing_agent.tools.clone(), |v| {
                    v.iter()
                        .filter_map(|s| serde_json::from_value(s.clone()).ok())
                        .collect()
                }),
            debug_mode: partial_agent
                .get("debug_mode")
                .and_then(|v| v.as_bool())
                .unwrap_or(existing_agent.debug_mode),
            config: partial_agent.get("config").map_or(existing_agent.config.clone(), |v| {
                serde_json::from_value(v.clone()).unwrap_or(existing_agent.config.clone())
            }),
            cron_tasks: None,
            full_identity_name, // Set the constructed full identity name
            tools_config_override: partial_agent
                .get("tools_config_override")
                .map_or(existing_agent.tools_config_override.clone(), |v| {
                    serde_json::from_value(v.clone()).unwrap_or(existing_agent.tools_config_override.clone())
                }),
        };

        // Update the agent in the database
        match db.update_agent(updated_agent.clone()) {
            Ok(_) => {
                let _ = res.send(Ok(updated_agent)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to update agent: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_get_agent(
        db: Arc<SqliteManager>,
        bearer: String,
        agent_id: String,
        res: Sender<Result<Agent, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Retrieve the agent from the database
        match db.get_agent(&agent_id) {
            Ok(Some(mut agent)) => {
                // Get cron tasks for this agent
                match db.get_cron_tasks_by_llm_provider_id(&agent.agent_id) {
                    Ok(cron_tasks) => {
                        agent.cron_tasks = if cron_tasks.is_empty() { None } else { Some(cron_tasks) };
                    }
                    Err(e) => {
                        agent.cron_tasks = None;
                    }
                }
                let _ = res.send(Ok(agent)).await;
            }
            Ok(None) => {
                let api_error = APIError {
                    code: StatusCode::NOT_FOUND.as_u16(),
                    error: "Not Found".to_string(),
                    message: "Agent not found".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to retrieve agent: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_get_all_agents(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<Vec<Agent>, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Retrieve all agents from the database
        match db.get_all_agents() {
            Ok(mut agents) => {
                // Get cron tasks for each agent
                for agent in &mut agents {
                    match db.get_cron_tasks_by_llm_provider_id(&agent.agent_id) {
                        Ok(cron_tasks) => {
                            agent.cron_tasks = if cron_tasks.is_empty() { None } else { Some(cron_tasks) };
                        }
                        Err(e) => {
                            agent.cron_tasks = None;
                        }
                    }
                }
                let _ = res.send(Ok(agents)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to retrieve agents: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_test_llm_provider(
        db: Arc<SqliteManager>,
        identity_manager: Arc<Mutex<IdentityManager>>,
        job_manager: Option<Arc<Mutex<JobManager>>>,
        identity_secret_key: SigningKey,
        bearer: String,
        provider: SerializedLLMProvider,
        node_name: ShinkaiName,
        node_encryption_sk: EncryptionStaticKey,
        node_encryption_pk: EncryptionPublicKey,
        _node_signing_sk: SigningKey,
        ws_manager: Option<Arc<Mutex<dyn WSUpdateHandler + Send>>>,
        res: Sender<Result<serde_json::Value, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Create a new SerializedLLMProvider with id and full_identity_name set to "llm_test"
        let name = node_name.extract_node().get_node_name_string();
        let profile = ShinkaiName::new(format!("{}/main", name)).unwrap();
        let llm_name = format!("{}/main/agent/test_llm_provider", name);

        let provider = SerializedLLMProvider {
            id: "llm_test".to_string(),
            full_identity_name: ShinkaiName::new(llm_name).unwrap(),
            external_url: provider.external_url.clone(),
            api_key: provider.api_key.clone(),
            model: provider.model.clone(),
        };

        Self::ensure_llm_provider(db.clone(), &profile, provider.clone()).await?;

        // Ensure job_manager is available
        let job_manager = match job_manager {
            Some(manager) => manager,
            None => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: "JobManager is required".to_string(),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Get the provider name as a ShinkaiName
        let profile = {
            let identity_manager_lock = identity_manager.lock().await;
            match identity_manager_lock.get_main_identity() {
                Some(identity) => identity.get_shinkai_name(),
                None => {
                    let api_error = APIError {
                        code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                        error: "Internal Server Error".to_string(),
                        message: "Failed to retrieve main identity".to_string(),
                    };
                    let _ = res.send(Err(api_error)).await;
                    return Ok(());
                }
            }
        };

        // Add the LLM provider
        match Self::internal_add_llm_provider(
            db.clone(),
            identity_manager.clone(),
            job_manager.clone(),
            identity_secret_key.clone(),
            provider.clone(),
            &profile,
            ws_manager.clone(),
        )
        .await
        {
            Ok(_) => {
                // Create Job and Send Message
                match tool_generation::v2_create_and_send_job_message(
                    bearer.clone(),
                    JobCreationInfo {
                        scope: MinimalJobScope::default(),
                        is_hidden: Some(true),
                        associated_ui: None,
                    },
                    provider.id.clone(),
                    "Repeat back the following message: dogcat. Just the word, no other words.".to_string(),
                    None, // tools
                    None, // fs_file_paths
                    None, // job_filenames
                    db.clone(),
                    profile.extract_node().clone(),
                    identity_manager.clone(),
                    job_manager.clone(),
                    node_encryption_sk,
                    node_encryption_pk,
                    identity_secret_key.clone(),
                )
                .await
                {
                    Ok(job_id) => {
                        // Wait for response
                        let timeout_duration = Duration::from_secs(60); // Set a timeout duration
                        match Self::check_job_response(db.clone(), job_id.clone(), "dogcat", timeout_duration).await {
                            Ok(_) => {
                                // Clean up the test LLM provider
                                if let Err(e) = db.remove_llm_provider(&provider.id, &profile) {
                                    eprintln!("Warning: Failed to clean up test LLM provider: {}", e);
                                }

                                let response = serde_json::json!({
                                    "message": "LLM provider tested successfully",
                                    "status": "success"
                                });
                                let _ = res.send(Ok(response)).await;
                                Ok(())
                            }
                            Err(err) => {
                                // Clean up the test LLM provider even if test failed
                                if let Err(e) = db.remove_llm_provider(&provider.id, &profile) {
                                    eprintln!("Warning: Failed to clean up test LLM provider: {}", e);
                                }

                                let api_error = APIError {
                                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                                    error: "Internal Server Error".to_string(),
                                    message: format!("Error: {:?}", err.message),
                                };
                                let _ = res.send(Err(api_error)).await;
                                Ok(())
                            }
                        }
                    }
                    Err(err) => {
                        let api_error = APIError {
                            code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            error: "Internal Server Error".to_string(),
                            message: format!("Failed to create job and send message: {:?}", err),
                        };
                        let _ = res.send(Err(api_error)).await;
                        Ok(())
                    }
                }
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to add LLM provider: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
                Ok(())
            }
        }
    }

    async fn check_job_response(
        db: Arc<SqliteManager>,
        job_id: String,
        expected_response: &str,
        timeout_duration: Duration,
    ) -> Result<(), APIError> {
        let start = Instant::now();
        loop {
            // Fetch the last messages from the job inbox
            let inbox_name = InboxName::get_job_inbox_name_from_params(job_id.clone()).unwrap();
            let inbox_name_value = match inbox_name {
                InboxName::RegularInbox { value, .. } | InboxName::JobInbox { value, .. } => value,
            };

            let last_messages_inbox = db
                .get_last_messages_from_inbox(inbox_name_value.clone().to_string(), 10, None)
                .unwrap_or_default();

            // Ensure there are at least two messages
            if last_messages_inbox.len() >= 2 {
                // Check the content of the second message
                if let Some(second_message_group) = last_messages_inbox.get(1) {
                    for message in second_message_group {
                        if let Ok(content) = message.get_message_content() {
                            if !content.is_empty() && content.contains(expected_response) {
                                return Ok(());
                            }
                        }
                    }
                }
                // If the second message does not contain the expected response, return an error
                let error_message = if let Some(second_message_group) = last_messages_inbox.get(1) {
                    let content = second_message_group
                        .iter()
                        .filter_map(|msg| {
                            // Get the raw content from the message
                            if let MessageBody::Unencrypted(body) = &msg.body {
                                if let MessageData::Unencrypted(data) = &body.message_data {
                                    // Parse the raw content as JobMessage
                                    if let Ok(job_message) =
                                        serde_json::from_str::<JobMessage>(&data.message_raw_content)
                                    {
                                        return Some(job_message.content);
                                    }
                                }
                            }
                            None
                        })
                        .collect::<Vec<String>>()
                        .join(", ");

                    content
                } else {
                    "Error but no specific error message received. Double-check the model name and your key."
                        .to_string()
                };
                return Err(APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: error_message,
                });
            }

            // Check if the timeout has been reached
            if start.elapsed() > timeout_duration {
                return Err(APIError {
                    code: StatusCode::REQUEST_TIMEOUT.as_u16(),
                    error: "Request Timeout".to_string(),
                    message: "Failed to receive the expected response in time".to_string(),
                });
            }

            // Sleep for a short duration before checking again
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    }

    pub async fn ensure_llm_provider(
        db: Arc<SqliteManager>,
        profile: &ShinkaiName,
        input_provider: SerializedLLMProvider,
    ) -> Result<(), NodeError> {
        // Check if the provider already exists
        let provider_exists = db.get_llm_provider(&input_provider.id, profile).is_ok();

        // If it exists, remove it
        if provider_exists {
            db.remove_llm_provider(&input_provider.id, profile)?;
        }

        Ok(())
    }

    async fn generate_agent_zip(
        db: Arc<SqliteManager>,
        node_env: NodeEnvironment,
        agent_id: String,
    ) -> Result<Vec<u8>, APIError> {
        fn internal_error(err: String) -> APIError {
            APIError {
                code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                error: "Internal Server Error".to_string(),
                message: format!("Failed to generate agent zip: {}", err),
            }
        }

        // Retrieve the agent from the database
        let agent = match db.get_agent(&agent_id) {
            Ok(Some(agent)) => agent,
            Ok(None) => return Err(internal_error(format!("Agent not found: {}", agent_id))),
            Err(err) => return Err(internal_error(format!("Failed to retrieve agent: {}", err))),
        };

        // Serialize the agent to JSON bytes
        let agent_bytes = match serde_json::to_vec(&agent) {
            Ok(bytes) => bytes,
            Err(err) => return Err(internal_error(format!("Failed to serialize agent: {}", err))),
        };

        // Create a temporary zip file
        let name = format!("{}.zip", agent.agent_id.replace(':', "_"));
        let path = std::env::temp_dir().join(&name);
        let file = match File::create(&path) {
            Ok(file) => file,
            Err(err) => return Err(internal_error(format!("Failed to create zip file: {}", err))),
        };

        let mut zip = ZipWriter::new(file);

        for tool_key in agent.tools {
            let tool = db.get_tool_by_key(&tool_key.to_string_without_version());
            match tool {
                Ok(tool) => {
                    let tool_bytes = Node::get_tool_zip(tool, node_env.clone()).await;
                    if let Err(err) = tool_bytes {
                        return Err(internal_error(format!("Failed to get tool zip: {}", err)));
                    }
                    let tool_bytes = tool_bytes.unwrap();
                    if let Err(err) = zip.start_file::<_, ()>(
                        format!("__tools/{}.zip", tool_key.to_string_without_version().replace(':', "_")),
                        FileOptions::default(),
                    ) {
                        return Err(internal_error(format!("Failed to create tool file in zip: {}", err)));
                    }
                    if let Err(err) = zip.write_all(&tool_bytes) {
                        return Err(internal_error(format!("Failed to write tool data to zip: {}", err)));
                    }
                }
                Err(err) => {
                    eprintln!(
                        "Warning: Failed to get tool {}: {}",
                        tool_key.to_string_without_version(),
                        err
                    );
                }
            }
        }

        // Add the agent JSON to the zip file
        if let Err(err) = zip.start_file::<_, ()>("__agent.json", FileOptions::default()) {
            return Err(internal_error(format!("Failed to create agent file in zip: {}", err)));
        }

        if let Err(err) = zip.write_all(&agent_bytes) {
            return Err(internal_error(format!("Failed to write agent data to zip: {}", err)));
        }

        // Finalize the zip file
        if let Err(err) = zip.finish() {
            return Err(internal_error(format!("Failed to finalize zip file: {}", err)));
        }

        // Read the zip file into memory
        match fs::read(&path).await {
            Ok(file_bytes) => {
                // Clean up the temporary file
                if let Err(err) = fs::remove_file(&path).await {
                    eprintln!("Warning: Failed to remove temporary file: {}", err);
                }
                Ok(file_bytes)
            }
            Err(err) => return Err(internal_error(format!("Failed to read zip file: {}", err))),
        }
    }

    pub async fn v2_api_export_agent(
        db: Arc<SqliteManager>,
        bearer: String,
        node_env: NodeEnvironment,
        agent_id: String,
        res: Sender<Result<Vec<u8>, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }
        let agent_zip: Result<Vec<u8>, APIError> = Node::generate_agent_zip(db, node_env, agent_id).await;
        if let Err(err) = agent_zip {
            let _ = res.send(Err(err)).await;
            return Ok(());
        }
        let agent_zip = agent_zip.unwrap();
        let _ = res.send(Ok(agent_zip)).await;

        Ok(())
    }

    pub async fn bytes_to_zip_tool(
        mut archive: ZipArchive<std::io::Cursor<Vec<u8>>>,
        file_name: String,
    ) -> Result<ZipFileContents, APIError> {
        // Extract and parse file
        let mut zip_buffer = Vec::new();
        {
            let mut file = match archive.by_name(&file_name) {
                Ok(file) => file,
                Err(_) => {
                    return Err(APIError {
                        code: StatusCode::BAD_REQUEST.as_u16(),
                        error: "Invalid Zip File".to_string(),
                        message: format!("Archive does not contain {}", file_name),
                    });
                }
            };

            // Read the file contents into a buffer
            if let Err(err) = file.read_to_end(&mut zip_buffer) {
                return Err(APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Read Error".to_string(),
                    message: format!("Failed to read file contents: {}", err),
                });
            }
        }

        // Create a new cursor and archive for returning
        let return_cursor = std::io::Cursor::new(zip_buffer.clone());
        let mut return_archive = zip::ZipArchive::new(return_cursor).unwrap();

        let mut tool_buffer: Vec<u8> = Vec::new();
        {
            let mut file = match return_archive.by_name("__tool.json") {
                Ok(file) => file,
                Err(_) => {
                    return Err(APIError {
                        code: StatusCode::BAD_REQUEST.as_u16(),
                        error: "Invalid Zip File".to_string(),
                        message: "Archive does not contain __tool.json".to_string(),
                    });
                }
            };

            if let Err(err) = file.read_to_end(&mut tool_buffer) {
                return Err(APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Invalid Tool JSON".to_string(),
                    message: format!("Failed to read tool.json: {}", err),
                });
            }
        }

        Ok(ZipFileContents {
            buffer: tool_buffer,
            archive: return_archive,
        })
    }

    pub async fn v2_api_import_agent(
        db: Arc<SqliteManager>,
        bearer: String,
        url: String,
        node_name: String,
        node_env: NodeEnvironment,
        signing_secret_key: SigningKey,
        res: Sender<Result<Value, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let zip_contents = match download_zip_file(url, "__agent.json".to_string(), node_name, signing_secret_key).await
        {
            Ok(contents) => contents,
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Invalid Agent Zip".to_string(),
                    message: format!("Failed to extract agent.json: {:?}", err),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        if let Err(err) = Node::import_agent_tools(db.clone(), node_env.clone(), zip_contents.archive).await {
            let api_error = APIError {
                code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                error: "Internal Server Error".to_string(),
                message: format!("Failed to import agent tools: {:?}", err),
            };
            let _ = res.send(Err(api_error)).await;
        }

        // Save the agent to the database
        // Parse the JSON into an Agent
        let agent: Agent = serde_json::from_slice(&zip_contents.buffer).map_err(|e| NodeError::from(e.to_string()))?;

        match db.add_agent(agent.clone(), &agent.full_identity_name) {
            Ok(_) => {
                let response = json!({
                    "status": "success",
                    "message": "Agent imported successfully",
                    "agent_id": agent.agent_id,
                    "agent": agent
                });
                let _ = res.send(Ok(response)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Database Error".to_string(),
                    message: format!("Failed to save agent to database: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    async fn import_agent_tools(
        db: Arc<SqliteManager>,
        node_env: NodeEnvironment,
        zip_contents: ZipArchive<std::io::Cursor<Vec<u8>>>,
    ) -> Result<(), APIError> {
        // Import tools from the zip file
        let archive_clone = zip_contents.clone();
        let files = archive_clone.file_names();
        for file in files {
            if !file.starts_with("__tools/") {
                continue;
            }

            let tool_zip = Node::bytes_to_zip_tool(zip_contents.clone(), file.to_string()).await;

            if let Err(err) = tool_zip {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Invalid Tool Archive".to_string(),
                    message: format!("Failed to extract tool.json: {:?}", err),
                };
                return Err(api_error);
            }
            let tool_zip: ZipFileContents = tool_zip.unwrap();

            let tool: ShinkaiTool = match serde_json::from_slice(&tool_zip.buffer) {
                Ok(tool) => tool,
                Err(err) => {
                    let api_error = APIError {
                        code: StatusCode::BAD_REQUEST.as_u16(),
                        error: "Invalid Tool JSON".to_string(),
                        message: format!("Failed to parse tool.json: {}", err),
                    };
                    return Err(api_error);
                }
            };
            let import_tool_result = Node::import_tool(db.clone(), node_env.clone(), tool_zip, tool).await;
            if let Err(err) = import_tool_result {
                println!("Error importing tool: {:?}", err);
            }
        }
        Ok(())
    }

    pub async fn v2_api_import_agent_zip(
        db: Arc<SqliteManager>,
        bearer: String,
        node_env: NodeEnvironment,
        file_data: Vec<u8>,
        res: Sender<Result<Value, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Process the zip file
        let cursor = std::io::Cursor::new(file_data);
        let mut archive = match zip::ZipArchive::new(cursor) {
            Ok(archive) => archive,
            Err(err) => {
                return Err(NodeError::from(format!("Failed to read zip archive: {}", err)));
            }
        };

        // Extract and parse tool.json
        let mut buffer = Vec::new();
        {
            let mut file = match archive.by_name("__agent.json") {
                Ok(file) => file,
                Err(_) => {
                    return Err(NodeError::from("Archive does not contain __agent.json".to_string()));
                }
            };

            if let Err(err) = file.read_to_end(&mut buffer) {
                return Err(NodeError::from(format!("Failed to read agent.json: {}", err)));
            }
        }

        if let Err(err) = Node::import_agent_tools(db.clone(), node_env.clone(), archive).await {
            let api_error = APIError {
                code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                error: "Internal Server Error".to_string(),
                message: format!("Failed to import agent tools: {:?}", err),
            };
            let _ = res.send(Err(api_error)).await;
        }

        // Parse the JSON into an Agent
        let agent: Agent = serde_json::from_slice(&buffer).map_err(|e| NodeError::from(e.to_string()))?;
        // Save the agent to the database
        let agent_status = db.add_agent(agent.clone(), &agent.full_identity_name);
        if agent_status.is_err() {
            let api_error = APIError {
                code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                error: "Database Error".to_string(),
                message: format!("Failed to save agent to database: {}", agent_status.err().unwrap()),
            };
            let _ = res.send(Err(api_error)).await;
        }

        let response = json!({
            "status": "success",
            "message": "Agent imported successfully",
            "agent_id": agent.agent_id,
            "agent": agent
        });
        let _ = res.send(Ok(response)).await;

        Ok(())
    }

    pub async fn v2_api_add_regex_pattern(
        db: Arc<SqliteManager>,
        bearer: String,
        provider_name: String,
        pattern: String,
        response: String,
        description: Option<String>,
        priority: i32,
        res: Sender<Result<i64, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        // Create the regex pattern
        let regex_pattern = match RegexPattern::new(provider_name, pattern, response, description, priority) {
            Ok(pattern) => pattern,
            Err(e) => {
                let api_error = APIError {
                    code: StatusCode::BAD_REQUEST.as_u16(),
                    error: "Bad Request".to_string(),
                    message: format!("Failed to create regex pattern: {}", e),
                };
                let _ = res.send(Err(api_error)).await;
                return Ok(());
            }
        };

        // Add the pattern to the database
        match db.add_regex_pattern(&regex_pattern) {
            Ok(id) => {
                let _ = res.send(Ok(id)).await;
            }
            Err(e) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to add regex pattern: {}", e),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn handle_periodic_maintenance(
        db: Arc<SqliteManager>,
        node_name: ShinkaiName,
        identity_manager: Arc<Mutex<IdentityManager>>,
        tool_router: Option<Arc<ToolRouter>>,
    ) -> Result<(), NodeError> {
        // Import tools from directory if tool_router is available
        if let Some(tool_router) = tool_router {
            if let Err(e) = tool_router.sync_tools_from_directory().await {
                eprintln!("Error during periodic tool import: {}", e);
            }
        }
        Ok(())
    }

    pub async fn v2_api_check_default_tools_sync(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<bool, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate bearer token
        if let Err(_) = Self::validate_bearer_token(&bearer, db.clone(), &res).await {
            return Ok(());
        }

        // Get the internal_comms preference from the database
        match db.get_preference::<ShinkaiInternalComms>("internal_comms") {
            Ok(Some(internal_comms)) => {
                let _ = res.send(Ok(internal_comms.internal_has_sync_default_tools)).await;
            }
            Ok(None) => {
                let _ = res.send(Ok(false)).await;
            }
            Err(e) => {
                eprintln!("Error getting internal_comms preference: {}", e);
                let _ = res.send(Ok(false)).await;
            }
        }
        Ok(())
    }

    async fn internal_compute_quests_status(
        db: Arc<SqliteManager>,
        node_name: ShinkaiName,
        encryption_public_key: EncryptionPublicKey,
        identity_public_key: VerifyingKey,
    ) -> Result<Value, String> {
        let quests_status = compute_quests(
            db.clone(),
            node_name.clone(),
            encryption_public_key.clone(),
            identity_public_key.clone(),
        )
        .await?;

        // Convert the Vec into a Vec of objects with just name and status
        let quests_array: Vec<_> = quests_status
            .into_iter()
            .map(|(_quest_type, quest_info)| {
                json!({
                    "name": quest_info.name,
                    "status": quest_info.status
                })
            })
            .collect();

        // Convert to string for signature generation
        let payload_string =
            serde_json::to_string(&quests_array).map_err(|e| format!("Failed to serialize quests array: {}", e))?;

        // Get the node's signature public key from the database
        let node_signature_public_key = db
            .query_row(
                "SELECT node_signature_public_key FROM local_node_keys LIMIT 1",
                params![],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .map_err(|e| format!("Failed to get node signature public key: {}", e))?;

        // Generate proof using the node's signature public key
        let (signature, metadata) = generate_proof(hex::encode(node_signature_public_key), payload_string)?;

        // Create the quests payload
        let quests_payload = json!({
            "quests": quests_array,
            "signed_proof": signature,
            "metadata": metadata,
            "node_name": node_name.to_string()
        });

        Ok(json!({
            "status": "success",
            "message": "Quests status computed successfully",
            "data": quests_payload,
        }))
    }

    pub async fn v2_api_compute_quests_status(
        db: Arc<SqliteManager>,
        node_name: ShinkaiName,
        encryption_public_key: EncryptionPublicKey,
        identity_public_key: VerifyingKey,
        bearer: String,
        res: Sender<Result<Value, APIError>>,
    ) -> Result<(), NodeError> {
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        match Self::internal_compute_quests_status(db, node_name, encryption_public_key, identity_public_key).await {
            Ok(response) => {
                let _ = res.send(Ok(response)).await;
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to compute quests status: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_compute_and_send_quests_status(
        db: Arc<SqliteManager>,
        node_name: ShinkaiName,
        encryption_public_key: EncryptionPublicKey,
        identity_public_key: VerifyingKey,
        bearer: String,
        res: Sender<Result<Value, APIError>>,
    ) -> Result<(), NodeError> {
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        match Self::internal_compute_quests_status(
            db.clone(),
            node_name.clone(),
            encryption_public_key.clone(),
            identity_public_key.clone(),
        )
        .await
        {
            Ok(response) => {
                // Use the production Galxe API endpoint
                let galxe_quests_url = std::env::var("GALXE_QUESTS_URL")
                    .unwrap_or_else(|_| "https://api.shinkai.com/galxe/user".to_string());

                // Wrap the data in the correct structure
                let payload = json!({
                    "data": response["data"]
                });

                // Send the quests data to the Galxe backend
                let client = reqwest::Client::new();
                match client
                    .post(&galxe_quests_url)
                    .header("Content-Type", "application/json; charset=utf-8")
                    .json(&payload)
                    .send()
                    .await
                {
                    Ok(galxe_response) => match galxe_response.status() {
                        StatusCode::OK => {
                            let success_response = json!({
                                "status": "success",
                                "message": "Quests status computed and sent successfully",
                                "data": response["data"],
                            });
                            let _ = res.send(Ok(success_response)).await;
                        }
                        status => {
                            let error_message = galxe_response
                                .text()
                                .await
                                .unwrap_or_else(|_| "Failed to read error response".to_string());
                            let api_error = APIError {
                                code: status.as_u16(),
                                error: "Failed to send quests status".to_string(),
                                message: error_message,
                            };
                            let _ = res.send(Err(api_error)).await;
                        }
                    },
                    Err(err) => {
                        let api_error = APIError {
                            code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                            error: "Internal Server Error".to_string(),
                            message: format!("Failed to send quests status: {}", err),
                        };
                        let _ = res.send(Err(api_error)).await;
                    }
                }
            }
            Err(err) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to compute quests status: {}", err),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }

        Ok(())
    }

    pub async fn v2_api_set_preferences(
        db: Arc<SqliteManager>,
        bearer: String,
        payload: HashMap<String, serde_json::Value>,
        res: Sender<Result<String, APIError>>,
    ) -> Result<(), NodeError> {
        // Validate the bearer token
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        let mut errors = vec![];
        for (key, value) in payload {
            if let Err(e) = db.set_preference(&key, &value, None) {
                errors.push(format!("Failed to set preference '{}': {}", key, e));
            }
        }

        if errors.is_empty() {
            let _ = res.send(Ok("Preferences set successfully".to_string())).await;
        } else {
            let error_message = errors.join("; ");
            let api_error = APIError {
                code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                error: "Internal Server Error".to_string(),
                message: error_message,
            };
            let _ = res.send(Err(api_error)).await;
        }

        Ok(())
    }

    pub async fn v2_api_get_preferences(
        db: Arc<SqliteManager>,
        bearer: String,
        res: Sender<Result<Value, APIError>>,
    ) -> Result<(), NodeError> {
        if Self::validate_bearer_token(&bearer, db.clone(), &res).await.is_err() {
            return Ok(());
        }

        match db.get_all_preferences() {
            Ok(preferences) => {
                let _ = res.send(Ok(json!(preferences))).await;
            }
            Err(e) => {
                let api_error = APIError {
                    code: StatusCode::INTERNAL_SERVER_ERROR.as_u16(),
                    error: "Internal Server Error".to_string(),
                    message: format!("Failed to get preferences: {}", e),
                };
                let _ = res.send(Err(api_error)).await;
            }
        }
        Ok(())
    }
}
