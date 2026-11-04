    use super::*;
    use std::time::{Duration, Instant};
    const MARK: &str = "BLUE-HERON-7";
    fn lab(case: &str) -> Result<PathBuf> {
        let base = PathBuf::from(std::env::var_os("HEIMDALL_OPENCODE_LAB").context("HEIMDALL_OPENCODE_LAB")?);
        anyhow::ensure!(base.is_absolute() && base.to_string_lossy().contains("/target/opencode-lab"), "isolated lab path required");
        // The runtime ships inside the app; a test binary finds the build cache's runtime (or HEIMDALL_AGENT_RUNTIME).
        agent::runtime::Runtime::installed()?;
        let data = base.join(case);
        let _ = std::fs::remove_dir_all(&data);
        agent::runtime::private_dir(&data.join("agent"))?;
        Ok(data)
    }
    /// A conversation held on the local model: its last model_change and answers are splash/qwen3.8-27b.
    fn local_session(data: &Path, last_usage: u64) -> Result<PathBuf> {
        let path = data.join("2026-10-14T20-24-54-925Z_local-history.jsonl");
        let mut rows = vec![
            json!({"type":"session","version":3,"id":"local-history","timestamp":"2026-10-14T20:24:54.925Z","cwd":heimdall_home::home_dir()}),
            json!({"type":"model_change","id":"m0","parentId":null,"timestamp":"2026-10-14T20:24:55.000Z","provider":"splash","modelId":"qwen3.8-27b"}),
            json!({"type":"thinking_level_change","id":"t0","parentId":"m0","timestamp":"2026-10-14T20:24:55.000Z","thinkingLevel":"medium"}),
            // Pi records the prompt it sent as a system entry before the first request; compactions copy it.
            json!({"type":"message","id":"s0","parentId":"t0","timestamp":"2026-10-14T20:24:56.000Z","message":{"role":"system","content":"","sections":{"preamble":"You are Heimdall."},"timestamp":1}}),
        ];
        let mut parent = "s0".to_owned();
        // Twelve exchanges of about 9 KB: Haiku's kept tail (20,000 tokens) holds only the newest ones.
        let filler = "and then the build ran again while we talked about the release. ".repeat(140);
        for i in 0..12 {
            let (u, a) = (format!("u{i}"), format!("a{i}"));
            let said = if i == 0 { format!("[Heimdall delivery: d{i}]\nTranscript:\n[Mic · User] the release codeword is {MARK}, remember it. {filler}") } else { format!("[Heimdall delivery: d{i}]\nTranscript:\n[Mic · User] ordinary remark number {i}. {filler}") };
            rows.push(json!({"type":"message","id":u,"parentId":parent,"timestamp":"2026-10-14T20:25:00.000Z","message":{"role":"user","content":[{"type":"text","text":said}],"timestamp":1}}));
            rows.push(json!({"type":"message","id":a,"parentId":u,"timestamp":"2026-10-14T20:25:01.000Z","message":{"role":"assistant","content":[{"type":"text","text":format!("Noted {i}.")}],"api":"openai-completions","provider":"splash","model":"qwen3.8-27b","stopReason":"stop","usage":{"input":last_usage,"output":4,"cacheRead":0,"cacheWrite":0,"totalTokens":last_usage+4,"cost":{"input":0,"output":0,"cacheRead":0,"cacheWrite":0,"total":0}},"timestamp":2}}));
            parent = a;
        }
        std::fs::write(&path, rows.iter().map(|r| format!("{r}\n")).collect::<String>())?;
        agent::runtime::atomic_write(&data.join("agent/active-session.json"), &serde_json::to_vec(&path)?)?;
        Ok(path)
    }
    fn anthropic_sse(text: &str, input_tokens: u64) -> axum::response::Response {
        use axum::response::IntoResponse;
        let mut out = String::new();
        let mut event = |v: Value| out.push_str(&format!("event: {}\ndata: {v}\n\n", v["type"].as_str().unwrap()));
        event(json!({"type":"message_start","message":{"id":"msg_lab","type":"message","role":"assistant","model":agent::provider::HAIKU,"content":[],"stop_reason":null,"stop_sequence":null,"usage":{"input_tokens":input_tokens,"output_tokens":1}}}));
        event(json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}));
        event(json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":text}}));
        event(json!({"type":"content_block_stop","index":0}));
        event(json!({"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":12}}));
        event(json!({"type":"message_stop"}));
        ([("content-type", "text/event-stream")], out).into_response()
    }
    type Bodies = Arc<Mutex<Vec<Value>>>;
    /// The fake OpenCode: the Console has no config (404); Messages answers a summary request with a summary that
    /// keeps the codeword, and anything else with LAB_OK.
    fn fake_opencode(rt: &tokio::runtime::Runtime) -> Result<(std::net::SocketAddr, Bodies)> {
        let bodies: Bodies = Arc::default();
        let seen = bodies.clone();
        let app = axum::Router::new().route("/zen/go/v1/messages", axum::routing::post(move |body: axum::body::Bytes| {
            let seen = seen.clone();
            async move {
                let body: Value = serde_json::from_slice(&body).unwrap_or_default();
                let summary = body.to_string().contains("<conversation>");
                seen.lock().unwrap().push(body);
                if summary { anthropic_sse(&format!("## Goal\nOwner keeps Heimdall running. The release codeword is {MARK}."), 90_000) } else { anthropic_sse("LAB_OK", 3_000) }
            }
        })).layer(axum::extract::DefaultBodyLimit::max(16 * 1024 * 1024));
        let listener = rt.block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))?;
        let addr = listener.local_addr()?;
        rt.spawn(async move { let _ = axum::serve(listener, app).await; });
        let p = agent::provider::provider(agent::provider::OPENCODE_GO).unwrap();
        // SAFETY: labs run one per process with --test-threads=1.
        unsafe { std::env::remove_var(p.key_env) };
        agent::provider::save_console_session(p, &agent::console::Session {
            access: "console-lab-token".into(), refresh: "lab-refresh".into(), expires_ms: agent::now_ms() + 3_600_000,
            org_id: Some("org-lab".into()), server: format!("http://{addr}/console"),
            base_url: Some(format!("http://{addr}/zen/go/v1")), headers: Default::default(), models: vec![agent::provider::HAIKU.into()], client_id: None,
        })?;
        Ok((addr, bodies))
    }
    fn records(session: &Path) -> Result<Vec<heimdall_archive::Record>> { heimdall_archive::Journal::open_read_only(session)?.records() }
    fn wait(session: &Path, what: &str, test: impl Fn(&[heimdall_archive::Record]) -> bool) -> Result<Vec<heimdall_archive::Record>> {
        let start = Instant::now();
        loop {
            if let Ok(rows) = records(session) { if test(&rows) { return Ok(rows); } }
            anyhow::ensure!(start.elapsed() < Duration::from_secs(120), "{what} did not happen");
            std::thread::sleep(Duration::from_millis(100));
        }
    }
    fn completed(rows: &[heimdall_archive::Record], text: &str) -> bool {
        rows.iter().any(|r| r.kind == "agent_reply" && r.payload["event"]["kind"] == "complete" && r.payload["event"]["text"].as_str().is_some_and(|t| t.contains(text)))
    }
    fn pi_state(rows: &[heimdall_archive::Record]) -> Option<Value> {
        rows.iter().rev().find(|r| r.kind == "agent_transport_reply" && r.payload["wire"]["command"] == "get_state").map(|r| r.payload["wire"]["data"].clone())
    }
    fn start(data: &Path, session: &Path, dry_run: bool) -> Result<AgentRuntime> {
        let tools = Arc::new(Tools::new(&data.join("no-models"), data));
        let runtime = AgentRuntime::start(Settings { dry_run, ..Default::default() }, data.to_owned(), session.to_owned(), tools, None)?;
        runtime.barrier()?;
        Ok(runtime)
    }

    /// The installed app's failure on 18 Oct: Haiku 5.5 selected, the conversation last held by the local model.
    /// The start must accept Pi's report of the selected model, resume the SAME session file (same id, history
    /// restored) and answer on Haiku with the old history in the request.
    #[test]
    #[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
    fn haiku_start_resumes_the_local_conversation_in_the_same_session() -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        let data = lab("start-resume")?;
        let session = data.join("archive/session-start-resume");
        let path = local_session(&data, 2_000)?;
        let (_addr, bodies) = fake_opencode(&rt)?;
        let runtime = start(&data, &session, false)?;
        let (_, running) = runtime.inspect()?.context("Pi did not start")?;
        let rows = records(&session)?;
        anyhow::ensure!(!rows.iter().any(|r| r.kind == "agent_start_failed"), "start failed: {:?}", rows.iter().filter(|r| r.kind == "agent_start_failed").map(|r| r.payload.clone()).collect::<Vec<_>>());
        let state = pi_state(&rows).context("no Pi state")?;
        assert_eq!((state["model"]["provider"].as_str(), state["model"]["id"].as_str()), (Some("opencode-go"), Some(agent::provider::HAIKU)), "{state}");
        assert_eq!(running, path);
        assert_eq!(std::fs::canonicalize(state["sessionFile"].as_str().unwrap_or_default())?, std::fs::canonicalize(&path)?);
        assert_eq!(state["sessionId"], "local-history");
        assert!(state["messageCount"].as_u64().unwrap_or(0) >= 24, "history not restored: {state}");
        runtime.send(Command::Send { text: "What is the release codeword?".into(), steer: false })?;
        wait(&session, "the Haiku answer", |r| completed(r, "LAB_OK"))?;
        runtime.barrier()?;
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "one answer request, no compaction for a short history");
        assert!(sent[0].to_string().contains(MARK), "the old local history must reach Haiku");
        assert_eq!(sent[0]["model"], agent::provider::HAIKU);
        let lines: Vec<Value> = std::fs::read_to_string(&path)?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        assert_eq!(lines[0]["id"], "local-history");
        assert_eq!(lines.iter().filter(|l| l["type"] == "session").count(), 1);
        assert!(lines.iter().any(|l| l["message"]["role"] == "assistant" && l["message"]["model"] == agent::provider::HAIKU), "the answer continues the same file");
        let others: Vec<_> = std::fs::read_dir(&data)?.filter_map(|e| e.ok()).map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "jsonl") && *p != path).collect();
        assert!(others.is_empty(), "a new session file appeared: {others:?}");
        println!("START_RESUME state={state} requests={}", sent.len());
        drop(runtime);
        Ok(())
    }

    /// Off-period speech joins a conversation that is already past Haiku's threshold: Pi compacts first (one
    /// summary request, no answer), then the production extension adds the catch-up as a context-only message
    /// after the checkpoint. A storage rotation then keeps exactly that logical conversation: the next request
    /// carries the summary, the kept tail and the catch-up, not the compacted history.
    #[test]
    #[ignore = "explicit pinned-runtime lab: HEIMDALL_OPENCODE_LAB"]
    fn catchup_compacts_first_then_adds_and_survives_rotation() -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        let data = lab("catchup")?;
        let session = data.join("archive/session-catchup");
        let path = local_session(&data, 121_298)?;
        let (_addr, bodies) = fake_opencode(&rt)?;
        let runtime = start(&data, &session, true)?;
        // Archived as the pipeline records speech while the agent is off.
        let archive = heimdall_archive::store::open(&crate::archive::store_root(&session))?;
        for (id, minutes, source_id, source, speaker, text) in [("old", 50, "mic:x", "Shure MV7i", "User", "said long ago"), ("k", 12, "app:discord", "Discord", "Dave", "the build is green"), ("t", 1, "mic:x", "Shure MV7i", "User", "ship it tonight")] {
            archive.execute("INSERT INTO utterances(session,id,wall_ms,source_id,source,speaker,start_ns,end_ns,text,words,notes,decision) VALUES(1,?1,?2,?3,?4,?5,0,0,?6,'[]','[]','{}')",
                rusqlite::params![id, (agent::now_ms() - minutes * 60_000) as i64, source_id, source, speaker, text])?;
        }
        drop(archive);
        runtime.send(Command::Restart(Settings { dry_run: false, ..Default::default() }))?;
        let rows = wait(&session, "the catch-up", |r| r.iter().any(|e| e.kind == "agent_catchup" && e.payload["state"] == "added"))?;
        runtime.barrier()?;
        let states: Vec<_> = rows.iter().filter(|e| e.kind == "agent_catchup").map(|e| e.payload.clone()).collect();
        assert_eq!(states.iter().map(|s| s["state"].as_str().unwrap_or_default()).collect::<Vec<_>>(), ["compacting_first", "added"], "{states:?}");
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "only the summary request: the catch-up starts no turn");
        assert!(sent[0].to_string().contains("<conversation>"));
        let lines: Vec<Value> = std::fs::read_to_string(&path)?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        let compaction = lines.iter().position(|l| l["type"] == "compaction").context("no compaction checkpoint")?;
        let context = lines.iter().position(|l| l["type"] == "custom_message" && l["customType"] == "heimdall_context").context("catch-up not in the session")?;
        assert!(compaction < context, "compaction must come before the catch-up");
        let text = lines[context]["content"].as_str().unwrap_or_default();
        // Each line carries the clock time it was heard: `[HH:MM:SS Discord · Dave] …`.
        assert!(text.contains(" Discord · Dave] the build is green") && text.contains(" Mic · User] ship it tonight") && !text.contains("said long ago"), "{text}");
        assert!(!lines[compaction..].iter().any(|l| l["message"]["role"] == "assistant"), "no reply to the catch-up");
        let summary = lines[compaction]["summary"].as_str().unwrap_or_default().to_owned();
        assert!(summary.contains(MARK));
        drop(runtime);
        // Storage rotation (normally past 32 MB, here forced): the hot file keeps the header, the checkpoint and its tail.
        let before = std::fs::metadata(&path)?.len();
        let rotated = agent::session::rotate(&path, 0, |_| Ok(()))?;
        let lines: Vec<Value> = std::fs::read_to_string(&path)?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        println!("ROTATION rotated={rotated} bytes {before} -> {} entries={}", std::fs::metadata(&path)?.len(), lines.len());
        assert!(rotated, "the checkpoint must allow bounding the hot file");
        assert_eq!(lines[0]["id"], "local-history");
        assert!(lines.iter().any(|l| l["type"] == "compaction") && lines.iter().any(|l| l["customType"] == "heimdall_context"));
        assert!(!std::fs::read_to_string(&path)?.contains("ordinary remark number 1."), "the hot file keeps only the checkpoint and its tail");
        let runtime = start(&data, &session, false)?;
        runtime.send(Command::Send { text: "What should happen tonight, and what is the codeword?".into(), steer: false })?;
        wait(&session, "the answer after rotation", |r| completed(r, "LAB_OK"))?;
        runtime.barrier()?;
        let sent = bodies.lock().unwrap().clone();
        let next = sent.last().context("no request after rotation")?.to_string();
        assert!(!next.contains("<conversation>"), "no second compaction expected");
        assert!(next.contains(MARK) && next.contains("ship it tonight"), "summary and catch-up must reach the model");
        assert!(!next.contains("ordinary remark number 1."), "compacted history must not return after rotation");
        // A reload records its outcome once, after the replacement Pi is verified, through the production extension.
        runtime.send(Command::Reload { reason: "lab reload".into(), abort: false, permit: None })?;
        wait(&session, "the reload note", |_| std::fs::read_to_string(&path).is_ok_and(|t| t.contains("lab reload")))?;
        runtime.barrier()?;
        let notes: Vec<Value> = std::fs::read_to_string(&path)?.lines().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter(|l| l["customType"] == "heimdall_reload").collect();
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0]["content"].as_str().is_some_and(|n| n.contains("lab reload") && n.contains("resumed successfully")));
        println!("CATCHUP states={states:?} summary_chars={} requests={} rotated={rotated}", summary.len(), sent.len());
        drop(runtime);
        Ok(())
    }

    /// Acceptance on the user's real conversation (a copy; the profile is never written): the production start path
    /// with Haiku 5.5 on the real OpenCode Go endpoint. Explicit and paid: HEIMDALL_REAL_SESSION names the session
    /// file to copy, HEIMDALL_REAL_OPENCODE_TOKEN carries a current Console access token (kept in memory only).
    /// Writes real-compaction-receipt.json beside the lab data with the summary, kept tail and request sizes.
    #[test]
    #[ignore = "explicit paid acceptance: HEIMDALL_OPENCODE_LAB, HEIMDALL_REAL_SESSION, HEIMDALL_REAL_OPENCODE_TOKEN"]
    fn real_conversation_compacts_before_the_first_haiku_answer() -> Result<()> {
        let source = PathBuf::from(std::env::var_os("HEIMDALL_REAL_SESSION").context("HEIMDALL_REAL_SESSION")?);
        let token = std::env::var("HEIMDALL_REAL_OPENCODE_TOKEN").context("HEIMDALL_REAL_OPENCODE_TOKEN")?;
        let data = lab("real-compaction")?;
        let session = data.join("archive/session-real-compaction");
        let path = data.join(source.file_name().context("session file name")?);
        std::fs::copy(&source, &path)?;
        agent::runtime::atomic_write(&data.join("agent/active-session.json"), &serde_json::to_vec(&path)?)?;
        let rt = tokio::runtime::Runtime::new()?;
        let p = agent::provider::provider(agent::provider::OPENCODE_GO).unwrap();
        // SAFETY: labs run one per process with --test-threads=1.
        unsafe { std::env::remove_var(p.key_env); std::env::remove_var("HEIMDALL_REAL_OPENCODE_TOKEN") };
        // As a sign-in does: the account's organization and Console config, then the session kept in memory.
        let signed_in = rt.block_on(agent::console::finish(agent::console::SERVER, p.id, agent::console::Token { access_token: token, refresh_token: "not-used".into(), expires_in: 1800, org_id: None }))?;
        agent::provider::save_console_session(p, &signed_in)?;
        let original_lines = std::fs::read_to_string(&path)?.lines().count();
        let started = Instant::now();
        let runtime = start(&data, &session, false)?;
        let rows = records(&session)?;
        anyhow::ensure!(!rows.iter().any(|r| r.kind == "agent_start_failed"), "start failed: {:?}", rows.iter().filter(|r| r.kind == "agent_start_failed").map(|r| r.payload["error"].clone()).collect::<Vec<_>>());
        let state = pi_state(&rows).context("no Pi state")?;
        let ask = |text: &str, done: usize| -> Result<String> {
            runtime.send(Command::Send { text: text.into(), steer: false })?;
            let rows = wait(&session, "a Haiku answer", |r| r.iter().filter(|e| e.kind == "agent_reply" && e.payload["event"]["kind"] == "complete").count() > done)?;
            runtime.barrier()?;
            Ok(rows.iter().rev().find(|e| e.kind == "agent_reply" && e.payload["event"]["kind"] == "complete").map(|e| e.payload["event"]["text"].as_str().unwrap_or_default().to_owned()).unwrap_or_default())
        };
        let first = ask("Heimdall lab check after tonight's update. Answer from memory in two or three sentences and do not use any tools: what were we in the middle of just before you went quiet on Friday evening?", 0)?;
        let follow = ask("Also from memory, no tools: earlier in this conversation (14 and 16 October) there was a '1014 soak'. What was it, and how did it end?", 1)?;
        drop(runtime);
        let lines: Vec<Value> = std::fs::read_to_string(&path)?.lines().map(serde_json::from_str).collect::<std::result::Result<_, _>>()?;
        let at = lines.iter().position(|l| l["type"] == "compaction").context("no compaction happened")?;
        let compaction = &lines[at];
        let kept = compaction["firstKeptEntryId"].as_str().unwrap_or_default();
        let kept_at = lines.iter().position(|l| l["id"] == kept);
        let answers: Vec<&Value> = lines[at..].iter().filter(|l| l["message"]["role"] == "assistant").collect();
        let first_answer_after_compaction = lines[at..].iter().position(|l| l["message"]["role"] == "assistant").is_some()
            && !lines[original_lines..at].iter().any(|l| l["message"]["role"] == "assistant");
        let usage = |m: &Value| { let u = &m["message"]["usage"]; u["input"].as_u64().unwrap_or(0) + u["cacheRead"].as_u64().unwrap_or(0) + u["cacheWrite"].as_u64().unwrap_or(0) };
        let receipt = json!({
            "date_unix_ms": agent::now_ms(), "elapsed_s": started.elapsed().as_secs_f64(), "endpoint": "OpenCode Go (real)", "model": state["model"]["id"],
            "session_id": state["sessionId"], "same_file": runtime_path_is(&state, &path),
            "original_entries": original_lines, "compaction_entry_index": at, "first_kept_entry_index": kept_at,
            "kept_tail_entries": kept_at.map(|k| at.saturating_sub(k)), "compaction_before_first_answer": first_answer_after_compaction,
            "tokens_before": compaction["tokensBefore"], "summary_usage": compaction["usage"], "summary_chars": compaction["summary"].as_str().map(str::len),
            "summary": compaction["summary"], "answer_request_tokens": answers.iter().map(|m| usage(m)).collect::<Vec<_>>(),
            "answer_usage": answers.iter().map(|m| m["message"]["usage"].clone()).collect::<Vec<_>>(),
            "first_answer": first, "follow_up_answer": follow,
        });
        agent::runtime::atomic_write(&data.join("real-compaction-receipt.json"), &serde_json::to_vec_pretty(&receipt)?)?;
        println!("REAL_COMPACTION receipt written");
        anyhow::ensure!(first_answer_after_compaction, "Haiku answered before compacting");
        anyhow::ensure!(answers.iter().all(|m| usage(m) < 128_000), "a request exceeded the 128K budget");
        Ok(())
    }
    fn runtime_path_is(state: &Value, path: &Path) -> bool {
        state["sessionFile"].as_str().and_then(|f| std::fs::canonicalize(f).ok()) == std::fs::canonicalize(path).ok()
    }
