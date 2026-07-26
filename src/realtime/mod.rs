//! Public OpenAI Realtime protocol policy.
//!
//! This facade owns route classification and header policy. Transport handlers
//! are added by later work phases; private wire/session adapters stay in `live`.

pub mod contract;
pub mod headers;
pub mod http;
pub mod path;
