//! ProtoBench desktop GUI.

mod app;
mod layout;
mod panels;
mod smoke;
mod state;
mod tools;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// ProtoBench desktop GUI.
#[derive(Parser, Debug)]
#[command(name = "protobench-gui", version, about)]
struct Args {
    /// Project folder to open.
    project: Option<PathBuf>,
    /// Connect with this profile after opening.
    #[arg(long)]
    profile: Option<String>,
    /// Load the demo project (or PROJECT), connect to the simulator, drive the
    /// command buttons through the UI actions, render frames and check that
    /// channels receive data. Exits 0 on success.
    #[arg(long)]
    smoke_test: bool,
    /// With --smoke-test: also open a real window for a few seconds.
    #[arg(long)]
    window: bool,
    /// Write the default layout.json for PROJECT and exit.
    #[arg(long)]
    write_default_layout: bool,
    /// Log filter.
    #[arg(long, default_value = "warn")]
    log: String,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let filter = tracing_subscriber::EnvFilter::try_new(&args.log)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    if args.write_default_layout {
        let Some(dir) = args.project else {
            eprintln!("--write-default-layout needs a project folder");
            return ExitCode::from(2);
        };
        let p = pb_core::project::Project::open(&dir);
        let dock = layout::default_layout(Some(&p));
        return match layout::save_layout(&p.layout_path(), &dock) {
            Ok(()) => {
                println!("wrote {}", p.layout_path().display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("{e}");
                ExitCode::from(2)
            }
        };
    }

    if args.smoke_test {
        let dir = args.project.unwrap_or_else(smoke::demo_dir);
        return match smoke::run(&dir, args.window) {
            Ok(summary) => {
                println!("{summary}");
                println!("smoke test passed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("smoke test FAILED: {e}");
                ExitCode::from(1)
            }
        };
    }

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ProtoBench")
            .with_inner_size([1400.0, 900.0]),
        ..Default::default()
    };
    let project = args.project;
    let profile = args.profile;
    match eframe::run_native(
        "ProtoBench",
        options,
        Box::new(move |cc| {
            Ok(Box::new(app::ProtoBenchApp::new(
                Some(cc),
                project,
                profile,
            )))
        }),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("cannot start the GUI: {e}");
            ExitCode::from(2)
        }
    }
}
