use cargo_loom::App;
use color_eyre::{
    eyre::{eyre, WrapErr},
    Help,
};
fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;

    let app = App::parse()?;
    let wanted_pkgs = app.wanted_packages();
    for pkg in wanted_pkgs {
        let failing = app
            .failing_tests(pkg)
            .context("collecting failing tests")
            .with_note(|| format!("package: {}", pkg.name))?;
        app.checkpoint_failed(&failing)
            .context("checkpointing failing tests")
            .with_note(|| format!("package: {}", pkg.name))?;
    }

    Ok(())
}
