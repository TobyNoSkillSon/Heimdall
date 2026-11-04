//! The durable menu/file stop intent and its writer thread.
use super::*;

/// Last explicit menu/file stop intent, independent of live-loop settings saves.
#[derive(serde::Serialize, serde::Deserialize)]
pub(super) struct StopIntent {
    #[serde(default)]
    pub(super) generation: u64,
    pub(super) agent_off: bool,
    pub(super) heimdall_off: bool,
}
/// The generations of the Agent delivery gate and of Listening's own latch. The settings file keeps only `agent`
/// (`delivery_generation`, the delivery epoch); Listening's generation orders intents within one run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IntentGeneration { pub agent: u64, pub listen: u64 }
pub(super) fn stop_snapshot(agent: &agent::delivery::DeliveryGate, listen: &agent::delivery::DeliveryGate) -> (IntentGeneration, bool, bool) {
    let (agent_generation, agent_off, _) = agent.stop_snapshot();
    let (listen_generation, _, listening_off) = listen.stop_snapshot();
    (IntentGeneration { agent: agent_generation, listen: listen_generation }, agent_off, listening_off)
}
pub(super) type IntentReply = std::sync::mpsc::Sender<std::result::Result<(), String>>;
pub(super) enum IntentCommand {
    Persist(Option<IntentReply>),
    Saved(IntentGeneration, IntentReply),
    Flush(IntentReply),
}
#[derive(Default)]
pub(super) struct IntentAcceptance {
    pub(super) closed: bool,
    pub(super) stops: Vec<std::sync::mpsc::Receiver<std::result::Result<(), String>>>,
}
/// Owns all intent-file I/O. Neither callback nor writer waits for the live loop.
pub(super) struct IntentWriter {
    pub(super) tx: Option<std::sync::mpsc::Sender<IntentCommand>>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}
impl IntentWriter {
    pub(super) fn new(path: PathBuf, gate: Arc<agent::delivery::DeliveryGate>, listen: Arc<agent::delivery::DeliveryGate>) -> Result<Self> {
        let (tx, rx) = std::sync::mpsc::channel();
        let thread = std::thread::Builder::new().name("heimdall-stop-intent".into()).spawn(move || {
            let mut durable = None;
            let mut requested = false;
            let current = || IntentGeneration { agent: gate.generation(), listen: listen.generation() };
            let write = || -> Result<IntentGeneration> {
                let (generation, agent_off, heimdall_off) = stop_snapshot(&gate, &listen);
                let intent = StopIntent { generation: generation.agent, agent_off, heimdall_off };
                heimdall_archive::atomic_write(&path, &serde_json::to_vec(&intent)?)?;
                Ok(generation)
            };
            while let Ok(command) = rx.recv() {
                let (result, reply) = match command {
                    IntentCommand::Persist(reply) => {
                        requested = true;
                        let result = if durable == Some(current()) { Ok(()) } else {
                            write().map(|generation| { durable = Some(generation); })
                        };
                        (result, reply)
                    }
                    IntentCommand::Flush(reply) => {
                        let result = (|| -> Result<()> {
                            // A callback can land during a disk write. A relaunch barrier only succeeds after
                            // the coherent snapshot written is still the latest generation.
                            let began = std::time::Instant::now();
                            while requested && durable != Some(current()) {
                                anyhow::ensure!(began.elapsed() < std::time::Duration::from_secs(10), "stop intent kept changing during flush");
                                durable = Some(write()?);
                            }
                            Ok(())
                        })();
                        (result, Some(reply))
                    }
                    IntentCommand::Saved(generation, reply) => {
                        let result = (|| -> Result<()> {
                            if current() != generation { return Ok(()); }
                            if path.exists() {
                                let parent = path.parent().context("stop intent parent")?;
                                heimdall_archive::full_sync(&std::fs::File::open(parent)?)?;
                                if current() != generation { return Ok(()); }
                                std::fs::remove_file(&path)?;
                                heimdall_archive::full_sync(&std::fs::File::open(parent)?)?;
                            }
                            durable = Some(generation);
                            Ok(())
                        })();
                        (result, Some(reply))
                    }
                };
                let result = result.map_err(|e| format!("{e:#}"));
                if let Err(e) = &result { eprintln!("Stop intent not saved: {e}"); }
                if let Some(reply) = reply { let _ = reply.send(result); }
            }
        })?;
        Ok(Self { tx: Some(tx), thread: Some(thread) })
    }
    pub(super) fn wait(&self, acceptance: Option<&Mutex<IntentAcceptance>>, command: impl FnOnce(IntentReply) -> IntentCommand) -> Result<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            // Order the enqueue with final exit, but never hold acceptance while
            // a normal settings save waits on disk (Off must stay immediate).
            let _acceptance = acceptance.map(|lock| lock.lock().unwrap());
            self.tx.as_ref().context("stop intent writer closed")?.send(command(tx))?;
        }
        rx.recv_timeout(std::time::Duration::from_secs(10)).context("stop intent writer did not finish")?
            .map_err(anyhow::Error::msg)
    }
}
impl Drop for IntentWriter {
    fn drop(&mut self) {
        self.tx.take();
        if let Some(thread) = self.thread.take() { let _ = thread.join(); }
    }
}
