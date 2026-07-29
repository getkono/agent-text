use agent_text::{
    Agent, Codex, ContextItem, GenerationOptions, GenerationRequest, ReasoningEffort,
};

#[tokio::main]
async fn main() -> Result<(), agent_text::Error> {
    let request = GenerationRequest::new("Write a one-sentence release note.")
        .with_context(ContextItem::text(
            "change",
            "Added exponential backoff with jitter to reconnect attempts.",
        ))
        .with_options(GenerationOptions {
            model: None,
            reasoning_effort: Some(ReasoningEffort::Low),
            timeout: None,
        });

    println!("{}", Codex::new().generate_text(&request).await?);
    Ok(())
}
