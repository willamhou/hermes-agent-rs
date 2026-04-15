use tokio::sync::oneshot;

/// Request from the clarify tool to the UI handler.
pub struct ClarifyRequest {
    pub question: String,
    pub choices: Vec<String>,
    pub response_tx: oneshot::Sender<ClarifyResponse>,
}

/// Response from the UI handler back to the clarify tool.
#[derive(Debug, Clone)]
pub enum ClarifyResponse {
    Answer(String),
    Timeout,
}
