use shinkai_embedding::model_type::{EmbeddingModelType, OllamaTextEmbeddingsInference};
use shinkai_message_primitives::schemas::inbox_name::InboxName;
use shinkai_message_primitives::shinkai_message::shinkai_message::ShinkaiMessage;
use shinkai_message_primitives::shinkai_message::shinkai_message_schemas::MessageSchemaType;
use shinkai_message_primitives::shinkai_utils::encryption::EncryptionMethod;
use shinkai_message_primitives::shinkai_utils::job_scope::MinimalJobScope;
use shinkai_message_primitives::shinkai_utils::shinkai_message_builder::ShinkaiMessageBuilder;
use shinkai_sqlite::SqliteManager;
use std::path::PathBuf;
use std::sync::Arc;
use tempfile::NamedTempFile;
use tokio::time::{sleep, Duration};

use ed25519_dalek::SigningKey;
use x25519_dalek::{PublicKey as EncryptionPublicKey, StaticSecret as EncryptionStaticKey};

async fn create_new_job(db: &Arc<SqliteManager>, job_id: String, agent_id: String, scope: MinimalJobScope) {
    match db.create_new_job(job_id, agent_id, scope, false, None, None) {
        Ok(_) => (),
        Err(e) => panic!("Failed to create a new job: {}", e),
    }
}

fn setup_test_db() -> SqliteManager {
    let temp_file = NamedTempFile::new().unwrap();
    let db_path = PathBuf::from(temp_file.path());
    let api_url = String::new();
    let model_type =
        EmbeddingModelType::OllamaTextEmbeddingsInference(OllamaTextEmbeddingsInference::SnowflakeArcticEmbedM);

    SqliteManager::new(db_path, api_url, model_type).unwrap()
}

