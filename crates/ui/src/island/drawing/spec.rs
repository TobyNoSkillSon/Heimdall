//! The agent's pointing commands (the `point` tool): one per line (or separated by ";"), one word and then numbers,
//! so a model writes them correctly from a short description and spends few tokens on them ("very
//! simple but comprehensive … one word maximum, two for a command and then just drawing").
//!
//! ```text
//! size 1728 1117
//! circle 742 614 82 53 "this price"
//! arrow 1082 823 244 44 "press this"
//! underline 498 442 600 46
//! path M 310 204 l 8 9 l 17 -20
//! ```
//!
//! Coordinates are pixels of the screenshot the agent is looking at, from its top-left corner; `size` is that
//! screenshot's size, and the pointer maps it onto the display's points (Retina, scaled copies, any display).
//! Screenshot coordinates only, approximate by nature ("based on the screenshot you draw on top of it
//! and that's it … it would be mostly approximate"); no Accessibility or other macOS lookups.

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct P {
    pub x: f64,
    pub y: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Move(P),
    Line(P),
    Quad(P, P),
    Cubic(P, P, P),
    Close,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Shape {
    Circle(Rect),
    Box(Rect),
    Underline(Rect),
    /// Points at the target box (w = h = 0 for a point) and stops just outside it.
    Arrow { from: Option<P>, to: Rect },
    Path(Vec<Seg>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Side {
    Right,
    Left,
    Above,
    Below,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Mark {
    pub shape: Shape,
    pub label: Option<String>,
    /// Where the label goes; None lets the pointer choose (right of the target, then left, below, above).
    pub side: Option<Side>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Spec {
    /// 1-based, the main display first (the order `NSScreen.screens` lists); None is the display the user is on (under
    /// the pointer), where the island shows.
    pub display: Option<usize>,
    /// The screenshot's size in pixels; it covers the whole display.
    pub space: (f64, f64),
    /// Seconds the marks stay after they have been drawn.
    pub hold: f64,
    pub marks: Vec<Mark>,
}

pub const DEFAULT_HOLD: f64 = 2.5;
const COMMANDS: &str = "size, circle, box, underline, arrow, path, hold, display";

/// What the agent is told, verbatim: the tool description and the instruction to act without deliberating.
pub const AGENT_GUIDE: &str = "Draw on User's screen to show him something. Do not deliberate: look at the screenshot \
and draw at once; approximate is fine. One command per line, coordinates in the screenshot's pixels from its top-left:
size W H  (the screenshot's size; first)
circle|box|underline|arrow X Y W H \"label\"  (the target's box; arrow also takes a point X Y)
path <SVG path: M L H V Q C Z> \"label\"
Labels are optional, 1-3 words, never the target's own text. Optional after a label: left|right|above|below. \
Optional after an arrow: from X Y. Optional lines: hold SECONDS, display N (1 is the main display; default: the one \
User is on).";

/// Splits a line into words, keeping a "quoted label" as one word (marked by its leading quote).
fn words(text: &str) -> Result<Vec<String>, String> {
    let mut out = vec![];
    let mut chars = text.chars().peekable();
    while let Some(&c) = chars.peek() {
        if c.is_whitespace() {
            chars.next();
        } else if c == '"' || c == '\u{201c}' {
            chars.next();
            let label: String = chars.by_ref().take_while(|&c| c != '"' && c != '\u{201d}').collect();
            out.push(format!("\"{label}"));
        } else {
            let mut word = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_whitespace() || c == '"' {
                    break;
                }
                word.push(c);
                chars.next();
            }
            out.push(word);
        }
    }
    Ok(out)
}

fn number(w: &str) -> Option<f64> {
    w.trim_end_matches(',').parse::<f64>().ok().filter(|n| n.is_finite())
}

/// The words after a mark's geometry: a label (quoted, or bare words), a side, and for arrows `from X Y`.
fn finish(command: &str, rest: &[String], shape: Shape) -> Result<Mark, String> {
    let (mut label_words, mut side, mut from) = (vec![], None, None);
    let mut i = 0;
    while i < rest.len() {
        let w = rest[i].as_str();
        match w {
            "left" => side = Some(Side::Left),
            "right" => side = Some(Side::Right),
            "above" => side = Some(Side::Above),
            "below" => side = Some(Side::Below),
            "from" if command == "arrow" => {
                let (x, y) = (rest.get(i + 1).and_then(|w| number(w)), rest.get(i + 2).and_then(|w| number(w)));
                match (x, y) {
                    (Some(x), Some(y)) => from = Some(P { x, y }),
                    _ => return Err("from needs X Y".into()),
                }
                i += 2;
            }
            "from" => return Err("from is for arrows only".into()),
            q if q.starts_with('"') => label_words.push(q[1..].to_string()),
            bare if number(bare).is_some() => return Err(format!("{command}: too many numbers")),
            bare => label_words.push(bare.to_string()),
        }
        i += 1;
    }
    let label = Some(label_words.join(" ").trim().to_string()).filter(|l| !l.is_empty());
    if let Some(text) = &label {
        if text.split_whitespace().count() > 3 || text.chars().count() > 28 {
            return Err(format!("label \"{text}\" should be 1-3 short words"));
        }
    }
    let shape = match (shape, from) {
        (Shape::Arrow { to, .. }, Some(p)) => Shape::Arrow { from: Some(p), to },
        (shape, _) => shape,
    };
    Ok(Mark { shape, label, side })
}

fn mark(command: &str, rest: &str) -> Result<Mark, String> {
    if command == "path" {
        // The SVG path runs up to the label's opening quote; anything after it is label and side.
        let (d, tail) = rest.split_once(['"', '\u{201c}']).map(|(d, t)| (d, format!("\"{t}"))).unwrap_or((rest, String::new()));
        let segs = path(d.trim())?;
        return finish(command, &words(&tail)?, Shape::Path(segs));
    }
    let all = words(rest)?;
    let count = all.iter().take_while(|w| number(w).is_some()).count();
    let n: Vec<f64> = all[..count].iter().filter_map(|w| number(w)).collect();
    let box_of = |n: &[f64]| -> Result<Rect, String> {
        match *n {
            [x, y, w, h] if w >= 0.0 && h >= 0.0 => Ok(Rect { x, y, w, h }),
            [_, _, _, _] => Err(format!("{command} needs a non-negative W and H")),
            _ => Err(format!("{command} needs X Y W H")),
        }
    };
    let shape = match command {
        "circle" => Shape::Circle(box_of(&n)?),
        "box" => Shape::Box(box_of(&n)?),
        "underline" => Shape::Underline(box_of(&n)?),
        _ => match *n.as_slice() {
            [x, y] => Shape::Arrow { from: None, to: Rect { x, y, w: 0.0, h: 0.0 } },
            [_, _, _, _] => Shape::Arrow { from: None, to: box_of(&n)? },
            _ => return Err("arrow needs X Y W H or X Y".into()),
        },
    };
    finish(command, &all[count..], shape)
}

pub fn parse(text: &str) -> Result<Spec, String> {
    let mut spec = Spec { display: None, space: (0.0, 0.0), hold: DEFAULT_HOLD, marks: vec![] };
    for (i, raw) in text.split(['\n', ';']).enumerate() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let at = |e: String| format!("line {}: {e}", i + 1);
        let (command, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let command = command.to_ascii_lowercase();
        let nums = || -> Vec<f64> { rest.split_whitespace().filter_map(number).collect() };
        match command.as_str() {
            "size" => match *nums().as_slice() {
                [w, h] if w > 0.0 && h > 0.0 => spec.space = (w, h),
                _ => return Err(at("size needs the screenshot's W H in pixels".into())),
            },
            "hold" => match *nums().as_slice() {
                [s] if (0.0..=30.0).contains(&s) => spec.hold = s,
                _ => return Err(at("hold needs seconds, 0-30".into())),
            },
            "display" => match *nums().as_slice() {
                [n] if n >= 1.0 && n.fract() == 0.0 => spec.display = Some(n as usize),
                _ => return Err(at("display needs a number from 1".into())),
            },
            "circle" | "box" | "underline" | "arrow" | "path" => spec.marks.push(mark(&command, rest).map_err(at)?),
            other => return Err(at(format!("unknown command \"{other}\" ({COMMANDS})"))),
        }
    }
    if spec.space == (0.0, 0.0) {
        return Err("size is missing: the screenshot's W H in pixels".into());
    }
    if spec.marks.is_empty() {
        return Err("no marks".into());
    }
    if spec.marks.len() > 8 {
        return Err("at most 8 marks at once".into());
    }
    Ok(spec)
}

/// An SVG path's M L H V Q C Z commands (absolute and relative) as absolute segments.
pub fn path(d: &str) -> Result<Vec<Seg>, String> {
    let mut tokens = vec![];
    let mut number = String::new();
    let flush = |number: &mut String, tokens: &mut Vec<Result<f64, char>>| -> Result<(), String> {
        if !number.is_empty() {
            tokens.push(Ok(number.parse::<f64>().map_err(|_| format!("path: bad number \"{number}\""))?));
            number.clear();
        }
        Ok(())
    };
    for ch in d.chars() {
        match ch {
            '0'..='9' | '.' => {
                // A second decimal point starts a new number (SVG allows "0.5.5").
                if ch == '.' && number.contains('.') && !number.contains(['e', 'E']) {
                    flush(&mut number, &mut tokens)?;
                }
                number.push(ch);
            }
            '-' | '+' => {
                if !(number.ends_with('e') || number.ends_with('E')) {
                    flush(&mut number, &mut tokens)?;
                }
                number.push(ch);
            }
            'e' | 'E' if !number.is_empty() => number.push(ch),
            ' ' | ',' | '\n' | '\t' | '\r' => flush(&mut number, &mut tokens)?,
            c if c.is_ascii_alphabetic() => {
                flush(&mut number, &mut tokens)?;
                if !"MmLlHhVvQqCcZz".contains(c) {
                    return Err(format!("path: \"{c}\" is not supported (use M L H V Q C Z)"));
                }
                tokens.push(Err(c));
            }
            c => return Err(format!("path: unexpected \"{c}\"")),
        }
    }
    flush(&mut number, &mut tokens)?;

    let mut segs = vec![];
    let (mut cur, mut start) = (P { x: 0.0, y: 0.0 }, P { x: 0.0, y: 0.0 });
    let mut i = 0;
    let mut command = None;
    let take = |i: &mut usize, n: usize, c: char| -> Result<Vec<f64>, String> {
        let mut out = vec![];
        for _ in 0..n {
            match tokens.get(*i) {
                Some(Ok(v)) => out.push(*v),
                _ => return Err(format!("path: \"{c}\" needs {n} numbers")),
            }
            *i += 1;
        }
        Ok(out)
    };
    let len = tokens.len();
    while i < len {
        let c = match tokens[i] {
            Err(c) => {
                i += 1;
                c
            }
            Ok(_) => match command {
                // Repeated coordinates continue the last command; after M they are lines.
                Some('M') => 'L',
                Some('m') => 'l',
                Some(c) => c,
                None => return Err("path must start with M".into()),
            },
        };
        let rel = c.is_ascii_lowercase();
        let at = |x: f64, y: f64, cur: P| if rel { P { x: cur.x + x, y: cur.y + y } } else { P { x, y } };
        match c.to_ascii_uppercase() {
            'M' => {
                let v = take(&mut i, 2, c)?;
                cur = at(v[0], v[1], cur);
                start = cur;
                segs.push(Seg::Move(cur));
            }
            'L' => {
                let v = take(&mut i, 2, c)?;
                cur = at(v[0], v[1], cur);
                segs.push(Seg::Line(cur));
            }
            'H' => {
                let v = take(&mut i, 1, c)?;
                cur = P { x: if rel { cur.x + v[0] } else { v[0] }, y: cur.y };
                segs.push(Seg::Line(cur));
            }
            'V' => {
                let v = take(&mut i, 1, c)?;
                cur = P { x: cur.x, y: if rel { cur.y + v[0] } else { v[0] } };
                segs.push(Seg::Line(cur));
            }
            'Q' => {
                let v = take(&mut i, 4, c)?;
                let control = at(v[0], v[1], cur);
                cur = at(v[2], v[3], cur);
                segs.push(Seg::Quad(control, cur));
            }
            'C' => {
                let v = take(&mut i, 6, c)?;
                let (a, b) = (at(v[0], v[1], cur), at(v[2], v[3], cur));
                cur = at(v[4], v[5], cur);
                segs.push(Seg::Cubic(a, b, cur));
            }
            'Z' => {
                cur = start;
                segs.push(Seg::Close);
            }
            _ => unreachable!(),
        }
        if segs.len() == 1 && !matches!(segs[0], Seg::Move(_)) {
            return Err("path must start with M".into());
        }
        command = Some(c);
    }
    if segs.len() < 2 {
        return Err("path needs at least M and one more command".into());
    }
    Ok(segs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_parses() {
        let spec = parse(
            "size 3456 2234\ncircle 1180 1012 488 88 \"this price\"\narrow 1900 1370\nunderline 10 20 100 16\n\
             box 1 2 3 4 Sign up below\narrow 10 10 50 60 \"press this\" from 300 300\n\
             path M 10 10 L 20 20 h 5 v 5 q 1 1 2 2 C 1 2 3 4 5 6 z \"a check\" left\nhold 6\ndisplay 1",
        )
        .unwrap();
        assert_eq!(spec.space, (3456.0, 2234.0));
        assert_eq!((spec.hold, spec.display), (6.0, Some(1)));
        assert_eq!(spec.marks.len(), 6);
        assert_eq!(spec.marks[0].label.as_deref(), Some("this price"));
        assert_eq!(spec.marks[1].shape, Shape::Arrow { from: None, to: Rect { x: 1900.0, y: 1370.0, w: 0.0, h: 0.0 } });
        assert_eq!((spec.marks[3].label.as_deref(), spec.marks[3].side), (Some("Sign up"), Some(Side::Below)));
        assert_eq!(
            spec.marks[4].shape,
            Shape::Arrow { from: Some(P { x: 300.0, y: 300.0 }), to: Rect { x: 10.0, y: 10.0, w: 50.0, h: 60.0 } }
        );
        let Shape::Path(segs) = &spec.marks[5].shape else { panic!() };
        assert_eq!(segs[2], Seg::Line(P { x: 25.0, y: 20.0 }));
        assert_eq!(segs.last(), Some(&Seg::Close));
        assert_eq!((spec.marks[5].label.as_deref(), spec.marks[5].side), (Some("a check"), Some(Side::Left)));
    }

    #[test]
    fn one_line_with_semicolons() {
        let spec = parse("size 1728 1117; box 10 10 100 40; arrow 50 50 \"here\"").unwrap();
        assert_eq!(spec.marks.len(), 2);
    }

    #[test]
    fn mistakes_get_short_specific_errors() {
        let err = |s: &str| parse(&format!("size 100 100\n{s}")).unwrap_err();
        assert_eq!(err("cirle 1 2 3 4"), "line 2: unknown command \"cirle\" (size, circle, box, underline, arrow, path, hold, display)");
        assert_eq!(err("circle 1 2 3"), "line 2: circle needs X Y W H");
        assert_eq!(err("arrow 1 2 3"), "line 2: arrow needs X Y W H or X Y");
        assert_eq!(err("box 1 2 3 4 from 1 2"), "line 2: from is for arrows only");
        assert!(err("box 1 2 3 4 \"far too many words here\"").contains("1-3 short words"));
        assert_eq!(err("path M 1 2 A 1 1 0 0 1 3 3"), "line 2: path: \"A\" is not supported (use M L H V Q C Z)");
        assert_eq!(parse("box 1 2 3 4").unwrap_err(), "size is missing: the screenshot's W H in pixels");
        assert_eq!(parse("size 10 10").unwrap_err(), "no marks");
    }

    #[test]
    fn compact_svg_numbers() {
        let segs = path("M10-5L.5.5l-1e1,2").unwrap();
        assert_eq!(segs[0], Seg::Move(P { x: 10.0, y: -5.0 }));
        assert_eq!(segs[1], Seg::Line(P { x: 0.5, y: 0.5 }));
        assert_eq!(segs[2], Seg::Line(P { x: -9.5, y: 2.5 }));
        assert_eq!(path("M 0 0 1 1").unwrap()[1], Seg::Line(P { x: 1.0, y: 1.0 }), "coordinates after M are lines");
    }
}
