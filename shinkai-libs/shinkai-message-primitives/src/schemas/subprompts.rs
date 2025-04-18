use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::fmt;

use super::{llm_message::LlmMessage, prompts::Prompt, shinkai_fs::ShinkaiFileChunk};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubPromptType {
    User,
    UserLastMessage,
    System,
    Assistant,
    ExtraContext,
    AvailableTool,
    Function,
}

impl fmt::Display for SubPromptType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            SubPromptType::User => "user",
            SubPromptType::UserLastMessage => "user",
            SubPromptType::System => "system",
            SubPromptType::Assistant => "assistant",
            SubPromptType::ExtraContext => "user",
            SubPromptType::AvailableTool => "tool",
            SubPromptType::Function => "function",
        };
        write!(f, "{}", s)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubPromptAssetType {
    Image,
    Video,
    Audio,
}

pub type SubPromptAssetContent = String;
pub type SubPromptAssetDetail = String;
pub type PriorityValue = u8;

/// Sub-prompts are composed of a 3-element tuple of (SubPromptType, text, priority_value)
/// Priority_value is a number between 0-100, where the higher it is the less likely it will be
/// removed if LLM context window limits are reached.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SubPrompt {
    Content(SubPromptType, String, PriorityValue),
    Asset(
        SubPromptType,
        SubPromptAssetType,
        SubPromptAssetContent,
        SubPromptAssetDetail,
        PriorityValue,
    ),
    ToolAvailable(SubPromptType, JsonValue, PriorityValue),
    FunctionCall(SubPromptType, JsonValue, PriorityValue),
    FunctionCallResponse(SubPromptType, JsonValue, PriorityValue),
    Omni(
        SubPromptType,
        String,
        Vec<(SubPromptAssetType, SubPromptAssetContent, SubPromptAssetDetail)>,
        PriorityValue,
    ),
}

impl SubPrompt {
    /// Returns the length of the SubPrompt content string
    pub fn len(&self) -> usize {
        match self {
            SubPrompt::Content(_, content, _) => content.len(),
            SubPrompt::Asset(_, _, content, _, _) => content.len(),
            SubPrompt::ToolAvailable(_, content, _) => content.to_string().len(),
            SubPrompt::FunctionCall(_, content, _) => content.to_string().len(),
            SubPrompt::FunctionCallResponse(_, content, _) => content.to_string().len(),
            SubPrompt::Omni(_, content, _, _) => content.len(), // Note: shouldn't this account for the assets as well?
        }
    }

