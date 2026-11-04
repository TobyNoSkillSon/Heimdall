use super::*;
use ui::HealthCue;
use std::{net::TcpListener, os::unix::fs::PermissionsExt};
use tempfile::TempDir;

// Like Splash, the launcher execs Python with -m server.server at the same PID.
const LAUNCHER: &str = r#"#!/usr/bin/env python3
import os, pathlib, sys
libexec=pathlib.Path(__file__).resolve().parent.parent/'libexec'
os.chdir(libexec)
args=sys.argv[2:]
def opt(name): return args[args.index(name)+1]
cache=opt('--cache-dir')
argv=[sys.executable,'-u','-P','-m','server.server','/resolved model dir',
      '--tokenizer','/resolved model dir/tokenizer','--model',opt('--model'),
      '--binary',str(libexec/'engine/splash'),'--port',opt('--port'),
      '--max-memory=51539607552','--max-cache-disk=107374182400','--persistent-cache',
      '--cache-dir='+cache,'--served-model-name='+opt('--served-model-name')]
os.environ['PYTHONPATH']=str(libexec)
os.execv(sys.executable,argv)
"#;
const SERVER: &str = r#"import http.server, json, pathlib, signal, subprocess, sys
args=sys.argv
def opt(name):
 for i,arg in enumerate(args):
  if arg==name: return args[i+1]
  if arg.startswith(name+'='): return arg[len(name)+1:]
 raise ValueError(name)
root=pathlib.Path(opt('--cache-dir')).parent
(root/'starts').open('a').write('start\n')
child=subprocess.Popen([opt('--binary'),'serve-native','/resolved model dir','auto',
                        '51539607552','107374182400','--cache-dir',opt('--cache-dir')])
(root/'child.pid').write_text(str(child.pid))
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args): pass
 def do_GET(self):
  body={'status':'ready'}; code=200
  if self.path=='/status': body={'weights':{'released':(root/'idle').exists()},'disk':{'used_bytes':4096,'capacity_bytes':107374182400},'cache':{'hits':0}}
  if self.path=='/v1/models': body={'data':[{'id':opt('--served-model-name'),'max_model_len':262144}]}
  if self.path=='/ready' and (root/'unready').exists(): code=503
  self.send_response(code); self.end_headers(); self.wfile.write(json.dumps(body).encode())
