use agent_text::{
    Agent, ClaudeCode, ContextItem, GenerationOptions, GenerationRequest, ReasoningEffort,
};

#[tokio::main]
async fn main() -> Result<(), agent_text::Error> {
    let request = GenerationRequest::new("Write a one-sentence release note.")
        .with_context(ContextItem::text(
            "change",
            "Added exponential backoff with jitter to reconnect attempts.",
        ))
        .with_options(GenerationOptions {
            model: Some("haiku".to_string()),
            reasoning_effort: Some(ReasoningEffort::Low),
            timeout: None,
        });

    println!("{}", ClaudeCode::new().generate_text(&request).await?);
    Ok(())
}
