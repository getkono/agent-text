# agent-text

Generate one string from an instruction and arbitrary context through a local AI
agent CLI.

`agent-text` supplies an async, provider-neutral [`Agent`] contract and a
Claude Code adapter. It is intentionally narrower than an agent runtime: one
request goes in, one final string comes out, and tools and sessions stay off.

```rust,no_run
use agent_text::{
    Agent, ClaudeCode, ContextItem, GenerationOptions, GenerationRequest,
    ReasoningEffort,
};

#[tokio::main]
async fn main() -> Result<(), agent_text::Error> {
    let request = GenerationRequest::new(
        "Write a concise release note from the supplied change.",
    )
    .with_system_prompt("Use an imperative sentence with no heading.")
    .with_context(ContextItem::text(
        "change",
        "Added retry jitter to reduce synchronized reconnects.",
    ))
    .with_options(GenerationOptions {
        model: Some("haiku".to_string()),
        reasoning_effort: Some(ReasoningEffort::Low),
        timeout: None,
    });

    let text = ClaudeCode::new().generate_text(&request).await?;
    println!("{text}");
    Ok(())
}
```

## Request model

A [`GenerationRequest`] separates:

- an optional system prompt containing behavior and output rules;
- the required task prompt;
- ordered [`ContextItem`] values containing labeled text or JSON;
- portable model, reasoning-effort, and timeout overrides.

The result is trimmed but otherwise unchanged. Code fences, prose preambles,
and domain-specific cleanup remain the caller's responsibility. Explicit
options are never silently ignored: an adapter returns
`Error::UnsupportedOption` if it cannot honor one.

Request and result types are Rust APIs, not a stable serialized wire format.

## Claude Code

The default `claude-code` feature exports [`ClaudeCode`]. It requires the
`claude` executable on `PATH` (tested with Claude Code 2.1.220) and delegates
authentication and provider routing to that installed CLI.

Each generation:

- uses Claude's non-interactive JSON output;
- disables tools, slash commands, and session persistence;
- enables safe mode and runs in a fresh empty working directory;
- applies a five-minute timeout and an 8 MiB stdout capture limit by default;
- reports token/cache usage, cost, model, and elapsed time when available.

Configure a non-default executable, model, effort, timeout, environment
variable, or output ceiling through `ClaudeCode`'s `with_*` methods.

To use only the provider-neutral contract for your own adapter:

```toml
[dependencies]
agent-text = { version = "0.1", default-features = false }
```

Implement [`Agent`] and return the shared non-exhaustive [`Error`] categories.
The trait is object-safe, so applications can choose an adapter at runtime.

## Non-goals

Version 0.1 does not provide tools, workspace inspection, session resume,
streaming, multimodal context, automatic retries, or direct API-key handling.

## License

Licensed under either of:

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT License ([LICENSE-MIT](LICENSE-MIT))

at your option.

[`Agent`]: https://docs.rs/agent-text/latest/agent_text/trait.Agent.html
[`ClaudeCode`]: https://docs.rs/agent-text/latest/agent_text/struct.ClaudeCode.html
[`ContextItem`]: https://docs.rs/agent-text/latest/agent_text/struct.ContextItem.html
[`Error`]: https://docs.rs/agent-text/latest/agent_text/enum.Error.html
[`GenerationRequest`]: https://docs.rs/agent-text/latest/agent_text/struct.GenerationRequest.html

