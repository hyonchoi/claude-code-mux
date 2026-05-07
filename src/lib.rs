/// Claude Code Mux Library
///
/// This library provides the core functionality for routing requests to Claude providers.
/// It exposes modules for OAuth authentication, provider management, and request routing.

pub mod auth;
pub mod cli;
pub mod models;
pub mod pid;
pub mod providers;
pub mod router;
pub mod server;

// Re-export commonly used types
pub use models::Message;
pub use router::Router;

