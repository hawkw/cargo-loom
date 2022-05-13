use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
use color_eyre::{
    eyre::{eyre, WrapErr},
    Help,
};
use escargot::{format::test, CargoTest, CommandMessages};
use owo_colors::{colors, OwoColorize};
use std::{
    collections::HashMap,
    fmt,
    process::{Command, Output},
    sync::Arc,
    time::{Instant, SystemTime},
};
use tokio::task::JoinSet;

mod trace;

#[derive(Debug)]
pub struct App {
    args: Args,
    pub checkpoint_dir: Utf8PathBuf,
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
pub struct Failed {
    failed: HashMap<String, Vec<FailedTest>>,
    test_cmds: HashMap<String, CargoTest>,
}

#[derive(Debug)]

struct FailedTest {
    name: String,
    checkpoint: Utf8PathBuf,
}

/// A utility for running Loom tests
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
    /// Path to Cargo.toml
    #[clap(long, env = "CARGO_MANIFEST_PATH", value_hint = clap::ValueHint::FilePath)]
    manifest_path: Option<std::path::PathBuf>,

    #[clap(flatten)]
    workspace: clap_cargo::Workspace,

    #[clap(flatten)]
    features: clap_cargo::Features,

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

    #[clap(flatten)]
    trace_settings: trace::TraceSettings,

    /// If specified, only run tests containing this string in their names
    testname: Option<String>,

    /// Arguments passed to the test binary.
    #[clap(raw = true)]
    test_args: Vec<String>,
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
    fn metadata(&self) -> color_eyre::Result<cargo_metadata::Metadata> {
        let mut cmd = cargo_metadata::MetadataCommand::new();
        if let Some(ref manifest_path) = self.manifest_path {
            cmd.manifest_path(manifest_path);
        }
        self.features.forward_metadata(&mut cmd);
        cmd.exec().context("getting cargo metadata")
    }
}

impl App {
    pub fn parse() -> color_eyre::Result<Self> {
        Self::from_args(Args::parse())
    }

    fn from_args(mut args: Args) -> color_eyre::Result<Self> {
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
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime should never be before UNIX_EPOCH")
            .as_secs();
        let checkpoint_dir = target_dir
            .as_path()
            .join("checkpoint")
            .join(format!("run_{}", timestamp));
        std::fs::create_dir_all(checkpoint_dir.as_os_str())
            .with_context(|| format!("creating checkpoint directory `{}`", checkpoint_dir))?;

        let mut features = String::new();
        let mut feature_list = args.features.features.iter();
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
        let max_duration = args.max_duration_secs.as_ref().map(ToString::to_string);
        let max_permutations = args.max_permutations.as_ref().map(ToString::to_string);
        let max_branches = args.max_branches.to_string();
        let max_threads = args.max_threads.to_string();
        let checkpoint_interval = args.checkpoint_interval.to_string();
        let loom_log = Arc::from(args.loom_log.clone());
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

    pub fn wanted_packages(&self) -> Vec<&cargo_metadata::Package> {
        self.args.workspace.partition_packages(&self.metadata).0
    }

    fn test_cmd(&self, pkg: &cargo_metadata::Package) -> escargot::CargoBuild {
        let mut cmd = escargot::Cargo::new()
            .build_with("test")
            .arg("--no-run")
            .env("RUSTFLAGS", &self.rustflags)
            .target_dir(&self.target_dir)
            .package(&pkg.name)
            .release();

        if self.args.lib {
            cmd = cmd.arg("--lib");
        }

        if self.args.tests || !self.args.lib {
            cmd = cmd.tests()
        }

        if self.args.features.all_features {
            cmd = cmd.all_features()
        }

        if self.args.features.no_default_features {
            cmd = cmd.no_default_features();
        }

        if !&self.args.features.features.is_empty() {
            cmd = cmd.features(&self.features)
        }

        if let Some(manifest) = self.args.manifest_path.as_deref() {
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

    pub fn failing_tests(&self, pkg: &cargo_metadata::Package) -> color_eyre::Result<Failed> {
        let json = self.args.trace_settings.message_format().is_json();
        let tests = self.test_cmd(pkg).run_tests()?;
        let mut failed = Failed::default();

        for test in tests {
            let mut any_failed = false;
            let test = test.context("getting next test failed")?;
            if test.kind() == "lib" {
                tracing::info!(path = %test.path().display(), "Running unittests")
            } else {
                tracing::info!(path = %test.path().display(), "Running {}", test.name())
            }

            let mut cmd = test.command();

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

            let res = CommandMessages::with_command(cmd)
                .with_note(|| format!("running test suite `{}`", test.name()))?;
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
                        failed.fail_test(&test, test_failed.name, &self.checkpoint_dir);
                        any_failed = true;
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
                        suite = %test.name(),
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

            if any_failed {
                failed.test_cmds.insert(test.name().to_string(), test);
            }
        }

        Ok(failed)
    }

    pub fn run_failed(
        &self,
        failed: Failed,
    ) -> color_eyre::Result<JoinSet<color_eyre::Result<(String, Output)>>> {
        let mut tasks = JoinSet::new();
        for (suite, tests) in failed.failed {
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
                    tracing::info!(test = %pretty_name, "Generating checkpoint");
                    tracing::trace!(?cmd);
                    let mut cmd = tokio::process::Command::from(cmd);
                    let _ = cmd
                        .status()
                        .await
                        .with_context(|| format!("spawn process to checkpoint {pretty_name}"));
                    let elapsed = t0.elapsed();
                    tracing::debug!(test = %pretty_name, ?elapsed, file = %checkpoint, "checkpointed");

                    // now, run it again with logging
                    let output = cmd
                        .env(ENV_LOOM_LOG, loom_log.as_ref())
                        .env(ENV_LOOM_LOCATION, "1")
                        .output()
                        .await
                        .with_context(|| format!("spawn process to rerun {pretty_name}"))?;
                    Ok((pretty_name, output))
                };
                tasks.spawn(task);
            }
        }
        Ok(tasks)
    }
}

impl FailedTest {
    fn new(name: String, suite: &CargoTest, checkpoint_dir: impl AsRef<Utf8Path>) -> Self {
        let checkpoint = checkpoint_dir
            .as_ref()
            .join(format!("{suite}-{name}.json", suite = suite.name()));
        Self { name, checkpoint }
    }
}

impl fmt::Display for FailedTest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.name.fmt(f)
    }
}

impl Failed {
    // fn collect_suite(&mut self, command: Command)

    fn fail_test(
        &mut self,
        suite: &CargoTest,
        test_name: String,
        checkpoint_dir: impl AsRef<Utf8Path>,
    ) {
        self.failed
            .entry(suite.name().to_owned())
            .or_default()
            .push(FailedTest::new(test_name, suite, checkpoint_dir));
    }
}

fn test_status<C: owo_colors::Color>(name: &str, status: &str) {
    eprintln!(
        "test {} ... {}",
        name,
        status.if_supports_color(owo_colors::Stream::Stderr, |text| text.fg::<C>())
    )
}
