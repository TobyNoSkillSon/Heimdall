use anyhow::{ensure, Result};
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(
        (2..=3).contains(&args.len()),
        "usage: heimdall-export ARCHIVE_ROOT NEW_DATASET_DIR [NIGHT_OUTPUT_DIR]"
    );
    let count = heimdall_archive::export::export_with_additional(
        std::path::Path::new(&args[0]),
        std::path::Path::new(&args[1]),
        args.get(2).map(std::path::Path::new),
    )?;
    println!("Exported {count} aligned utterances and full source/model evidence");
    Ok(())
}
