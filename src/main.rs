use camino::Utf8PathBuf;
use clap::{Command, Parser};
use color_eyre::{eyre::WrapErr, Help};
use escargot::{format::test, CargoTest, CommandMessages};
use owo_colors::{colors, OwoColorize};
use std::{collections::HashMap, fmt, time::SystemTime};

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

    /// Maximum number of permutations to explore
    ///
    /// This sets the value of the `LOOM_MAX_PERMUTATIONS` environment variable
    /// for the test executable.
    #[clap(long, env = "LOOM_MAX_PREEMPTIONS", default_value_t = 2)]
    max_preemptions: usize,

    /// How often to write the checkpoint file
    ///
    /// This sets the value of the `LOOM_CHECKPOINT_INTERVAL` environment
    /// variable for the test executable.
    #[clap(long, env = "LOOM_CHECKPOINT_INTERVAL", default_value_t = 5)]
    checkpoint_interval: usize,

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

    /// Run loom tests in release mode.
    #[clap(long)]
    release: bool,
}

#[derive(Debug)]
struct App {
    args: Args,
    checkpoint_dir: Utf8PathBuf,
    timestamp: u64,
    metadata: cargo_metadata::Metadata,
    target_dir: Utf8PathBuf,
    features: String,
    rustflags: String,
}

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
    fn from_args(mut args: Args) -> color_eyre::Result<Self> {
        let metadata = args.metadata()?;
        let target_dir = {
            let mut target_dir = metadata.workspace_root.clone();
            target_dir.push("target");
            target_dir.push("loom");
            target_dir
        };
        let checkpoint_dir = target_dir.as_path().join("checkpoint");
        std::fs::create_dir_all(checkpoint_dir.as_os_str())
            .with_context(|| format!("creating checkpoint directory `{}`", checkpoint_dir))?;
        let timestamp = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("SystemTime should never be before UNIX_EPOCH")
            .as_secs();
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
        Ok(Self {
            args,
            metadata,
            target_dir,
            checkpoint_dir,
            timestamp,
            features,
            rustflags,
        })
    }

    fn wanted_packages(&self) -> Vec<&cargo_metadata::Package> {
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

        if !dbg!(&self.args.features.features).is_empty() {
            cmd = cmd.features(&self.features)
        }

        if let Some(manifest) = self.args.manifest_path.as_deref() {
            cmd = cmd.manifest_path(manifest);
        }

        cmd
    }

    fn failing_tests(
        &self,
        pkg: &cargo_metadata::Package,
    ) -> color_eyre::Result<HashMap<String, Vec<String>>> {
        let tests = self.test_cmd(pkg).run_tests()?;
        let mut failed: HashMap<String, Vec<String>> = HashMap::new();

        for test in tests {
            let test = test?;
            if test.kind() == "lib" {
                eprintln!("\n Running unittests ({})\n", test.path().display())
            } else {
                eprintln!("\n Running {} ({})\n", test.name(), test.path().display())
            }

            let checkpt_file = self.checkpoint_dir.as_path().join(format!(
                "loom-{}-{}-{}.json",
                pkg.name,
                test.name(),
                self.timestamp
            ));
            let mut cmd = test.command();
            cmd.env(
                "LOOM_MAX_PREEMPTIONS",
                format!("{}", self.args.max_preemptions),
            )
            .env("LOOM_CHECKPOINT_FILE", checkpt_file)
            .env(
                "LOOM_CHECKPOINT_INTERVAL",
                format!("{}", self.args.checkpoint_interval),
            )
            .env("LOOM_LOG", "off");

            let res = CommandMessages::with_command(cmd)
                .with_note(|| format!("running test suite `{}`", test.name()))?;
            let t0 = std::time::Instant::now();
            for msg in res {
                use test::*;
                let msg = msg.with_note(|| format!("running test `{}`", test.name()))?;
                match msg.decode_custom::<Event>() {
                    Ok(Event::Test(Test::Failed(TestFailed { name, .. }))) => {
                        test_status::<colors::Red>(&name, "failed");
                        failed.entry(test.name().to_owned()).or_default().push(name);
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
                    _ => {} // TODO(eliza: do something nice here...
                }
            }
        }

        Ok(failed)
    }
}

fn test_status<C: owo_colors::Color>(name: &str, status: &str) {
    eprintln!(
        "test {} ... {}",
        name,
        status.if_supports_color(owo_colors::Stream::Stderr, |text| text.fg::<C>())
    )
}

fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let args = Args::parse();
    let app = App::from_args(args)?;
    let wanted_pkgs = app.wanted_packages();
    println!(
        "wanted_pkgs={:?}",
        wanted_pkgs.iter().map(|pkg| &pkg.name).collect::<Vec<_>>()
    );
    for pkg in wanted_pkgs {
        let failing = app
            .failing_tests(pkg)
            .with_note(|| format!("package: {}", pkg.name))?;

        println!("package: {}", pkg.name);
        if failing.is_empty() {
            println!("\tno tests failed");
            continue;
        }

        for (test, failed) in failing {
            println!("\t{}: {:?}", test, failed);
        }
    }

    Ok(())
}
