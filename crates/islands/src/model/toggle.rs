//! Hiding and showing every island (⌃⌘, and the menu, "a very, very nice animation of them getting
//! smaller, and then expanding from nothing"). Each island shrinks into its own centre on a spring and fades out, or
//! grows back out of it to its exact place; a slight stagger runs across the stack, top first coming in and bottom
//! first going out. Reduce Motion: a plain fade. Pure timing; the app applies it (`Ctl::set_screen_hidden`).

use super::geometry::Rect;

/// The animation key of the shrink and growth on the island's shape.
pub const TOGGLE_SCALE: &str = "toggleScale";

/// The scale an island shrinks to and grows from: nothing, without a singular transform.
pub const GONE_SCALE: f64 = 0.01;
/// Going: a critically damped spring (no overshoot through nothing) and an ease-in fade that ends with it.
pub const HIDE_RESPONSE: f64 = 0.26;
pub const HIDE_FADE: f64 = 0.22;
/// Coming: the islands' own growth spring (`Spring::GROW`, one small living overshoot) and a quick fade in.
pub const SHOW_FADE: f64 = 0.14;
/// Reduce Motion: only the fade.
pub const PLAIN_FADE: f64 = 0.2;
/// Between one island and the next in the stack, and the most the last one waits.
pub const STAGGER: f64 = 0.03;
pub const STAGGER_MAX: f64 = 0.1;

/// How long after its start an island going away is off the screen.
pub fn hide_time(reduce: bool) -> f64 {
    if reduce { PLAIN_FADE } else { HIDE_FADE.max(HIDE_RESPONSE) }
}

/// Each island's delay, in the order given (`rects`: where each is on screen, y up). Coming, the top of the stack
/// leads and the rest follow downward; going, the reverse, so the stack folds up toward the menu bar. Reduce Motion:
/// all at once.
pub fn delays(rects: &[Rect], showing: bool, reduce: bool) -> Vec<f64> {
    if reduce || rects.is_empty() {
        return vec![0.0; rects.len()];
    }
    let mut order: Vec<usize> = (0..rects.len()).collect();
    // Top to bottom, then left to right.
    order.sort_by(|&a, &b| rects[b].max_y().total_cmp(&rects[a].max_y()).then(rects[a].min_x().total_cmp(&rects[b].min_x())));
    if !showing {
        order.reverse();
    }
    let step = if rects.len() > 1 { STAGGER.min(STAGGER_MAX / (rects.len() - 1) as f64) } else { 0.0 };
    let mut out = vec![0.0; rects.len()];
    for (k, &i) in order.iter().enumerate() {
        out[i] = k as f64 * step;
    }
    out
}

/// Which of an island's repeating timers may be armed: the running task's breath (`work`), the open body's "started
/// 12 min ago" line (`clock`) and the attention pulse (`attention`). At rest nothing draws: none runs while its window
/// is off screen (every island hidden and this one not up for news) or once it is leaving.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Timers {
    pub work: bool,
    pub clock: bool,
    pub attention: bool,
}

/// `running_task`: its look is a running task; `attention`: a question or bell is showing on the pill; `open_clock`:
/// it is open and its body has a clock line.
pub fn timers(on_screen: bool, removing: bool, running_task: bool, attention: bool, open_clock: bool) -> Timers {
    let live = on_screen && !removing;
    Timers { work: live && running_task && !attention, clock: live && open_clock, attention: live && attention }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_islands_arm_no_timers() {
        let none = Timers::default();
        for (task, attn, clock) in [(true, false, false), (false, true, false), (false, false, true), (true, true, true)] {
            assert_eq!(timers(false, false, task, attn, clock), none, "hidden {task} {attn} {clock}");
            assert_eq!(timers(true, true, task, attn, clock), none, "leaving {task} {attn} {clock}");
        }
        // On screen, what the user sees keeps moving: the breath, the pulse (which takes over from the breath), the clock.
        assert_eq!(timers(true, false, true, false, true), Timers { work: true, clock: true, attention: false });
        assert_eq!(timers(true, false, true, true, false), Timers { work: false, clock: false, attention: true });
        assert_eq!(timers(true, false, false, false, false), none);
    }

    fn r(x: f64, top: f64) -> Rect {
        Rect::new(x, top - 36.0, 120.0, 36.0)
    }

    #[test]
    fn coming_top_leads_going_bottom_leads() {
        let rects = [r(0.0, 500.0), r(0.0, 900.0), r(800.0, 700.0)];
        let show = delays(&rects, true, false);
        assert_eq!(show[1], 0.0);
        assert!(show[1] < show[2] && show[2] < show[0]);
        let hide = delays(&rects, false, false);
        assert_eq!(hide[0], 0.0);
        assert!(hide[0] < hide[2] && hide[2] < hide[1]);
    }

    #[test]
    fn stagger_stays_slight_however_many_islands() {
        for n in [1usize, 2, 4, 12] {
            let rects: Vec<Rect> = (0..n).map(|k| r(0.0, 1000.0 - 50.0 * k as f64)).collect();
            let d = delays(&rects, true, false);
            let last = d.iter().cloned().fold(0.0, f64::max);
            assert!(last <= STAGGER_MAX + 1e-9, "{n}: {last}");
            // The whole fold, last island included, stays within about 0.3 to 0.45 s.
            assert!(last + hide_time(false) <= 0.45, "{n}");
        }
    }

    #[test]
    fn reduce_motion_fades_all_at_once() {
        let rects = [r(0.0, 500.0), r(0.0, 900.0)];
        assert_eq!(delays(&rects, true, true), vec![0.0, 0.0]);
        assert_eq!(hide_time(true), PLAIN_FADE);
    }
}
