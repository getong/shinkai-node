use crate::llm_provider::error::LLMProviderError;
use crate::managers::model_capabilities_manager::ModelCapabilitiesManager;
use crate::managers::model_capabilities_manager::PromptResult;
use crate::managers::model_capabilities_manager::PromptResultEnum;
use serde::ser::{SerializeStruct, Serializer};
use serde::{Deserialize, Serialize};
use serde_json::{self};
use shinkai_message_primitives::schemas::llm_providers::serialized_llm_provider::LLMProviderInterface;
use shinkai_message_primitives::schemas::prompts::Prompt;
use shinkai_message_primitives::schemas::subprompts::{SubPrompt, SubPromptType};

use super::shared_model_logic;

#[derive(Debug, Deserialize)]
pub struct OpenAIResponse {
    #[serde(default)]
    id: Option<String>,
    object: String,
    created: u64,
    pub choices: Vec<Choice>,
    usage: Usage,
    system_fingerprint: Option<String>,
    #[serde(rename = "x_groq", default)]
    groq: Option<GroqInfo>,
}

#[derive(Debug, Deserialize)]
pub struct Choice {
    pub index: i32,
    pub message: OpenAIApiMessage,
    #[serde(rename = "finish_reason")]
    pub finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct GroqInfo {
    id: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OpenAIApiMessage {
    pub role: String,
    pub content: Option<MessageContent>,
    pub function_call: Option<FunctionCall>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionCallResponse {
    pub response: String,
    pub function_call: FunctionCall,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MessageContent {
    FunctionCall(FunctionCallResponse),
    Text(String),
    ImageUrl { url: String },
}

impl Serialize for OpenAIApiMessage {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_struct("OpenAIApiMessage", 3)?;
        map.serialize_field("role", &self.role)?;
        if let Some(content) = &self.content {
            map.serialize_field("content", content)?;
        }
        if let Some(function_call) = &self.function_call {
            map.serialize_field("function_call", function_call)?;
        }
        map.end()
    }
}

#[derive(Debug, Deserialize)]
pub struct Usage {
    prompt_tokens: i32,
    completion_tokens: i32,
    total_tokens: i32,
    #[serde(default)]
    queue_time: Option<f64>,
    #[serde(default)]
    prompt_time: Option<f64>,
    #[serde(default)]
    completion_time: Option<f64>,
    #[serde(default)]
    total_time: Option<f64>,
}

pub fn openai_prepare_messages_deprecated(
    model: &LLMProviderInterface,
    prompt: Prompt,
) -> Result<PromptResult, LLMProviderError> {
    let mut prompt = prompt.clone();

    // If this is a reasoning model, filter out system prompts before any processing
    if ModelCapabilitiesManager::has_reasoning_capabilities(model) {
        prompt.sub_prompts.retain(|sp| match sp {
            SubPrompt::Content(SubPromptType::System, _, _) => false,
            SubPrompt::Omni(SubPromptType::System, _, _, _) => false,
            _ => true,
        });
    }

    let max_input_tokens = ModelCapabilitiesManager::get_max_input_tokens(model);

    // Generate the messages and filter out images
    let chat_completion_messages = prompt.generate_llm_messages(
        Some(max_input_tokens),
        Some("function".to_string()),
        &ModelCapabilitiesManager::num_tokens_from_llama3,
    )?;

    // Get a more accurate estimate of the number of used tokens
    let used_tokens = ModelCapabilitiesManager::num_tokens_from_messages(&chat_completion_messages);
    // Calculate the remaining output tokens available
    let remaining_output_tokens = ModelCapabilitiesManager::get_remaining_output_tokens(model, used_tokens);

    // Separate messages into those with a valid role and those without
    let (messages_with_role, tools): (Vec<_>, Vec<_>) = chat_completion_messages
        .into_iter()
        .partition(|message| message.role.is_some());

    // Convert both sets of messages to serde Value
    let messages_json = serde_json::to_value(messages_with_role)?;

    // Convert tools to serde Value with name transformation
    let tools_json = serde_json::to_value(
        tools
            .clone()
            .into_iter()
            .map(|mut tool| {
                if let Some(functions) = tool.functions.as_mut() {
                    for function in functions {
                        // Replace any characters that aren't alphanumeric, underscore, or hyphen
                        function.name = function
                            .name
                            .chars()
                            .map(|c| {
                                if c.is_alphanumeric() || c == '_' || c == '-' {
                                    c
                                } else {
                                    '_'
                                }
                            })
                            .collect::<String>()
                            .to_lowercase();
                    }
                }
                tool
            })
            .collect::<Vec<_>>(),
    )?;

    // Convert messages_json and tools_json to Vec<serde_json::Value>
    let messages_vec = match messages_json {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|mut message| {
                let images = message.get("images").cloned();
                let text = message.get("content").cloned();

                if let Some(serde_json::Value::Array(images_array)) = images {
                    let mut content = vec![];
                    if let Some(text) = text {
                        content.push(serde_json::json!({"type": "text", "text": text}));
                    }
                    for image in images_array {
                        if let serde_json::Value::String(image_str) = image {
                            if let Some(image_type) = shared_model_logic::get_image_type(&image_str) {
                                content.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:image/{};base64,{}", image_type, image_str)}
                                }));
                            }
                        }
                    }
                    message["content"] = serde_json::json!(content);
                    message.as_object_mut().unwrap().remove("images");
                }
                message
            })
            .collect(),
        _ => vec![],
    };

    // Flatten the tools array to extract functions directly
    // TODO: this is to support the old functions format. We need to update it to tools
    let tools_vec = match tools_json {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .flat_map(|tool| {
                if let serde_json::Value::Object(mut map) = tool {
                    // TODO: functions is deprecated in favor of tools. Update it
                    map.remove("functions")
                        .and_then(|functions| {
                            if let serde_json::Value::Array(funcs) = functions {
                                Some(funcs)
                            } else {
                                None
                            }
                        })
                        .unwrap_or_default()
                } else {
                    vec![]
                }
            })
            .collect(),
        _ => vec![],
    };

    Ok(PromptResult {
        messages: PromptResultEnum::Value(serde_json::Value::Array(messages_vec)),
        functions: Some(tools_vec),
        remaining_output_tokens,
        tokens_used: used_tokens,
    })
}

