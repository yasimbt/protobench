//! `protobench` command-line interface.

mod cmd;
mod cmd_check;
mod cmd_decode;
mod cmd_run;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand, ValueEnum};

/// ProtoBench: develop and test Protocol Buffers devices from the command line.
#[derive(Parser, Debug)]
#[command(name = "protobench", version, about)]
struct Cli {
    /// Log level filter (e.g. `info`, `debug`, `pb_core=trace`).
    #[arg(long, global = true, default_value = "warn")]
    log: String,
    #[command(subcommand)]
    command: Command,
}

/// What to do with `prompt_operator` steps when nobody is there to answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NonInteractive {
    /// Fail the step.
    Fail,
    /// Log the prompt and continue.
    Continue,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List serial ports with USB IDs.
    ListPorts {
        /// Print JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the messages of a project's schema.
    Schema {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// Show only this message (full or simple name).
        #[arg(long)]
        message: Option<String>,
    },
    /// Send one command and optionally wait for the reply.
    Send {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// Connection profile (default: the project's default).
        #[arg(long)]
        profile: Option<String>,
        /// Override the serial port, or host:port for network profiles.
        #[arg(long)]
        port: Option<String>,
        /// Command name from project.toml.
        #[arg(long)]
        command: String,
        /// Override a payload field: `--set millivolts=300`.
        #[arg(long = "set", value_name = "FIELD=VALUE")]
        set: Vec<String>,
        /// Wait for the expected reply and print it.
        #[arg(long)]
        wait: bool,
    },
    /// Print decoded messages and channels as they arrive.
    Monitor {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// Connection profile.
        #[arg(long)]
        profile: Option<String>,
        /// Override the serial port or host:port.
        #[arg(long)]
        port: Option<String>,
        /// Print one JSON object per line.
        #[arg(long)]
        json: bool,
        /// Print these channels instead of messages (comma-separated).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// Commands to send after connecting (e.g. `--send "Start Stream"`).
        #[arg(long = "send")]
        send: Vec<String>,
        /// Stop after this many seconds.
        #[arg(long)]
        duration: Option<f64>,
        /// Record the session to this `.pbsession` file.
        #[arg(long)]
        record: Option<PathBuf>,
        /// Serve live data to Foxglove (WebSocket protocol) on this port.
        #[arg(long, value_name = "PORT")]
        foxglove: Option<u16>,
        /// Address the Foxglove server binds (use 0.0.0.0 for other machines).
        #[arg(long, default_value = "127.0.0.1")]
        foxglove_bind: String,
    },
    /// Run a test sequence and write JSON and JUnit reports.
    Run {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// Sequence name (file stem in sequences/) or path.
        #[arg(long)]
        sequence: String,
        /// Connection profile.
        #[arg(long)]
        profile: Option<String>,
        /// Override the serial port or host:port.
        #[arg(long)]
        port: Option<String>,
        /// Report folder (default: <project>/reports).
        #[arg(long)]
        report_dir: Option<PathBuf>,
        /// How to handle prompt_operator steps (default: the sequence's
        /// `non_interactive` setting, else `fail`).
        #[arg(long, value_enum)]
        non_interactive: Option<NonInteractive>,
    },
    /// Decode a recorded session file.
    Decode {
        /// The `.pbsession` file.
        file: PathBuf,
        /// Print one JSON object per record.
        #[arg(long)]
        json: bool,
        /// Export channels to CSV (uses the project config embedded in the file).
        #[arg(long)]
        csv: Option<PathBuf>,
        /// Export to MCAP.
        #[arg(long)]
        mcap: Option<PathBuf>,
        /// Export only these channels (comma-separated; default: all).
        #[arg(long, value_delimiter = ',')]
        channels: Vec<String>,
        /// Export only this time range in seconds, `t0:t1` (either side may
        /// be empty).
        #[arg(long, value_name = "T0:T1")]
        range: Option<String>,
        /// Write one CSV per channel instead of one joined file.
        #[arg(long)]
        per_channel: bool,
    },
    /// Check a project for errors: project.toml, schema, sequences and
    /// layout. Exit code 0 = no errors, 1 = errors.
    Check {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// Print findings as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Run the device simulator as a TCP server.
    Sim {
        /// Project folder.
        #[arg(long)]
        project: PathBuf,
        /// TCP port to listen on.
        #[arg(long, default_value_t = 9000)]
        tcp: u16,
        /// Address to bind.
        #[arg(long, default_value = "0.0.0.0")]
        bind: String,
    },
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let filter = tracing_subscriber::EnvFilter::try_new(&cli.log)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
    let r = match cli.command {
        Command::ListPorts { json } => cmd::list_ports(json),
        Command::Schema { project, message } => cmd::schema(&project, message.as_deref()),
        Command::Send {
            project,
            profile,
            port,
            command,
            set,
            wait,
        } => cmd::send(&project, profile.as_deref(), port, &command, &set, wait),
        Command::Monitor {
            project,
            profile,
            port,
            json,
            channels,
            send,
            duration,
            record,
            foxglove,
            foxglove_bind,
        } => cmd::monitor(cmd::MonitorArgs {
            project,
            profile,
            port,
            json,
            channels,
            send,
            duration,
            record,
            foxglove,
            foxglove_bind,
        }),
        Command::Run {
            project,
            sequence,
            profile,
            port,
            report_dir,
            non_interactive,
        } => cmd::run(
            &project,
            &sequence,
            profile.as_deref(),
            port,
            report_dir,
            non_interactive,
        ),
        Command::Decode {
            file,
            json,
            csv,
            mcap,
            channels,
            range,
            per_channel,
        } => cmd_decode::decode(cmd_decode::DecodeArgs {
            file,
            json,
            csv,
            mcap,
            channels,
            range,
            per_channel,
        }),
        Command::Check { project, json } => cmd_check::check(&project, json),
        Command::Sim { project, tcp, bind } => cmd::sim(&project, &bind, tcp),
    };
    match r {
        Ok(code) => code,
        Err(e) => {
            // A closed stdout (e.g. `| head`) is not an error.
            let broken_pipe = e.chain().any(|c| {
                c.downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::BrokenPipe)
            });
            if broken_pipe {
                return ExitCode::SUCCESS;
            }
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}
