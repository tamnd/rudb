fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args().nth(1).ok_or("usage: certify_summaries FILE")?;
    rudb_native::Writer::certify_summaries(path)?;
    Ok(())
}
