use crate::{Generation, GenerationRequest, Result};

/// Generates a single textual result from an instruction and supplied context.
///
/// Implementations must not silently ignore an explicit request option. If an
/// adapter cannot honor one, it returns [`Error::UnsupportedOption`].
///
/// [`Error::UnsupportedOption`]: crate::Error::UnsupportedOption
#[crate::async_trait]
pub trait Agent: Send + Sync {
    /// Generate text and retain any execution metadata the adapter reports.
    async fn generate(&self, request: &GenerationRequest) -> Result<Generation>;

    /// Generate text and discard execution metadata.
    async fn generate_text(&self, request: &GenerationRequest) -> Result<String> {
        Ok(self.generate(request).await?.text)
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    struct Stub;

    #[crate::async_trait]
    impl Agent for Stub {
        async fn generate(&self, _: &GenerationRequest) -> Result<Generation> {
            Ok(Generation {
                text: "answer".to_string(),
                usage: None,
                model: Some("stub".to_string()),
                elapsed: Duration::from_millis(1),
            })
        }
    }

    #[tokio::test]
    async fn generate_text_discards_metadata() {
        assert_eq!(
            Stub.generate_text(&GenerationRequest::new("task"))
                .await
                .unwrap(),
            "answer"
        );
    }
}
