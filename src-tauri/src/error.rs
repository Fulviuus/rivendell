#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("invalid request: {0}")]
    Invalid(String),
    #[error("limit reached: {0}")]
    Limit(String),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    /// JSON-RPC error code for the MCP surface.
    pub fn rpc_code(&self) -> i64 {
        match self {
            Error::NotFound(_) => -32004,
            Error::Forbidden(_) => -32003,
            Error::Invalid(_) => -32602,
            Error::Limit(_) => -32005,
            _ => -32603,
        }
    }
}

impl serde::Serialize for Error {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}
