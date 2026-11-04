//! One authoritative UI stop intent shared by every provider and tool path.
use anyhow::Result;
use std::sync::{atomic::{AtomicU64, AtomicUsize, Ordering}, Arc};
use tokio::{io::AsyncWrite, sync::watch};

#[derive(Debug)]
pub struct DeliveryBlocked {
    pub written: usize,
    pub total: usize,
}
impl std::fmt::Display for DeliveryBlocked {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Agent off; transport stopped after {}/{} bytes",
            self.written, self.total
        )
    }
}
impl std::error::Error for DeliveryBlocked {}

/// Reasons share one atomic intent word. Settings are not an On authority.
#[derive(Clone, Copy)]
pub enum StopReason { Agent = 1, Heimdall = 2, Quit = 4 }
const REASONS: u64 = 7;
pub struct DeliveryGate {
    // Upper bits are the monotonic intent generation; low bits are stop reasons.
    state: AtomicU64,
    changes: watch::Sender<u64>,
    /// State-writing tool calls running now (`crucial_write`).
    writes: AtomicUsize,
}
/// Agent off lets crucial work finish before the harness stops, never longer than these ("When
/// the user turns the Agent toggle off, Heimdall must first let crucial in-flight work finish, then stop"; 22 Oct 16:02: a
/// compaction must never be interrupted, he cut two and paid for both). A state write takes milliseconds.
///
/// `COMPACTION_WAIT` is also how long a running compaction may stay silent before the harness ends it
/// (`PiConfig::compaction_inactivity`): Pi writes nothing while the model summarises, and a 230k-token Sol summary
/// ran past three minutes (27 Oct 03:24-03:46). One limit, so Agent off never cuts a compaction the harness would
/// still let finish (it waited 5 minutes for a 15-minute limit, night audit H8).
pub const COMPACTION_WAIT: std::time::Duration = std::time::Duration::from_secs(900);
pub const WRITE_WAIT: std::time::Duration = std::time::Duration::from_secs(10);
impl DeliveryGate {
    pub fn new(on: bool) -> Self {
        Self {
            state: AtomicU64::new(if on { 0 } else { StopReason::Agent as u64 }),
            changes: watch::channel(0).0,
            writes: AtomicUsize::new(0),
        }
    }
    pub fn load(&self, _: Ordering) -> bool {
        self.state.load(Ordering::Acquire) & REASONS == 0
    }
    pub fn stopped_for(&self, reason: StopReason) -> bool {
        self.state.load(Ordering::Acquire) & reason as u64 != 0
    }
    /// One coherent generation and Agent/Heimdall stop state for durable settings/intent snapshots.
    pub fn stop_snapshot(&self) -> (u64, bool, bool) {
        let state = self.state.load(Ordering::Acquire);
        (state >> 3, state & StopReason::Agent as u64 != 0, state & StopReason::Heimdall as u64 != 0)
    }
    /// Generation of the latest explicit intent, including Off not yet processed by the live loop.
    pub fn generation(&self) -> u64 {
        self.state.load(Ordering::Acquire) >> 3
    }
    /// A helper's completion may resume only the intent that started it. The generation check is retried
    /// with the same compare-and-swap as the intent, so a concurrent Off cannot be overwritten.
    pub fn intent_if_current(&self, reason: StopReason, on: bool, generation: u64) -> Option<u64> {
        self.intent_unless(reason, on, || self.generation() != generation)
    }
    /// Explicit intent only: Off and On are ordered at this atomic update, never
    /// at later settings processing. On cannot clear another reason or Quit.
    pub fn intent(&self, reason: StopReason, on: bool) -> u64 {
        self.intent_unless(reason, on, || false).unwrap()
    }
    /// The same ordered update, abandoned if `superseded` reports a newer intent
    /// for this reason. It is re-evaluated whenever another intent wins the race.
    pub fn intent_unless(&self, reason: StopReason, on: bool, superseded: impl Fn() -> bool) -> Option<u64> {
        let previous = self.state.try_update(Ordering::AcqRel, Ordering::Acquire, |state| {
            if superseded() { return None; }
            let reasons = if on && !matches!(reason, StopReason::Quit) {
                (state & REASONS) & !(reason as u64)
            } else { (state & REASONS) | reason as u64 };
            Some((state & !REASONS).checked_add(8).expect("delivery generation exhausted") | reasons)
        }).ok()?;
        let generation = (previous & !REASONS) + 8;
        self.changes.send_replace(generation);
        Some(generation >> 3)
    }
    /// Legacy callers are explicit Agent intents (server lifetime uses Off only).
    pub fn store(&self, on: bool, _: Ordering) {
        self.intent(StopReason::Agent, on);
    }
    pub fn check(&self) -> Result<()> {
        if !self.load(Ordering::Acquire) {
            return Err(DeliveryBlocked { written: 0, total: 0 }.into());
        }
        Ok(())
    }
    /// Capture before any request conversion, logging or serialization.
    /// A request belongs to one On epoch. Off/On never revives it.
    pub fn permit(self: &Arc<Self>) -> Result<DeliveryPermit> {
        let state = self.state.load(Ordering::Acquire);
        if state & REASONS != 0 {
            return Err(DeliveryBlocked { written: 0, total: 0 }.into());
        }
        Ok(DeliveryPermit { gate: self.clone(), epoch: state, scope: None, sealed: false })
    }
    /// A state-writing tool call (`mcp_contract::writes_state`: a plan edit, an island, a bug note, a wake) admitted
    /// inside `permit`'s epoch runs to its end: Off is never a reason to leave it half-written.
    /// Its steps check the returned guard's sealed permit, which Off does not revoke, and the harness stop waits
    /// while any guard lives (`writes_in_flight`, at most `WRITE_WAIT`). A call not yet admitted is refused at once.
    pub fn crucial_write(self: &Arc<Self>, permit: &DeliveryPermit) -> Result<CrucialWrite> {
        anyhow::ensure!(permit.belongs_to(self), "delivery permit belongs to another gate");
        self.writes.fetch_add(1, Ordering::AcqRel);
        let write = CrucialWrite { gate: self.clone(), permit: DeliveryPermit { sealed: true, ..permit.clone() } };
        // Counted before the check: an Off that the check misses sees the write in flight.
        permit.check()?;
        Ok(write)
    }
    /// State-writing tool calls still running; the harness stop waits for zero.
    pub fn writes_in_flight(&self) -> usize { self.writes.load(Ordering::Acquire) }
    /// Restore the durable epoch before callbacks or permits exist.
    pub fn seed_generation(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(generation < (1_u64 << 61), "invalid durable delivery generation");
        self.state.compare_exchange(0, generation << 3, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| anyhow::anyhow!("delivery already initialized"))?;
        Ok(())
    }
    /// Supervisor stop notification. The atomic generation remains the authority.
    pub async fn changed(&self, generation: u64) {
        let mut changes = self.changes.subscribe();
        loop {
            if self.generation() != generation { return; }
            if changes.changed().await.is_err() { return; }
        }
    }
    /// Convenience for callers without preparation; adapters carry an existing permit.
    pub async fn write(self: &Arc<Self>, input: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
        self.permit().map_err(|_| DeliveryBlocked { written: 0, total: bytes.len() })?
            .write(input, bytes).await
    }
}

