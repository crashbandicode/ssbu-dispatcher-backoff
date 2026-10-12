//! ssbu-dispatcher-backoff: a Skyline plugin for Super Smash Bros. Ultimate
//! 13.0.5 that stops the game's task dispatcher from busy-polling six empty
//! queues while it waits for an unfinished dependency.
//!
//! `policy` is pure and host-tested. `runtime` is the Switch-only hook.

pub mod policy;

#[cfg(target_os = "switch")]
mod runtime;
