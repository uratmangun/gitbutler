use async_openai::types::{
    ChatCompletionRequestSystemMessage, ChatCompletionRequestUserMessage,
    CreateChatCompletionRequestArgs, ResponseFormat, ResponseFormatJsonSchema,
};
use schemars::{JsonSchema, schema_for};

pub fn commit_message_blocking(
    external_summary: &str,
    external_prompt: &str,
    diff: &str,
) -> anyhow::Result<String> {
    let change_summary_owned = external_summary.to_string();
    let external_prompt_owned = external_prompt.to_string();
    let diff_owned = diff.to_string();

    std::thread::spawn(move || {
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(commit_message(
                &change_summary_owned,
                &external_prompt_owned,
                &diff_owned,
            ))
    })
    .join()
    .unwrap()
}

pub async fn commit_message(
    external_summary: &str,
    external_prompt: &str,
    diff: &str,
) -> anyhow::Result<String> {
    let system_message =
        "You are a version control assistant that helps with Git branch committing.".to_string();
    let user_message = format!(
        "Extract the git commit data from the prompt, summary and diff output. Return the commit message. Determine from this AI prompt, summary and diff output what the git commit data should be.\n\n{}\n\nHere is the data:\n\nPrompt: {}\n\nSummary: {}\n\nDiff:\n```\n{}\n```\n\n",
        DEFAULT_COMMIT_MESSAGE_INSTRUCTIONS, external_prompt, external_summary, diff
    );

    let client = crate::provider::OpenAiProvider::new(reqwest::Client::default())?.gitbutler()?;

    let schema = schema_for!(StructuredOutput);
    let schema_json = serde_json::to_value(schema).unwrap();
    let response_format = ResponseFormat::JsonSchema {
        json_schema: ResponseFormatJsonSchema {
            description: None,
            name: "commit_message".into(),
            schema: Some(schema_json),
            strict: Some(true),
        },
    };

    let request = CreateChatCompletionRequestArgs::default()
        .model("gpt-4o")
        .messages([
            ChatCompletionRequestSystemMessage::from(system_message).into(),
            ChatCompletionRequestUserMessage::from(user_message).into(),
        ])
        .response_format(response_format)
        .build()?;

    let response = client.chat().create(request).await?;
    let response_string = response
        .choices
        .first()
        .unwrap()
        .message
        .content
        .as_ref()
        .unwrap();

    let structured_output: StructuredOutput = serde_json::from_str(response_string)
        .map_err(|e| anyhow::anyhow!("Failed to parse response: {}", e))?;

    Ok(structured_output.commit_message)
}

#[derive(serde::Serialize, serde::Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(deny_unknown_fields)]
pub struct StructuredOutput {
    pub commit_message: String,
}

const DEFAULT_COMMIT_MESSAGE_INSTRUCTIONS: &str = r#"The message should be a short summary line, followed by two newlines, then a short paragraph explaining WHY the change was needed based off the prompt.

- If a summary is provided, use it to create more short paragraphs or bullet points explaining the changes.
- The first summary line should be no more than 50 characters.
- Use the imperative mood for the message (e.g. "Add user authentication system" instead of "Adding user authentication system").

Here is an example of a good commit message:

bundle-uri: copy all bundle references ino the refs/bundle space

When downloading bundles via the bundle-uri functionality, we only copy the
references from refs/heads into the refs/bundle space. I'm not sure why this
refspec is hardcoded to be so limited, but it makes the ref negotiation on
the subsequent fetch suboptimal, since it won't use objects that are
referenced outside of the current heads of the bundled repository.

This change to copy everything in refs/ in the bundle to refs/bundles/
significantly helps the subsequent fetch, since nearly all the references
are now included in the negotiation.

The update to the bundle-uri unbundling refspec puts all the heads from a
bundle file into refs/bundle/heads instead of directly into refs/bundle/ so
the tests also need to be updated to look in the new heirarchy."#;
