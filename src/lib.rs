#![doc = include_str!("../README.md")]

#[cfg(not(tokio_unstable))]
compile_error!(
    "cargo-loom must be installed with: \
    `RUSTFLAGS=\"--cfg tokio_unstable\" cargo install cargo-loom`"
);

use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use color_eyre::{
    eyre::{eyre, WrapErr},
    Help, Result,
};
use escargot::{format::test, CargoTest, CommandMessages};
use owo_colors::{colors, OwoColorize};
use std::{
    collections::{HashMap, HashSet},
    ffi::OsStr,
    fmt, fs,
    process::{Command, Output, Stdio},
    sync::Arc,
    time::Instant,
};
use tokio::task::JoinSet;

mod trace;

/// The `cargo-loom` command line application.
///
/// This type contains everything necessary to run a set of `loom` tests and
/// display their output.
#[derive(Debug)]
pub struct App {
    args: Args,
    checkpoint_dir: Utf8PathBuf,
    metadata: cargo_metadata::Metadata,
    target_dir: Utf8PathBuf,
    features: String,
    rustflags: String,
    max_branches: String,
    max_permutations: Option<String>,
    max_duration: Option<String>,
    max_threads: String,
    checkpoint_interval: String,
    loom_log: Arc<str>,
    test_args: Arc<Vec<String>>,
}

#[derive(Default)]
struct Failed {
    failed: HashMap<Arc<str>, Vec<FailedTest>>,
    test_cmds: HashMap<Arc<str>, CargoTest>,
    checkpoint_dirs: HashSet<Utf8PathBuf>,
    curr_suite_name: Option<Arc<str>>,
}

#[derive(Debug)]
struct TestOutput {
    name: String,
    output: Output,
}

#[derive(Debug)]

struct FailedTest {
    name: String,
    checkpoint: Utf8PathBuf,
}

/// A cargo subcommand for automating Loom testing workflows.
///
/// This utility will compile Loom tests, run them once to collect a list of
/// those tests which fail, generate checkpoint files for all failing tests, and
/// then finally rerun the failing tests from the generated checkpoint with
/// logging and location capture enabled.
///
/// By initially running the suite without without logging, location
/// capture, or checkpointing enabled, `cargo-loom` can quickly identify those
/// tests that fail, run them until a failing iteration is found, and then
/// re-run only the failing iterations with diagnostics enabled. This makes
/// running a large Loom suite much more efficient.
#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
    #[clap(flatten)]
    loom: LoomOptions,

    #[clap(flatten)]
    cargo: CargoOptions,

    #[clap(flatten)]
    trace_settings: trace::TraceSettings,

    /// If specified, only run tests containing this string in their names
    testname: Option<String>,

    /// Arguments passed to the test binary.
    #[clap(raw = true)]
    test_args: Vec<String>,
}

/// Options that configure the underlying `cargo test` invocation.
#[derive(Debug, clap::Args)]
#[clap(
    next_help_heading = "CARGO OPTIONS",
    group = clap::ArgGroup::new("cargo-opts")
)]
struct CargoOptions {
    /// Path to Cargo.toml
    #[clap(long, env = "CARGO_MANIFEST_PATH", value_hint = clap::ValueHint::FilePath)]
    manifest_path: Option<std::path::PathBuf>,

    #[clap(flatten)]
    workspace: clap_cargo::Workspace,

    #[clap(flatten)]
    features: clap_cargo::Features,

    /// Test only this package's library unit tests
    #[clap(long)]
    lib: bool,

    /// Test all tests
    #[clap(long)]
    tests: bool,

    /// Test all examples
    #[clap(long)]
    examples: bool,

    /// Test all binaries
    #[clap(long)]
    bins: bool,
}

