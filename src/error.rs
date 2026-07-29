use std::path::PathBuf;
use std::time::Duration;

/// A transport-specific error that remains safe to move between async tasks.
pub type BoxError = Box<dyn std::error::Error + Send + Sync + 'static>;

/// The result type returned by agent operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Failures shared by agent adapters.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A required field is empty or otherwise invalid.
    #[error("invalid `{field}`: {message}")]
    InvalidRequest {
        field: &'static str,
        message: String,
    },

    /// The adapter cannot preserve an explicitly requested option.
    #[error("{adapter} does not support `{option}` value `{value}`")]
    UnsupportedOption {
        adapter: &'static str,
        option: &'static str,
        value: String,
    },

    /// The configured agent executable could not be started.
    #[error("failed to spawn `{binary}`: {source}")]
    Spawn {
        binary: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// I/O with a running agent process failed.
    #[error("failed to {operation}: {source}")]
    Io {
        operation: &'static str,
        #[source]
        source: std::io::Error,
    },

    /// The configured deadline elapsed.
    #[error("agent generation timed out after {duration:?}")]
    Timeout { duration: Duration },

    /// An output stream exceeded its configured capture ceiling.
    #[error("agent {stream} exceeded the {limit}-byte capture limit")]
    OutputTooLarge { stream: &'static str, limit: usize },

    /// The process exited unsuccessfully.
    #[error("agent exited with status {status:?}: {stderr}")]
    Exit { status: Option<i32>, stderr: String },

    /// The agent returned a successful process status but reported a failure.
    #[error("agent reported an error: {message}")]
    AgentReported { message: String },

    /// The adapter could not decode the agent's response envelope.
    #[error("invalid agent response: {message}")]
    InvalidResponse { message: String },

    /// The response contained no usable text.
    #[error("agent returned empty text")]
    EmptyOutput,

    /// An extension adapter failed in a way not covered by the common variants.
    #[error("{adapter} failed: {source}")]
    Other {
        adapter: String,
        #[source]
        source: BoxError,
    },
}
