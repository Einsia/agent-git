#[tokio::main]
async fn main() -> anyhow::Result<()> {
    agit_controller::host::run().await
}
