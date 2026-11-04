//! The island's one curve family: damped springs evaluated in closed form, so any frame at any time is a pure
//! function of the motion's start (DESIGN.md §4).

/// A spring: `response` is the undamped period in seconds, `damping` the damping ratio (1 = critical).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Spring {
    pub response: f64,
    pub damping: f64,
}

/// Every shape change that is not a retraction to rest: about 1 % overshoot, within 0.5 % by 0.46 s.
pub const GROW: Spring = Spring { response: 0.45, damping: 0.82 };
/// Retracting to rest: critically damped, 99 % by 0.36 s.
pub const CALM: Spring = Spring { response: 0.34, damping: 1.0 };
/// Drawing mode's light running round the screen edge: critically damped, about 0.64 s round.
pub const LIGHT: Spring = Spring { response: 0.60, damping: 1.0 };

impl Spring {
    fn omega(&self) -> f64 {
        2.0 * std::f64::consts::PI / self.response.max(1e-3)
    }

    /// Position and velocity `tau` seconds after leaving `x0` with velocity `v0` towards `target`.
    pub fn eval(&self, x0: f64, v0: f64, target: f64, tau: f64) -> (f64, f64) {
        if tau <= 0.0 {
            return (x0, v0);
        }
        let w = self.omega();
        let z = self.damping;
        let c1 = x0 - target;
        if z >= 1.0 {
            let c2 = v0 + w * c1;
            let e = (-w * tau).exp();
            let x = target + (c1 + c2 * tau) * e;
            let v = (c2 - w * (c1 + c2 * tau)) * e;
            (x, v)
        } else {
            let wd = w * (1.0 - z * z).sqrt();
            let c2 = (v0 + z * w * c1) / wd;
            let e = (-z * w * tau).exp();
            let (s, c) = (wd * tau).sin_cos();
            let x = target + e * (c1 * c + c2 * s);
            let v = e * ((c2 * wd - z * w * c1) * c - (c1 * wd + z * w * c2) * s);
            (x, v)
        }
    }

    /// When the motion from (`x0`, `v0`) to `target` is within `eps` of the target for good, and slow: a bound on
    /// the decaying envelope, so the frame after it is exactly the target.
    pub fn settle_time(&self, x0: f64, v0: f64, target: f64, eps: f64) -> f64 {
        let w = self.omega();
        let z = self.damping.min(1.0);
        let c1 = (x0 - target).abs();
        if c1 < eps && v0.abs() < eps {
            return 0.0;
        }
        // Envelope amplitude, generous for both regimes (velocity included at one radian per second).
        let amp = c1 + v0.abs() / w + v0.abs() + c1 * w * 0.1;
        let rate = z * w;
        let mut tau = (amp / eps).max(1.0).ln() / rate;
        if self.damping >= 1.0 {
            // (c1 + c2 τ) e^{-ωτ}: the polynomial factor needs a little more time; step until the bound holds.
            let c2 = (v0 + w * (x0 - target)).abs();
            while (c1 + c2 * tau + (c2 + w * (c1 + c2 * tau))) * (-w * tau).exp() > eps && tau < 20.0 {
                tau += 1.0 / 240.0;
            }
        }
        tau.min(20.0)
    }

    /// When the motion from (`x0`, `v0`) to `target` is within `eps` of the target for good: the last time the
    /// motion itself (sampled at 480 Hz up to the envelope bound, against 0.8 `eps`) is farther, plus a sample.
    /// Tighter than `settle_time`'s envelope, so a motion stops asking for frames once nothing visible moves.
    pub fn settle_exact(&self, x0: f64, v0: f64, target: f64, eps: f64) -> f64 {
        let bound = self.settle_time(x0, v0, target, eps);
        if bound <= 0.0 {
            return 0.0;
        }
        let step = 1.0 / 480.0;
        let mut last = 0.0;
        let mut tau = 0.0;
        while tau <= bound {
            if (self.eval(x0, v0, target, tau).0 - target).abs() >= 0.8 * eps {
                last = tau;
            }
            tau += step;
        }
        (last + step).min(bound)
    }

    /// When a unit motion from rest first reaches `x` (at most the reach time).
    pub fn time_to(&self, x: f64) -> f64 {
        let end = self.reach_time();
        let mut tau = 0.0;
        while tau < end && self.unit(tau) < x {
            tau += 1.0 / 480.0;
        }
        tau.min(end)
    }

    /// Progress of a unit motion from rest (0 → 1) at `tau`, unclamped: overshoot shows.
    pub fn unit(&self, tau: f64) -> f64 {
        self.eval(0.0, 0.0, 1.0, tau).0
    }

