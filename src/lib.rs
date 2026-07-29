//! Provider-neutral text generation through local AI agent CLIs.
//!
//! An [`Agent`] accepts a single [`GenerationRequest`] and returns one textual
//! [`Generation`]. Requests carry an explicit prompt plus ordered, labeled text
//! or JSON context. Adapters own transport-specific details without leaking
//! them into callers.

mod agent;
mod error;
mod request;

pub use agent::Agent;
pub use async_trait::async_trait;
pub use error::{BoxError, Error, Result};
pub use request::{
    ContextItem, ContextValue, Generation, GenerationOptions, GenerationRequest, ReasoningEffort,
    Usage,
};