/// One state write in flight (`DeliveryGate::crucial_write`); dropping it ends the write.
pub struct CrucialWrite { gate: Arc<DeliveryGate>, permit: DeliveryPermit }
impl CrucialWrite {
    /// For the write's own steps only; never stored or handed to transport.
    pub fn permit(&self) -> &DeliveryPermit { &self.permit }
}
impl Drop for CrucialWrite {
    fn drop(&mut self) { self.gate.writes.fetch_sub(1, Ordering::AcqRel); }
}

#[derive(Clone)]
pub struct DeliveryPermit {
    gate: Arc<DeliveryGate>,
    epoch: u64,
    // MCP tools also belong to a particular server, independent of Agent On.
    scope: Option<(Arc<DeliveryGate>, u64)>,
    /// A crucial write's permit: admitted in an On epoch, it finishes whatever Off says (`crucial_write`).
    sealed: bool,
}
impl DeliveryPermit {
    pub fn generation(&self) -> u64 { self.epoch >> 3 }
    /// An accepted reload deliberately retires its issuing MCP server. Carry
    /// the SAME Agent epoch into replacement admission without that old scope.
    pub fn for_replacement(&self) -> Result<Self> {
        self.check()?;
        Ok(Self { gate:self.gate.clone(), epoch:self.epoch, scope:None, sealed:false })
    }
    pub(crate) fn gate(&self) -> Arc<DeliveryGate> { self.gate.clone() }
    pub fn belongs_to(&self, gate: &Arc<DeliveryGate>) -> bool {
        Arc::ptr_eq(&self.gate, gate)
    }
    pub(crate) fn scoped(mut self, scope: DeliveryPermit) -> Self {
        debug_assert!(self.scope.is_none() && scope.scope.is_none());
        self.scope = Some((scope.gate, scope.epoch));
        self
    }
    /// Bind evidence continuations to this caller epoch and server generation.
    pub fn same_authority(&self,other:&Self)->bool {
        Arc::ptr_eq(&self.gate,&other.gate)&&self.epoch==other.epoch&&match (&self.scope,&other.scope){(Some((a,x)),Some((b,y)))=>Arc::ptr_eq(a,b)&&x==y,(None,None)=>true,_=>false}
    }
    /// Allocation-free revocation check for the audio render callback.
    pub fn is_current(&self)->bool {
        self.sealed || self.gate.state.load(Ordering::Acquire)==self.epoch &&
            self.scope.as_ref().is_none_or(|(gate,epoch)|gate.state.load(Ordering::Acquire)==*epoch)
    }
    pub fn check(&self) -> Result<()> {
        if self.sealed { return Ok(()); }
        anyhow::ensure!(self.gate.state.load(Ordering::Acquire) == self.epoch,
            "Agent off; operation cancelled");
        if let Some((gate, epoch)) = &self.scope {
            anyhow::ensure!(gate.state.load(Ordering::Acquire) == *epoch,
                "MCP server retired; operation cancelled");
        }
        Ok(())
    }
    /// Only nonblocking commit polls belong here; revocation never waits for a
    /// transport, tool, queue mutex or join. Work admitted before Off cannot be
    /// recalled from an OS pipe/audio buffer; subsequent polls fail closed.
    pub fn with<T>(&self, operation: impl FnOnce() -> Result<T>) -> Result<T> {
        self.check()?;
        let result = operation();
        // Ready results cannot escape if revocation overlapped the poll.
        self.check()?;
        result
    }
    pub(crate) async fn cancelled(&self) {
        let mut changes = self.gate.changes.subscribe();
        let mut scope_changes = self.scope.as_ref().map(|(gate, _)| gate.changes.subscribe());
        loop {
            if self.check().is_err() { return; }
            tokio::select! {
                biased;
                _ = changes.changed() => {},
                _ = async {
                    match scope_changes.as_mut() {
                        Some(changes) => { let _ = changes.changed().await; },
                        None => std::future::pending::<()>().await,
                    }
                } => {},
            }
            // Notifications may be published out of order by concurrent intents.
            // Only the atomic word, never the notification value, revokes a permit.
        }
    }