    /// The furthest progress a unit motion from rest has reached by `tau`, in [0, 1]: an overshoot never moves
    /// content backwards (DESIGN.md §4: "p is the furthest progress the spring has reached").
    pub fn reached(&self, tau: f64) -> f64 {
        if tau <= 0.0 {
            return 0.0;
        }
        if self.damping < 1.0 {
            let w = self.omega();
            let wd = w * (1.0 - self.damping * self.damping).sqrt();
            // From rest, the first peak (velocity zero again) is the first time it reaches 1 and beyond.
            if tau >= std::f64::consts::PI / wd {
                return 1.0;
            }
        }
        let y = self.unit(tau);
        if y >= 0.999 {
            1.0
        } else {
            y.clamp(0.0, 1.0)
        }
    }

    /// When `reached` is 1.
    pub fn reach_time(&self) -> f64 {
        if self.damping < 1.0 {
            let w = self.omega();
            std::f64::consts::PI / (w * (1.0 - self.damping * self.damping).sqrt())
        } else {
            // Critically damped from rest: 1 − (1 + ωτ)e^{−ωτ} = 0.999.
            let mut tau = 0.0;
            while self.unit(tau) < 0.999 && tau < 10.0 {
                tau += 1.0 / 480.0;
            }
            tau
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grow_overshoots_about_one_percent_and_settles_by_half_a_second() {
        let peak = (0..2000).map(|i| GROW.unit(i as f64 / 2000.0)).fold(0.0, f64::max);
        assert!(peak > 1.005 && peak < 1.03, "peak {peak}");
        assert!((GROW.unit(0.46) - 1.0).abs() < 0.01);
        assert!((CALM.unit(0.36) - 1.0).abs() < 0.01, "{}", CALM.unit(0.36));
        assert!(LIGHT.unit(0.64) > 0.98);
    }

    #[test]
    fn velocity_is_the_derivative_and_retargets_keep_it() {
        for s in [GROW, CALM] {
            for &(x0, v0, b) in &[(0.0, 0.0, 100.0), (50.0, -300.0, 10.0), (285.0, 120.0, 185.0)] {
                for k in 1..40 {
                    let t = k as f64 * 0.013;
                    let (x1, v) = s.eval(x0, v0, b, t);
                    let (x2, _) = s.eval(x0, v0, b, t + 1e-6);
                    assert!(((x2 - x1) / 1e-6 - v).abs() < 1e-2 * (1.0 + v.abs()), "{s:?} {t}");
                }
            }
        }
    }

    #[test]
    fn settle_time_bounds_the_motion() {
        for s in [GROW, CALM, LIGHT] {
            for &(x0, v0, b) in &[(0.0, 0.0, 100.0), (50.0, -900.0, 10.0), (185.0, 0.0, 480.0), (32.0, 400.0, 600.0)] {
                let ts = s.settle_time(x0, v0, b, 0.02);
                for k in 0..400 {
                    let (x, _) = s.eval(x0, v0, b, ts + k as f64 * 0.01);
                    assert!((x - b).abs() < 0.02, "{s:?} from {x0} {v0} to {b}: {x} at {}", ts + k as f64 * 0.01);
                }
                assert!(ts < 3.0, "{ts}");
            }
        }
    }

    #[test]
    fn settle_exact_bounds_the_motion_and_is_tighter_than_the_envelope() {
        for s in [GROW, CALM, LIGHT] {
            for &(x0, v0, b) in &[(0.0, 0.0, 100.0), (50.0, -900.0, 10.0), (185.0, 0.0, 480.0), (32.0, 400.0, 600.0), (285.0, 0.0, 185.0)] {
                let ts = s.settle_exact(x0, v0, b, 0.05);
                for k in 0..2000 {
                    let (x, _) = s.eval(x0, v0, b, ts + k as f64 * 0.001);
                    assert!((x - b).abs() < 0.05, "{s:?} from {x0} {v0} to {b}: {x} at {}", ts + k as f64 * 0.001);
                }
                assert!(ts <= s.settle_time(x0, v0, b, 0.05) + 1e-9);
            }
        }
        // A 100 pt growth stops asking for frames once it is within a fifth of a pixel: about 0.62 s (the envelope
        // bound said 0.82 s), a retraction by about 0.52 s (was 0.78 s).
        assert!(GROW.settle_exact(185.0, 0.0, 285.0, 0.1) < 0.65, "{}", GROW.settle_exact(185.0, 0.0, 285.0, 0.1));
        assert!(CALM.settle_exact(285.0, 0.0, 185.0, 0.1) < 0.55, "{}", CALM.settle_exact(285.0, 0.0, 185.0, 0.1));
    }

    #[test]
    fn reached_is_monotone_and_ends_at_one() {
        for s in [GROW, CALM] {
            let mut last = 0.0;
            for k in 0..1000 {
                let r = s.reached(k as f64 / 1000.0);
                assert!(r >= last);
                last = r;
            }
            assert_eq!(s.reached(s.reach_time() + 1e-9), 1.0);
        }
    }
}
