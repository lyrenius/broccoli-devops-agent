//! Core architecture scaffold for the Broccoli operations agent.
//!
//! This crate describes domain objects, component interfaces, minimal scheduling behavior, and
//! in-memory storage. Real networking, model access, and machine execution will be integrated only
//! after these boundaries are stable.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod collector;
pub mod domain;
pub mod error;
pub mod ports;
pub mod runner;
pub mod scheduler;
pub mod store;
pub mod team;
pub mod topology;
pub mod view;

pub use error::{AgentError, AgentResult};
