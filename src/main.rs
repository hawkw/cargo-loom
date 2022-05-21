use cargo_loom::App;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    let app = App::parse()?;
    tokio::spawn(async move { app.run_all().await })
        .await
        .unwrap()
}
