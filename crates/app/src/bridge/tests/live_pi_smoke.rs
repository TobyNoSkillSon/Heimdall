    use super::*;
    use std::time::{Duration,Instant};
    fn wait(session:&Path,start:Instant,predicate:impl Fn(&[heimdall_archive::Record])->bool)->Result<Vec<heimdall_archive::Record>>{
        loop{anyhow::ensure!(start.elapsed()<Duration::from_secs(600),"live smoke reached its 10 minute bound");if let Ok(j)=heimdall_archive::Journal::open_read_only(session){let rows=j.records()?;if predicate(&rows){return Ok(rows);}}std::thread::sleep(Duration::from_millis(200));}
    }
    fn completed(rows:&[heimdall_archive::Record],text:&str)->bool{
        rows.iter().any(|r|r.kind=="agent_reply"&&r.payload["event"]["kind"]=="complete"&&r.payload["event"]["error"]==false&&r.payload["event"]["text"].as_str().is_some_and(|t|t.contains(text)))
    }
    fn usage(pid:u32)->Result<(f64,u64)>{
        unsafe extern "C" {fn proc_pid_rusage(pid:i32,flavor:i32,info:*mut libc::rusage_info_v2)->i32;fn mach_timebase_info(info:*mut Timebase)->i32;}
        #[repr(C)]struct Timebase{numer:u32,denom:u32}
        let mut info:libc::rusage_info_v2=unsafe{std::mem::zeroed()};let mut t=Timebase{numer:0,denom:0};
        anyhow::ensure!(unsafe{proc_pid_rusage(pid as i32,2,&mut info)}==0,"Pi process metrics unavailable");anyhow::ensure!(unsafe{mach_timebase_info(&mut t)}==0,"timebase unavailable");
        Ok(((info.ri_user_time+info.ri_system_time) as f64*t.numer as f64/t.denom as f64/1e9,info.ri_resident_size))
    }
    /// Explicit end check only: uses the real pinned runtime and already-ready Splash.
    #[test]
    #[ignore="real local GPU smoke; explicitly authorized end check only"]
    fn real_pinned_pi_splash_mcp_reload_and_automatic_recovery()->Result<()> {
        let start=Instant::now();let data=PathBuf::from(std::env::var_os("HEIMDALL_PI_SMOKE_DATA").context("explicit lab data path required")?);
        anyhow::ensure!(data.to_string_lossy().contains("target/pi-harness"),"smoke must use the owned lab path");let session=data.join("archive/session-live-smoke");
        let tools=Arc::new(Tools::new(&data.join("no-models"),&data));
        struct SmokeCredentials;
        impl t3::CredentialStore for SmokeCredentials {
            fn load(&self)->Result<Option<t3::client::Credential>>{Ok(Some(t3::client::Credential{token:"fake-smoke-token".into(),expires_ms:agent::now_ms()+30*24*60*60*1000,environment:"fake-smoke".into(),revoked:false}))}
            fn save(&self,_:&t3::client::Credential)->Result<()>{anyhow::bail!("no real authorization in smoke")}
        }
        let actions=tools.mcp_actions.actions.as_ref().map_err(|e|anyhow::anyhow!(e.clone()))?.clone();
        let client=Arc::new(t3::Client::new(data.join("fake-t3-runtime.json"),Arc::new(SmokeCredentials))?);
        *tools.mcp_actions.workers.lock().unwrap()=Some(Arc::new(t3::Workers::open(&data.join("agent/t3-workers.sqlite"),client,actions)?));
        let runtime=AgentRuntime::start(Settings{dry_run:false,..Default::default()},data.clone(),session.clone(),tools.clone(),Some(ui::Ui::handle()))?;
        runtime.barrier()?;let (first_pid,path)=runtime.inspect()?.context("Pi did not start; inspect the private smoke journal")?;
        let mut png=std::io::Cursor::new(Vec::new()); image::RgbImage::from_pixel(32,32,image::Rgb([20,40,180])).write_to(&mut png,image::ImageFormat::Png)?;
        runtime.send(Command::Observe(Observation{id:"smoke-image-request".into(),timestamp_ms:agent::now_ms(),source:"microphone".into(),speaker:"User".into(),language:"en".into(),text:"This is a local Heimdall runtime smoke. The attached image is a blue square. Reply with exactly LIME and no other text. Do not use any tools.".into()}))?;
        let send_ns=capture::host_now_ns();runtime.send(Command::Wake {images:vec![Attachment{media_type:"image/png".into(),bytes:png.into_inner()}],urgent:false})?;
        let rows=wait(&session,start,|r|completed(r,"LIME"))?;
        let first_token_ns=rows.iter().find(|r|r.host_ns>=send_ns&&r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="message_update"&&matches!(r.payload["wire"]["assistantMessageEvent"]["type"].as_str(),Some("thinking_delta"|"text_delta"))).context("no streamed first token")?.host_ns;
        let first_token_ms=(first_token_ns-send_ns) as f64/1e6;
        runtime.barrier()?;
        let warm_ns=capture::host_now_ns();runtime.send(Command::Send{text:"This is a local Heimdall runtime smoke. Reply with exactly MINT and no other text. Do not use any tools.".into(),steer:false})?;
        let rows=wait(&session,start,|r|completed(r,"MINT"))?;
        let warm_token_ns=rows.iter().find(|r|r.host_ns>=warm_ns&&r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="message_update"&&matches!(r.payload["wire"]["assistantMessageEvent"]["type"].as_str(),Some("thinking_delta"|"text_delta"))).context("no warm streamed first token")?.host_ns;
        let warm_token_ms=(warm_token_ns-warm_ns) as f64/1e6;
        runtime.send(Command::Send{text:"MCP smoke step 1: call mcp__heimdall__timeline_search once with query runtime-smoke and limit 1. Empty hits are expected. Do not search again or call another tool. After the result reply exactly SEARCH_OK.".into(),steer:false})?;
        wait(&session,start,|r|completed(r,"SEARCH_OK"))?;
        runtime.send(Command::Send{text:"MCP smoke step 2: call mcp__heimdall__island_notice once with text Smoke notice and detail {\"type\":\"text\",\"text\":\"smoke\"}. After the result reply exactly NOTICE_OK. Do not call another tool.".into(),steer:false})?;
        wait(&session,start,|r|completed(r,"NOTICE_OK"))?;
        runtime.send(Command::Send{text:"MCP smoke step 3: call mcp__heimdall__reload_me once with reason integration smoke. Reload happens automatically after you end this turn. After the receipt reply exactly TOOL_OK and END THE TURN. Do not call another tool or try to check the reload.".into(),steer:false})?;
        let rows=wait(&session,start,|r|completed(r,"TOOL_OK"))?;
        let count=|name:&str|rows.iter().filter(|r|r.kind=="agent_transport_reply"&&r.payload["wire"]["type"]=="tool_execution_start"&&r.payload["wire"]["toolName"]==name).count();
        let tool_calls=count("mcp__heimdall__timeline_search");let island_calls=count("mcp__heimdall__island_notice");let reload_calls=count("mcp__heimdall__reload_me");
        anyhow::ensure!(tool_calls==1&&island_calls==1&&reload_calls==1,"expected exactly one of each MCP call, got timeline {tool_calls}, island {island_calls}, reload {reload_calls}");
        let header:Value=serde_json::from_str(std::fs::read_to_string(&path)?.split('\n').next().unwrap())?;
        let initial_id=header["id"].clone();
        wait(&session,start,|r|r.iter().any(|e|e.kind=="agent_reload"))?;runtime.barrier()?;
        let (reloaded_pid,new_path)=runtime.inspect()?.context("Pi not available after reload")?;anyhow::ensure!(reloaded_pid!=first_pid&&new_path==path,"reload changed the session or kept the old process");
        std::thread::sleep(Duration::from_secs(3));let (cpu_before,_)=usage(reloaded_pid)?;let idle_start=Instant::now();std::thread::sleep(Duration::from_secs(3));let (cpu_after,idle_rss)=usage(reloaded_pid)?;let idle_cpu_percent=(cpu_after-cpu_before)/idle_start.elapsed().as_secs_f64()*100.;
        // Kill only the owned lab Pi. The real app supervisor must recover it.
        unsafe{libc::kill(reloaded_pid as i32,libc::SIGKILL);}
        let recovered_pid=loop{anyhow::ensure!(start.elapsed()<Duration::from_secs(600),"recovery did not finish within the smoke budget");std::thread::sleep(Duration::from_millis(250));if let Some((pid,same))=runtime.inspect()?{if pid!=reloaded_pid{anyhow::ensure!(same==path,"recovery changed the session path");break pid;}}};
        runtime.barrier()?;
        let session_lines=std::fs::read_to_string(&path)?;
        let parsed:Vec<Value>=session_lines.lines().map(serde_json::from_str).collect::<std::result::Result<_,_>>()?;
        anyhow::ensure!(parsed.iter().any(|e|e["customType"]=="heimdall_reload"&&e["content"].as_str().is_some_and(|n|n.contains("reload-")&&n.contains("resumed successfully"))),"missing receipt/outcome resume notice");
        let tool_count=agent::mcp_contract::tools().as_array().map_or(0,Vec::len);anyhow::ensure!(parsed.iter().filter(|e|e["customType"]=="heimdall_bridge").all(|e|e["data"]["toolCount"]==tool_count),"bridge did not register exactly {tool_count} tools");
        let action_db=rusqlite::Connection::open(data.join("agent/mcp-actions.sqlite"))?;
        let receipt_operations:Vec<String>=action_db.prepare("SELECT operation FROM receipts WHERE kind IN ('notice','reload') ORDER BY kind")?.query_map([],|r|r.get(0))?.collect::<rusqlite::Result<_>>()?;
        let tool_ids:Vec<String>=rows.iter().filter(|r|r.payload["wire"]["type"]=="tool_execution_start"&&matches!(r.payload["wire"]["toolName"].as_str(),Some("mcp__heimdall__island_notice"|"mcp__heimdall__reload_me"))).filter_map(|r|r.payload["wire"]["toolCallId"].as_str().map(str::to_owned)).collect();
        anyhow::ensure!(receipt_operations.len()==2&&receipt_operations.iter().all(|id|tool_ids.contains(id)),"MCP bridge dropped stable Pi toolCallId metadata");
        let usage_messages:Vec<Value>=parsed.iter().filter(|e|e["type"]=="message"&&e["message"]["role"]=="assistant").map(|e|e["message"]["usage"].clone()).collect();
        let header:Value=serde_json::from_str(std::fs::read_to_string(&path)?.split('\n').next().unwrap())?;anyhow::ensure!(header["id"]==initial_id,"session identity changed after recovery");
        let receipt=json!({"date_unix_ms":agent::now_ms(),"hardware":"User's Apple Silicon Mac, shared/busy","pi_version":agent::runtime::pi_version(),"node_version":agent::runtime::node_version(),"model":agent::runtime::MODEL,"context_limit":serde_json::from_slice::<Value>(&std::fs::read(data.join("agent/config/models.json"))?)?["providers"]["splash"]["models"][0]["contextWindow"],"first_token_ms_including_thinking":first_token_ms,"warm_first_token_ms_including_thinking":warm_token_ms,"image_wake":true,"island_notice_calls":island_calls,"reload_me_calls":reload_calls,"idle_rss_bytes":idle_rss,"idle_cpu_percent_one_core_3s":idle_cpu_percent,"mcp_calls":tool_calls,"reload_changed_pid":true,"forced_kill_automatic_recovery":true,"session_id":initial_id,"first_pid":first_pid,"reloaded_pid":reloaded_pid,"recovered_pid":recovered_pid,"usage_messages":usage_messages,"stable_action_ids":receipt_operations,"elapsed_seconds":start.elapsed().as_secs_f64(),"splash_started_by_worker":false,"lab_data":data});
        agent::runtime::atomic_write(&data.join("smoke-receipt.json"),&serde_json::to_vec_pretty(&receipt)?)?;
        println!("PI_SMOKE {}",receipt);drop(runtime);Ok(())
    }