if (root/'ignore-term').exists(): signal.signal(signal.SIGTERM,signal.SIG_IGN)
else: signal.signal(signal.SIGTERM,lambda *_:sys.exit(0))
http.server.HTTPServer(('127.0.0.1',int(opt('--port'))),Handler).serve_forever()
"#;
const NATIVE: &str = r#"#!/usr/bin/env python3
import signal,time
signal.signal(signal.SIGTERM,signal.SIG_IGN)
time.sleep(300)
"#;
fn fixture() -> (TempDir, Config, Paths) {
    let t = TempDir::new().unwrap();
    fs::create_dir(t.path().join("bin")).unwrap();
    let executable = t.path().join("bin/splash");
    fs::create_dir_all(t.path().join("libexec/server")).unwrap();
    fs::create_dir_all(t.path().join("libexec/engine")).unwrap();
    fs::write(t.path().join("libexec/server/__init__.py"), "").unwrap();
    fs::write(t.path().join("libexec/server/server.py"), SERVER).unwrap();
    let native = t.path().join("libexec/engine/splash");
    fs::write(&native, NATIVE).unwrap();
    fs::write(&executable, LAUNCHER).unwrap();
    for path in [&executable, &native] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut config = Config::bundled();
    config.executable = executable;
    config.port = port;
    let paths = Paths::new(
        &t.path().join("Library/Application Support/Heimdall"),
        t.path().join("logs/splash.log"),
    );
    (t, config, paths)
}
fn ready(config: &Config) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while probe(config).is_err() {
        assert!(Instant::now() < deadline, "fake did not start");
        thread::sleep(Duration::from_millis(25));
    }
}
fn worker(config: Config, paths: Paths) -> (Worker, Receiver<UiUpdate>) {
    let (tx, rx) = crossbeam_channel::unbounded();
    let timing = Timing {
        stop: Duration::from_millis(250),
        startup_limit: Duration::from_millis(100),
        ..Timing::default()
    };
    (Worker::new(config, paths, tx, timing), rx)
}
#[test]
fn adoption_idle_metrics_and_clean_stop_preserve_cache() {
    let (_t, config, paths) = fixture();
    let mut external = launch(&config, &config.default_model, &config.models[0].quant, &paths).unwrap();
    ready(&config);
    let all = processes().unwrap();
    let owner = owner_in(&paths, &all).unwrap().unwrap();
    assert_eq!(owner.pid, external.process.pid);
    assert!(owner.command.contains(" -m server.server "));
    assert!(owner.command.contains("--cache-dir="));
    assert!(all
        .iter()
        .any(|p| p.parent == owner.pid && native_cache(p, &paths)));
    let sentinel = paths.cache.join("restore-point");
    fs::write(&sentinel, "keep").unwrap();
    let (mut w, rx) = worker(config.clone(), paths.clone());
    w.set(true, config.default_model.clone());
    assert_eq!(
        w.running.as_ref().unwrap().process.pid,
        external.process.pid
    );
    assert_eq!(
        fs::read_to_string(paths.pid.parent().unwrap().join("starts"))
            .unwrap()
            .lines()
            .count(),
        1
    );
    w.tick();
    assert!(rx.try_iter().any(|u| matches!(
        u,
        UiUpdate::Engine {
            state: EngineState::Ready,
            context_limit: Some(262144),
            cache_used_bytes: Some(4096),
            cache_limit_bytes: Some(107374182400),
            memory_limit_bytes: Some(51539607552),
            ..
        }
    )));
    fs::write(paths.pid.parent().unwrap().join("idle"), "").unwrap();
    w.tick();
    assert_eq!(w.state, EngineState::Idle);
    w.set(false, config.default_model.clone());
    assert!(!external.alive().unwrap());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    assert!(!paths.pid.exists());
    assert_eq!(w.state, EngineState::Off);
}
#[test]
fn crash_recovers_once_then_red_without_restart_loop() {
    let (_t, config, paths) = fixture();
    let (mut w, rx) = worker(config.clone(), paths.clone());
    w.set(true, config.default_model.clone());
    ready(&config);
    w.tick();
    signal(&w.running.as_ref().unwrap().process, libc::SIGKILL).unwrap();
    thread::sleep(Duration::from_millis(50));
    w.tick();
    ready(&config);
    w.tick();
    let updates: Vec<_> = rx.try_iter().collect();
    assert!(updates
        .iter()
        .any(|u| matches!(u, UiUpdate::Health(HealthCue::Restarting { .. }))));
    assert!(updates
        .iter()
        .any(|u| matches!(u, UiUpdate::Health(HealthCue::Recovered { .. }))));
    signal(&w.running.as_ref().unwrap().process, libc::SIGKILL).unwrap();
    thread::sleep(Duration::from_millis(50));
    w.tick();
    assert!(matches!(w.state, EngineState::Error(_)));
    for _ in 0..3 {
        w.tick();
    }
    assert_eq!(
        fs::read_to_string(paths.pid.parent().unwrap().join("starts"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    assert!(rx.try_iter().any(|u| matches!(
        u,
        UiUpdate::Health(HealthCue::Problem {
            control: HealthControl::OpenMenu,
            ..
        })
    )));
}
#[test]
fn persistent_unready_is_bounded_and_requires_explicit_retry() {
    let (_t, config, paths) = fixture();
    let (mut w, _) = worker(config.clone(), paths.clone());
    w.set(true, config.default_model.clone());
    ready(&config);
    w.tick();
    fs::write(paths.pid.parent().unwrap().join("unready"), "").unwrap();
    w.tick();
    assert_eq!(w.retries, 0); // tolerate one transient probe
    w.tick();
    assert_eq!(w.retries, 1);
    thread::sleep(Duration::from_millis(150));
    w.tick();
    assert!(matches!(w.state, EngineState::Error(_)));
    assert!(w.running.is_none());
    fs::remove_file(paths.pid.parent().unwrap().join("unready")).unwrap();
    w.set(true, config.default_model.clone());
    ready(&config);
    w.tick();
    assert_eq!(w.state, EngineState::Ready);
    w.set(false, config.default_model.clone());
}
#[test]
fn explicit_retry_keeps_red_until_ready_or_idle_then_says_ok_once() {
    for idle in [false, true] {
        let (_t, config, paths) = fixture();
        let (mut w, rx) = worker(config.clone(), paths.clone());
        w.fail(anyhow::anyhow!("test engine unavailable"));
        let cues: Vec<_> = rx.try_iter().filter_map(|u| match u {
            UiUpdate::Health(cue) => Some(cue), _ => None,
        }).collect();
        assert!(matches!(cues.as_slice(), [HealthCue::Problem { key, .. }] if key == KEY));

        w.set(true, config.default_model.clone());
        assert!(w.health.holds(KEY), "Retry is not proof of recovery");
        assert!(!rx.try_iter().any(|u| matches!(u, UiUpdate::Health(_))), "no premature Clear or OK");
        ready(&config);
        if idle { fs::write(paths.pid.parent().unwrap().join("idle"), "").unwrap(); }
        w.tick();
        assert_eq!(w.state, if idle { EngineState::Idle } else { EngineState::Ready });
        let cues: Vec<_> = rx.try_iter().filter_map(|u| match u {
            UiUpdate::Health(cue) => Some(cue), _ => None,
        }).collect();
        assert_eq!(cues, [HealthCue::Recovered {
            key: KEY.into(), part: Part::InferenceEngine, label: "Inference engine OK".into(),
        }]);
        assert!(!w.health.holds(KEY));
        w.tick();
        w.set(false, config.default_model.clone());
        assert!(!rx.try_iter().any(|u| matches!(u, UiUpdate::Health(_))), "no repeated OK or needless Clear");
    }
}

#[test]
fn failed_retry_keeps_red_and_explicit_off_clears_without_ok() {
    let (_t, mut config, paths) = fixture();
    config.executable = paths.pid.parent().unwrap().join("missing-launcher");
    let (mut w, rx) = worker(config.clone(), paths);
    w.set(true, config.default_model.clone());
    assert!(rx.try_iter().any(|u| matches!(u, UiUpdate::Health(HealthCue::Problem { .. }))));
    w.set(true, config.default_model.clone());
    assert!(matches!(w.state, EngineState::Error(_)));
    assert!(w.health.holds(KEY));
    assert!(!rx.try_iter().any(|u| matches!(u, UiUpdate::Health(_))), "failed retry never clears or says OK");
    w.set(false, config.default_model.clone());
    let cues: Vec<_> = rx.try_iter().filter_map(|u| match u {
        UiUpdate::Health(cue) => Some(cue), _ => None,
    }).collect();
    assert_eq!(cues, [HealthCue::Clear { key: KEY.into() }]);
    assert_eq!(w.state, EngineState::Off);
    assert!(!w.health.holds(KEY));
    w.set(false, config.default_model.clone());
    assert!(!rx.try_iter().any(|u| matches!(u, UiUpdate::Health(_))));
}

#[test]
fn termination_escalates_and_reaps_native_descendant() {
    let (_t, config, paths) = fixture();
    fs::create_dir_all(paths.pid.parent().unwrap()).unwrap();
    fs::write(paths.pid.parent().unwrap().join("ignore-term"), "").unwrap();
    let mut r = launch(&config, &config.default_model, &config.models[0].quant, &paths).unwrap();
    ready(&config);
    let child: i32 = fs::read_to_string(paths.pid.parent().unwrap().join("child.pid"))
        .unwrap()
        .parse()
        .unwrap();
    let started = Instant::now();
    stop(&mut r, &paths, Duration::from_millis(250)).unwrap();
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(!r.alive().unwrap());
    let deadline = Instant::now() + Duration::from_secs(2);
    while processes().unwrap().iter().any(|p| p.pid == child) {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(25));
    }
}
#[test]
fn stale_pid_lock_and_foreign_listener_never_create_an_engine() {
    let (_t, config, paths) = fixture();
    fs::create_dir_all(paths.pid.parent().unwrap()).unwrap();
    fs::write(&paths.pid, std::process::id().to_string()).unwrap();
    assert!(launch(&config, &config.default_model, &config.models[0].quant, &paths).is_err());
    fs::write(&paths.pid, "2147483647").unwrap();
    let lock = Lock::acquire(&paths.lock).unwrap();
    assert!(launch(&config, &config.default_model, &config.models[0].quant, &paths).is_err());
    drop(lock);
    let listener = TcpListener::bind(("127.0.0.1", config.port)).unwrap();
    assert!(launch(&config, &config.default_model, &config.models[0].quant, &paths).is_err());
    drop(listener);
    let mut r = launch(&config, &config.default_model, &config.models[0].quant, &paths).unwrap();
    ready(&config);
    stop(&mut r, &paths, Duration::from_millis(250)).unwrap();
}
#[test]
fn settings_roundtrip_and_model_validation() {
    let t = TempDir::new().unwrap();
    let mut settings = crate::settings::Settings::default();
    // The local engine's model is the agent's; an API selection keeps the engine off (normalize_agent_model).
    settings.agent_model = agent::provider::AgentModel::Local;
    settings.engine_on = true;
    settings.engine_model = "another-model".into();
    let p = t.path().join("settings.json");
    settings.save(&p).unwrap();
    assert_eq!(crate::settings::Settings::load(&p).unwrap(), settings);
    assert!(Config::bundled().model("another-model").is_err());
    let config = Config::load(t.path()).unwrap();
    assert_eq!(
        config
            .args(&config.default_model, &config.models[0].quant, &t.path().join("cache"))
            .unwrap(),
        vec![
            "serve",
            "--model",
            "huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q4_K_XL",
            "--port",
            "8100",
            "--served-model-name",
            "qwen3.8-27b",
            "--persistent-cache",
            "--max-cache-disk",
            "100G",
            "--cache-dir",
            &t.path().join("cache").display().to_string(),
            "--max-memory",
            "48G"
        ]
    );
}

#[test]
fn pipeline_actions_leave_listening_off_and_reject_missing_models() {
    let t = TempDir::new().unwrap();
    let settings = crate::settings::Settings {
        paused: true,
        screen_off: true,
        ..Default::default()
    };
    let mut pipeline = crate::pipeline::Pipeline::new(
        t.path().join("data"),
        t.path().join("models"),
        settings,
        None,
        true,
    )
    .unwrap();
    pipeline.action(ui::UiAction::SetEngine(true)).unwrap();
    let state = &pipeline.settings;
    assert!(state.engine_on && state.paused && state.screen_off && state.dry_run);
    pipeline
        .action(ui::UiAction::SelectEngineModel("qwen3.8-27b".into()))
        .unwrap();
    assert!(pipeline
        .action(ui::UiAction::SelectEngineModel("missing".into()))
        .is_err());
    // Its weights are not on this Mac (no fixture here): nothing is picked, nothing downloads in a test.
    assert_eq!(pipeline.settings.engine_model, "");
    pipeline.action(ui::UiAction::SetEngine(false)).unwrap();
    assert!(!pipeline.settings.engine_on);
}

#[test]
fn a_broken_override_falls_back_to_the_bundled_default() {
    let t = TempDir::new().unwrap();
    std::fs::create_dir_all(t.path().join("splash")).unwrap();
    assert!(
        Config::effective(t.path()).1.is_none(),
        "no override: the default, nothing to note"
    );
    std::fs::write(t.path().join("splash/engine.json"), br#"{"executable":"/x","port":8100,"max_memory":"96G","max_cache_disk":"100G","default_model":"m","models":[{"id":"m","label":"M","quant":"q","source":"s"}]}"#).unwrap();
    let (config, note) = Config::effective(t.path());
    assert!(
        Config::load(t.path()).is_err(),
        "a lifted memory ceiling is refused"
    );
    assert_eq!(config.default_model, Config::bundled().default_model);
    assert!(note.is_some_and(|n| n.contains("bundled default")));
    std::fs::write(t.path().join("splash/engine.json"), b"{not json").unwrap();
    assert_eq!(Config::effective(t.path()).0.port, Config::bundled().port);
}

#[test]
fn real_argv_options_and_parent_ownership() {
    let (_t, config, paths) = fixture();
    let cache = paths.cache.display().to_string();
    for separator in ["=", " "] {
        for tail in ["", " --persistent-cache"] {
            let command = format!("/opt/homebrew/Cellar/splash/1.3.1/libexec/python/bin/python3 -u -P -m server.server /resolved model dir --binary /opt/homebrew/Cellar/splash/1.3.1/libexec/engine/splash --model {} --served-model-name{}{} --port{}{} --cache-dir{}{}{}",
                config.models[0].source, separator, config.default_model,
                separator, config.port, separator, cache, tail);
            let owner = Process {
                pid: 61050,
                parent: 1,
                birth: "server".into(),
                command,
            };
            let mut native = Process {
                pid: 12461, parent: owner.pid, birth: "native".into(),
                command: format!("/opt/homebrew/Cellar/splash/1.3.1/libexec/engine/splash serve-native /resolved model dir auto 51539607552 107374182400 --cache-dir {cache}"),
            };
            assert!(owns_cache(&owner, &paths));
            assert!(!owns_cache(&native, &paths));
            assert!(matches_configuration(&owner, &config, &config.default_model, &config.models[0].quant).unwrap());
            assert_eq!(
                owner_in(&paths, &[owner.clone(), native.clone()])
                    .unwrap()
                    .unwrap()
                    .pid,
                owner.pid
            );
            native.parent = 1; // gone parent, including beside a healthy server
            assert!(owner_in(&paths, &[owner.clone(), native.clone()]).is_err());
            native.parent = 999; // live parent that is not the server
            let foreign = Process {
                pid: 999,
                command: "/usr/bin/python3 unrelated.py".into(),
                ..owner.clone()
            };
            assert!(owner_in(&paths, &[foreign, native]).is_err());
        }
    }
    assert!(!has_option(
        "--cache-dir=/cache suffix",
        "--cache-dir",
        "/cache"
    ));
    assert!(!has_option(
        "--cache-dir=/cache-other",
        "--cache-dir",
        "/cache"
    ));
    assert!(!has_option("--port=81000", "--port", "8100"));
    assert!(!has_option("x--port=8100", "--port", "8100"));
}

#[test]
fn true_native_orphan_blocks_launch() {
    let (t, config, paths) = fixture();
    let mut child = Command::new(t.path().join("libexec/engine/splash"))
        .args([
            "serve-native",
            "/resolved model dir",
            "auto",
            "51539607552",
            "107374182400",
            "--cache-dir",
        ])
        .arg(&paths.cache)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    let error = loop {
        let result = find_owner(&paths);
        if let Err(error) = result {
            break error;
        }
        assert!(Instant::now() < deadline, "native fixture did not exec");
        thread::sleep(Duration::from_millis(25));
    };
    let result = launch(&config, &config.default_model, &config.models[0].quant, &paths);
    child.kill().unwrap();
    child.wait().unwrap();
    assert!(error.to_string().contains("orphaned native"));
    assert!(result.is_err());
}

// Explicit opt-in: snapshots and configuration checks only. Never call launch,
// Worker/Manager, stop, signal, or anything that writes the real profile.
#[test]
#[ignore = "read-only check of the live Splash server; requires HEIMDALL_VERIFY_SPLASH_PID"]
fn live_server_is_owned_and_adoptable_read_only() {
    let expected: i32 = std::env::var("HEIMDALL_VERIFY_SPLASH_PID")
        .unwrap()
        .parse()
        .unwrap();
    let data = PathBuf::from(std::env::var_os("HOME").unwrap())
        .join("Library/Application Support/Heimdall");
    let paths = Paths::new(&data, data.join("unused-log"));
    let config = Config::load(&data).unwrap();
    let all = processes().unwrap();
    let owner = owner_in(&paths, &all).unwrap().expect("live owner");
    assert_eq!(owner.pid, expected);
    assert_eq!(
        fs::read_to_string(&paths.pid).unwrap().trim(),
        expected.to_string()
    );
    assert!(matches_configuration(&owner, &config, &config.default_model, &config.models[0].quant).unwrap());
    let children: Vec<_> = all
        .iter()
        .filter(|p| p.parent == owner.pid && native_cache(p, &paths))
        .collect();
    assert!(!children.is_empty(), "missing native child");
    println!(
        "owned/adoptable server pid={}, native children={:?}, configuration matches",
        owner.pid,
        children.iter().map(|p| p.pid).collect::<Vec<_>>()
    );
}

#[test]
fn first_profile_adopts_and_persists_verified_owner_without_start_or_stop() {
    let (_t,config,paths)=fixture();
    let data=paths.pid.parent().unwrap().parent().unwrap();
    fs::create_dir_all(data.join("splash")).unwrap();
    fs::write(data.join("splash/engine.json"),serde_json::to_vec(&serde_json::json!({
        "executable":config.executable,"port":config.port,"max_memory":"48G","max_cache_disk":"100G",
        "default_model":config.default_model,"models":config.models.iter().map(|m|serde_json::json!({"id":m.id,"label":m.label,"quant":m.quant,"source":m.source})).collect::<Vec<_>>()
    })).unwrap()).unwrap();
    let mut external=launch(&config,&config.default_model,&config.models[0].quant,&paths).unwrap();
    ready(&config);
    let settings_path=data.join("settings.json");
    // The default agent model is Claude Haiku 5.5 on OpenCode Go: one model at a time, so a new profile does not
    // adopt the local engine. A profile on the local model adopts the verified running engine as before.
    for contents in [None,Some(b"{}".as_slice())] {
        if let Some(bytes)=contents {fs::write(&settings_path,bytes).unwrap();}
        let settings=crate::settings::Settings::load_and_migrate(&settings_path).unwrap();
        assert!(!settings.engine_on && !settings.agent_model.is_local());
    }
    for contents in [br#"{"agent_model":"local"}"#.as_slice()] {
        fs::write(&settings_path,contents).unwrap();
        let settings=crate::settings::Settings::load_and_migrate(&settings_path).unwrap();
        assert!(settings.engine_on);
        assert_eq!(settings.engine_model,config.default_model);
        assert!(serde_json::from_slice::<Value>(&fs::read(&settings_path).unwrap()).unwrap()["engine_on"]==true);
    }
    let (mut w,_)=worker(config.clone(),paths.clone());
    w.startup(true,config.default_model.clone());
    assert!(w.running.as_ref().unwrap().child.is_none());
    w.shutdown_owned();
    drop(w);
    assert!(external.alive().unwrap());
    let (mut w,_)=worker(config.clone(),paths.clone());
    w.startup(false,config.default_model.clone());
    drop(w);
    assert!(external.alive().unwrap());
    fs::write(&settings_path,b"{\"engine_on\":false}").unwrap();
    assert!(!crate::settings::Settings::load_and_migrate(&settings_path).unwrap().engine_on);
    fs::remove_file(&settings_path).unwrap();
    fs::remove_file(&paths.pid).unwrap();
    assert!(!crate::settings::Settings::load_and_migrate(&settings_path).unwrap().engine_on);
    assert!(external.alive().unwrap(),"unverified discovery must never stop the process");
    fs::write(&paths.pid,external.process.pid.to_string()).unwrap();
    stop(&mut external,&paths,Duration::from_millis(250)).unwrap();
}

#[test]
fn adopted_health_failure_reports_without_killing_and_off_still_stops_tree() {
    let (_t,config,paths)=fixture();
    let mut external=launch(&config,&config.default_model,&config.models[0].quant,&paths).unwrap();
    ready(&config);
    let child:i32=fs::read_to_string(paths.pid.parent().unwrap().join("child.pid")).unwrap().parse().unwrap();
    let (mut w,rx)=worker(config.clone(),paths.clone());
    w.startup(true,config.default_model.clone());w.tick();
    fs::write(paths.pid.parent().unwrap().join("unready"),"").unwrap();
    w.tick();w.tick();
    assert!(matches!(w.state,EngineState::Error(_)));
    assert_eq!(w.retries,0);
    assert!(external.alive().unwrap());
    assert!(rx.try_iter().any(|u|matches!(u,UiUpdate::Health(HealthCue::Problem{..}))));
    w.shutdown_owned();assert!(external.alive().unwrap());
    w.set(false,config.default_model.clone());
    assert!(!external.alive().unwrap());
    assert!(!processes().unwrap().iter().any(|p|p.pid==child));
    assert_eq!(Timing::default().stop,Duration::from_secs(30));
}

#[test]
fn owned_shutdown_stops_only_the_engine_heimdall_started() {
    let (_t,config,paths)=fixture();
    let (mut w,_)=worker(config.clone(),paths.clone());
    w.startup(true,config.default_model.clone());ready(&config);
    assert!(w.running.as_ref().unwrap().child.is_some());
    let owner=w.running.as_ref().unwrap().process.clone();
    w.shutdown_owned();
    assert!(!same_process(&owner,&processes().unwrap()));
}

/// the failed tile says why in the menu, and a click toggles: on a failed engine it turns it off, the
/// next click starts it again (a click that retried in place left him unable to tell what happened).
#[test]
fn a_failed_engine_shows_its_reason_and_the_tile_toggles_off_then_on_through_pipeline_and_manager() {
    let (t,config,paths)=fixture();
    // Leave the port occupied to provoke Error without starting any engine.
    let foreign=TcpListener::bind(("127.0.0.1",config.port)).unwrap();
    let (tx,requests)=crossbeam_channel::unbounded();
    let (out,events)=crossbeam_channel::unbounded();
    let timing=Timing {starting:Duration::from_millis(20),ready:Duration::from_millis(20),stop:Duration::from_millis(250),..Timing::default()};
    let worker=Worker::new(config.clone(),paths.clone(),out,timing);
    let thread=thread::spawn(move||worker.run(requests));
    let manager=Manager{tx,events:events.clone(),worker:Some(thread)};
    let mut pipeline=crate::pipeline::Pipeline::new(t.path().join("data"),t.path().join("models"),crate::settings::Settings {engine_on:true,engine_model:config.default_model.clone(),paused:true,screen_off:true,..Default::default()},None,true).unwrap();
    // The chosen precision is on disk (a fixture), so the engine may start: missing weights never start Splash.
    crate::inference_engine::models::fixture::install_quant(&crate::inference_engine::models::Roots::for_data(&t.path().join("data")), &config.models[0].source, "incoai/Qwen3.8-27B-DFlash2");
    pipeline.engine_refresh();
    pipeline.attach_test_engine(manager);
    // The actual tile: Tile::toggle on what the tile shows.
    let click=|shown:&EngineState|ui::Tile::Engine.toggle(ui::Tile::Engine.is_on(&ui::ListenState::Paused,&ui::RuntimeSettings::default(),&ui::Settings::default(),shown));
    let wait_state=|expected:EngineState| {
        let deadline=Instant::now()+Duration::from_secs(5);
        loop {
            let update=events.recv_timeout(deadline.saturating_duration_since(Instant::now())).unwrap();
            if matches!(update,UiUpdate::Engine{ref state,..} if *state==expected) {break;}
        }
    };
    // Saved On, explicit: it starts and fails on the taken port, the reason in its own words.
    pipeline.action(ui::UiAction::SetEngine(true)).unwrap();
    let failed=EngineState::Error("Engine port is already occupied".into());
    wait_state(failed.clone());
    assert!(pipeline.settings.engine_on);
    // A click on the failed tile turns it off.
    assert_eq!(click(&failed),ui::UiAction::SetEngine(false));
    pipeline.action(click(&failed)).unwrap();
    wait_state(EngineState::Off);
    assert!(!pipeline.settings.engine_on);
    drop(foreign);
    // The next click starts it again.
    pipeline.action(click(&EngineState::Off)).unwrap();
    wait_state(EngineState::Starting);
    wait_state(EngineState::Ready);
    assert!(pipeline.settings.engine_on);
    assert_eq!(fs::read_to_string(paths.pid.parent().unwrap().join("starts")).unwrap().lines().count(),1);
    pipeline.action(ui::UiAction::SetEngine(false)).unwrap();
    wait_state(EngineState::Off);
}

/// Splash's own status lines say how far a start got: nothing yet (starting), loading, a download's percentage.
#[test]
fn loading_follows_splash_status_lines() {
    assert_eq!(loading_state("=== Heimdall engine start\n"), None, "only starting");
    assert_eq!(loading_state("01:15:02 Loading · huihui-ai/Huihui-Qwen3.8-27B-abliterated-GGUF:UD-Q4_K_XL\n"), Some(EngineState::Loading { percent: None }));
    let log = "01:15:02 Loading · x/y:Q\n01:15:03 Downloading · x/y · y-Q.gguf · 40% of 17.4 GB\n01:15:09 Downloading · x/y · y-Q.gguf · 45% of 17.4 GB\n";
    assert_eq!(loading_state(log), Some(EngineState::Loading { percent: Some(45) }));
    // The download done, the weights load: no percentage (Splash reports none), an honest spinner.
    assert_eq!(loading_state(&format!("{log}01:20:00 Chat template · chatml\n")), Some(EngineState::Loading { percent: None }));
    assert_eq!(failure_reason(&anyhow::anyhow!("engine port is already occupied")), "Engine port is already occupied");
    assert_eq!(failure_reason(&anyhow::anyhow!("")), "Inference engine unavailable");
}

#[test]
fn model_pick_while_off_never_stops_an_external_engine() {
    let (_t,mut config,paths)=fixture();
    let mut other=config.models[0].clone();
    other.id="other-model".into();
    config.models.push(other);
    let mut external=launch(&config,&config.default_model,&config.models[0].quant,&paths).unwrap();
    ready(&config);
    let (mut w,_)=worker(config.clone(),paths.clone());
    w.startup(false,config.default_model.clone());
    w.set(false,"other-model".into());
    assert!(external.alive().unwrap(),"a model pick while Off must not stop the engine");
    assert_eq!(w.model,"other-model");
    drop(w);
    assert!(external.alive().unwrap());
    stop(&mut external,&paths,Duration::from_millis(250)).unwrap();
}

#[test]
fn engine_failures_archive_the_cause_once_until_recovery() {
    let (temp, config, paths) = fixture();
    let (out, _events) = crossbeam_channel::unbounded();
    let mut worker = Worker::new(config, paths, out, Timing::default());
    let session = temp.path().join("archive/session-engine-health");
    worker.journal = Some(heimdall_archive::records::Writer::open(&session).unwrap());
    let journal = heimdall_archive::Journal::open(&session).unwrap();
    worker.fail(anyhow::anyhow!("Splash exited with status 9"));
    worker.fail(anyhow::anyhow!("Splash exited with status 9"));
    worker.journal.as_ref().unwrap().flush().unwrap();
    let rows = journal.records().unwrap();
    assert_eq!(rows.iter().filter(|r| r.payload["reason"] == "Splash exited with status 9").count(), 1);
    worker.publish(EngineState::Ready, None, None);
    worker.fail(anyhow::anyhow!("Splash exited with status 9"));
    worker.journal.as_ref().unwrap().flush().unwrap();
    assert_eq!(journal.records().unwrap().iter().filter(|r| r.payload["reason"] == "Splash exited with status 9").count(), 2);
}

/// A private, lightweight Splash owner for the pipeline's Quit regression.
pub(crate) fn owned_quit_fixture() -> (TempDir, Manager, i32) {
    let (t, config, paths) = fixture();
    let (tx, requests) = crossbeam_channel::unbounded();
    let (mut worker, events) = worker(config.clone(), paths);
    worker.startup(true, config.default_model.clone());
    ready(&config);
    let pid = worker.running.as_ref().unwrap().process.pid;
    let worker = Some(thread::spawn(move || worker.run(requests)));
    (t, Manager { tx, events, worker }, pid)
}

#[test]
fn engine_error_is_written_to_the_existing_splash_log() {
    let (_t, config, paths) = fixture();
    let (mut worker, _) = worker(config, paths.clone());
    worker.fail(anyhow::anyhow!("fixture stop failure"));
    assert!(fs::read_to_string(&paths.log).unwrap().contains("Inference engine: fixture stop failure"));
}

#[test]
fn bundled_server_runs_beside_heimdall_and_owns_its_cache() {
    // The bundled default names heimdall-splash, the Rust server beside Heimdall's own binary (docs/splash-native.md).
    let config = Config::bundled();
    let exe = std::env::current_exe().unwrap();
    assert_eq!(config.executable(), exe.parent().unwrap().join("heimdall-splash"));
    let t = TempDir::new().unwrap();
    let paths = Paths::new(t.path(), t.path().join("splash.log"));
    let cache = paths.cache.display().to_string();
    let p = |command: String| Process { pid: 10, parent: 1, birth: "b".into(), command };
    let server = p(format!("/Applications/Heimdall.app/Contents/MacOS/heimdall-splash serve --model m --port 8100 --cache-dir {cache} --max-memory 48G"));
    assert!(owns_cache(&server, &paths));
    let native = p(format!("/Applications/Heimdall.app/Contents/Resources/splash/engine/splash serve-native /m auto 51539607552 107374182400 --persistent-cache on --cache-dir {cache}"));
    assert!(native_cache(&native, &paths) && !owns_cache(&native, &paths));
    // An absolute override (Homebrew's launcher, the rollback) is used as given.
    let mut homebrew = config.clone();
    homebrew.executable = "/opt/homebrew/bin/splash".into();
    assert_eq!(homebrew.executable(), PathBuf::from("/opt/homebrew/bin/splash"));
}