pub fn openai_prepare_messages_gemini(
    model: &LLMProviderInterface,
    prompt: Prompt,
) -> Result<PromptResult, LLMProviderError> {
    let max_input_tokens = ModelCapabilitiesManager::get_max_input_tokens(model);

    // Generate the messages and filter out images
    let chat_completion_messages = prompt.generate_llm_messages(
        Some(max_input_tokens),
        Some("function".to_string()),
        &ModelCapabilitiesManager::num_tokens_from_llama3,
    )?;

    // Get a more accurate estimate of the number of used tokens
    let used_tokens = ModelCapabilitiesManager::num_tokens_from_messages(&chat_completion_messages);
    // Calculate the remaining output tokens available
    let remaining_output_tokens = ModelCapabilitiesManager::get_remaining_output_tokens(model, used_tokens);

    // Separate messages into those with a valid role and those without
    let (messages_with_role, tools): (Vec<_>, Vec<_>) = chat_completion_messages
        .into_iter()
        .partition(|message| message.role.is_some());

    // Convert messages to serde Value
    let messages_json = serde_json::to_value(messages_with_role)?;

    // Convert tools to the new Gemini format
    let tools_vec: Vec<serde_json::Value> = tools
        .into_iter()
        .flat_map(|tool| {
            tool.functions.unwrap_or_default().into_iter().map(|function| {
                serde_json::json!({
                    "type": "function",
                    "function": {
                        "name": function.name.chars()
                            .map(|c| if c.is_alphanumeric() || c == '_' || c == '-' { c } else { '_' })
                            .collect::<String>()
                            .to_lowercase(),
                        "description": function.description,
                        "parameters": function.parameters
                    }
                })
            })
        })
        .collect();

    // Process messages similar to the original function
    let messages_vec = match messages_json {
        serde_json::Value::Array(arr) => arr
            .into_iter()
            .map(|mut message| {
                let images = message.get("images").cloned();
                let text = message.get("content").cloned();

                if let Some(serde_json::Value::Array(images_array)) = images {
                    let mut content = vec![];
                    if let Some(text) = text {
                        content.push(serde_json::json!({"type": "text", "text": text}));
                    }
                    for image in images_array {
                        if let serde_json::Value::String(image_str) = image {
                            if let Some(image_type) = shared_model_logic::get_image_type(&image_str) {
                                content.push(serde_json::json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!("data:image/{};base64,{}", image_type, image_str)}
                                }));
                            }
                        }
                    }
                    message["content"] = serde_json::json!(content);
                    message.as_object_mut().unwrap().remove("images");
                }
                message
            })
            .collect(),
        _ => vec![],
    };

    Ok(PromptResult {
        messages: PromptResultEnum::Value(serde_json::Value::Array(messages_vec)),
        functions: Some(tools_vec),
        remaining_output_tokens,
        tokens_used: used_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use shinkai_message_primitives::schemas::llm_providers::serialized_llm_provider::SerializedLLMProvider;
    use shinkai_message_primitives::schemas::subprompts::{SubPrompt, SubPromptAssetType, SubPromptType};

    #[test]
    fn test_openai_from_llm_messages() {
        let sub_prompts = vec![
            SubPrompt::Omni(
                SubPromptType::System,
                "You are Neko the cat respond like one".to_string(),
                vec![],
                98,
            ),
            SubPrompt::Omni(SubPromptType::User, "Hello".to_string(), vec![], 97),
            SubPrompt::Omni(
                SubPromptType::Assistant,
                "Great to meet you. What would you like to know?".to_string(),
                vec![],
                97,
            ),
            SubPrompt::Omni(
                SubPromptType::User,
                "I have two dogs in my house. How many paws are in my house?".to_string(),
                vec![(
                    SubPromptAssetType::Image,
                    "iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAMAAABrrFhUAAAABGdBTUEAALGPC/xhBQAAAAFzUkdCAK7OHOkAAAD5UExURQAAAACl7QCl7ACm7ACl7ACl7ACm7QCm7QCm7QCm7ACm7ACm7QCl7QCl6wCl7ACl7ACl7ACl7QCl6wCl7QCl7ACm7ACm7ACl7QCm7ACl7QCl6wCm7QCm7QCl7ACl7QCm7QCl7QCl7QCm7ACm7QCl6wCl7ACl7QCl7ACm7ACm7ACl7QCl7ACl7QCm7QCm7ACm7ACl7ACl7QCl6wCm7QCm6wCm7QCm7QCm7QCl7QCl7ACm7QCl7ACm7QCl7QCl7ACk6wCl7QCl7ACm7ACl7QCm7ACl7QCl7ACm7QCl7ACm7ACm7ACm7QCl7ACl7ACm7QCl7QCk7ACm7ACm7ahktTwAAABSdFJOUwDoJJubI+v7+vaN9fswD50JzFrLCCbo+esOCcvUWZvM6S9Z+YzP0cyc0VrriQ6MCCX0JIoK7J5Z9p6ZDi8PiCPr6NMl1CTRJSbn+p2cWiSgD4gNsVXUAAACIElEQVR42u3X11KVMRiG0Wx2+femN+kgiL1Ls4MKKhZQc/8X4xmnye+BM3yznjt4VyaTSUqSJEmSJElX/Z7/ObfS5Gtes3Kz+/D9P8y/MTPKYXr167zl/LXxYQ7V8OlBm/1jGzlcZ3v1+98t5YB1pqrPv5NDtj1Wef83ctC+LFYBjOewPa56/4ZxAXYvKwBe58BtlfdPjyIDTKwWAeZz6DaLAPdjA/SKAPdiAywUAQ5jAwyKAE1sgMkiQA4eAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA8D8Amtj7J4sAg9gAj4oAc7EB1osAvdgA3SLARWyAW0WA6VHk/ROrRYA0ExngR3l/ej6Mu3/5tAIg3Y4L8Kdmf5r9FHX/g2dVAKn/Meb+pX6qbOdbxP2dqVRd/068/XdPUosWj5djzf++P5va9fnNizjzR0dfU/uevOyuf7j2v+NmsNDbfJskSZIkSZKu+gtLvn0aIyUzCwAAAABJRU5ErkJggg==".to_string(),
                    "image.png".to_string(),
                )],
                100,
            ),
        ];

        let mut prompt = Prompt::new();
        prompt.add_sub_prompts(sub_prompts);

        // Use the mock provider
        let model = SerializedLLMProvider::mock_provider().model;

        // Call the openai_prepare_messages function
        let result = openai_prepare_messages_deprecated(&model, prompt).expect("Failed to prepare messages");

        // Define the expected messages and functions
        let expected_messages = json!([
            {
                "role": "system",
                "content": [
                    { "type": "text", "text": "You are Neko the cat respond like one" }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "Hello" }
                ]
            },
            {
                "role": "assistant",
                "content": [
                    { "type": "text", "text": "Great to meet you. What would you like to know?" }
                ]
            },
            {
                "role": "user",
                "content": [
                    { "type": "text", "text": "I have two dogs in my house. How many paws are in my house?" },
                    {
                        "type": "image_url",
                        "image_url": { "url": "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAQAAAAEACAMAAABrrFhUAAAABGdBTUEAALGPC/xhBQAAAAFzUkdCAK7OHOkAAAD5UExURQAAAACl7QCl7ACm7ACl7ACl7ACm7QCm7QCm7QCm7ACm7ACm7QCl7QCl6wCl7ACl7ACl7ACl7QCl6wCl7QCl7ACm7ACm7ACl7QCm7ACl7QCl6wCm7QCm7QCl7ACl7QCm7QCl7QCl7QCm7ACm7QCl6wCl7ACl7QCl7ACm7ACm7ACl7QCl7ACl7QCm7QCm7ACm7ACl7ACl7QCl6wCm7QCm6wCm7QCm7QCm7QCl7QCl7ACm7QCl7ACm7QCl7QCl7ACk6wCl7QCl7ACm7ACl7QCm7ACl7QCl7ACm7QCl7ACm7ACm7ACm7QCl7ACl7ACm7QCl7QCk7ACm7ACm7ahktTwAAABSdFJOUwDoJJubI+v7+vaN9fswD50JzFrLCCbo+esOCcvUWZvM6S9Z+YzP0cyc0VrriQ6MCCX0JIoK7J5Z9p6ZDi8PiCPr6NMl1CTRJSbn+p2cWiSgD4gNsVXUAAACIElEQVR42u3X11KVMRiG0Wx2+femN+kgiL1Ls4MKKhZQc/8X4xmnye+BM3yznjt4VyaTSUqSJEmSJElX/Z7/ObfS5Gtes3Kz+/D9P8y/MTPKYXr167zl/LXxYQ7V8OlBm/1jGzlcZ3v1+98t5YB1pqrPv5NDtj1Wef83ctC+LFYBjOewPa56/4ZxAXYvKwBe58BtlfdPjyIDTKwWAeZz6DaLAPdjA/SKAPdiAywUAQ5jAwyKAE1sgMkiQA4eAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA8D8Amtj7J4sAg9gAj4oAc7EB1osAvdgA3SLARWyAW0WA6VHk/ROrRYA0ExngR3l/ej6Mu3/5tAIg3Y4L8Kdmf5r9FHX/g2dVAKn/Meb+pX6qbOdbxP2dqVRd/068/XdPUosWj5djzf++P5va9fnNizjzR0dfU/uevOyuf7j2v+NmsNDbfJskSZIkSZKu+gtLvn0aIyUzCwAAAABJRU5ErkJggg==" }
                    }
                ]
            }
        ]);

        // Assert the results
        assert_eq!(result.messages, PromptResultEnum::Value(expected_messages));
        assert!(result.remaining_output_tokens > 0);
    }

    #[test]
    fn test_openai_api_message_with_function_call() {
        let json_str = json!({
            "role": "assistant",
            "content": null,
            "function_call":{
                "name": "concat_strings",
                "arguments":  json!({
                    "first_string": "hola",
                    "second_string": " chao"
                }).to_string()
            }
        })
        .to_string();

        // Deserialize the JSON string to OpenAIApiMessage
        let message: OpenAIApiMessage = serde_json::from_str(&json_str).expect("Failed to deserialize");

        // Check the deserialized values
        assert_eq!(message.role, "assistant");
        assert!(message.content.is_none());
        assert!(message.function_call.is_some());

        if let Some(function_call) = message.function_call.clone() {
            assert_eq!(function_call.name, "concat_strings");
            assert_eq!(
                function_call.arguments,
                json!({"first_string": "hola", "second_string": " chao"}).to_string()
            );
        }

        // Serialize the OpenAIApiMessage back to JSON
        let serialized_json = serde_json::to_string(&message).expect("Failed to serialize");

        // Deserialize again to check round-trip consistency
        let deserialized_message: OpenAIApiMessage =
            serde_json::from_str(&serialized_json).expect("Failed to deserialize");

        // Check the deserialized values again
        assert_eq!(deserialized_message.role, "assistant");
        assert!(deserialized_message.content.is_none());
        assert!(deserialized_message.function_call.is_some());

        if let Some(function_call) = deserialized_message.function_call {
            assert_eq!(function_call.name, "concat_strings");
            assert_eq!(
                function_call.arguments,
                json!({"first_string": "hola", "second_string": " chao"}).to_string()
            );
        }
    }

    #[test]
    fn test_openai_response_after_tool_usage_parsing() {
        let response_text = r#"
        {
            "id": "chatcmpl-9cQYyc4ENYwJ5ChU4WHtRv7uPRHbN",
            "object": "chat.completion",
            "created": 1718945600,
            "model": "gpt-4-1106-preview",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": "The concatenated result of \"hola\" and \"chao\" is \"hola chao\"."
                    },
                    "logprobs": null,
                    "finish_reason": "stop"
                }
            ],
            "usage": {
                "prompt_tokens": 156,
                "completion_tokens": 21,
                "total_tokens": 177
            },
            "system_fingerprint": null
        }
        "#;

        // Deserialize the JSON string to OpenAIResponse
        let response: OpenAIResponse = serde_json::from_str(response_text).expect("Failed to deserialize");

        // Check the deserialized values
        assert_eq!(response.id.clone().unwrap(), "chatcmpl-9cQYyc4ENYwJ5ChU4WHtRv7uPRHbN");
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.created, 1718945600);
        assert_eq!(response.choices.len(), 1);
        assert_eq!(response.usage.prompt_tokens, 156);
        assert_eq!(response.usage.completion_tokens, 21);
        assert_eq!(response.usage.total_tokens, 177);

        let choice = &response.choices[0];
        assert_eq!(choice.index, 0);
        assert_eq!(choice.message.role, "assistant");
        if let Some(MessageContent::Text(content)) = &choice.message.content {
            assert_eq!(
                content,
                "The concatenated result of \"hola\" and \"chao\" is \"hola chao\"."
            );
        } else {
            panic!("Expected text content");
        }
    }

    #[test]
    fn test_groq_response_with_tool_calls() {
        let response_text = r#"{
            "id": "chatcmpl-0cae310a-2b36-470a-9261-0f24d77b01bc",
            "object": "chat.completion",
            "created": 1736736692,
            "model": "llama-3.2-11b-vision-preview",
            "choices": [
                {
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "tool_calls": [
                            {
                                "id": "call_sa3n",
                                "type": "function",
                                "function": {
                                    "name": "duckduckgo_search",
                                    "arguments": "{\"message\": \"best movie 2024\"}"
                                }
                            }
                        ]
                    },
                    "logprobs": null,
                    "finish_reason": "tool_calls"
                }
            ],
            "usage": {
                "queue_time": 0.018144843999999993,
                "prompt_tokens": 1185,
                "prompt_time": 0.077966956,
                "completion_tokens": 21,
                "completion_time": 0.028,
                "total_tokens": 1206,
                "total_time": 0.105966956
            },
            "system_fingerprint": "fp_9cb648b966",
            "x_groq": {
                "id": "req_01jhes5nvkedsb8hcw0x912fa6"
            }
        }"#;

        let response: OpenAIResponse = serde_json::from_str(response_text).expect("Failed to deserialize");

        // Verify basic response fields
        assert_eq!(
            response.id.clone().unwrap(),
            "chatcmpl-0cae310a-2b36-470a-9261-0f24d77b01bc"
        );
        assert_eq!(response.object, "chat.completion");
        assert_eq!(response.created, 1736736692);
        assert_eq!(response.system_fingerprint, Some("fp_9cb648b966".to_string()));

        // Verify choices
        assert_eq!(response.choices.len(), 1);
        let choice = &response.choices[0];
        assert_eq!(choice.index, 0);
        assert_eq!(choice.finish_reason, Some("tool_calls".to_string()));

        // Verify tool calls
        let message = &choice.message;
        assert_eq!(message.role, "assistant");
        assert!(message.content.is_none());

        let tool_calls = message.tool_calls.as_ref().expect("Should have tool_calls");
        assert_eq!(tool_calls.len(), 1);

        let tool_call = &tool_calls[0];
        assert_eq!(tool_call.id, "call_sa3n");
        assert_eq!(tool_call.call_type, "function");
        assert_eq!(tool_call.function.name, "duckduckgo_search");
        assert_eq!(tool_call.function.arguments, "{\"message\": \"best movie 2024\"}");

        // Verify usage
        assert_eq!(response.usage.prompt_tokens, 1185);
        assert_eq!(response.usage.completion_tokens, 21);
        assert_eq!(response.usage.total_tokens, 1206);
        assert!(response.usage.queue_time.is_some());
        assert!(response.usage.prompt_time.is_some());
        assert!(response.usage.completion_time.is_some());
        assert!(response.usage.total_time.is_some());

        // Verify Groq info
        let groq = response.groq.expect("Should have Groq info");
        assert_eq!(groq.id, "req_01jhes5nvkedsb8hcw0x912fa6");
    }

    #[test]
    fn test_system_prompt_filtering() {
        // Create a prompt with both Content and Omni system prompts
        let sub_prompts = vec![
            SubPrompt::Content(
                SubPromptType::System,
                "System prompt that should be filtered".to_string(),
                98,
            ),
            SubPrompt::Content(SubPromptType::User, "User message that should remain".to_string(), 100),
            SubPrompt::Omni(
                SubPromptType::UserLastMessage,
                "Last user message that should remain".to_string(),
                vec![],
                100,
            ),
        ];

        let mut prompt = Prompt::new();
        prompt.add_sub_prompts(sub_prompts);

        // Create a mock model with reasoning capabilities
        let model = SerializedLLMProvider::mock_provider_with_reasoning().model;

        // Process the prompt
        let result = openai_prepare_messages_deprecated(&model, prompt).expect("Failed to prepare messages");

        // Extract the messages from the result
        let messages = match &result.messages {
            PromptResultEnum::Value(value) => value.as_array().unwrap(),
            _ => panic!("Expected Value variant"),
        };

        // Verify that only non-system messages remain
        assert_eq!(messages.len(), 2, "Should only have 2 messages after filtering");

        // Check that the remaining messages are the user messages
        for message in messages {
            let role = message["role"].as_str().unwrap();
            assert_eq!(role, "user", "All remaining messages should be user messages");
        }
    }
}
