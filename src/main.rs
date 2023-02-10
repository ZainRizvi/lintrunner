use std::{collections::HashSet, convert::TryFrom, io::Write};

use anyhow::{Context, Result};
use chrono::SecondsFormat;
use clap::Parser;

use lintrunner::{
    do_init, do_lint,
    git::get_head,
    init::check_init_changed,
    lint_config::{get_linters_from_config, LintRunnerConfig},
    log_utils::setup_logger,
    path::AbsPath,
    persistent_data::{ExitInfo, PersistentDataStore, RunInfo},
    rage::do_rage,
    render::print_error,
    PathsOpt, RenderOpt, RevisionOpt,
};
use log::debug;

const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Parser)]
#[clap(version, name = "lintrunner", infer_subcommands(true))]
struct Args {
    /// Verbose mode (-v, or -vv to show full list of paths being linted)
    #[clap(short, long, parse(from_occurrences), global = true)]
    verbose: u8,

    /// Path to a toml file defining which linters to run
    #[clap(long, default_value = ".lintrunner.toml", global = true)]
    config: String,

    /// If set, any suggested patches will be applied
    #[clap(short, long, global = true)]
    apply_patches: bool,

    /// Shell command that returns new-line separated paths to lint
    ///
    /// Example: To run on all files in the repo, use `--paths-cmd='git grep -Il .'`.
    #[clap(long, conflicts_with = "paths-from", global = true)]
    paths_cmd: Option<String>,

    /// File with new-line separated paths to lint
    #[clap(long, global = true)]
    paths_from: Option<String>,

    /// Lint all files that differ between the working directory and the
    /// specified revision. This argument can be any <tree-ish> that is accepted
    /// by `git diff-tree`
    #[clap(long, short, conflicts_with_all=&["paths", "paths-cmd", "paths-from"], global = true)]
    revision: Option<String>,

    /// Lint all files that differ between the merge base of HEAD with the
    /// specified revision and HEAD. This argument can be any <tree-sh> that is
    /// accepted by `git diff-tree`
    ///
    /// Example: lintrunner -m master
    #[clap(long, short, conflicts_with_all=&["paths", "paths-cmd", "paths-from", "revision"], global = true)]
    merge_base_with: Option<String>,

    /// Comma-separated list of linters to skip (e.g. --skip CLANGFORMAT,NOQA)
    #[clap(long, global = true)]
    skip: Option<String>,

    /// Comma-separated list of linters to run (opposite of --skip)
    #[clap(long, global = true)]
    take: Option<String>,

    /// With 'default' show lint issues in human-readable format, for interactive use.
    /// With 'json', show lint issues as machine-readable JSON (one per line)
    /// With 'oneline', show lint issues in compact format (one per line)
    #[clap(long, arg_enum, default_value_t = RenderOpt::Default, global=true)]
    output: RenderOpt,

    #[clap(subcommand)]
    cmd: Option<SubCommand>,

    /// Paths to lint. lintrunner will still respect the inclusions and
    /// exclusions defined in .lintrunner.toml; manually specifying a path will
    /// not override them.
    #[clap(conflicts_with_all = &["paths-cmd", "paths-from"], global = true)]
    paths: Vec<String>,

    /// If set, always output with ANSI colors, even if we detect the output is
    /// not a user-attended terminal.
    #[clap(long, global = true)]
    force_color: bool,

    /// If set, use ths provided path to store any metadata generated by
    /// lintrunner. By default, this is a platform-specific location for
    /// application data (e.g. $XDG_DATA_HOME for UNIX systems.)
    #[clap(long, global = true)]
    data_path: Option<String>,

    /// If set, output json to the provided path as well as the terminal.
    #[clap(long, global = true)]
    tee_json: Option<String>,

    /// Run lintrunner on all files in the repo. This could take a while!
    #[clap(long, conflicts_with_all=&["paths", "paths-cmd", "paths-from", "revision", "merge-base-with"], global = true)]
    all_files: bool,
}

#[derive(Debug, Parser)]
enum SubCommand {
    /// Perform first-time setup for linters
    Init {
        /// If set, do not actually execute initialization commands, just print them
        #[clap(long, short)]
        dry_run: bool,
    },
    /// Run and accept changes for formatting linters only. Equivalent to
    /// `lintrunner --apply-patches --take <formatters>`.
    Format,

    /// Run linters. This is the default if no subcommand is provided.
    Lint,

    /// Create a bug report for a past invocation of lintrunner.
    Rage {
        /// Choose a specific invocation to report on. 0 is the most recent run.
        #[clap(long, short)]
        invocation: Option<usize>,
    },
}

