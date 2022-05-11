use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::{
    eyre::{eyre, WrapErr},
    Help,
};
use escargot::{format::test, CargoTest, CommandMessages};
use owo_colors::{colors, OwoColorize};
use std::{
    collections::HashMap,
    process::{Command, Stdio},
    time::{Instant, SystemTime},
};

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
}

#[derive(Default)]
pub struct Failed {
    failed: HashMap<String, Vec<String>>,
    test_cmds: HashMap<String, CargoTest>,
}

/// A utility for running Loom tests
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
    // /// Run loom tests in release mode.
    // #[clap(long)]
    // release: bool,
}

const ENV_CHECKPOINT_INTERVAL: &str = "LOOM_CHECKPOINT_INTERVAL";
const ENV_MAX_BRANCHES: &str = "LOOM_MAX_BRANCHES";
const ENV_MAX_DURATION: &str = "LOOM_MAX_DURATION";
const ENV_MAX_PERMUTATIONS: &str = "LOOM_MAX_PERMUTATIONS";
const ENV_MAX_THREADS: &str = "LOOM_MAX_THREADS";
const ENV_LOOM_LOG: &str = "LOOM_LOG";
const ENV_CHECKPOINT_FILE: &str = "LOOM_CHECKPOINT_FILE";

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

    fn from_args(args: Args) -> color_eyre::Result<Self> {
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

    fn configure_loom_command(&self, test: &CargoTest) -> Command {
        let mut cmd = test.command();

        cmd.env(ENV_MAX_BRANCHES, &self.max_branches);

        if let Some(max_permutations) = self.max_permutations.as_deref() {
            cmd.env(ENV_MAX_PERMUTATIONS, max_permutations);
        }

        cmd.env(ENV_MAX_THREADS, &self.max_threads);
        cmd
    }

    pub fn failing_tests(&self, pkg: &cargo_metadata::Package) -> color_eyre::Result<Failed> {
        let tests = self.test_cmd(pkg).run_tests()?;
        let mut failed = Failed::default();

        for test in tests {
            let mut any_failed = false;
            let test = test.context("getting next test failed")?;
            if test.kind() == "lib" {
                eprintln!("\n Running unittests ({})\n", test.path().display())
            } else {
                eprintln!("\n Running {} ({})\n", test.name(), test.path().display())
            }

            let mut cmd = self.configure_loom_command(&test);

            cmd.env(ENV_LOOM_LOG, "off");

            if let Some(max_duration) = self.max_duration.as_deref() {
                cmd.env(ENV_MAX_DURATION, max_duration);
            }
            // Don't enable checkpoints, logging, or location tracking for this
            // run. Our goal here is *only* to get the names of the failing
            // tests so we can re-run them individually with their own
            // checkpoint files.

            let res = CommandMessages::with_command(cmd)
                .with_note(|| format!("running test suite `{}`", test.name()))?;
            let t0 = std::time::Instant::now();
            for msg in res {
                use test::*;
                match msg.and_then(|msg| msg.decode_custom::<Event>()) {
                    Ok(Event::Test(Test::Failed(TestFailed { name, .. }))) => {
                        test_status::<colors::Red>(&name, "failed");
                        failed.fail_test(&test, name);
                        any_failed = true;
                    }
                    Ok(Event::Test(Test::Ok(TestOk { name, .. }))) => {
                        test_status::<colors::Green>(&name, "ok")
                    }
                    Ok(Event::Test(Test::Ignored(TestIgnored { name, .. }))) => {
                        test_status::<colors::Yellow>(&name, "ignored")
                    }
                    Ok(Event::Suite(Suite::Started(SuiteStarted { test_count, .. }))) => {
                        eprintln!("running {} tests", test_count);
                    }
                    Ok(Event::Suite(Suite::Ok(SuiteOk {
                        passed,
                        failed,
                        ignored,
                        measured,
                        filtered_out,
                        ..
                    }))) => {
                        eprintln!("\ntest result: ok. {passed} passed; {failed} failed; {ignored} ignored; {measured} measured; {filtered_out} filtered out; finished in {:?}", t0.elapsed());
                    }
                    Ok(Event::Suite(Suite::Failed(SuiteFailed {
                        passed,
                        failed,
                        ignored,
                        measured,
                        filtered_out,
                        ..
                    }))) => {
                        eprintln!("\ntest result: FAILED. {passed} passed; {failed} failed; {ignored} ignored; {measured} measured; {filtered_out} filtered out; finished in {:?}", t0.elapsed());
                    }
                    Err(error) => eprintln!(
                        "error from test (suite = {suite}): {error}",
                        suite = test.name()
                    ),
                    _ => {} // TODO(eliza: do something nice here...
                }
            }

            if any_failed {
                failed.test_cmds.insert(test.name().to_string(), test);
            }
        }

        Ok(failed)
    }

    pub fn checkpoint_failed(&self, failed: &Failed) -> color_eyre::Result<()> {
        for (suite, tests) in &failed.failed {
            let suite = failed
                .test_cmds
                .get(suite)
                .ok_or_else(|| eyre!("missing test command for suite `{}`", suite))?;
            for test in tests {
                let t0 = Instant::now();
                let file = self.checkpoint_file(suite, test.as_str());
                let mut cmd = self.configure_loom_command(suite);
                cmd.env(ENV_CHECKPOINT_INTERVAL, &self.checkpoint_interval)
                    .env(ENV_CHECKPOINT_FILE, &file)
                    .arg(test);
                println!("checkpoint command: {cmd:?}");
                cmd.stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .spawn()
                    .and_then(|mut child| child.wait())
                    .with_context(|| format!("checkpointing {}::{}", suite.name(), &test))?;
                let elapsed = t0.elapsed();
                println!(
                    "checkpointed {suite}::{test} in {elapsed:?} ({file}) ",
                    suite = suite.name()
                );
            }
        }
        Ok(())
    }

    fn checkpoint_file(&self, suite: &CargoTest, test_name: &str) -> Utf8PathBuf {
        self.checkpoint_dir
            .join(format!("{suite}-{test_name}.json", suite = suite.name()))
    }
}

impl Failed {
    // fn collect_suite(&mut self, command: Command)

    fn fail_test(&mut self, suite: &CargoTest, test_name: String) {
        self.failed
            .entry(suite.name().to_owned())
            .or_default()
            .push(test_name);
    }
}

fn test_status<C: owo_colors::Color>(name: &str, status: &str) {
    eprintln!(
        "test {} ... {}",
        name,
        status.if_supports_color(owo_colors::Stream::Stderr, |text| text.fg::<C>())
    )
}
