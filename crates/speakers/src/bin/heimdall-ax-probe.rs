fn main() -> anyhow::Result<()> {
    use speakers::ax;
    if !ax::is_trusted() {
        anyhow::bail!("Accessibility is not granted; no prompt was requested");
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let pids = if args.is_empty() {
        ax::discord_processes()
    } else {
        vec![args[0].parse::<i32>()?]
    };
    anyhow::ensure!(
        !pids.is_empty(),
        "Discord is not running; optional first argument is its PID"
    );
    let observed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_secs_f64();
    for pid in pids {
        let snapshot = ax::snapshot(pid, observed_at)?;
        println!(
            "{}",
            serde_json::to_string_pretty(
                &serde_json::json!({"participants":ax::participants(&snapshot.root),"snapshot":snapshot})
            )?
        );
    }
    Ok(())
}
