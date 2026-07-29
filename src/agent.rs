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
