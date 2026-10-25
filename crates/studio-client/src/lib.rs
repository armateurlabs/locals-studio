//! Lightweight local IPC shared by the desktop shell and the command line.
//! Inference, model downloads and process supervision live in the separate service.
pub mod client;
pub mod paths;
pub mod types;
