//! Package registry: HTTP server and synchronous client.

pub mod client;
pub mod server;
pub mod trust;

pub use client::RegistryClient;
pub use server::RegistryServer;
