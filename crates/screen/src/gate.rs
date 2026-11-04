//! A stopped/dark/locked display never contributes footage.
#[derive(Clone, Copy, Debug)]
pub struct CaptureGate {
    pub seeing: bool,
    pub awake: bool,
    pub unlocked: bool,
    pub brightness: Option<f32>,
}
impl CaptureGate {
    pub fn allows(self) -> bool {
        self.seeing
            && self.awake
            && self.unlocked
            && self.brightness.is_none_or(|v| v.is_finite() && v > 0.)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_stop_gate_wins() {
        let g = CaptureGate {
            seeing: true,
            awake: true,
            unlocked: true,
            brightness: Some(1.),
        };
        assert!(g.allows());
        for off in [
            CaptureGate { seeing: false, ..g },
            CaptureGate { awake: false, ..g },
            CaptureGate {
                unlocked: false,
                ..g
            },
            CaptureGate {
                brightness: Some(0.),
                ..g
            },
            CaptureGate {
                brightness: Some(f32::NAN),
                ..g
            },
        ] {
            assert!(!off.allows());
        }
        assert!(CaptureGate {
            brightness: None,
            ..g
        }
        .allows()); // external display without brightness control
    }
}
