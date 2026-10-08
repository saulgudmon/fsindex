//! fsindex-core: the reusable in-process filename and metadata engine.
//!
//! The engine keeps a live, in-memory index of filenames plus metadata
//! (size, mtime, inode, kind) across one or more configured roots, grouped by
//! the underlying volume/device. It is updated by an inotify watcher with
//! debounced directory resyncs and periodic reconciliation.

pub mod catalog;
pub mod config;
pub mod engine;
pub mod index;
pub mod model;
pub mod paths;
pub mod query;
pub mod scan;
pub mod volume;
pub mod watch;

pub use config::Config;
pub use engine::Engine;
pub use model::*;
