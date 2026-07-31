pub mod server;
pub mod tools;

/// Protocol revision we implement. We echo back whatever the client asks for
/// when we recognise it, and fall back to this otherwise.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
pub const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
