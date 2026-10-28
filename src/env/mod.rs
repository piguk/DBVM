//! The RL environment service: templates, tasks and rollouts behind an HTTP API, for many tenants on a
//! node. See docs/env.md.
//!
//! - `control`: the control database (tenants, keys, templates, layers, tasks, sessions, usage)
//! - `store`: the node's data directory (layers, templates, run directories, trajectories)
//! - `task`: the JSON shapes of tasks, actions and their results
//! - `service`: everything the API does, with ownership and quota checks
//! - `api`: the HTTP routes

pub mod api;
pub mod control;
pub mod service;
pub mod store;
pub mod task;
