mod cli;
mod token_file;

#[tokio::main(flavor = "multi_thread")]
async fn main() -> anyhow::Result<()> {
    cli::run().await
}
