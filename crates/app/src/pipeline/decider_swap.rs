//! Hot-swapping the decider's precision ("choose the quantization, so we can hot-swap") or its
//! version (V6 or V5, 4 Nov), without restarting Heimdall. The unit swapped is a `Pick` (version and precision).
//!
//! The swap is one task on the single d1 owner (`send_worker`), so it is ordered with every decision: a decision in
//! flight finishes first (a send it preempts is retried, `complete_send`), and sentences arriving meanwhile queue behind
//! it in their lanes and are answered by the new decider (none is dropped; a full lane keeps the speech pending, as
//! always). On the owner thread `swap` destroys the old instance before the new one loads (only one is ever in memory),
//! then asks the new one a short decision: the menu shows the swap until that answer. A load or answer that fails goes
//! back to the previous precision the same way, and the reason reaches the menu.

use decide::precision::Pick;

/// What a swap moves between: a `Pick` in the app; a bare precision in this module's tests.
pub(crate) trait Unit: Copy + PartialEq + std::fmt::Debug {
    /// How a swap's reason names it: "V6 8-bit".
    fn label(&self) -> String;
}
impl Unit for Pick {
    fn label(&self) -> String {
        Pick::label(*self)
    }
}
impl Unit for decide::precision::Precision {
    fn label(&self) -> String {
        decide::precision::Precision::label(*self).into()
    }
}

/// The decider the owner's loader loads (`chosen`) and the one it loaded last (`running`, the nearest present one when
/// the chosen folder is missing), shared by the owner thread, its replacements and the live loop.
#[derive(Debug)]
pub(crate) struct Choice {
    chosen: std::sync::Mutex<Pick>,
    running: std::sync::Mutex<Option<Pick>>,
}

impl Choice {
    pub(crate) fn new(chosen: Pick) -> Self {
        Self { chosen: std::sync::Mutex::new(chosen), running: std::sync::Mutex::new(None) }
    }
    pub(crate) fn chosen(&self) -> Pick {
        *self.chosen.lock().unwrap()
    }
    pub(crate) fn set_chosen(&self, p: Pick) {
        *self.chosen.lock().unwrap() = p;
    }
    pub(crate) fn running(&self) -> Option<Pick> {
        *self.running.lock().unwrap()
    }
    pub(crate) fn set_running(&self, p: Option<Pick>) {
        *self.running.lock().unwrap() = p;
    }
}

/// What a swap ended as.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Outcome<U = Pick> {
    /// The new decider answered. `used` differs from the asked one when its folder was missing (the nearest present).
    Swapped { used: U },
    /// The new one failed (`reason`); the previous decider is back and answered (`used`).
    RolledBack { reason: String, used: U },
    /// Neither loaded: nothing is in memory; the owner loads the previous decider again on its next request.
    Failed { reason: String },
}

/// One swap on the owner thread: `held` dropped first, then `to` loaded and asked (`probe`); on failure `from` the same
/// way. `load` returns the decider and the folder it actually loaded.
pub(crate) fn swap<D, U: Unit>(
    held: &mut Option<D>,
    from: U,
    to: U,
    mut load: impl FnMut(U) -> anyhow::Result<(D, U)>,
    mut probe: impl FnMut(&mut D) -> anyhow::Result<()>,
) -> Outcome<U> {
    // The old instance goes before the new one loads (memory).
    drop(held.take());
    let mut attempt = |p: U, held: &mut Option<D>| -> anyhow::Result<U> {
        let (mut d, used) = load(p)?;
        if let Err(e) = probe(&mut d) {
            drop(d);
            return Err(e.context("did not answer"));
        }
        *held = Some(d);
        Ok(used)
    };
    match attempt(to, held) {
        Ok(used) => Outcome::Swapped { used },
        Err(e) => {
            let reason = format!("{} didn\u{2019}t load: {}", to.label(), short(&e));
            match attempt(from, held) {
                Ok(used) => Outcome::RolledBack { reason, used },
                Err(back) => Outcome::Failed { reason: format!("{reason}; {} didn\u{2019}t load either: {}", from.label(), short(&back)) },
            }
        }
    }
}

