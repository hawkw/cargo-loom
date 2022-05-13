use cargo_loom::App;
use color_eyre::{eyre::WrapErr, Help};
use std::process::Output;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let app = App::parse()?;
    let wanted_pkgs = app.wanted_packages();
    for pkg in wanted_pkgs {
        let mut failing = app
            .failing_tests(pkg)
            .context("collecting failing tests")
            .with_note(|| format!("package: {}", pkg.name))?;
        let checkpoint_dirs = failing.take_checkpoint_dirs();
        let mut tasks = app
            .run_failed(failing)
            .context("running failed tests failing tests")
            .with_note(|| format!("package: {}", pkg.name))?;
        while let Some(result) = tasks.join_one().await? {
            let (name, Output { stdout, .. }) = result?;
            let stdout =
                std::str::from_utf8(&stdout[..]).with_context(|| format!("stdout from {name}"))?;
            println!("\n --- test {name} ---\n\n{stdout}");
        }

        for checkpoint_dir in checkpoint_dirs {
            tracing::info!(checkpoint_dir = %checkpoint_dir, "Completed loom run");
        }
    }

    Ok(())
}