fn generate_message_with_text(
    content: String,
    my_encryption_secret_key: EncryptionStaticKey,
    my_signature_secret_key: SigningKey,
    receiver_public_key: EncryptionPublicKey,
    recipient_subidentity_name: String,
    origin_destination_identity_name: String,
    timestamp: String,
) -> ShinkaiMessage {
    let inbox_name = InboxName::get_job_inbox_name_from_params("test_job".to_string()).unwrap();

    let inbox_name_value = match inbox_name {
        InboxName::RegularInbox { value, .. } | InboxName::JobInbox { value, .. } => value,
    };

    ShinkaiMessageBuilder::new(my_encryption_secret_key, my_signature_secret_key, receiver_public_key)
        .message_raw_content(content.to_string())
        .body_encryption(EncryptionMethod::None)
        .message_schema_type(MessageSchemaType::TextContent)
        .internal_metadata_with_inbox(
            "".to_string(),
            recipient_subidentity_name.clone().to_string(),
            inbox_name_value,
            EncryptionMethod::None,
            None,
        )
        .external_metadata_with_schedule(
            origin_destination_identity_name.clone().to_string(),
            origin_destination_identity_name.clone().to_string(),
            timestamp,
        )
        .build()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::*;
    use shinkai_message_primitives::{
        schemas::{
            identity::{StandardIdentity, StandardIdentityType}, inbox_name::InboxName, inbox_permission::InboxPermission, job::ForkedJob, shinkai_name::ShinkaiName, subprompts::SubPrompt
        }, shinkai_message::shinkai_message_schemas::{IdentityPermissions, JobMessage}, shinkai_utils::{
            encryption::unsafe_deterministic_encryption_keypair, job_scope::MinimalJobScope, shinkai_message_builder::ShinkaiMessageBuilder, signatures::{clone_signature_secret_key, unsafe_deterministic_signature_keypair}
        }
    };
    use shinkai_sqlite::errors::SqliteManagerError;

    #[tokio::test]
    async fn test_create_new_job() {
        let job_id = "job1".to_string();
        let agent_id = "agent1".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope).await;

        // Retrieve all jobs
        let jobs = shinkai_db.get_all_jobs().unwrap();

        // Check if the job exists
        let job_ids: Vec<String> = jobs.iter().map(|job| job.job_id().to_string()).collect();
        assert!(job_ids.contains(&job_id));

        // Check that the job has the correct properties
        let job = shinkai_db.get_job(&job_id).unwrap();
        assert_eq!(job.job_id, job_id);
        assert_eq!(job.parent_agent_or_llm_provider_id, agent_id);
        assert!(!job.is_finished);
    }

    #[tokio::test]
    async fn test_get_agent_jobs() {
        let agent_id = "agent2".to_string();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create new jobs for the agent
        for i in 1..=5 {
            let job_id = format!("job{}", i);
            eprintln!("job_id: {}", job_id.clone());
            let scope = MinimalJobScope::default();
            let _ = create_new_job(&shinkai_db, job_id, agent_id.clone(), scope).await;
        }

        eprintln!("agent_id: {}", agent_id.clone());

        // Get all jobs for the agent
        let jobs = shinkai_db.get_agent_jobs(agent_id.clone()).unwrap();

        // Assert that all jobs are returned
        assert_eq!(jobs.len(), 5);

        // Additional check that all jobs have correct agent_id
        for job in jobs {
            assert_eq!(job.parent_llm_provider_id(), &agent_id);
        }
    }

    #[tokio::test]
    async fn test_change_job_agent() {
        let job_id = "job_to_change_agent".to_string();
        let initial_agent_id = "initial_agent".to_string();
        let new_agent_id = "new_agent".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job with the initial agent
        let _ = create_new_job(&shinkai_db, job_id.clone(), initial_agent_id.clone(), scope).await;

        // Change the agent of the job
        shinkai_db.change_job_llm_provider(&job_id, &new_agent_id).unwrap();

        // Retrieve the job and check that the agent has been updated
        let job = shinkai_db.get_job(&job_id).unwrap();
        assert_eq!(job.parent_agent_or_llm_provider_id, new_agent_id);

        // Check that the job is listed under the new agent
        let new_agent_jobs = shinkai_db.get_agent_jobs(new_agent_id.clone()).unwrap();
        let job_ids: Vec<String> = new_agent_jobs.iter().map(|job| job.job_id().to_string()).collect();
        assert!(job_ids.contains(&job_id));

        // Check that the job is no longer listed under the initial agent
        let initial_agent_jobs = shinkai_db.get_agent_jobs(initial_agent_id.clone()).unwrap();
        let initial_job_ids: Vec<String> = initial_agent_jobs.iter().map(|job| job.job_id().to_string()).collect();
        assert!(!initial_job_ids.contains(&job_id));
    }

    #[tokio::test]
    async fn test_update_job_to_finished() {
        let job_id = "job3".to_string();
        let agent_id = "agent3".to_string();
        // let inbox_name =
        //     InboxName::new("inbox::@@node1.shinkai/subidentity::@@node2.shinkai/subidentity2::true".to_string())
        //         .unwrap();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope).await;

        // Update job to finished
        shinkai_db.update_job_to_finished(&job_id.clone()).unwrap();

        // Retrieve the job and check that is_finished is set to true
        let job = shinkai_db.get_job(&job_id.clone()).unwrap();
        assert!(job.is_finished);
    }

    #[tokio::test]
    async fn test_get_non_existent_job() {
        let job_id = "non_existent_job".to_string();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);
        let db_read = shinkai_db;

        match db_read.get_job(&job_id) {
            Ok(_) => panic!("Expected an error when getting a non-existent job"),
            Err(e) => assert_eq!(matches!(e, SqliteManagerError::DataNotFound), true),
        }
    }

    #[tokio::test]
    async fn test_get_agent_jobs_none_exist() {
        let agent_id = "agent_without_jobs".to_string();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Attempt to get all jobs for the agent
        let jobs_result = shinkai_db.get_agent_jobs(agent_id.clone());

        match jobs_result {
            Ok(jobs) => {
                // If we got a result, assert that no jobs are returned
                assert_eq!(jobs.len(), 0);
            }
            Err(e) => {
                // If we got an error, check if it's because the agent doesn't exist
                assert_eq!(matches!(e, SqliteManagerError::DataNotFound), true);
            }
        }
    }

    #[tokio::test]
    async fn test_update_non_existent_job() {
        let job_id = "non_existent_job".to_string();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);
        let db_write = shinkai_db;

        match db_write.update_job_to_finished(&job_id.clone()) {
            Ok(_) => panic!("Expected an error when updating a non-existent job"),
            Err(e) => assert_eq!(matches!(e, SqliteManagerError::DataNotFound), true),
        }
    }

    #[tokio::test]
    async fn test_get_agent_jobs_multiple_jobs() {
        let agent_id = "agent5".to_string();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create new jobs for the agent
        for i in 1..=5 {
            let job_id = format!("job{}", i);
            // let inbox_name =
            //     InboxName::new("inbox::@@node1.shinkai/subidentity::@@node2.shinkai/subidentity2::true".to_string())
            //         .unwrap();
            // let inbox_names = vec![inbox_name];
            // let documents = vec!["document1".to_string(), "document2".to_string()];

            let scope = MinimalJobScope::default();
            let _ = create_new_job(&shinkai_db, job_id, agent_id.clone(), scope).await;
        }

        // Get all jobs for the agent
        let jobs = shinkai_db.get_agent_jobs(agent_id.clone()).unwrap();

        // Assert that all jobs are returned
        assert_eq!(jobs.len(), 5);

        // Additional check that all jobs have correct agent_id and they are unique
        let unique_jobs: HashSet<String> = jobs.iter().map(|job| job.job_id().to_string()).collect();
        assert_eq!(unique_jobs.len(), 5);
    }

    #[tokio::test]
    async fn test_job_inbox_empty() {
        let job_id = "job_test".to_string();
        let agent_id = "agent_test".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope).await;

        // Check if the job inbox is empty after creating a new job
        assert!(shinkai_db.is_job_inbox_empty(&job_id).unwrap());

        let (placeholder_signature_sk, _) = unsafe_deterministic_signature_keypair(0);
        let shinkai_message = ShinkaiMessageBuilder::job_message_from_llm_provider(
            job_id.to_string(),
            "something".to_string(),
            vec![],
            None,
            placeholder_signature_sk,
            "@@node1.shinkai".to_string(),
            "@@node1.shinkai".to_string(),
        )
        .unwrap();

        // Add a message to the job
        let _ = shinkai_db
            .add_message_to_job_inbox(&job_id.clone(), &shinkai_message, None, None)
            .await;

        // Check if the job inbox is not empty after adding a message
        assert!(!shinkai_db.is_job_inbox_empty(&job_id).unwrap());
    }

    #[tokio::test]
    async fn test_job_inbox_tree_structure() {
        let job_id = "job_test".to_string();
        let agent_id = "agent_test".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope).await;

        let (placeholder_signature_sk, _) = unsafe_deterministic_signature_keypair(0);

        let mut parent_message_hash: Option<String> = None;
        let mut parent_message_hash_2: Option<String> = None;

        /*
        The tree that we are creating looks like:
            1
            ├── 2
            │   ├── 4
            └── 3
         */
        for i in 1..=4 {
            let shinkai_message = ShinkaiMessageBuilder::job_message_from_llm_provider(
                job_id.clone(),
                format!("Hello World {}", i),
                vec![],
                None,
                placeholder_signature_sk.clone(),
                "@@node1.shinkai".to_string(),
                "@@node1.shinkai".to_string(),
            )
            .unwrap();

            let parent_hash: Option<String> = match i {
                2 | 3 => parent_message_hash.clone(),
                4 => parent_message_hash_2.clone(),
                _ => None,
            };

            // Add a message to the job
            let _ = shinkai_db
                .add_message_to_job_inbox(&job_id.clone(), &shinkai_message, parent_hash.clone(), None)
                .await;

            // Update the parent message according to the tree structure
            if i == 1 {
                parent_message_hash = Some(shinkai_message.calculate_message_hash_for_pagination());
            } else if i == 2 {
                parent_message_hash_2 = Some(shinkai_message.calculate_message_hash_for_pagination());
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        // Check if the job inbox is not empty after adding a message
        assert!(!shinkai_db.is_job_inbox_empty(&job_id).unwrap());

        // Get the inbox name
        let inbox_name = InboxName::get_job_inbox_name_from_params(job_id.clone()).unwrap();
        let inbox_name_value = match inbox_name {
            InboxName::RegularInbox { value, .. } | InboxName::JobInbox { value, .. } => value,
        };

        // Get the messages from the job inbox
        let last_messages_inbox = shinkai_db
            .get_last_messages_from_inbox(inbox_name_value.clone().to_string(), 4, None)
            .unwrap();

        // Check the content of the messages
        assert_eq!(last_messages_inbox.len(), 3);

        // Check the content of the first message array
        assert_eq!(last_messages_inbox[0].len(), 1);
        let message_content_1 = last_messages_inbox[0][0].clone().get_message_content().unwrap();
        let job_message_1: JobMessage = serde_json::from_str(&message_content_1).unwrap();
        assert_eq!(job_message_1.content, "Hello World 1".to_string());

        // Check the content of the second message array
        assert_eq!(last_messages_inbox[1].len(), 2);
        let message_content_2 = last_messages_inbox[1][0].clone().get_message_content().unwrap();
        let job_message_2: JobMessage = serde_json::from_str(&message_content_2).unwrap();
        assert_eq!(job_message_2.content, "Hello World 2".to_string());

        let message_content_3 = last_messages_inbox[1][1].clone().get_message_content().unwrap();
        let job_message_3: JobMessage = serde_json::from_str(&message_content_3).unwrap();
        assert_eq!(job_message_3.content, "Hello World 3".to_string());

        // Check the content of the third message array
        assert_eq!(last_messages_inbox[2].len(), 1);
        let message_content_4 = last_messages_inbox[2][0].clone().get_message_content().unwrap();
        let job_message_4: JobMessage = serde_json::from_str(&message_content_4).unwrap();
        assert_eq!(job_message_4.content, "Hello World 4".to_string());
    }

    #[tokio::test]
    async fn test_insert_steps_with_simple_tree_structure() {
        let node1_identity_name = "@@node1.shinkai";
        let node1_subidentity_name = "main_profile_node1";
        let (node1_identity_sk, _) = unsafe_deterministic_signature_keypair(0);
        let (node1_encryption_sk, node1_encryption_pk) = unsafe_deterministic_encryption_keypair(0);

        let job_id = "test_job";
        let agent_id = "agent_test".to_string();
        let scope = MinimalJobScope::default();

        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        let _ = create_new_job(&shinkai_db, job_id.to_string(), agent_id.clone(), scope).await;

        eprintln!("Inserting steps...\n\n");
        let mut parent_message_hash: Option<String> = None;
        let mut parent_message_hash_2: Option<String> = None;

        /*
        The tree that we are creating looks like:
            1
            ├── 2
            │   └── 4
            └── 3
         */
        for i in 1..=4 {
            let message_content = format!("Message {}", i);

            // Generate the ShinkaiMessage
            let message = generate_message_with_text(
                message_content.clone(),
                node1_encryption_sk.clone(),
                clone_signature_secret_key(&node1_identity_sk),
                node1_encryption_pk,
                node1_subidentity_name.to_string(),
                node1_identity_name.to_string(),
                format!("2023-07-02T20:53:34.81{}Z", i),
            );

            let parent_hash: Option<String> = match i {
                2 | 3 => parent_message_hash.clone(),
                4 => parent_message_hash_2.clone(),
                _ => None,
            };

            // Insert the ShinkaiMessage into the database
            shinkai_db
                .unsafe_insert_inbox_message(&message, parent_hash.clone(), None)
                .await
                .unwrap();

            // Update the parent message hash according to the tree structure
            if i == 1 {
                parent_message_hash = Some(message.calculate_message_hash_for_pagination());
            } else if i == 2 {
                parent_message_hash_2 = Some(message.calculate_message_hash_for_pagination());
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        let inbox_name = InboxName::get_job_inbox_name_from_params(job_id.to_string()).unwrap();
        let last_messages_inbox = shinkai_db
            .get_last_messages_from_inbox(inbox_name.to_string(), 3, None)
            .unwrap();

        let last_messages_content: Vec<Vec<String>> = last_messages_inbox
            .iter()
            .map(|message_array| {
                message_array
                    .iter()
                    .map(|message| message.clone().get_message_content().unwrap())
                    .collect()
            })
            .collect();

        eprintln!("Messages: {:?}", last_messages_content);

        eprintln!("\n\n Getting steps...");

        let step_history = shinkai_db.get_step_history(job_id, true).unwrap().unwrap();

        let step_history_content: Vec<String> = step_history
            .iter()
            .map(|shinkai_message| {
                eprintln!("Shinkai message: {:?}", shinkai_message);
                let prompt = shinkai_message.to_prompt();
                eprintln!("Prompt: {:?}", prompt);

                let message_content = match &prompt.sub_prompts[0] {
                    SubPrompt::Omni(_, text, _, _) => text,
                    _ => panic!("Unexpected SubPrompt variant"),
                };
                message_content.clone()
            })
            .collect();

        eprintln!("Step history: {:?}", step_history_content);

        assert_eq!(step_history.len(), 3);

        // Check the content of the steps
        assert_eq!(step_history_content[0], "Message 1".to_string());
        assert_eq!(step_history_content[1], "Message 2".to_string());
        assert_eq!(step_history_content[2], "Message 4".to_string());
    }

    #[tokio::test]
    async fn test_job_inbox_tree_structure_with_invalid_date() {
        let job_id = "job_test".to_string();
        let agent_id = "agent_test".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope).await;

        let (placeholder_signature_sk, _) = unsafe_deterministic_signature_keypair(0);

        // Create the messages
        let mut messages = Vec::new();
        for i in [1, 3, 2].iter() {
            let shinkai_message = ShinkaiMessageBuilder::job_message_from_llm_provider(
                job_id.clone(),
                format!("Hello World {}", i),
                vec![],
                None,
                placeholder_signature_sk.clone(),
                "@@node1.shinkai".to_string(),
                "@@node1.shinkai".to_string(),
            )
            .unwrap();
            messages.push(shinkai_message);

            sleep(Duration::from_millis(10)).await;
        }

        /*
        The tree that we are creating looks like:
            1
            ├── 2
                └── 3 (older date than 2. it should'nt fail)
         */

        // Add the messages to the job in a specific order to simulate an invalid date scenario
        for i in [0, 2, 1].iter() {
            let _result = shinkai_db
                .add_message_to_job_inbox(&job_id.clone(), &messages[*i], None, None)
                .await;

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Check if the job inbox is not empty after adding a message
        assert!(!shinkai_db.is_job_inbox_empty(&job_id).unwrap());

        // Get the inbox name
        let inbox_name = InboxName::get_job_inbox_name_from_params(job_id.clone()).unwrap();
        let inbox_name_value = match inbox_name {
            InboxName::RegularInbox { value, .. } | InboxName::JobInbox { value, .. } => value,
        };

        // Get the messages from the job inbox
        let last_messages_inbox = shinkai_db
            .get_last_messages_from_inbox(inbox_name_value.clone().to_string(), 3, None)
            .unwrap();

        // Check the content of the messages
        assert_eq!(last_messages_inbox.len(), 3);

        // Check the content of the first message array
        assert_eq!(last_messages_inbox[0].len(), 1);
        let message_content_1 = last_messages_inbox[0][0].clone().get_message_content().unwrap();
        let job_message_1: JobMessage = serde_json::from_str(&message_content_1).unwrap();
        assert_eq!(job_message_1.content, "Hello World 1".to_string());

        // Check the content of the second message array
        assert_eq!(last_messages_inbox[1].len(), 1);
        let message_content_2 = last_messages_inbox[1][0].clone().get_message_content().unwrap();
        let job_message_2: JobMessage = serde_json::from_str(&message_content_2).unwrap();
        assert_eq!(job_message_2.content, "Hello World 2".to_string());

        // Check the content of the second message array
        assert_eq!(last_messages_inbox[2].len(), 1);
        let message_content_3 = last_messages_inbox[2][0].clone().get_message_content().unwrap();
        let job_message_3: JobMessage = serde_json::from_str(&message_content_3).unwrap();
        assert_eq!(job_message_3.content, "Hello World 3".to_string());
    }

    #[tokio::test]
    async fn test_add_forked_job() {
        let job_id = "job1".to_string();
        let agent_id = "agent1".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create a new job
        let _ = create_new_job(&shinkai_db, job_id.clone(), agent_id.clone(), scope.clone()).await;

        let (placeholder_signature_sk, _) = unsafe_deterministic_signature_keypair(0);

        let mut parent_message_hash: Option<String> = None;
        let mut parent_message_hash_2: Option<String> = None;

        /*
        The tree that we are creating looks like:
            1
            ├── 2
            │   ├── 4
            └── 3
         */
        for i in 1..=4 {
            let shinkai_message = ShinkaiMessageBuilder::job_message_from_llm_provider(
                job_id.clone(),
                format!("Hello World {}", i),
                vec![],
                None,
                placeholder_signature_sk.clone(),
                "@@node1.shinkai".to_string(),
                "@@node1.shinkai".to_string(),
            )
            .unwrap();

            let parent_hash: Option<String> = match i {
                2 | 3 => parent_message_hash.clone(),
                4 => parent_message_hash_2.clone(),
                _ => None,
            };

            // Add a message to the job
            let _ = shinkai_db
                .add_message_to_job_inbox(&job_id.clone(), &shinkai_message, parent_hash.clone(), None)
                .await;

            // Update the parent message according to the tree structure
            if i == 1 {
                parent_message_hash = Some(shinkai_message.calculate_message_hash_for_pagination());
            } else if i == 2 {
                parent_message_hash_2 = Some(shinkai_message.calculate_message_hash_for_pagination());
            }

            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        // Get the inbox name
        let inbox_name = InboxName::get_job_inbox_name_from_params(job_id.clone()).unwrap();
        let inbox_name_value = inbox_name.to_string();

        // Get the messages from the job inbox
        let last_messages_inbox = shinkai_db
            .get_last_messages_from_inbox(inbox_name_value.clone().to_string(), 4, None)
            .unwrap();

        // Create forked jobs
        let forked_job1_id = "forked_job1".to_string();
        let forked_message1_id = last_messages_inbox
            .last()
            .unwrap()
            .last()
            .unwrap()
            .calculate_message_hash_for_pagination();
        let forked_job2_id = "forked_job2".to_string();
        let forked_message2_id = last_messages_inbox
            .first()
            .unwrap()
            .first()
            .unwrap()
            .calculate_message_hash_for_pagination();
        let _ = create_new_job(&shinkai_db, forked_job1_id.clone(), agent_id.clone(), scope.clone()).await;
        let _ = create_new_job(&shinkai_db, forked_job2_id.clone(), agent_id.clone(), scope).await;

        let forked_job1 = ForkedJob {
            job_id: forked_job1_id.clone(),
            message_id: forked_message1_id.clone(),
        };
        let forked_job2 = ForkedJob {
            job_id: forked_job2_id.clone(),
            message_id: forked_message2_id.clone(),
        };
        match shinkai_db.add_forked_job(&job_id, forked_job1) {
            Ok(_) => {}
            Err(e) => panic!("Error adding forked job: {:?}", e),
        }
        match shinkai_db.add_forked_job(&job_id, forked_job2) {
            Ok(_) => {}
            Err(e) => panic!("Error adding forked job: {:?}", e),
        }

        // Check that the forked jobs are added
        let job = shinkai_db.get_job(&job_id).unwrap();
        assert_eq!(job.forked_jobs.len(), 2);
        assert_eq!(job.forked_jobs[0].job_id, forked_job1_id);
        assert_eq!(job.forked_jobs[0].message_id, forked_message1_id);
        assert_eq!(job.forked_jobs[1].job_id, forked_job2_id);
        assert_eq!(job.forked_jobs[1].message_id, forked_message2_id);
    }

    #[tokio::test]
    async fn test_remove_job() {
        let job1_id = "job1".to_string();
        let job2_id = "job2".to_string();
        let agent_id = "agent1".to_string();
        let scope = MinimalJobScope::default();
        let db = setup_test_db();
        let shinkai_db = Arc::new(db);

        // Create new jobs
        let _ = create_new_job(&shinkai_db, job1_id.clone(), agent_id.clone(), scope.clone()).await;
        let _ = create_new_job(&shinkai_db, job2_id.clone(), agent_id.clone(), scope).await;

        // Check smart_inboxes
        let node1_identity_name = "@@node1.shinkai";
        let node1_subidentity_name = "main_profile_node1";
        let (_, node1_identity_pk) = unsafe_deterministic_signature_keypair(0);
        let (_, node1_encryption_pk) = unsafe_deterministic_encryption_keypair(0);

        let (_, node1_subidentity_pk) = unsafe_deterministic_signature_keypair(100);
        let (_, node1_subencryption_pk) = unsafe_deterministic_encryption_keypair(100);

        let node1_profile_identity = StandardIdentity::new(
            ShinkaiName::from_node_and_profile_names(
                node1_identity_name.to_string(),
                node1_subidentity_name.to_string(),
            )
            .unwrap(),
            None,
            node1_encryption_pk.clone(),
            node1_identity_pk.clone(),
            Some(node1_subencryption_pk),
            Some(node1_subidentity_pk),
            StandardIdentityType::Profile,
            IdentityPermissions::Standard,
        );

        let _ = shinkai_db.insert_profile(node1_profile_identity.clone());

        let inbox1_name = InboxName::get_job_inbox_name_from_params(job1_id.clone()).unwrap();
        let inbox2_name = InboxName::get_job_inbox_name_from_params(job2_id.clone()).unwrap();

        shinkai_db
            .add_permission(
                &inbox1_name.to_string(),
                &node1_profile_identity,
                InboxPermission::Admin,
            )
            .unwrap();
        shinkai_db
            .add_permission(
                &inbox2_name.to_string(),
                &node1_profile_identity,
                InboxPermission::Admin,
            )
            .unwrap();

        let smart_inboxes = shinkai_db
            .get_all_smart_inboxes_for_profile(node1_profile_identity.clone(), None, None)
            .unwrap();
        assert_eq!(smart_inboxes.len(), 2);

        // Remove the first job
        shinkai_db.remove_job(&job1_id).unwrap();

        // Check if the job is removed
        match shinkai_db.get_job(&job1_id) {
            Ok(_) => panic!("Expected an error when getting a removed job"),
            Err(e) => assert_eq!(matches!(e, SqliteManagerError::DataNotFound), true),
        }

        // Check if the smart_inbox is removed
        let smart_inboxes = shinkai_db
            .get_all_smart_inboxes_for_profile(node1_profile_identity.clone(), None, None)
            .unwrap();
        assert_eq!(smart_inboxes.len(), 1);
        assert!(smart_inboxes[0].inbox_id != inbox1_name.to_string());
    }
}
