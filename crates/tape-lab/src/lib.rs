//! The research desk: AI agents propose trading ideas, write them as
//! sandboxed strategies, backtest them on training data and critique the
//! results. Only when every attempt is finished does anything touch the
//! held-out test period, once.
//!
//! - [`model`] talks to the language model and enforces the spending cap.
//! - [`build`] checks and compiles model-written strategies.
//! - [`prompts`] is what each agent is told.
//! - [`lab`] runs the experiment and writes the report.

pub mod build;
pub mod lab;
pub mod model;
pub mod prompts;

pub use build::GuestBuilder;
pub use lab::{LabConfig, Market, Period, Report, run};
pub use model::{Budget, Model, OpenAi, Scripted};