    pub async fn run<T>(&self, operation: impl std::future::Future<Output = Result<T>>) -> Result<T> {
        self.check()?;
        let result = tokio::select! {
            biased;
            _ = self.cancelled() => anyhow::bail!("Agent off or MCP server retired; operation cancelled"),
            result = operation => result,
        };
        self.check()?;
        result
    }
    /// Every write and flush poll uses the request's original epoch.
    /// No lock is held while waiting for pipe capacity.
    pub async fn write(&self, input: &mut (impl AsyncWrite + Unpin), bytes: &[u8]) -> Result<()> {
        let mut written = 0;
        let cancelled = self.cancelled();
        tokio::pin!(cancelled);
        while written < bytes.len() {
            let n = tokio::select! {
                biased;
                _ = &mut cancelled => return Err(DeliveryBlocked { written, total: bytes.len() }.into()),
                result = std::future::poll_fn(|cx| {
                    // Account for a poll admitted before Off even if it
                    // accepts bytes while revocation races that nonblocking poll.
                    match self.check().map(|_| std::pin::Pin::new(&mut *input).poll_write(cx, &bytes[written..])) {
                        Ok(poll) => poll,
                        Err(_) => std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "delivery revoked"))),
                    }
                }) => match result {
                    Ok(n) => n,
                    Err(e) => {
                        if self.check().is_err() { return Err(DeliveryBlocked { written, total: bytes.len() }.into()); }
                        return Err(e.into());
                    }
                }
            };
            anyhow::ensure!(n != 0, "harness stdin write returned zero");
            written += n;
            if self.check().is_err() {
                return Err(DeliveryBlocked { written, total: bytes.len() }.into());
            }
        }
        tokio::select! {
            biased;
            _ = &mut cancelled => Err(DeliveryBlocked { written, total: bytes.len() }.into()),
            result = std::future::poll_fn(|cx| {
                match self.with(|| Ok(std::pin::Pin::new(&mut *input).poll_flush(cx))) {
                    Ok(poll) => poll,
                    Err(_) => std::task::Poll::Ready(Err(std::io::Error::new(std::io::ErrorKind::Interrupted, "delivery revoked"))),
                }
            }) => match result {
                Ok(()) => Ok(()),
                Err(_) if self.check().is_err() => Err(DeliveryBlocked { written, total: bytes.len() }.into()),
                Err(e) => Err(e.into()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::AsyncReadExt;
    #[test]
    fn helper_completion_cannot_clear_a_newer_off_or_revive_an_old_on_epoch() {
        let gate = DeliveryGate::new(false);
        let login = gate.generation();
        gate.intent(StopReason::Agent, false);
        assert!(gate.intent_if_current(StopReason::Agent, true, login).is_none());
        assert!(gate.stopped_for(StopReason::Agent));
        gate.intent(StopReason::Agent, true);
        assert!(gate.intent_if_current(StopReason::Agent, true, login).is_none());
        let current = gate.generation();
        assert!(gate.intent_if_current(StopReason::Agent, true, current).is_some());
    }
    #[test]
    fn settings_intent_that_loses_the_race_to_a_menu_intent_yields() {
        // The menu click lands between the settings check and its update.
        let gate = DeliveryGate::new(false);
        let pending = std::sync::atomic::AtomicU64::new(0);
        let calls = std::cell::Cell::new(0);
        let applied = gate.intent_unless(StopReason::Agent, true, || {
            calls.set(calls.get() + 1);
            if calls.get() == 1 {
                pending.fetch_add(1, Ordering::AcqRel);
                gate.intent(StopReason::Agent, false);
                return false;
            }
            pending.load(Ordering::Acquire) > 0
        });
        assert!(applied.is_none() && calls.get() == 2);
        assert!(gate.stopped_for(StopReason::Agent));
        // Without a newer intent the same update applies once, in order.
        assert!(gate.intent_unless(StopReason::Agent, true, || false).is_some());
        assert!(gate.load(Ordering::Acquire));
    }
    /// Agent off lets a state write finish. One admitted before Off completes every step and is
    /// counted until it ends; one asked for after Off never starts.
    #[test]
    fn a_state_write_admitted_before_off_finishes_and_one_after_off_never_starts() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        let write = gate.crucial_write(&permit).unwrap();
        assert_eq!(gate.writes_in_flight(), 1);
        gate.intent(StopReason::Agent, false);
        assert!(permit.check().is_err(), "everything else stops at once");
        assert_eq!(write.permit().with(|| Ok("second step")).unwrap(), "second step");
        assert!(write.permit().is_current());
        drop(write);
        assert_eq!(gate.writes_in_flight(), 0);
        assert!(gate.crucial_write(&permit).is_err());
        assert_eq!(gate.writes_in_flight(), 0, "a refused write is not counted");
        assert!(gate.crucial_write(&Arc::new(DeliveryGate::new(true)).permit().unwrap()).is_err(), "another gate's permit");
    }
    #[test]
    fn result_ready_during_off_is_suppressed_without_waiting_for_operation() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        assert!(permit.with(|| {
            gate.intent(StopReason::Agent, false);
            Ok("private result")
        }).is_err());
    }
    #[tokio::test]
    async fn late_intent_notification_does_not_cancel_a_fresh_permit() {
        let gate = Arc::new(DeliveryGate::new(true));
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        let permit = gate.permit().unwrap();
        let cancellation = permit.cancelled();
        tokio::pin!(cancellation);
        assert!(tokio::time::timeout(std::time::Duration::from_millis(1), &mut cancellation).await.is_err());
        gate.changes.send_replace(8); // delayed publication of the older Off
        assert!(tokio::time::timeout(std::time::Duration::from_millis(1), &mut cancellation).await.is_err());
        gate.store(false, Ordering::Release);
        tokio::time::timeout(std::time::Duration::from_secs(1), cancellation).await.unwrap();
    }
    #[tokio::test]
    async fn permit_captured_before_preparation_cannot_write_in_new_epoch() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let error = permit.write(&mut writer, b"private prompt\n").await.unwrap_err();
        assert_eq!(error.downcast_ref::<DeliveryBlocked>().unwrap().written, 0);
        drop(writer);
        let mut received = vec![];
        reader.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
    }
    struct RevokingWriter { gate: Arc<DeliveryGate> }
    impl AsyncWrite for RevokingWriter {
        fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, bytes: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
            // A poll already admitted to the OS may accept bytes during Off.
            self.gate.store(false, Ordering::Release);
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { panic!("no flush after Off") }
        fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> { std::task::Poll::Ready(Ok(())) }
    }
    #[tokio::test]
    async fn revocation_overlapping_an_admitted_write_counts_accepted_bytes() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        let mut writer = RevokingWriter { gate };
        let error = permit.write(&mut writer, b"private").await.unwrap_err();
        assert_eq!(error.downcast_ref::<DeliveryBlocked>().unwrap().written, 7);
    }
    struct PendingFlush {
        started: Arc<tokio::sync::Notify>,
        polls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl AsyncWrite for PendingFlush {
        fn poll_write(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>, bytes: &[u8]) -> std::task::Poll<std::io::Result<usize>> {
            std::task::Poll::Ready(Ok(bytes.len()))
        }
        fn poll_flush(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            self.polls.fetch_add(1, Ordering::SeqCst);
            self.started.notify_one();
            std::task::Poll::Pending
        }
        fn poll_shutdown(self: std::pin::Pin<&mut Self>, _: &mut std::task::Context<'_>) -> std::task::Poll<std::io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }
    #[tokio::test]
    async fn off_on_revokes_a_pending_flush_without_polling_it_again() {
        let gate = Arc::new(DeliveryGate::new(true));
        let permit = gate.permit().unwrap();
        let started = Arc::new(tokio::sync::Notify::new());
        let polls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut writer = PendingFlush { started: started.clone(), polls: polls.clone() };
        let send = tokio::spawn(async move { permit.write(&mut writer, b"private\n").await });
        tokio::time::timeout(std::time::Duration::from_secs(2), started.notified()).await.unwrap();
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        let error = tokio::time::timeout(std::time::Duration::from_secs(2), send).await.unwrap().unwrap().unwrap_err();
        assert_eq!(error.downcast_ref::<DeliveryBlocked>().unwrap().written, 8);
        assert_eq!(polls.load(Ordering::SeqCst), 1);
    }
    #[tokio::test]
    async fn off_before_transport_writes_zero_bytes() {
        let gate = Arc::new(DeliveryGate::new(false));
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let error = gate
            .write(&mut writer, b"private prompt\n")
            .await
            .unwrap_err();
        assert_eq!(error.downcast_ref::<DeliveryBlocked>().unwrap().written, 0);
        drop(writer);
        let mut received = vec![];
        reader.read_to_end(&mut received).await.unwrap();
        assert!(received.is_empty());
    }
    #[tokio::test]
    async fn off_on_cancels_original_partial_write_and_no_tail_is_written() {
        let gate = Arc::new(DeliveryGate::new(true));
        let (mut writer, mut reader) = tokio::io::duplex(64);
        let original = gate.clone();
        let task = tokio::spawn(async move {
            let e = original
                .write(&mut writer, &vec![b'x'; 1024])
                .await
                .unwrap_err();
            (writer, e)
        });
        let mut first = [0u8; 16];
        reader.read_exact(&mut first).await.unwrap();
        gate.store(false, Ordering::Release);
        gate.store(true, Ordering::Release);
        let (writer, error) = tokio::time::timeout(std::time::Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        let blocked = error.downcast_ref::<DeliveryBlocked>().unwrap();
        assert!(blocked.written > 0 && blocked.written < 1024);
        drop(writer);
        let mut rest = vec![];
        reader.read_to_end(&mut rest).await.unwrap();
        assert_eq!(16 + rest.len(), blocked.written);
    }
}
