//! Test sequences: model, runner, assertions and reports.

mod assert;
mod input;
mod model;
mod report;
mod runner;
pub mod script;

pub use assert::{
    check, check_each, check_stat, measure_all, measure_field, AssertionResult, Measured,
};
pub use input::{InputType, OperatorInput};
pub use model::{Assertion, NonInteractive, SequenceFile, Step};
pub use report::{esc as xml_escape, RunReport, StepReport, StepStatus, Summary};
pub use runner::{
    run_sequence, run_sequence_on, OperatorPrompt, PromptAnswer, PromptFn, RunEvent, RunOptions,
    Target,
};
