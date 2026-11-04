//! The user's strokes: a pure model (points in the overlay view's coordinates, y up, in points). The overlay draws them
//! as shape layers and the screenshot composite draws them into the image with the same path (`mac::draw::ink`), so
//! what the user sees is what the agent receives.

/// Points closer than this to the previous one add nothing but jitter.
const MIN_STEP: f64 = 0.75;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pt {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Debug, Default)]
pub struct Ink {
    pub strokes: Vec<Vec<Pt>>,
    drawing: bool,
}

impl Ink {
    pub fn begin(&mut self, p: Pt) {
        self.strokes.push(vec![p]);
        self.drawing = true;
    }

    /// Adds a point to the stroke in progress; false when there is none or the point is too close to matter.
    pub fn extend(&mut self, p: Pt) -> bool {
        if !self.drawing {
            return false;
        }
        let Some(stroke) = self.strokes.last_mut() else { return false };
        let last = *stroke.last().expect("a stroke starts with a point");
        if (p.x - last.x).hypot(p.y - last.y) < MIN_STEP {
            return false;
        }
        stroke.push(p);
        true
    }

    pub fn end(&mut self) {
        self.drawing = false;
    }

    /// The point the stroke in progress has reached, if one is being drawn.
    pub fn current(&self) -> Option<Pt> {
        if self.drawing { self.strokes.last()?.last().copied() } else { None }
    }

    pub fn undo(&mut self) -> bool {
        self.drawing = false;
        self.strokes.pop().is_some()
    }

    pub fn clear(&mut self) -> bool {
        self.drawing = false;
        let had = !self.strokes.is_empty();
        self.strokes.clear();
        had
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(x: f64, y: f64) -> Pt {
        Pt { x, y }
    }

    #[test]
    fn strokes_undo_and_clear() {
        let mut ink = Ink::default();
        assert!(!ink.extend(p(1.0, 1.0)), "no stroke in progress");
        ink.begin(p(0.0, 0.0));
        assert!(!ink.extend(p(0.3, 0.3)), "jitter is dropped");
        assert!(ink.extend(p(5.0, 0.0)));
        assert_eq!(ink.current(), Some(p(5.0, 0.0)));
        ink.end();
        assert_eq!(ink.current(), None);
        assert!(!ink.extend(p(9.0, 0.0)), "a finished stroke does not grow");
        ink.begin(p(10.0, 10.0));
        ink.end();
        assert_eq!(ink.strokes.len(), 2);
        assert_eq!(ink.strokes[0].len(), 2);
        assert!(ink.undo());
        assert_eq!(ink.strokes.len(), 1);
        assert!(ink.clear());
        assert!(!ink.clear());
        assert!(!ink.undo());
    }
}
