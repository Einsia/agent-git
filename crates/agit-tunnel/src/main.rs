fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(agit_tunnel::worker::run(
        tokio::io::BufReader::new(tokio::io::stdin()),
        tokio::io::stdout(),
    ))
}