fn do_main() -> Result<i32> {
    let args = Args::parse();

    let config_path = AbsPath::try_from(&args.config)
        .with_context(|| format!("Could not read lintrunner config at: '{}'", args.config))?;

    if args.force_color {
        console::set_colors_enabled(true);
        console::set_colors_enabled_stderr(true);
    }
    let log_level = match (args.verbose, args.output != RenderOpt::Default) {
        // Default
        (0, false) => log::LevelFilter::Info,
        // If just json is asked for, suppress most output except hard errors.
        (0, true) => log::LevelFilter::Error,

        // Verbose overrides json.
        (1, false) => log::LevelFilter::Debug,
        (1, true) => log::LevelFilter::Debug,

        // Any higher verbosity goes to trace.
        (_, _) => log::LevelFilter::Trace,
    };

    let run_info = RunInfo {
        args: std::env::args().collect(),
        timestamp: chrono::Local::now().to_rfc3339_opts(SecondsFormat::Millis, true),
    };
    let persistent_data_store = PersistentDataStore::new(&config_path, run_info)?;

    setup_logger(
        log_level,
        &persistent_data_store.log_file(),
        args.force_color,
    )?;

    debug!("Version: {VERSION}");
    debug!("Passed args: {:?}", std::env::args());
    debug!("Computed args: {:?}", args);
    debug!("Current rev: {}", get_head()?);

    let cmd = args.cmd.unwrap_or(SubCommand::Lint);
    let lint_runner_config = LintRunnerConfig::new(&config_path)?;

    let skipped_linters = args.skip.map(|linters| {
        linters
            .split(',')
            .map(|linter_name| linter_name.to_string())
            .collect::<HashSet<_>>()
    });
    let taken_linters = args.take.map(|linters| {
        linters
            .split(',')
            .map(|linter_name| linter_name.to_string())
            .collect::<HashSet<_>>()
    });

    // If we are formatting, the universe of linters to select from should be
    // restricted to only formatters.
    // (NOTE: we pay an allocation for `placeholder` even in cases where we are
    // just passing through a reference in the else-branch. This doesn't matter,
    // but if we want to fix it we should impl Cow for LintConfig and use that
    // instead.).
    let mut placeholder = Vec::new();
    let all_linters = if let SubCommand::Format = &cmd {
        let iter = lint_runner_config
            .linters
            .iter()
            .filter(|l| l.is_formatter)
            .cloned();
        placeholder.extend(iter);
        &placeholder
    } else {
        // If we're not formatting, all linters defined in the config are
        // eligible to run.
        &lint_runner_config.linters
    };

    let linters =
        get_linters_from_config(all_linters, skipped_linters, taken_linters, &config_path)?;

    let enable_spinners = args.verbose == 0 && args.output == RenderOpt::Default;

    let revision_opt = if let Some(revision) = args.revision {
        RevisionOpt::Revision(revision)
    } else if let Some(merge_base_with) = args.merge_base_with {
        RevisionOpt::MergeBaseWith(merge_base_with)
    } else if !lint_runner_config.merge_base_with.is_empty() {
        RevisionOpt::MergeBaseWith(lint_runner_config.merge_base_with.clone())
    } else {
        RevisionOpt::Head
    };

    let paths_opt = if let Some(paths_file) = args.paths_from {
        let path_file = AbsPath::try_from(&paths_file)
            .with_context(|| format!("Failed to find `--paths-from` file '{}'", paths_file))?;
        PathsOpt::PathsFile(path_file)
    } else if let Some(paths_cmd) = args.paths_cmd {
        PathsOpt::PathsCmd(paths_cmd)
    } else if !args.paths.is_empty() {
        PathsOpt::Paths(args.paths)
    } else if args.all_files {
        PathsOpt::AllFiles
    } else {
        PathsOpt::Auto
    };

    let res = match cmd {
        SubCommand::Init { dry_run } => {
            // Just run initialization commands, don't actually lint.
            do_init(linters, dry_run, &persistent_data_store, &config_path)
        }
        SubCommand::Format => {
            check_init_changed(&persistent_data_store, &lint_runner_config)?;
            do_lint(
                linters,
                paths_opt,
                true, // always apply patches when we use the format command
                args.output,
                enable_spinners,
                revision_opt,
                args.tee_json,
            )
        }
        SubCommand::Lint => {
            // Default command is to just lint.
            check_init_changed(&persistent_data_store, &lint_runner_config)?;
            do_lint(
                linters,
                paths_opt,
                args.apply_patches,
                args.output,
                enable_spinners,
                revision_opt,
                args.tee_json,
            )
        }
        SubCommand::Rage { invocation } => do_rage(&persistent_data_store, invocation),
    };

    let exit_info = match &res {
        Ok(code) => ExitInfo {
            code: *code,
            err: None,
        },
        Err(err) => ExitInfo {
            code: 1,
            err: Some(err.to_string()),
        },
    };

    // Write data related to this run out to the persistent data store.
    persistent_data_store.write_run_info(exit_info)?;

    res
}

fn main() {
    let code = match do_main() {
        Ok(code) => code,
        Err(err) => {
            print_error(&err)
                .context("failed to print exit error")
                .unwrap();
            1
        }
    };

    // Flush the output before exiting, in case there is anything left in the buffers.
    drop(std::io::stdout().flush());
    drop(std::io::stderr().flush());

    // exit() abruptly ends the process while running no destructors. We should
    // make sure that nothing is alive before running this.
    std::process::exit(code);
}