/// Options that configure Loom's behavior.
#[derive(Debug, clap::Args)]
#[clap(
    next_help_heading = "LOOM OPTIONS",
    group = clap::ArgGroup::new("loom-opts")
)]
struct LoomOptions {
    /// Maximum number of thread switches per permutation.
    ///
    /// This sets the value of the `LOOM_MAX_BRANCHES` environment variable for
    /// the test executable.
    #[clap(long, env = ENV_MAX_BRANCHES, default_value_t = 1_000)]
    max_branches: usize,

    /// Maximum number of permutations to explore
    ///
    /// If no value is provided, the number of permutations will not be bounded.
    ///
    /// This sets the value of the `LOOM_MAX_PERMUTATIONS` environment variable
    /// for the test executable.
    #[clap(long, env = ENV_MAX_PERMUTATIONS)]
    max_permutations: Option<usize>,

    /// Max number of threads to check as part of the execution.
    ///
    /// This should be set as low as possible and must be less than 4.
    ///
    /// This sets the value of the `LOOM_MAX_THREADS` environment variable for
    /// the test execution.
    #[clap(long, env = ENV_MAX_THREADS, default_value_t = 4)]
    max_threads: usize,

    /// How often to write the checkpoint file
    ///
    /// This sets the value of the `LOOM_CHECKPOINT_INTERVAL` environment
    /// variable for the test executable.
    #[clap(long, env = ENV_CHECKPOINT_INTERVAL, default_value_t = 5)]
    checkpoint_interval: usize,

    /// Maximum duration to run each loom model for, in seconds
    ///
    /// If a value is not provided, no duration limit will be set.
    ///
    /// This sets the value of the `LOOM_MAX_DURATION` environment variable for
    /// the test executable.
    #[clap(long, env = ENV_MAX_DURATION)]
    max_duration_secs: Option<usize>,

    /// Log level filter for `loom` when re-running failed tests
    #[clap(long, env = ENV_LOOM_LOG, default_value = "trace")]
    loom_log: String,
}

const ENV_CHECKPOINT_INTERVAL: &str = "LOOM_CHECKPOINT_INTERVAL";
const ENV_MAX_BRANCHES: &str = "LOOM_MAX_BRANCHES";
const ENV_MAX_DURATION: &str = "LOOM_MAX_DURATION";
const ENV_MAX_PERMUTATIONS: &str = "LOOM_MAX_PERMUTATIONS";
const ENV_MAX_THREADS: &str = "LOOM_MAX_THREADS";
const ENV_LOOM_LOG: &str = "LOOM_LOG";
const ENV_CHECKPOINT_FILE: &str = "LOOM_CHECKPOINT_FILE";
const ENV_LOOM_LOCATION: &str = "LOOM_LOCATION";

impl Args {
    fn metadata(&self) -> Result<cargo_metadata::Metadata> {
        let mut cmd = cargo_metadata::MetadataCommand::new();
        if let Some(ref manifest_path) = self.cargo.manifest_path {
            cmd.manifest_path(manifest_path);
        }
        self.cargo.features.forward_metadata(&mut cmd);
        cmd.exec().context("getting cargo metadata")
    }
}

impl App {
    /// Parse an [`App`] configuration from command-line arguments and
    /// environment variables.
    pub fn parse() -> Result<Self> {
        Self::from_args(Args::parse())
    }

    /// Run all tests specified by this `App`'s command-line arguments and print
    /// the output of any failing tests.
    pub async fn run_all(&self) -> Result<()> {
        for pkg in self.wanted_packages() {
            self.run_package(pkg).await?;
        }

        Ok(())
    }

    async fn run_package(&self, pkg: &cargo_metadata::Package) -> Result<()> {
        let mut failing = self.failing_tests(pkg).with_context(|| {
            format!("Error collecting failing tests for package `{}`", pkg.name)
        })?;
        let mut tasks = self
            .run_failed(&mut failing)
            .with_context(|| format!("Error rerunning failing tests for package `{}`", pkg.name))?;
        while let Some(result) = tasks.join_one().await? {
            let output = result?;
            println!("\n --- test {} ---\n\n{}", output.name(), output.stdout()?);
        }

        for checkpoint_dir in failing.checkpoint_dirs() {
            tracing::info!(checkpoint_dir = %checkpoint_dir, "Completed loom run");
        }

        Ok(())
    }

