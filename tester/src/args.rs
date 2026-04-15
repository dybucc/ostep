use clap::Parser;

/// Test harness for OSTEP projects (or anything that aligns with the OSTEP
/// testing practices.)
#[derive(Debug, Parser)]
#[command(
    version,
    about,
    long_about = None,
    disable_help_flag = true,
    disable_help_subcommand = true,
    infer_long_args = true
)]
pub(crate) struct Args {
    /// Specify the package to test on in a multi-package workspace.
    #[arg(short, long)]
    pub(crate) package: Option<String>,
}
