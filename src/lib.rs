//! Easy Anti-Cheat module tracker.
//!
//! Exposed as a library so the fetch/parse/render layers can be tested
//! independently of the Discord gateway.

pub mod bot;
pub mod config;
pub mod eac;
pub mod embed;
pub mod state;
pub mod tracker;
