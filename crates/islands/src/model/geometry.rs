//! CGRect-like geometry with CoreGraphics' own rules (an edge shared by two rects is no intersection; an inset past
//! the size is the null rect), so placement maths reads and behaves like the Swift build.

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Size {
    pub w: f64,
    pub h: f64,
}

impl Size {
    pub const fn new(w: f64, h: f64) -> Size {
        Size { w, h }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Vector {
    pub dx: f64,
    pub dy: f64,
}

impl Vector {
    pub const ZERO: Vector = Vector { dx: 0.0, dy: 0.0 };
    pub const fn new(dx: f64, dy: f64) -> Vector {
        Vector { dx, dy }
    }
    pub fn length(&self) -> f64 {
        self.dx.hypot(self.dy)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f64,
    pub y: f64,
}

impl Point {
    pub const fn new(x: f64, y: f64) -> Point {
        Point { x, y }
    }
}

/// A rectangle (origin and size). `NULL` is CoreGraphics' null rect (infinite origin).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    pub const NULL: Rect = Rect { x: f64::INFINITY, y: f64::INFINITY, w: 0.0, h: 0.0 };
    pub const ZERO: Rect = Rect { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };

    pub const fn new(x: f64, y: f64, w: f64, h: f64) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn of(origin: Point, size: Size) -> Rect {
        Rect::new(origin.x, origin.y, size.w, size.h)
    }

    pub fn is_null(&self) -> bool {
        self.x.is_infinite() || self.y.is_infinite()
    }

    pub fn is_empty(&self) -> bool {
        self.is_null() || self.w == 0.0 || self.h == 0.0
    }

    fn std(&self) -> Rect {
        let mut r = *self;
        if r.w < 0.0 {
            r.x += r.w;
            r.w = -r.w;
        }
        if r.h < 0.0 {
            r.y += r.h;
            r.h = -r.h;
        }
        r
    }

    pub fn min_x(&self) -> f64 {
        self.std().x
    }
    pub fn min_y(&self) -> f64 {
        self.std().y
    }
    pub fn max_x(&self) -> f64 {
        let r = self.std();
        r.x + r.w
    }
    pub fn max_y(&self) -> f64 {
        let r = self.std();
        r.y + r.h
    }
    pub fn mid_x(&self) -> f64 {
        let r = self.std();
        r.x + r.w / 2.0
    }
    pub fn mid_y(&self) -> f64 {
        let r = self.std();
        r.y + r.h / 2.0
    }
    pub fn width(&self) -> f64 {
        self.w.abs()
    }
    pub fn height(&self) -> f64 {
        self.h.abs()
    }
    pub fn size(&self) -> Size {
        Size::new(self.w, self.h)
    }
    pub fn origin(&self) -> Point {
        Point::new(self.x, self.y)
    }

    pub fn inset(&self, dx: f64, dy: f64) -> Rect {
        if self.is_null() {
            return *self;
        }
        let r = self.std();
        let n = Rect::new(r.x + dx, r.y + dy, r.w - 2.0 * dx, r.h - 2.0 * dy);
        if n.w < 0.0 || n.h < 0.0 {
            Rect::NULL
        } else {
            n
        }
    }

    pub fn offset(&self, dx: f64, dy: f64) -> Rect {
        if self.is_null() {
            return *self;
        }
        Rect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    /// CGRectIntersectsRect: rects that only share an edge do not intersect.
    pub fn intersects(&self, o: &Rect) -> bool {
        if self.is_null() || o.is_null() {
            return false;
        }
        let (a, b) = (self.std(), o.std());
        !(a.x + a.w <= b.x || b.x + b.w <= a.x || a.y + a.h <= b.y || b.y + b.h <= a.y)
    }

    pub fn contains(&self, p: Point) -> bool {
        if self.is_null() {
            return false;
        }
        let r = self.std();
        p.x >= r.x && p.x < r.x + r.w && p.y >= r.y && p.y < r.y + r.h
    }

    /// CGRectContainsRect.
    pub fn contains_rect(&self, o: &Rect) -> bool {
        if self.is_null() || o.is_null() {
            return false;
        }
        let (a, b) = (self.std(), o.std());
        b.x >= a.x && b.y >= a.y && b.x + b.w <= a.x + a.w && b.y + b.h <= a.y + a.h
    }

    pub fn union(&self, o: &Rect) -> Rect {
        if self.is_null() {
            return *o;
        }
        if o.is_null() {
            return *self;
        }
        let (a, b) = (self.std(), o.std());
        let x0 = a.x.min(b.x);
        let y0 = a.y.min(b.y);
        let x1 = (a.x + a.w).max(b.x + b.w);
        let y1 = (a.y + a.h).max(b.y + b.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    pub fn intersection(&self, o: &Rect) -> Rect {
        if !self.intersects(o) {
            return Rect::NULL;
        }
        let (a, b) = (self.std(), o.std());
        let x0 = a.x.max(b.x);
        let y0 = a.y.max(b.y);
        let x1 = (a.x + a.w).min(b.x + b.w);
        let y1 = (a.y + a.h).min(b.y + b.h);
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }

    /// CGRectIntegral.
    pub fn integral(&self) -> Rect {
        if self.is_null() {
            return *self;
        }
        let r = self.std();
        let x0 = r.x.floor();
        let y0 = r.y.floor();
        let x1 = (r.x + r.w).ceil();
        let y1 = (r.y + r.h).ceil();
        Rect::new(x0, y0, x1 - x0, y1 - y0)
    }
}