    fn failing_tests(&self, pkg: &cargo_metadata::Package) -> Result<Failed> {
        let json = self.args.trace_settings.message_format().is_json();
        let tests = self.test_cmd(pkg).run_tests()?;
        let mut failed = Failed::default();

        for suite in tests {
            let suite = suite.context("Getting next test failed")?;

            let bin_path = suite
                .path()
                .file_name()
                .ok_or_else(|| eyre!("test binary must have a file name"))
                .and_then(|os_str| {
                    os_str
                        .to_str()
                        .ok_or_else(|| eyre!("binary path was not utf8"))
                })
                .with_note(|| format!("bin path: {}", suite.path().display()))?;

            let checkpoint_dir = self.checkpoint_dir.as_path().join(bin_path);

            if suite.kind() == "lib" {
                tracing::info!(path = %suite.path().display(), "Running unittests")
            } else {
                tracing::info!(path = %suite.path().display(), "Running {}", suite.name())
            }

            let mut cmd = suite.command();

            // Don't enable checkpoints, logging, or location tracking for this
            // run. Our goal here is *only* to get the names of the failing
            // tests so we can re-run them individually with their own
            // checkpoint files.
            self.configure_loom_command(&mut cmd)
                .env(ENV_LOOM_LOG, "off");

            // If a test name filter was provided, pass that to the test
            // command.
            //
            // This isn't added by `configure_loom_command`, because we don't
            // want to set duration limits when re-running with logging etc (as
            // it may be slower).
            if let Some(max_duration) = self.max_duration.as_deref() {
                cmd.env(ENV_MAX_DURATION, max_duration);
            }

            // If a test name filter was provided, pass that to the test command.
            if let Some(testname) = self.args.testname.as_deref() {
                cmd.arg(testname);
            }

            // If there is already a checkpoint dir for this artifact hash, skip
            // any previously checkpointed tests.
            if checkpoint_dir.exists() {
                (|| {
                    let mut has_printed = false;
                    for entry in fs::read_dir(checkpoint_dir.as_std_path())? {
                        let path = entry?.path();
                        match path.extension() {
                            Some(extension) if extension == "json" => {
                                if let Some(test) = path.file_stem().and_then(OsStr::to_str) {
                                    // does the test name filter care about
                                    // this test?
                                    let is_included = self
                                        .args
                                        .testname
                                        .as_deref()
                                        .map(|testname| test.contains(testname))
                                        .unwrap_or(true);
                                    if is_included {
                                        cmd.arg("--skip").arg(test);
                                        failed.fail_test(&suite, test.to_owned(), &checkpoint_dir);
                                        if !has_printed {
                                            eprintln!("\npreviously checkpointed");
                                            has_printed = true;
                                        }

                                        test_status::<colors::Red>(test, "failed")
                                    }
                                }
                            }
                            _ => continue,
                        }
                    }
                    Ok::<(), std::io::Error>(())
                })()
                .with_context(|| {
                    format!("failed to read checkpoint directory `{}`", checkpoint_dir)
                })?;
            } else {
                fs::create_dir_all(checkpoint_dir.as_os_str()).with_context(|| {
                    format!("failed to create checkpoint directory `{}`", checkpoint_dir)
                })?;
            }

            let res = CommandMessages::with_command(cmd)
                .with_note(|| format!("running test suite `{}`", suite.name()))?;
            let t0 = std::time::Instant::now();
            for msg in res {
                use test::*;
                match msg.and_then(|msg| msg.decode_custom::<Event>()) {
                    Ok(Event::Test(Test::Failed(test_failed))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &test_failed)
                                .context("write json message")?;
                        } else {
                            test_status::<colors::Red>(&test_failed.name, "failed");
                        }
                        failed.fail_test(&suite, test_failed.name, &checkpoint_dir);
                    }
                    Ok(Event::Test(Test::Ok(ok))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &ok)
                                .context("write json message")?;
                        } else {
                            test_status::<colors::Green>(&ok.name, "ok");
                        }
                    }
                    Ok(Event::Test(Test::Ignored(ignored))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &ignored)
                                .context("write json message")?;
                        } else {
                            test_status::<colors::Yellow>(&ignored.name, "ignored")
                        }
                    }
                    Ok(Event::Suite(Suite::Started(started))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &started)
                                .context("write json message")?;
                        } else {
                            eprintln!("\nrunning {} tests", started.test_count);
                        }
                    }
                    Ok(Event::Suite(Suite::Ok(ok))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &ok)
                                .context("write json message")?;
                        } else {
                            let SuiteOk {
                                passed,
                                failed,
                                ignored,
                                measured,
                                filtered_out,
                                ..
                            } = ok;
                            eprintln!("\ntest result: ok. {passed} passed; {failed} failed; {ignored} ignored; {measured} measured; {filtered_out} filtered out; finished in {:?}", t0.elapsed());
                        }
                    }
                    Ok(Event::Suite(Suite::Failed(suite_failed))) => {
                        if json {
                            serde_json::to_writer(std::io::stderr(), &suite_failed)
                                .context("write json message")?;
                        } else {
                            let SuiteFailed {
                                passed,
                                failed,
                                ignored,
                                measured,
                                filtered_out,
                                ..
                            } = suite_failed;
                            eprintln!("\ntest result: FAILED. {passed} passed; {failed} failed; {ignored} ignored; {measured} measured; {filtered_out} filtered out; finished in {:?}", t0.elapsed());
                        }
                    }
                    Err(error) => tracing::warn!(
                        suite = %suite.name(),
                        %error,
                        "error from test",
                    ),
                    Ok(msg) if json => {
                        serde_json::to_writer(std::io::stderr(), &msg)
                            .context("write json message")?;
                    }
                    _ => {} // TODO(eliza: do something nice here...
                }
            }

            failed.finish_suite(suite);
        }

        Ok(failed)
    }

    fn run_failed(&self, failed: &mut Failed) -> Result<JoinSet<Result<TestOutput>>> {
        let mut tasks = JoinSet::new();
        for (suite, tests) in failed.failed.drain() {
            let suite = failed
                .test_cmds
                .get(&suite)
                .ok_or_else(|| eyre!("missing test command for suite `{}`", suite))?;
            for FailedTest { name, checkpoint } in tests {
                let mut cmd = Command::new(suite.path());
                self.configure_loom_command(&mut cmd)
                    .env(ENV_CHECKPOINT_INTERVAL, &self.checkpoint_interval)
                    .env(ENV_CHECKPOINT_FILE, &checkpoint)
                    .arg(&name);
                let loom_log = self.loom_log.clone();
                let pretty_name = format!("{suite}::{name}", suite = suite.name());
                let task = async move {
                    let t0 = Instant::now();
                    let mut cmd = tokio::process::Command::from(cmd);
                    if checkpoint.exists() {
                        tracing::debug!(test = %pretty_name, "Already checkpointed", )
                    } else {
                        tracing::info!(test = %pretty_name, "Generating checkpoint");
                        tracing::trace!(?cmd);
                        let _ = cmd
                            .stderr(Stdio::null())
                            .stdout(Stdio::null())
                            .status()
                            .await
                            .with_context(|| format!("spawn process to checkpoint {pretty_name}"));
                        let elapsed = t0.elapsed();
                        tracing::debug!(test = %pretty_name, ?elapsed, file = %checkpoint, "checkpointed");
                    }

                    // now, run it again with logging
                    let output = cmd
                        .env(ENV_LOOM_LOG, loom_log.as_ref())
                        .env(ENV_LOOM_LOCATION, "1")
                        .output()
                        .await
                        .with_context(|| format!("spawn process to rerun {pretty_name}"))?;
                    let output = TestOutput {
                        name: pretty_name,
                        output,
                    };
                    Ok(output)
                };
                tasks.spawn(task);
            }
        }
        Ok(tasks)
    }

    fn from_args(mut args: Args) -> Result<Self> {
        color_eyre::config::HookBuilder::default()
            .issue_url(concat!(env!("CARGO_PKG_REPOSITORY"), "/issues/new"))
            .add_issue_metadata("version", env!("CARGO_PKG_VERSION"))
            .add_issue_metadata(
                "args",
                std::env::args().fold(String::new(), |mut s, arg| {
                    s.push_str(arg.as_str());
                    s.push(' ');
                    s
                }),
            )
            .issue_filter(|kind| match kind {
                color_eyre::ErrorKind::NonRecoverable(_) => true,
                color_eyre::ErrorKind::Recoverable(error) =>
                // Skip any IO errors and any errors forwarded from a cargo
                // subcommand, as these may not be our fault.
                {
                    error_is_issue(error)
                }
            })
            .display_env_section(true)
            .add_default_filters()
            .add_frame_filter(Box::new(|frames| {
                const SKIPPED: &[&str] = &[
                    "tokio::runtime",
                    "tokio::coop",
                    "tokio::park",
                    "std::thread::local",
                ];
                frames.retain(|frame| match frame.name.as_ref() {
                    Some(name) => !SKIPPED.iter().any(|prefix| name.starts_with(prefix)),
                    None => true,
                })
            }))
            .install()?;
        args.trace_settings
            .try_init()
            .context("initialize tracing")?;
        let metadata = args.metadata()?;
        let target_dir = {
            let mut target_dir = metadata.workspace_root.clone();
            target_dir.push("target");
            target_dir.push("loom");
            target_dir
        };
        let checkpoint_dir = target_dir.as_path().join("checkpoint");
        fs::create_dir_all(checkpoint_dir.as_os_str())
            .with_context(|| format!("creating checkpoint directory `{}`", checkpoint_dir))?;

        let mut features = String::new();
        let mut feature_list = args.cargo.features.features.iter();
        if let Some(feature) = feature_list.next() {
            features.push_str(feature);
            for feature in feature_list {
                features.push(' ');
                features.push_str(feature);
            }
        }
        let mut rustflags = std::env::var("RUSTFLAGS").unwrap_or_default();
        if !rustflags.is_empty() {
            rustflags.push(' ');
        }
        rustflags.push_str("--cfg loom --cfg debug_assertions");

        // These all need to be represented as strings to pass them as env
        // variables. Format them a single time so we don't have to do it every
        // time we run a test.
        let max_duration = args
            .loom
            .max_duration_secs
            .as_ref()
            .map(ToString::to_string);
        let max_permutations = args.loom.max_permutations.as_ref().map(ToString::to_string);
        let max_branches = args.loom.max_branches.to_string();
        let max_threads = args.loom.max_threads.to_string();
        let checkpoint_interval = args.loom.checkpoint_interval.to_string();
        let loom_log = Arc::from(args.loom.loom_log.clone());
        let test_args = Arc::from(args.test_args.clone());
        Ok(Self {
            args,
            metadata,
            target_dir,
            checkpoint_dir,
            features,
            rustflags,
            max_branches,
            max_duration,
            max_permutations,
            max_threads,
            checkpoint_interval,
            loom_log,
            test_args,
        })
    }

    fn wanted_packages(&self) -> Vec<&cargo_metadata::Package> {
        self.args
            .cargo
            .workspace
            .partition_packages(&self.metadata)
            .0
    }

    fn test_cmd(&self, pkg: &cargo_metadata::Package) -> escargot::CargoBuild {
        let mut cmd = escargot::Cargo::new()
            .build_with("test")
            .arg("--no-run")
            .env("RUSTFLAGS", &self.rustflags)
            .target_dir(&self.target_dir)
            .package(&pkg.name)
            .release();

        if self.args.cargo.lib {
            cmd = cmd.arg("--lib");
        }

        if self.args.cargo.tests || !self.args.cargo.lib {
            cmd = cmd.tests()
        }

        if self.args.cargo.features.all_features {
            cmd = cmd.all_features()
        }

        if self.args.cargo.features.no_default_features {
            cmd = cmd.no_default_features();
        }

        if !&self.args.cargo.features.features.is_empty() {
            cmd = cmd.features(&self.features)
        }

        if let Some(manifest) = self.args.cargo.manifest_path.as_deref() {
            cmd = cmd.manifest_path(manifest);
        }

        cmd
    }

    fn configure_loom_command<'cmd>(&self, cmd: &'cmd mut Command) -> &'cmd mut Command {
        cmd.env(ENV_MAX_BRANCHES, &self.max_branches);

        if let Some(max_permutations) = self.max_permutations.as_deref() {
            cmd.env(ENV_MAX_PERMUTATIONS, max_permutations);
        }

        cmd.env(ENV_MAX_THREADS, &self.max_threads);

        if !self.test_args.is_empty() {
            cmd.args(&self.test_args[..]);
        }

        cmd
    }
}

