//! Scheduled wakes (`agent::wakes`, selfwake night 21 Oct) end to end: the heartbeat the supervisor runs, the outbox,
//! the agent's notice queue and the fixture Pi. "When I tell you 'at seven, do this', you receive
//! that notification later."
use super::*;
use agent::wakes::{Source, When};
fn wait_for(session: &Path, what: &str, test: impl Fn(&[heimdall_archive::Record]) -> bool) -> Result<()> {
    let began = std::time::Instant::now();
    loop {
        let rows = heimdall_archive::Journal::open_read_only(session)?.records()?;
        if test(&rows) { return Ok(()); }
        anyhow::ensure!(began.elapsed() < std::time::Duration::from_secs(10), "{what} did not arrive");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}
fn start(data: &Path, session: &Path, tools: &Arc<Tools>) -> Result<AgentRuntime> {
    test_harness(Some(Path::new(env!("CARGO_MANIFEST_DIR")).join("../agent/tests/fixtures/mock-adapter.py")));
    let result = AgentRuntime::start(Settings { dry_run: false, ..Default::default() }, data.into(), session.into(), tools.clone(), None);
    test_harness(None);
    let runtime = result?;
    runtime.barrier()?;
    Ok(runtime)
}
/// The agent's queued or delivered wakes (not context, not cancelled) that carry `words`.
fn wake_rows(data: &Path, words: &str) -> Result<usize> {
    let db = rusqlite::Connection::open(data.join("pi-spool.sqlite"))?;
    let rows = db.prepare("SELECT wire FROM deliveries WHERE query=1 AND state!='cancelled'")?.query_map([], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
    let session = data.join("archive/session-integration");
    let mut n = 0;
    for wire in rows {
        // Delivered wires are retired to the session archive (`Bridge::retire`).
        let value: Value = serde_json::from_str(&wire)?;
        let body = match value["archive_ref"].as_str() { Some(r) => String::from_utf8(heimdall_archive::store::read_log(&session, r)?)?, None => wire };
        if body.contains(words) { n += 1; }
    }
    Ok(n)
}
/// Turns (not context) that carried `words`.
fn turns(session: &Path, words: &str) -> Result<usize> {
    Ok(heimdall_archive::Journal::open_read_only(session)?.records()?.iter().filter(|e| e.kind == "agent_send" && e.payload.to_string().contains(words)).count())
}
#[test]
fn a_scheduled_wake_starts_one_turn_right_after_a_start_and_never_again_after_a_relaunch() -> Result<()> {
    let d = tempfile::tempdir()?;
    let data = d.path().join("data");
    let session = data.join("archive/session-integration");
    std::fs::create_dir_all(&data)?;
    let tools = Arc::new(Tools::new(d.path(), &data));
    let runtime = start(&data, &session, &tools)?;
    let wakes = tools.mcp_actions.wakes.as_ref().map_err(|e| anyhow::anyhow!(e.clone()))?.clone();
    // The agent's tool: one line, replayed by its operation id.
    let p = tools.agent_enabled.permit()?;
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let line = rt.block_on(tools.mcp_actions.action(&tools, "schedule_wake", json!({"in_minutes": 5, "message": "much later"}), "op-tool", &p))?;
    assert!(line.starts_with("Wake wake-") && line.ends_with("(in 5 min)."), "{line}");
    assert_eq!(rt.block_on(tools.mcp_actions.action(&tools, "schedule_wake", json!({"in_minutes": 5, "message": "much later"}), "op-tool", &p))?, line);
    assert!(rt.block_on(tools.mcp_actions.action(&tools, "list_wakes", json!({}), "op-list", &p))?.contains("much later"));
    let id = line.split(' ').nth(1).unwrap().to_owned();
    assert!(rt.block_on(tools.mcp_actions.action(&tools, "cancel_wake", json!({"id": id}), "op-cancel", &p))?.ends_with("cancelled."));
    // Due in 300 ms, before the user has said anything since the start: a turn of its own (a worker result would be context).
    wakes.schedule("op-soon", &When::In { ms: 300 }, "check the integration build", Source::Agent)?;
    wait_for(&session, "the scheduled wake's turn", |r| r.iter().any(|e| e.kind == "agent_send" && e.payload.to_string().contains("check the integration build")))?;
    // A local agent's command-line wake while the app runs: picked up within one heartbeat (shortened here) and queued
    // as a wake; its turn follows the 30-second gap between notice-only turns (E1.18), so the test checks the queue.
    wakes.set_tick_ms(200);
    let args: Vec<String> = ["--data-dir", data.to_str().unwrap(), "--in", "0", "--message", "re-check your tracked tasks"].iter().map(|s| s.to_string()).collect();
    assert!(agent::wakes::cli(&args, || unreachable!())?.starts_with("Wake wake-"));
    let began = std::time::Instant::now();
    while wake_rows(&data, "re-check your tracked tasks")? == 0 {
        anyhow::ensure!(began.elapsed() < std::time::Duration::from_secs(5), "the command-line wake was not queued for the agent");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    drop(runtime);
    // Relaunch twice: nothing is delivered again.
    for _ in 0..2 {
        let runtime = start(&data, &session, &tools)?;
        std::thread::sleep(std::time::Duration::from_millis(400));
        runtime.barrier()?;
        drop(runtime);
    }
    assert_eq!(turns(&session, "check the integration build")?, 1);
    assert_eq!(wake_rows(&data, "re-check your tracked tasks")?, 1);
    assert_eq!(wake_rows(&data, "check the integration build")?, 1);
    assert_eq!(turns(&session, "much later")?, 0, "the cancelled wake never fires");
    Ok(())
}
