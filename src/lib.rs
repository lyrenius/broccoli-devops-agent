//! Core architecture scaffold for the Broccoli operations agent.
//!
//! This crate describes domain objects, component interfaces, minimal scheduling behavior, and
//! in-memory storage. Real networking, model access, and machine execution will be integrated only
//! after these boundaries are stable.

#![forbid(unsafe_code)]
#![deny(missing_docs)]

pub mod api;
pub mod collector;
pub mod config;
pub mod domain;
pub mod error;
pub mod evidence;
pub mod i18n;
pub mod platform;
pub mod policy;
pub mod ports;
pub mod runner;
pub mod scheduler;
pub mod session;
pub mod settings;
pub mod store;
pub mod team;
pub mod topology;
pub mod usage;
pub mod view;

pub use error::{AgentError, AgentResult};