    /// Checks if the SubPrompt content is empty
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Generates a human/LLM-readable output string from the SubPrompt content
    pub fn generate_output_string(&self) -> String {
        match self {
            SubPrompt::Content(_, content, _) => content.clone(),
            SubPrompt::Asset(_, asset_type, _asset_content, asset_detail, _) => {
                format!("Asset Type: {:?}, Content: ..., Detail: {:?}", asset_type, asset_detail)
            }
            SubPrompt::ToolAvailable(_, content, _) => content.to_string(),
            SubPrompt::FunctionCall(_, content, _) => content.to_string(),
            SubPrompt::FunctionCallResponse(_, content, _) => content.to_string(),
            SubPrompt::Omni(_, content, assets, _) => {
                let assets_str = assets
                    .iter()
                    .map(|(asset_type, _asset_content, asset_detail)| {
                        format!("Asset Type: {:?}, Detail: {:?}", asset_type, asset_detail)
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("Content: {}, Assets: [{}]", content, assets_str)
            }
        }
    }

    /// Extracts generic subprompt data, returning a tuple of the prompt type, content, and content type.
    pub fn extract_generic_subprompt_data(&self) -> (SubPromptType, String, &'static str) {
        match self {
            SubPrompt::Content(prompt_type, _, _) => (prompt_type.clone(), self.generate_output_string(), "text"),
            SubPrompt::Asset(prompt_type, _, asset_content, _, _) => {
                (prompt_type.clone(), asset_content.clone(), "image")
            }
            SubPrompt::ToolAvailable(prompt_type, _, _) => (prompt_type.clone(), self.generate_output_string(), "text"),
            SubPrompt::FunctionCall(prompt_type, _, _) => (prompt_type.clone(), self.generate_output_string(), "text"),
            SubPrompt::FunctionCallResponse(prompt_type, _, _) => {
                (prompt_type.clone(), self.generate_output_string(), "text")
            }
            SubPrompt::Omni(prompt_type, _, _, _) => (prompt_type.clone(), self.generate_output_string(), "omni"),
        }
    }
    /// Gets the content of the SubPrompt (aka. updates it to the provided string)
    pub fn get_content(&self) -> String {
        match self {
            SubPrompt::Content(_, content, _) => content.clone(),
            SubPrompt::Asset(_, _, asset_content, _, _) => asset_content.clone(),
            SubPrompt::ToolAvailable(_, content, _) => content.to_string(),
            SubPrompt::FunctionCall(_, content, _) => content.to_string(),
            SubPrompt::FunctionCallResponse(_, content, _) => content.to_string(),
            SubPrompt::Omni(_, content, _, _) => content.clone(),
        }
    }

    /// Sets the content of the SubPrompt (aka. updates it to the provided string)
    pub fn set_content(&mut self, new_content: String) {
        match self {
            SubPrompt::Content(_, content, _) => *content = new_content,
            SubPrompt::Asset(_, _, asset_content, _, _) => *asset_content = new_content,
            SubPrompt::ToolAvailable(_, content, _) => *content = serde_json::Value::String(new_content),
            SubPrompt::FunctionCall(_, content, _) => *content = serde_json::Value::String(new_content),
            SubPrompt::FunctionCallResponse(_, content, _) => *content = serde_json::Value::String(new_content),
            SubPrompt::Omni(_, content, _, _) => *content = new_content, /* Note: shouldn't this account for the
                                                                          * assets as well? */
        }
    }

    /// Trims the content inside of the subprompt to the specified length.
    pub fn trim_content_to_length(&mut self, max_length: usize) {
        let (_prompt_type, content, _type_) = self.extract_generic_subprompt_data();
        if content.len() > max_length {
            self.set_content(content.chars().take(max_length).collect());
        }
    }

    /// Converts a subprompt into a ChatCompletionRequestMessage
    pub fn into_chat_completion_request_message(&self) -> LlmMessage {
        match self {
            SubPrompt::Omni(prompt_type, content, assets, _) => {
                let images = assets
                    .iter()
                    .map(|(_, asset_content, _)| asset_content.clone())
                    .collect::<Vec<_>>();
                LlmMessage {
                    role: Some(prompt_type.to_string()),
                    content: Some(content.clone()),
                    name: None,
                    function_call: None,
                    functions: None,
                    images: Some(images),
                    tool_calls: None,
                }
            }
            _ => {
                let (prompt_type, content, type_) = self.extract_generic_subprompt_data();
                LlmMessage {
                    role: Some(prompt_type.to_string()),
                    content: Some(content),
                    name: if type_ == "text" { None } else { Some(type_.to_string()) },
                    function_call: None,
                    functions: None,
                    images: None,
                    tool_calls: None,
                }
            }
        }
    }

    /// Counts the number of (estimated) tokens that the sub-prompt will be treated as when converted into a completion
    /// message. In other words, this is the "real" estimated token count (not just naive utf-8 character count).
    pub fn count_tokens_as_completion_message<F>(&self, token_counter: F) -> usize
    where
        F: Fn(&[LlmMessage]) -> usize,
    {
        let new_message = self.into_chat_completion_request_message();
        self.count_tokens_with_pregenerated_completion_message(&new_message, token_counter)
    }

    /// Counts the number of (estimated) tokens that the sub-prompt will be treated as when converted into a completion
    /// message. In other words, this is the "real" estimated token count (not just naive utf-8 character count).
    /// This accepts a pregenerated completion message made from self.into_chat_completion_request_message() for greater
    /// efficiency.
    pub fn count_tokens_with_pregenerated_completion_message<F>(
        &self,
        completion_message: &LlmMessage,
        token_counter: F,
    ) -> usize
    where
        F: Fn(&[LlmMessage]) -> usize,
    {
        // Only count tokens for non-image content
        let (_, _, type_) = self.extract_generic_subprompt_data();
        if type_ == "image" {
            return 0;
        }

        token_counter(&[completion_message.clone()])
    }

    // /// Converts a vector resource into a series of subprompts to be used in a prompt
    // /// If the VR is ordered, the output will be as well.
    // pub fn convert_resource_into_subprompts(resource: &BaseVectorResource, subprompt_priority: u8) -> Vec<SubPrompt>
    // {     let mut temp_prompt = Prompt::new();

    //     let nodes = resource.as_trait_object().get_all_nodes_flattened();

    //     // Iterate through each node and add its text string to the prompt (which is the name of the VR)
    //     for node in nodes {
    //         if let Ok(content) = node.get_text_content() {
    //             temp_prompt.add_content(content.to_string(), SubPromptType::ExtraContext, subprompt_priority);
    //         }
    //         if let Ok(resource) = node.get_vector_resource_content() {
    //             temp_prompt.add_content(
    //                 resource.as_trait_object().name().to_string(),
    //                 SubPromptType::ExtraContext,
    //                 subprompt_priority,
    //             );
    //         }
    //     }

    //     temp_prompt.sub_prompts
    // }

    // // TODO: if we have content with the same extra_info, don't repeat the extra info!
    // pub fn convert_resource_into_subprompts_with_extra_info(
    //     resource: &BaseVectorResource,
    //     subprompt_priority: u8,
    // ) -> Vec<SubPrompt> {
    //     let mut temp_prompt = Prompt::new();
    //     let resource_trait = resource.as_trait_object();
    //     let nodes = resource_trait.get_all_nodes_flattened();
    //     let mut last_content = String::new();
    //     let mut last_reference = String::new();
    //     let mut buffer_content = String::new();

    //     for (i, node) in nodes.iter().enumerate() {
    //         let mut current_content = String::new();

    //         if let Ok(content) = node.get_text_content() {
    //             current_content = content.to_string();
    //         } else if let Ok(resource) = node.get_vector_resource_content() {
    //             current_content = resource.as_trait_object().name().to_string();
    //         }

    //         // Some text is repeated between nodes, so we skip it
    //         if current_content.is_empty() || current_content == last_content {
    //             continue;
    //         }

    //         let mut extra_info = String::new();
    //         let file_name = resource_trait.source().format_source_string();

    //         if let Some(metadata) = &node.metadata {
    //             if let Some(pg_nums) = metadata.get("pg_nums") {
    //                 extra_info = format!("\nRef. page: {} from {}.", pg_nums, file_name);
    //             } else {
    //                 extra_info = format!("\nRef. from {}.", file_name);
    //             }
    //         } else {
    //             extra_info = format!("\nRef. from {}.", file_name);
    //         }

    //         if extra_info != last_reference {
    //             if !buffer_content.is_empty() {
    //                 temp_prompt.add_content(buffer_content.clone(), SubPromptType::ExtraContext, subprompt_priority);
    //             }
    //             buffer_content.clone_from(&current_content);
    //             last_reference.clone_from(&extra_info);
    //         } else {
    //             buffer_content.push_str(&format!(" {}", current_content));
    //         }

    //         if i == nodes.len() - 1 || extra_info != last_reference {
    //             buffer_content.push_str(&extra_info);
    //             temp_prompt.add_content(buffer_content.clone(), SubPromptType::ExtraContext, subprompt_priority);
    //             buffer_content.clear();
    //         }

    //         last_content = current_content;
    //     }

    //     temp_prompt.remove_all_subprompts()
    // }

    pub fn convert_chunks_into_subprompts_with_extra_info(
        chunks: &[ShinkaiFileChunk],
        subprompt_priority: u8,
    ) -> Vec<SubPrompt> {
        let mut temp_prompt = Prompt::new();
        let mut last_content = String::new();
        let mut last_reference = String::new();
        let mut buffer_content = String::new();

        for (i, chunk) in chunks.iter().enumerate() {
            let current_content = chunk.content.clone();

            // Skip empty or repeated content
            if current_content.is_empty() || current_content == last_content {
                continue;
            }

            // Generate extra info based on the chunk's position and file ID
            let extra_info = format!(
                "\nRef. chunk: {} from file ID: {}.",
                chunk.position, chunk.parsed_file_id
            );

            if extra_info != last_reference {
                if !buffer_content.is_empty() {
                    temp_prompt.add_content(buffer_content.clone(), SubPromptType::ExtraContext, subprompt_priority);
                }
                buffer_content.clone_from(&current_content);
                last_reference.clone_from(&extra_info);
            } else {
                buffer_content.push_str(&format!(" {}", current_content));
            }

            if i == chunks.len() - 1 || extra_info != last_reference {
                buffer_content.push_str(&extra_info);
                temp_prompt.add_content(buffer_content.clone(), SubPromptType::ExtraContext, subprompt_priority);
                buffer_content.clear();
            }

            last_content = current_content;
        }

        temp_prompt.remove_all_subprompts()
    }
}
