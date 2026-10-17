//! Adit maintained fork of vnc-rs 0.6.0. See UPSTREAM.md for provenance.
pub mod client;
mod codec;
pub mod config;
mod desktop;
pub mod error;
pub mod event;
pub use desktop::*;

pub use client::VncClient;
pub use client::VncConnector;
pub use config::*;
pub use error::*;
pub use event::*;

mod limits;

mod security;
pub use security::{SecurityInfo, SecurityPolicy, TlsConfig};

mod flow;

mod clipboard;

pub use client::connection::ClipboardPolicy;

pub use clipboard::ClipboardText;

mod tight;
