use thiserror::Error;

#[derive(Debug, Error)]
pub enum HermesError {
    #[error("provider error: {0}")]
    Provider(#[from] ProviderError),

    #[error("tool error: {name}: {message}")]
    Tool { name: String, message: String },

    #[error("config error: {0}")]
    Config(String),

    #[error("memory error: {0}")]
    Memory(String),

    #[error("mcp error: {0}")]
    Mcp(String),

    #[error("approval denied")]
    ApprovalDenied,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("API error ({status}): {message}")]
    ApiError { status: u16, message: String },

    #[error("rate limited, retry after {retry_after:?}s")]
    RateLimited { retry_after: Option<f64> },

    #[error("authentication failed")]
    AuthError,

    #[error("model not found: {0}")]
    ModelNotFound(String),

    #[error("context length exceeded: {used}/{max} tokens")]
    ContextLengthExceeded { used: usize, max: usize },

    #[error("network error: {0}")]
    Network(String),

    #[error("timeout after {0}s")]
    Timeout(u64),

    #[error("SSE parse error: {0}")]
    SseParse(String),
}

pub type Result<T> = std::result::Result<T, HermesError>;