impl FailedTest {
    fn new(name: String, checkpoint_dir: impl AsRef<Utf8Path>) -> Self {
        let checkpoint = checkpoint_dir.as_ref().join(format!("{name}.json"));
        Self { name, checkpoint }
    }
}

impl fmt::Display for FailedTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)
    }
}

impl Failed {
    pub fn checkpoint_dirs(&self) -> &HashSet<Utf8PathBuf> {
        &self.checkpoint_dirs
    }

    fn fail_test(
        &mut self,
        suite: &CargoTest,
        test_name: String,
        checkpoint_dir: impl AsRef<Utf8Path>,
    ) {
        let checkpoint_dir = checkpoint_dir.as_ref();
        if !self.checkpoint_dirs.contains(checkpoint_dir) {
            self.checkpoint_dirs.insert(checkpoint_dir.to_path_buf());
        }
        let suite_name = self
            .curr_suite_name
            .get_or_insert_with(|| Arc::from(suite.name().to_owned()))
            .clone();
        debug_assert_eq!(suite_name.as_ref(), suite.name());
        self.failed
            .entry(suite_name)
            .or_default()
            .push(FailedTest::new(test_name, checkpoint_dir));
    }

    fn finish_suite(&mut self, suite: CargoTest) {
        if let Some(suite_name) = self.curr_suite_name.take() {
            self.test_cmds.insert(suite_name, suite);
        }
    }
}

// === impl TestOutput ===

impl TestOutput {
    fn name(&self) -> &str {
        self.name.as_str()
    }

    fn stdout(&self) -> Result<&str> {
        std::str::from_utf8(&self.output.stdout[..])
            .with_context(|| format!("stdout from test `{}` was not utf8", self.name))
    }

    // fn stderr(&self) -> Result<&str> {
    //     std::str::from_utf8(&self.output.stderr[..])
    //         .with_context(|| format!("stderr from test `{}` was not utf8", self.name))
    // }
}

fn test_status<C: owo_colors::Color>(name: &str, status: &str) {
    eprintln!(
        "test {} ... {}",
        name,
        status.if_supports_color(owo_colors::Stream::Stderr, |text| text.fg::<C>())
    )
}

fn error_is_issue(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current.take() {
        if error.is::<std::io::Error>() || error.is::<escargot::error::CargoError>() {
            return false;
        }

        current = error.source();
    }

    true
}
