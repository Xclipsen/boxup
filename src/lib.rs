#![forbid(unsafe_op_in_unsafe_fn)]

pub mod backend;
pub mod borg;
pub mod config;
pub mod domain;
pub mod index;
pub mod jobs;
pub mod restore;
pub mod tui;

pub use backend::{Backend, DiffStream, FileStream};
pub use borg::BorgBackend;
pub use config::Config;