/// An error as one line for the menu: its outermost causes, never a backtrace.
fn short(e: &anyhow::Error) -> String {
    let text = format!("{e:#}");
    let line = text.lines().next().unwrap_or_default().trim().to_string();
    if line.chars().count() > 120 { format!("{}\u{2026}", line.chars().take(119).collect::<String>()) } else { line }
}

#[cfg(test)]
mod tests {
    use super::*;
    use decide::precision::Precision;
    use std::cell::RefCell;
    use std::rc::Rc;

    /// A fake decider that records its life: loaded, dropped.
    struct Fake { p: Precision, log: Rc<RefCell<Vec<String>>> }
    impl Drop for Fake {
        fn drop(&mut self) { self.log.borrow_mut().push(format!("drop {}", self.p.label())); }
    }

    fn rig(fail_load: &'static [Precision], fail_probe: &'static [Precision]) -> (Rc<RefCell<Vec<String>>>, impl FnMut(Precision) -> anyhow::Result<(Fake, Precision)>, impl FnMut(&mut Fake) -> anyhow::Result<()>) {
        let log = Rc::new(RefCell::new(vec![]));
        let (l1, l2) = (log.clone(), log.clone());
        let load = move |p: Precision| {
            l1.borrow_mut().push(format!("load {}", p.label()));
            if fail_load.contains(&p) { anyhow::bail!("no Metal device") }
            Ok((Fake { p, log: l1.clone() }, p))
        };
        let probe = move |d: &mut Fake| {
            l2.borrow_mut().push(format!("ask {}", d.p.label()));
            if fail_probe.contains(&d.p) { anyhow::bail!("tokenizer failed") }
            Ok(())
        };
        (log, load, probe)
    }

    #[test]
    fn the_old_decider_is_destroyed_before_the_new_one_loads_and_the_new_one_answers() {
        let (log, load, probe) = rig(&[], &[]);
        let mut held = Some(Fake { p: Precision::Bf16, log: log.clone() });
        let out = swap(&mut held, Precision::Bf16, Precision::Int4, load, probe);
        assert_eq!(out, Outcome::Swapped { used: Precision::Int4 });
        assert_eq!(*log.borrow(), ["drop 16-bit", "load 4-bit", "ask 4-bit"]);
        assert_eq!(held.as_ref().map(|d| d.p), Some(Precision::Int4));
    }

    #[test]
    fn a_failed_load_rolls_back_with_its_reason() {
        let (log, load, probe) = rig(&[Precision::Int8], &[]);
        let mut held = Some(Fake { p: Precision::Int4, log: log.clone() });
        let out = swap(&mut held, Precision::Int4, Precision::Int8, load, probe);
        assert_eq!(out, Outcome::RolledBack { reason: "8-bit didn\u{2019}t load: no Metal device".into(), used: Precision::Int4 });
        assert_eq!(*log.borrow(), ["drop 4-bit", "load 8-bit", "load 4-bit", "ask 4-bit"]);
        assert_eq!(held.as_ref().map(|d| d.p), Some(Precision::Int4));
    }

    #[test]
    fn a_new_decider_that_does_not_answer_is_destroyed_and_rolled_back() {
        let (log, load, probe) = rig(&[], &[Precision::Bf16]);
        let mut held = Some(Fake { p: Precision::Int4, log: log.clone() });
        let out = swap(&mut held, Precision::Int4, Precision::Bf16, load, probe);
        assert!(matches!(&out, Outcome::RolledBack { reason, used: Precision::Int4 } if reason.contains("did not answer")), "{out:?}");
        assert_eq!(*log.borrow(), ["drop 4-bit", "load 16-bit", "ask 16-bit", "drop 16-bit", "load 4-bit", "ask 4-bit"], "never two in memory");
    }

    #[test]
    fn both_failing_leaves_nothing_loaded_and_says_both() {
        let (log, load, probe) = rig(&[Precision::Int8, Precision::Int4], &[]);
        let mut held = None::<Fake>;
        let out = swap(&mut held, Precision::Int4, Precision::Int8, load, probe);
        assert!(matches!(&out, Outcome::Failed { reason } if reason.contains("8-bit") && reason.contains("4-bit didn\u{2019}t load either")), "{out:?}");
        assert!(held.is_none());
        assert_eq!(log.borrow().len(), 2);
    }
}
