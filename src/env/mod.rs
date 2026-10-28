//! The RL environment service: templates, tasks and rollouts behind an HTTP API, for many tenants on a
//! node. See docs/env.md.
//!
//! - `control`: the control database (tenants, keys, templates, layers, tasks, sessions, usage)
//! - `store`: the node's data directory (layers, templates, run directories, trajectories)
//! - `oci`: OCI images (image layouts, `docker save` archives) as templates
//! - `task`: the JSON shapes of tasks, actions and their results
//! - `service`: everything the API does, with ownership and quota checks
//! - `transfer`: templates and layers as byte streams, between nodes
//! - `api`: the HTTP routes
//! - `cluster`: the coordinator of several nodes
//! - `metrics`: Prometheus metrics
//! - `doctor`: host checks and support bundles

pub mod api;
pub mod cluster;
pub mod control;
pub mod doctor;
pub mod metrics;
pub mod oci;
pub mod service;
pub mod store;
pub mod task;
pub mod transfer;
