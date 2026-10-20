//! `protobench check`: project lint for CI.

use std::path::Path;
use std::process::ExitCode;

use anyhow::Result;

use pb_core::project::Project;

pub fn check(dir: &Path, json: bool) -> Result<ExitCode> {
    let p = Project::open(dir);
    let found = pb_core::check::check_project(&p);
    let errors = found.iter().filter(|d| d.is_error()).count();
    let warnings = found.len() - errors;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "project": p.dir,
                "errors": errors,
                "warnings": warnings,
                "findings": found,
            }))?
        );
    } else {
        for d in &found {
            println!("{d}");
        }
        eprintln!(
            "{}: {errors} error{}, {warnings} warning{}",
            p.dir.display(),
            if errors == 1 { "" } else { "s" },
            if warnings == 1 { "" } else { "s" }
        );
    }
    Ok(if errors > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}
