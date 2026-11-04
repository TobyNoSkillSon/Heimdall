//! Isolated subprocess supervision. No provider request, installed app or real profile.
use agent::{harness::{Pi, PiConfig}, mcp::{McpServer, ToolHost, UnavailableArchive}};
use anyhow::{Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::{path::{Path, PathBuf}, sync::Arc, time::{Duration, Instant}};
struct Host;
#[async_trait]
impl ToolHost for Host {
    fn speak(&self, _: &str, _: &str, _: &agent::delivery::DeliveryPermit) -> Result<u64> { anyhow::bail!("lab does not speak") }
}
fn identity(pid: i32) -> Option<(u64,u64)> {
    let mut b:libc::proc_bsdinfo=unsafe{std::mem::zeroed()};
    let size=std::mem::size_of_val(&b) as i32;
    let n=unsafe{libc::proc_pidinfo(pid,libc::PROC_PIDTBSDINFO,0,&mut b as *mut _ as *mut _,size)};
    (n==size && b.pbi_status!=5).then_some((b.pbi_start_tvsec,b.pbi_start_tvusec))
}
fn wait_file(path: &Path) -> Result<String> {
    let began=Instant::now();loop {
        if let Ok(s)=std::fs::read_to_string(path) { if !s.is_empty() { return Ok(s); } }
        anyhow::ensure!(began.elapsed()<Duration::from_secs(15),"missing {}",path.display());
        std::thread::sleep(Duration::from_millis(20));
    }
}
#[test]
#[ignore = "only run as an isolated subprocess by lifetime tests"]
fn lab_supervisor() -> Result<()> {
    let dir=PathBuf::from(std::env::var_os("HEIMDALL_LIFETIME_LAB_DIR").context("lab directory required")?);
    let rt=tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let gate=Arc::new(agent::delivery::DeliveryGate::new(true));
        let server=McpServer::start(Arc::new(Host),Arc::new(UnavailableArchive),gate.clone()).await?;
        let runtime=std::env::var_os("HEIMDALL_LIFETIME_PI_RUNTIME").map(PathBuf::from);
        // Fail locally if a custom command accidentally becomes model inference.
        let provider_requests=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let listener=tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let provider_url=format!("http://{}/v1",listener.local_addr()?);
        let calls=provider_requests.clone();
        tokio::spawn(async move {
            let router=axum::Router::new().fallback(move || { let calls=calls.clone(); async move {
                calls.fetch_add(1,std::sync::atomic::Ordering::SeqCst);
                (axum::http::StatusCode::SERVICE_UNAVAILABLE,"Lab forbids provider requests")
            }});
            let _=axum::serve(listener,router).await;
        });
        let mut extension_hash=None;
        let mut config=if let Some(root)=&runtime {
            let runtime=agent::runtime::Runtime::at(root.clone());
            let cfg_dir=agent::runtime::configure(&dir,102400)?;
            let models=cfg_dir.join("models.json");
            let mut registry:Value=serde_json::from_slice(&std::fs::read(&models)?)?;
            registry["providers"][agent::runtime::PROVIDER]["baseUrl"]=json!(provider_url);
            agent::runtime::atomic_write(&models,&serde_json::to_vec(&registry)?)?;
            let mut c=PiConfig::owned(&runtime,&dir,cfg_dir,server.url.clone(),server.token.clone());
            c.engine=None;c.working_dir=dir.clone();c
        } else {
            let script=dir.join("fixture.py");
            std::fs::write(&script,format!("#!/usr/bin/python3\nimport subprocess,time\nc=subprocess.Popen(['/usr/bin/python3',{}],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\nopen('python.pid','w').write(str(c.pid))\n{}",serde_json::to_string(&dir.join("escape.py").to_string_lossy())?,include_str!("fixtures/mock-adapter.py")))?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&script,std::fs::Permissions::from_mode(0o700))?;
            PiConfig::fixture(script,dir.join("session.jsonl"),dir.clone())
        };
        // Production bridge; the lab then runs a detaching command through Pi's own bash executor (RPC `bash`,
        // the bundled code path a model's bash tool uses), so no provider request is needed.
        if let Some(root)=&runtime {
            let original=std::env::var_os("HEIMDALL_LIFETIME_EXTENSION").map(PathBuf::from)
                .unwrap_or_else(||root.join("heimdall.ts"));
            use sha2::Digest;
            extension_hash=Some(format!("{:x}",sha2::Sha256::digest(std::fs::read(&original)?)));
            let extension=dir.join("lifetime-probe.ts");
            let production=serde_json::to_string(&original.to_string_lossy())?;
            std::fs::write(&extension,format!("import production from {production};\nprocess.env.HEIMDALL_PARENT_PID='{}';\nexport default function(pi) {{ production(pi); }}\n",std::process::id()))?;
            config.extension=Some(extension);
            // The old production extension receives the same original parent env.
            // Current Rust no longer relies on it; it is solely baseline evidence.
        }
        let mut pi=Pi::launch_config_permitted(config,Some(gate.permit()?))?;
        let state=pi.state().await?;
        if runtime.is_some() { pi.query(json!({"type":"prompt","message":"/heimdall_bridge_status"})).await?;
            let entries=pi.entries(None).await?;
            anyhow::ensure!(entries["entries"].as_array().is_some_and(|a|a.iter().any(|e|e["customType"]=="heimdall_bridge"&&e["data"]["connected"]==true)),"production bridge not connected");
            pi.query(json!({"type":"bash","command":"/usr/bin/python3 escape.py >/dev/null 2>&1 & echo $! > python.pid","excludeFromContext":true})).await?;
        }
        std::fs::write(dir.join("ready.json"),serde_json::to_vec(&json!({"pi_pid":pi.pid(),"state":state,"supervisor_pid":std::process::id(),"production_extension":runtime.is_some(),"production_extension_sha256":extension_hash,"provider_requests":provider_requests.load(std::sync::atomic::Ordering::SeqCst)}))?)?;
        loop { tokio::time::sleep(Duration::from_millis(100)).await;
            if dir.join("inspect").exists() {
                let mut state=pi.state().await?;
                state["lab_provider_requests"]=json!(provider_requests.load(std::sync::atomic::Ordering::SeqCst));
                std::fs::write(dir.join("healthy.json"),serde_json::to_vec(&state)?)?;
                std::fs::remove_file(dir.join("inspect"))?;
            }
        }
        #[allow(unreachable_code)] Ok::<_,anyhow::Error>(())
    })
}
fn scenario(runtime: Option<&Path>, hold: Duration, pi_exit_first: bool) -> Result<Value> {
    let dir=tempfile::tempdir()?;
    std::fs::write(dir.path().join("escape.py"),include_str!("fixtures/escaping-tool.py"))?;
    let mut unrelated=std::process::Command::new("/bin/sleep").arg("60").spawn()?;
    let unrelated_id=identity(unrelated.id() as i32).context("unrelated process birth unavailable")?;
    let mut cmd=std::process::Command::new(std::env::current_exe()?);
    cmd.args(["--exact","lab_supervisor","--ignored","--nocapture"])
        .env("HEIMDALL_LIFETIME_LAB_DIR",dir.path())
        .stdout(std::fs::File::create(dir.path().join("supervisor.log"))?).stderr(std::process::Stdio::inherit());
    if let Some(root)=runtime {cmd.env("HEIMDALL_LIFETIME_PI_RUNTIME",root);}
    let mut supervisor=cmd.spawn()?;
    let mut tracked=vec![];
    let result=(|| {
        let ready:Value=serde_json::from_str(&wait_file(&dir.path().join("ready.json"))?)?;
        let pi=ready["pi_pid"].as_i64().context("missing Pi pid")? as i32;
        let setsid=wait_file(&dir.path().join("setsid.pid"))?.trim().parse::<i32>()?;
        let double=wait_file(&dir.path().join("double.pid"))?.trim().parse::<i32>()?;
        let python=wait_file(&dir.path().join("python.pid"))?.trim().parse::<i32>()?;
        let groups=dir.path().join("tool-groups");
        let owner_file=std::fs::read_dir(&groups)?.next().context("no generation")??.path().join("generation.owner");
        let receipt:Value=serde_json::from_slice(&std::fs::read(&owner_file)?)?;
        let guardian=receipt["root"]["pid"].as_i64().context("no guardian")? as i32;
        let owned:Vec<_>=[pi,setsid,double,python,guardian].into_iter().map(|pid|(pid,identity(pid))).collect();
        tracked=owned.clone();
        let began=Instant::now();
        while began.elapsed()<hold {
            std::thread::sleep(Duration::from_millis(100));
            anyhow::ensure!(identity(pi)==owned[0].1,"healthy Pi died after {:.3} seconds",began.elapsed().as_secs_f64());
        }
        anyhow::ensure!(supervisor.try_wait()?.is_none(),"supervisor exited while healthy");
        anyhow::ensure!(owned.iter().all(|(pid,id)|id.is_some()&&identity(*pid)==*id),"healthy generation died");
        std::fs::write(dir.path().join("inspect"),b"")?;
        let healthy:Value=serde_json::from_str(&wait_file(&dir.path().join("healthy.json"))?)?;
        anyhow::ensure!(ready["provider_requests"]==0&&healthy["lab_provider_requests"]==0,"lab attempted provider inference");
        if pi_exit_first {
            // Guardian must still supervise EOF after Pi exited while detached
            // tools hold their own lifetimes. Parent deliberately does not drain.
            anyhow::ensure!(identity(pi)==owned[0].1,"Pi birth changed");
            unsafe {libc::kill(pi,libc::SIGKILL);}
            wait_file(&owner_file.parent().unwrap().join("exit.status"))?;
            anyhow::ensure!(identity(guardian)==owned.last().unwrap().1,"guardian exited before cleanup");
            anyhow::ensure!(identity(setsid)==owned[1].1&&identity(double)==owned[2].1,"detached tools not alive for EOF test");
        }
        supervisor.kill()?;supervisor.wait()?;
        let killed=Instant::now();
        while owned.iter().any(|(pid,id)|identity(*pid)==*id) {
            anyhow::ensure!(killed.elapsed()<Duration::from_secs(5),"owned descendants survived supervisor SIGKILL");
            std::thread::sleep(Duration::from_millis(20));
        }
        anyhow::ensure!(identity(unrelated.id() as i32)==Some(unrelated_id),"unrelated process touched");
        anyhow::ensure!(owner_file.parent().unwrap().join("generation.retired").exists(),"missing durable retirement");
        Ok(json!({"pi_version":runtime.map(|_|agent::runtime::pi_version()),"production_extension":runtime.is_some(),"healthy_seconds":began.elapsed().as_secs_f64(),"supervisor_kill_cleanup_seconds":killed.elapsed().as_secs_f64(),"owned_pids":owned.iter().map(|x|x.0).collect::<Vec<_>>(),"survivors":0,"unrelated_untouched":true,"retirement_durable":true,"provider_requests":healthy["lab_provider_requests"],"provider_tripwire":true,"production_extension_sha256":ready["production_extension_sha256"],"cleanup_limited":receipt["cleanup_limited"],"node_version":runtime.map(|_|agent::runtime::node_version()),"guardian_executable":std::env::var("HEIMDALL_TEST_GUARDIAN_EXECUTABLE").ok(),"date_unix_ms":agent::now_ms()}))
    })();
    if let Some(path)=std::env::var_os("HEIMDALL_LIFETIME_EVIDENCE").map(PathBuf::from) {
        for (file,suffix) in [("ready.json","ready.json"),("healthy.json","state.json"),("supervisor.log","supervisor.log")] {
            if let Ok(bytes)=std::fs::read(dir.path().join(file)) { std::fs::write(path.with_extension(suffix),bytes)?; }
        }
        if let Ok(entries)=std::fs::read_dir(dir.path().join("tool-groups")) {for e in entries.flatten() {
            if let Ok(bytes)=std::fs::read(e.path().join("generation.owner")) { std::fs::write(path.with_extension("owner.json"),bytes)?; }
        }}
    }
    let _=supervisor.kill();let _=supervisor.wait();
    // Failure cleanup never signals a group or a PID whose birth changed.
    if result.is_err() { for (pid,id) in tracked { if id.is_some() && identity(pid)==id { unsafe{libc::kill(pid,libc::SIGKILL);} } } }
    unrelated.kill()?;unrelated.wait()?;
    result
}
#[test]
fn supervisor_sigkill_retires_detached_children_without_next_launch() -> Result<()> {
    scenario(None,Duration::from_millis(100),false)?;Ok(())
}
#[test]
#[ignore = "explicit production pinned-Pi extension and real guardian lab; 30 s"]
fn production_extension_survives_30_seconds_and_supervisor_death_retires_every_owned_child() -> Result<()> {
    let runtime=PathBuf::from(std::env::var_os("HEIMDALL_LIFETIME_PI_RUNTIME").context("runtime required")?);
    let evidence=PathBuf::from(std::env::var_os("HEIMDALL_LIFETIME_EVIDENCE").context("evidence required")?);
    anyhow::ensure!(std::env::var_os("HEIMDALL_TEST_GUARDIAN_EXECUTABLE").is_some(),"real main executable required");
    agent::runtime::Runtime::at(runtime.clone()).verify()?;
    let result=scenario(Some(&runtime),Duration::from_secs(std::env::var("HEIMDALL_LIFETIME_HOLD_SECONDS").ok().and_then(|s|s.parse().ok()).unwrap_or(30)),false);
    std::fs::write(evidence,serde_json::to_vec_pretty(&match &result {Ok(v)=>v.clone(),Err(e)=>json!({"error":format!("{e:#}")})})?)?;
    result?;Ok(())
}

#[test]
fn supervisor_sigkill_after_pi_exit_retires_the_waiting_guardian_and_detached_tools() -> Result<()> {
    scenario(None,Duration::from_millis(100),true)?;Ok(())
}
