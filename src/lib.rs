//! A faithful standalone relay for the GPT-Live / OpenAI Realtime voice protocol.
//!
//! The wire contract this implements is documented in `docs/`, with line-level
//! citations into the upstream architecture record. Where this relay deliberately
//! preserves a quirk of the original rather than "improving" it, the code says so.

pub mod admission;
pub mod app;
pub mod config;
pub mod error;
pub mod live;
pub mod observability;
pub mod realtime;
pub mod relay;
pub mod wire;

pub use admission::DrainState;
pub use config::{AccountId, BearerToken, Config, UpstreamCredentialMode, UpstreamProfile};
pub use error::{RelayError, RequestKind};
pub use observability::{redacted_headers, Direction, FrameLogger};
pub use wire::{SidebandJoinStyle, WireAdapter};
