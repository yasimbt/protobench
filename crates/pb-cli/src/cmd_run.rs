//! `protobench run`: headless sequence execution with JSON and JUnit reports.
//!
//! Exit codes: 0 all steps passed, 1 any failure, 2 configuration or
//! connection error.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{anyhow, Result};

use pb_core::project::Project;
use pb_core::sequence::{run_sequence, RunEvent, RunOptions, SequenceFile, StepStatus};

use crate::NonInteractive;

pub fn run(
    dir: &Path,
    sequence: &str,
    profile: Option<&str>,
    port: Option<String>,
    report_dir: Option<PathBuf>,
    ni: Option<NonInteractive>,
) -> Result<ExitCode> {
    let p = Project::open(dir);
    for d in &p.diagnostics {
        eprintln!("{d}");
    }
    if p.has_errors() {
        return Err(anyhow!("project {} has errors", dir.display()));
    }
    let path = p.sequence_path(sequence);
    let seq = SequenceFile::load(&path)?;
    let session = crate::cmd::connect(&p, profile, port)?;
    eprintln!("running `{}` on {}", seq.name, session.endpoint());
    let opts = RunOptions {
        non_interactive: ni.map(|n| match n {
            NonInteractive::Fail => pb_core::sequence::NonInteractive::Fail,
            NonInteractive::Continue => pb_core::sequence::NonInteractive::Continue,
        }),
        ..Default::default()
    };
    let mut on_event = |ev: RunEvent| match ev {
        RunEvent::StepStarted { index, label } => eprintln!("  … {index} {label}"),
        RunEvent::StepFinished(r) => {
            let mark = match r.status {
                StepStatus::Pass => "PASS",
                StepStatus::Fail => "FAIL",
                StepStatus::Error => "ERR ",
                StepStatus::Skipped => "SKIP",
            };
            eprintln!(
                "  {mark} {} {} ({:.0} ms){}",
                r.index,
                r.label,
                r.duration_ms,
                r.message.map(|m| format!(" — {m}")).unwrap_or_default()
            );
            for a in r.assertions {
                eprintln!(
                    "       {} {} = {}",
                    if a.passed { "✓" } else { "✗" },
                    a.description,
                    a.measured
                );
            }
        }
        RunEvent::Log(l) => eprintln!("  log: {l}"),
        RunEvent::Finished(_) => {}
    };
    let report = run_sequence(&session, &p.config, &seq, opts, &mut on_event, None);
    let dir = report_dir.unwrap_or_else(|| p.dir.join("reports"));
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "sequence".into());
    let (json, xml) = report.write(&dir, &stem)?;
    let s = report.summary;
    println!(
        "{}: {} passed, {} failed, {} errors, {} skipped in {:.2} s",
        if report.passed { "PASSED" } else { "FAILED" },
        s.passed,
        s.failed,
        s.errors,
        s.skipped,
        report.duration_s
    );
    println!("report: {}", json.display());
    println!("junit:  {}", xml.display());
    // A connection error mid-run counts as a configuration/connection error.
    let conn_lost = report
        .steps
        .iter()
        .any(|st| st.status == StepStatus::Error && st.message.as_deref() == Some("not connected"));
    Ok(if report.passed {
        ExitCode::SUCCESS
    } else if conn_lost {
        ExitCode::from(2)
    } else {
        ExitCode::from(1)
    })
}
