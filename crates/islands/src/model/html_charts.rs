//! Chart tags for model HTML: the model writes the data, the gate writes the geometry. A small model is good at
//! choosing what to show and poor at computing SVG coordinates, so five tags expand into plain inline SVG or HTML
//! before the page is drawn (`model/html.rs` calls `expand` for every `<x-…>` start tag; the end tag is dropped).
//!
//!   <x-line series="2.2 5.0 6.6|3.1 5.5 6.7" names="Opus|Sol" min="0" max="10" target="8.5"
//!           w="328" h="80" ends="1" labels="1|11" fill="1">     several series, a dashed target, end dots and labels
//!   <x-bars items="Wake:83|Suggest:92" max="100" unit="%" colors="blue" w="328" hl="Suggest">   labelled bars
//!   <x-spark values="1 3 2 5" w="80" h="20" color="green">    an inline sparkline
//!   <x-ring value="0.29" size="44" color="blue" label="29%">   a progress ring
//!   <x-dots done="11" total="16" now="12">                     a row of dots: done, now (lit), still to come
//!
//! Colours ("I would stick to the default, with just some accent colours when something is more
//! important"): everything is drawn in the system's neutrals (label, secondary, tertiary) unless the model names an
//! accent: `blue` (Heimdall's running blue, #0A84FF), `green` (the Goals green, #30D158), `orange` or `red` (a real
//! block or failure). Series are told apart by weight and line style (solid label, dashed secondary, then tertiary)
//! and by their names at their ends, never by hue. The retired hues (purple, pink, cyan, teal, indigo, mint, yellow)
//! read as neutrals, and so does any hex but the four accents'. A line takes at most one accent. Numbers outside the attributes' limits are clamped; at most
//! `MAX_POINTS` per series.

pub const MAX_POINTS: usize = 120;
pub const MAX_SERIES: usize = 4;
pub const MAX_BARS: usize = 8;
pub const BODY_WIDTH: f64 = 328.0;

/// The neutrals: Apple's dark label, secondary and tertiary label on black.
pub const INK: &str = "rgba(255,255,255,.92)";
pub const INK2: &str = "rgba(255,255,255,.58)";
pub const INK3: &str = "rgba(255,255,255,.32)";

pub fn colour(name: &str) -> String {
    let n = name.trim().to_ascii_lowercase();
    let hex = match n.as_str() {
        "blue" => "#0A84FF",
        "green" => "#30D158",
        "orange" => "#FF9F0A",
        "red" => "#FF453A",
        "ink" | "label" | "white" => INK,
        "ink2" | "secondary" | "grey" | "gray" => INK2,
        "ink3" | "tertiary" => INK3,
        // Retired accents (3 Nov) fall back to neutrals, so an old habit cannot bring purple or light blue back.
        "purple" | "pink" | "cyan" | "teal" | "indigo" | "mint" | "yellow" => INK2,
        _ => "",
    };
    if !hex.is_empty() {
        return hex.into();
    }
    // A hex colour passes only as one of the palette's own; any other hue reads as a neutral.
    match n.as_str() {
        "#0a84ff" => "#0A84FF".into(),
        "#30d158" => "#30D158".into(),
        "#ff9f0a" => "#FF9F0A".into(),
        "#ff453a" => "#FF453A".into(),
        h if h.starts_with('#') => INK2.into(),
        _ => INK.into(),
    }
}

/// The series' defaults, told apart by weight and dash: (colour, stroke width, dash).
const SERIES_STYLE: [(&str, f64, &str); 4] = [(INK, 2.0, ""), (INK2, 1.6, "5 3"), (INK3, 1.6, ""), (INK3, 1.4, "1.5 2.5")];

fn nums(s: &str) -> Vec<f64> {
    s.split(|c: char| c == ',' || c.is_whitespace()).filter(|t| !t.is_empty()).filter_map(|t| t.replace('_', "").parse::<f64>().ok()).filter(|v| v.is_finite()).take(MAX_POINTS).collect()
}

fn num(attrs: &[(String, String)], k: &str) -> Option<f64> {
    attrs.iter().find(|(a, _)| a == k).and_then(|(_, v)| v.trim().parse::<f64>().ok()).filter(|v| v.is_finite())
}

fn text<'a>(attrs: &'a [(String, String)], k: &str) -> Option<&'a str> {
    attrs.iter().find(|(a, _)| a == k).map(|(_, v)| v.as_str())
}

fn esc(s: &str) -> String { s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;") }

/// A number as people write it: 8.5, 6.38, 1,905.
pub fn fmt(v: f64) -> String {
    if v.abs() >= 1000.0 {
        let s = format!("{:.0}", v.abs());
        let mut out = String::new();
        for (i, c) in s.chars().enumerate() {
            if i > 0 && (s.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        return if v < 0.0 { format!("-{out}") } else { out };
    }
    let s = if v.fract() == 0.0 { format!("{v:.0}") } else if (v * 10.0).fract().abs() < 1e-9 { format!("{v:.1}") } else { format!("{v:.2}") };
    s
}

fn split<'a>(s: Option<&'a str>) -> Vec<&'a str> { s.map(|s| s.split('|').map(str::trim).collect()).unwrap_or_default() }

/// The expansion of one chart tag (`name` without `x-`), or `None` for an unknown one.
pub fn expand(name: &str, attrs: &[(String, String)]) -> Option<String> {
    match name {
        "line" => Some(line(attrs)),
        "bars" => Some(bars(attrs)),
        "spark" => Some(spark(attrs)),
        "ring" => Some(ring(attrs)),
        "dots" => Some(dots(attrs)),
        "steps" => Some(steps(attrs)),
        _ => None,
    }
}

fn line(a: &[(String, String)]) -> String {
    let w = num(a, "w").unwrap_or(BODY_WIDTH).clamp(40.0, 400.0);
    let h = num(a, "h").unwrap_or(72.0).clamp(16.0, 240.0);
    // Each series keeps its own name and colour by position, even when an empty one before it is dropped.
    let (c, n) = (split(text(a, "colors").or(text(a, "colours"))), split(text(a, "names")));
    let kept: Vec<(Vec<f64>, Option<&str>, String)> = split(text(a, "series")).into_iter().enumerate()
        .map(|(i, s)| (nums(s), c.get(i).copied().filter(|v| !v.is_empty()), n.get(i).map(|v| esc(v)).unwrap_or_default()))
        .filter(|s| !s.0.is_empty()).take(MAX_SERIES).collect();
    if kept.is_empty() {
        return String::new();
    }
    // At most one accent: the first series that names one keeps it, the rest fall back to their neutral.
    let mut accented = false;
    let colours: Vec<String> = kept.iter().enumerate().map(|(i, s)| match s.1.map(colour) {
        Some(v) if !v.starts_with("rgba") && !accented => { accented = true; v }
        Some(v) if v.starts_with("rgba") => v,
        _ => SERIES_STYLE[i].0.into(),
    }).collect();
    let names: Vec<String> = kept.iter().map(|s| s.2.clone()).collect();
    let series: Vec<Vec<f64>> = kept.into_iter().map(|s| s.0).collect();
    let target = num(a, "target");
    let all = series.iter().flatten().copied().chain(target);
    let (lo0, hi0) = all.fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), v| (l.min(v), h.max(v)));
    let mut lo = num(a, "min").unwrap_or(lo0);
    let mut hi = num(a, "max").unwrap_or(hi0);
    if num(a, "min").is_none() && num(a, "max").is_none() {
        let pad = ((hi - lo) * 0.08).max(1e-9);
        lo -= pad;
        hi += pad;
    }
    if hi - lo < 1e-9 {
        hi = lo + 1.0;
    }
    let ends = text(a, "ends").is_some_and(|v| v != "0");
    let labels = split(text(a, "labels"));
    let n_max = series.iter().map(Vec::len).max().unwrap_or(1).max(2);
    // Room on the right for end values, at the bottom for x labels.
    // A name at the end of its line ("Sol 6.38") takes about 6 pt a character at 10.5 px.
    let end_w = |k: usize, v: f64| (names.get(k).map_or(0, |n| n.chars().count() + 1) + fmt(v).len()) as f64 * 6.0 + 6.0;
    let right = if ends { series.iter().enumerate().map(|(k, p)| end_w(k, *p.last().unwrap_or(&0.0)) + 4.0).fold(42.0, f64::max).min(w * 0.42) } else { 5.0 };
    let bottom = if labels.is_empty() { 4.0 } else { 15.0 };
    let (pw, ph, top) = (w - right - 4.0, h - bottom - 5.0, 5.0);
    let x = |i: usize| 4.0 + pw * i as f64 / (n_max - 1) as f64;
    let y = |v: f64| top + ph * (1.0 - (v.clamp(lo, hi) - lo) / (hi - lo));
    let mut s = format!("<svg width=\"{w:.0}\" height=\"{h:.0}\" viewBox=\"0 0 {w:.0} {h:.0}\" style=\"display:block\">");
    if text(a, "fill").is_some_and(|v| v != "0") && series[0].len() >= 2 {
        let c = &colours[0];
        // An id of its own per chart: two filled charts in one body must not share one gradient.
        let id = format!("xf{:x}", text(a, "series").unwrap_or("").bytes().fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3)) ^ (h as u64));
        s.push_str(&format!("<defs><linearGradient id=\"{id}\" x1=\"0\" y1=\"0\" x2=\"0\" y2=\"1\"><stop offset=\"0\" stop-color=\"{c}\" stop-opacity=\".28\"/><stop offset=\"1\" stop-color=\"{c}\" stop-opacity=\"0\"/></linearGradient></defs>"));
        let mut d = format!("M{:.1},{:.1}", x(0), top + ph);
        for (i, v) in series[0].iter().enumerate() {
            d.push_str(&format!(" L{:.1},{:.1}", x(i), y(*v)));
        }
        d.push_str(&format!(" L{:.1},{:.1} Z", x(series[0].len() - 1), top + ph));
        s.push_str(&format!("<path d=\"{d}\" fill=\"url(#{id})\"/>"));
    }
    if let Some(t) = target {
        let ty = y(t);
        let label = text(a, "target-label").map(esc).unwrap_or_else(|| fmt(t));
        s.push_str(&format!("<line x1=\"4\" y1=\"{ty:.1}\" x2=\"{:.1}\" y2=\"{ty:.1}\" stroke=\"rgba(255,255,255,.38)\" stroke-width=\"1\" stroke-dasharray=\"3 3\"/>", 4.0 + pw));
        s.push_str(&format!("<text x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"start\" style=\"fill:rgba(255,255,255,.62);font-size:10px;font-weight:600\">{label}</text>", 4.0 + pw + 5.0, ty + 3.5));
    }
    for (k, pts) in series.iter().enumerate() {
        let c = &colours[k];
        if pts.len() >= 2 {
            let p: Vec<String> = pts.iter().enumerate().map(|(i, v)| format!("{:.1},{:.1}", x(i), y(*v))).collect();
            let (_, sw, dash) = SERIES_STYLE[k];
            let dash = if dash.is_empty() { String::new() } else { format!(" stroke-dasharray=\"{dash}\"") };
            s.push_str(&format!("<polyline points=\"{}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"{sw}\"{dash} stroke-linejoin=\"round\" stroke-linecap=\"round\"/>", p.join(" ")));
        }
        let (lx, ly) = (x(pts.len() - 1), y(*pts.last().unwrap_or(&lo)));
        s.push_str(&format!("<circle cx=\"{lx:.1}\" cy=\"{ly:.1}\" r=\"3.5\" fill=\"{c}\" stroke=\"#000\" stroke-width=\"1.5\"/>"));
    }
    if ends {
        // End values, nudged apart when they would overlap.
        // A series that ends early (a judge not in yet) is named beside its own last point, not in the end column,
        // so its label never sits away from its dot (review, 4 Nov).
        let full = series.iter().map(Vec::len).max().unwrap_or(0);
        let tag = |k: usize, p: &Vec<f64>| (y(*p.last().unwrap_or(&lo)) + 3.5, fmt(*p.last().unwrap_or(&lo)), colours[k].clone(), names.get(k).cloned().unwrap_or_default());
        for (k, p) in series.iter().enumerate().filter(|(_, p)| !p.is_empty() && p.len() < full) {
            let (ty, v, c, n) = tag(k, p);
            let name = if n.is_empty() { n } else { format!("<tspan style=\"fill:{INK2};font-weight:500\">{n} </tspan>") };
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{ty:.1}\" style=\"fill:{c};font-size:10.5px;font-weight:600\">{name}{v}</text>", x(p.len() - 1) + 7.0));
        }
        let mut tags: Vec<(f64, String, String, String)> = series.iter().enumerate().filter(|(_, p)| p.len() == full).map(|(k, p)| tag(k, p)).collect();
        tags.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        for i in 1..tags.len() {
            if tags[i].0 - tags[i - 1].0 < 11.0 {
                tags[i].0 = tags[i - 1].0 + 11.0;
            }
        }
        for (ty, v, c, n) in tags {
            // The name in secondary grey, the value in its line's colour: the label is the legend.
            let name = if n.is_empty() { n } else { format!("<tspan style=\"fill:{INK2};font-weight:500\">{n} </tspan>") };
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{ty:.1}\" style=\"fill:{c};font-size:10.5px;font-weight:600\">{name}{v}</text>", w - right + 8.0));
        }
    }
    if !labels.is_empty() {
        let ly = h - 3.0;
        let first = labels[0];
        s.push_str(&format!("<text x=\"4\" y=\"{ly:.1}\" text-anchor=\"start\">{}</text>", esc(first)));
        if labels.len() > 1 {
            s.push_str(&format!("<text x=\"{:.1}\" y=\"{ly:.1}\" text-anchor=\"end\">{}</text>", 4.0 + pw, esc(labels[labels.len() - 1])));
        }
    }
    s.push_str("</svg>");
    s
}

fn bars(a: &[(String, String)]) -> String {
    let items: Vec<(String, f64)> = split(text(a, "items")).into_iter().filter_map(|it| {
        let (l, v) = it.rsplit_once(':')?;
        Some((l.trim().to_owned(), v.trim().replace(',', "").parse::<f64>().ok()?))
    }).take(MAX_BARS).collect();
    if items.is_empty() {
        return String::new();
    }
    let max = num(a, "max").unwrap_or_else(|| items.iter().map(|i| i.1).fold(0.0, f64::max)).max(1e-9);
    let unit = text(a, "unit").map(esc).unwrap_or_default();
    let colours = split(text(a, "colors").or(text(a, "colours")));
    let hl = text(a, "hl");
    let w = num(a, "w").map(|w| format!("width:{:.0}px;", w.clamp(40.0, 400.0))).unwrap_or_default();
    let mut s = format!("<div style=\"{w}display:grid;grid-template-columns:auto 1fr auto;gap:5px 9px;align-items:center\">");
    for (k, (l, v)) in items.iter().enumerate() {
        // Neutral bars; with `hl` and no colours named, the lit bar takes the one accent (blue).
        let lit = hl.is_some_and(|h| h == l);
        let c = match colours.get(k).or(colours.first()) {
            Some(v) => colour(v),
            None if lit => colour("blue"),
            None => INK2.into(),
        };
        let dim = hl.is_some() && !lit && !colours.is_empty();
        let pct = (v / max * 100.0).clamp(1.5, 100.0);
        let op = if dim { ";opacity:.38" } else { "" };
        s.push_str(&format!(
            "<span style=\"color:var(--ink2);font-size:12px\">{}</span><div style=\"height:7px;border-radius:4px;background:var(--fill)\"><div style=\"width:{pct:.1}%;height:100%;border-radius:4px;background:{c}{op}\"></div></div><span style=\"font-size:12px;font-weight:600;text-align:right\">{}{unit}</span>",
            esc(l), fmt(*v)
        ));
    }
    s.push_str("</div>");
    s
}

fn spark(a: &[(String, String)]) -> String {
    let v = nums(text(a, "values").unwrap_or(""));
    if v.len() < 2 {
        return String::new();
    }
    let w = num(a, "w").unwrap_or(80.0).clamp(20.0, 328.0);
    let h = num(a, "h").unwrap_or(20.0).clamp(8.0, 80.0);
    let c = colour(text(a, "color").or(text(a, "colour")).unwrap_or("ink"));
    let (lo, hi) = v.iter().fold((f64::INFINITY, f64::NEG_INFINITY), |(l, h), x| (l.min(*x), h.max(*x)));
    let span = (hi - lo).max(1e-9);
    let p: Vec<String> = v.iter().enumerate().map(|(i, x)| format!("{:.1},{:.1}", 2.0 + (w - 6.0) * i as f64 / (v.len() - 1) as f64, 2.0 + (h - 4.0) * (1.0 - (x - lo) / span))).collect();
    let last = p.last().cloned().unwrap_or_default();
    let (lx, ly) = last.split_once(',').unwrap_or(("0", "0"));
    format!("<svg width=\"{w:.0}\" height=\"{h:.0}\" viewBox=\"0 0 {w:.0} {h:.0}\" style=\"display:inline-block;vertical-align:middle\"><polyline points=\"{}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"1.6\" stroke-linejoin=\"round\" stroke-linecap=\"round\"/><circle cx=\"{lx}\" cy=\"{ly}\" r=\"2.2\" fill=\"{c}\"/></svg>", p.join(" "))
}

fn ring(a: &[(String, String)]) -> String {
    let v = num(a, "value").or_else(|| Some(num(a, "done")? / num(a, "total").filter(|t| *t > 0.0)?)).unwrap_or(0.0).clamp(0.0, 1.0);
    let size = num(a, "size").unwrap_or(44.0).clamp(16.0, 160.0);
    // Neutral while it fills; green once it is whole.
    let c = colour(text(a, "color").or(text(a, "colour")).unwrap_or(if v >= 1.0 { "green" } else { "ink" }));
    let sw = (size * 0.11).max(3.0);
    let r = (size - sw) / 2.0;
    let circ = 2.0 * std::f64::consts::PI * r;
    let label = text(a, "label").map(esc).unwrap_or_default();
    let fs = (size * 0.27).max(9.0);
    format!(
        "<svg width=\"{size:.0}\" height=\"{size:.0}\" viewBox=\"0 0 {size:.0} {size:.0}\" style=\"display:inline-block;vertical-align:middle\"><circle cx=\"{m:.1}\" cy=\"{m:.1}\" r=\"{r:.1}\" fill=\"none\" stroke=\"rgba(255,255,255,.12)\" stroke-width=\"{sw:.1}\"/><circle cx=\"{m:.1}\" cy=\"{m:.1}\" r=\"{r:.1}\" fill=\"none\" stroke=\"{c}\" stroke-width=\"{sw:.1}\" stroke-linecap=\"round\" stroke-dasharray=\"{d:.1} {circ:.1}\" transform=\"rotate(-90 {m:.1} {m:.1})\"/><text x=\"{m:.1}\" y=\"{ty:.1}\" text-anchor=\"middle\" style=\"fill:#fff;font-size:{fs:.0}px;font-weight:600\">{label}</text></svg>",
        m = size / 2.0,
        d = circ * v,
        ty = size / 2.0 + fs * 0.36
    )
}

fn dots(a: &[(String, String)]) -> String {
    let total = num(a, "total").unwrap_or(0.0).clamp(0.0, 40.0) as usize;
    if total == 0 {
        return String::new();
    }
    let done = num(a, "done").unwrap_or(0.0).clamp(0.0, total as f64) as usize;
    let now = num(a, "now").map(|n| n as usize);
    let c = colour(text(a, "color").or(text(a, "colour")).unwrap_or("ink"));
    let mut s = String::from("<span style=\"display:inline-flex;gap:4px;align-items:center;vertical-align:middle\">");
    for i in 1..=total {
        let style = if Some(i) == now {
            format!("width:8px;height:8px;border-radius:4px;background:{c};box-shadow:0 0 6px {c}")
        } else if i <= done {
            format!("width:6px;height:6px;border-radius:3px;background:{c};opacity:.55")
        } else {
            "width:6px;height:6px;border-radius:3px;border:1px solid rgba(255,255,255,.28)".into()
        };
        s.push_str(&format!("<i style=\"display:block;{style}\"></i>"));
    }
    s.push_str("</span>");
    s
}

/// `<x-steps items="Voice engine:done|Decider V6:now|Install:wait">`: the job's parts as one row of pills (4 Nov), each
/// with its state: done a green tick, the first `now` the one blue dot (later ones white), wait faint, failed a red
/// cross. At most 4 (3 with `size="l"`); the row never wraps: the lit one keeps its width, a long name elsewhere gives way
/// with an ellipsis.
fn steps(a: &[(String, String)]) -> String {
    let mut lit = false;
    // `size="l"`: the steps lead the body (a job with no measured number yet), a size up.
    let large = text(a, "size") == Some("l");
    let (fs, pad, gap, most) = if large { (13.0, "4px 10px", 7, 3) } else { (11.0, "2px 7px", 6, 4) };
    let mut s = format!("<div style=\"display:flex;flex-wrap:nowrap;gap:{gap}px;overflow:hidden\">");
    for it in split(text(a, "items")).into_iter().filter(|t| !t.is_empty()).take(most) {
        let (label, state) = it.rsplit_once(':').map_or((it, "wait"), |(l, st)| (l.trim(), st.trim()));
        let label: String = label.chars().take(18).collect();
        let keep = state == "now" && !lit;
        let (mark, bg, ink) = match state {
            "done" => ("<span style=\"color:#30D158\">\u{2713}</span> ".to_owned(), "rgba(255,255,255,.07)", INK),
            "failed" => ("<span style=\"color:#FF453A\">\u{2717}</span> ".to_owned(), "rgba(255,255,255,.07)", INK),
            "now" if !lit => {
                lit = true;
                ("<span style=\"color:#0A84FF\">\u{25CF}</span> ".to_owned(), "rgba(255,255,255,.16)", INK)
            }
            "now" => ("\u{25CF} ".to_owned(), "rgba(255,255,255,.12)", INK),
            _ => (String::new(), "rgba(255,255,255,.05)", INK3),
        };
        s.push_str(&format!("<span style=\"flex:0 {} auto;min-width:0;overflow:hidden;text-overflow:ellipsis;white-space:nowrap;padding:{pad};border-radius:12px;background:{bg};color:{ink};font-size:{fs}px;font-weight:500\">{mark}{}</span>", if keep { 0 } else { 1 }, esc(&label)));
    }
    // `note="target 8.5"`: one small grey fact at the row's end (a target before any score), never above it.
    if let Some(n) = text(a, "note").filter(|n| !n.trim().is_empty()) {
        s.push_str(&format!("<span style=\"margin-left:auto;flex:0 0 auto;align-self:center;padding-left:6px;color:{INK2};font-size:11px;font-weight:500;white-space:nowrap\">{}</span>", esc(&n.chars().take(24).collect::<String>())));
    }
    s.push_str("</div>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    fn at(v: &[(&str, &str)]) -> Vec<(String, String)> { v.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() }

    #[test]
    fn a_line_maps_values_into_its_box() {
        let s = expand("line", &at(&[("series", "0 10"), ("min", "0"), ("max", "10"), ("w", "104"), ("h", "50")])).unwrap();
        // x from 4 to 4 + (104 - 5 - 4) = 99; y from 5 + 41 = 46 (0) to 5 (10).
        assert!(s.contains("points=\"4.0,46.0 99.0,5.0\""), "{s}");
    }

    #[test]
    fn steps_light_one_pill_and_never_wrap() {
        let s = expand("steps", &at(&[("items", "Voice engine:done|Decider V6:now|Grants:now|Install:wait")])).unwrap();
        assert_eq!(s.matches("#0A84FF").count(), 1, "{s}");
        assert!(s.contains("nowrap") && s.contains('\u{2713}') && s.contains("Install"));
        let lead = expand("steps", &at(&[("items", "Brief:done|Judges:now"), ("size", "l")])).unwrap();
        assert!(lead.contains("font-size:13px") && s.contains("font-size:11px"));
    }

    #[test]
    fn a_series_that_ends_early_is_named_at_its_own_point() {
        let s = expand("line", &at(&[("series", "6.84 6.54|5.85"), ("names", "Opus|Sol"), ("ends", "1"), ("w", "200")])).unwrap();
        let sol = s.split("<text").find(|t| t.contains("Sol")).unwrap();
        let x: f64 = sol.split("x=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        assert!(x < 30.0, "Sol is named beside its one point at the left: {sol}");
    }

    #[test]
    fn target_and_ends_are_drawn_and_numbers_read_as_written() {
        let s = expand("line", &at(&[("series", "6.03 6.57 7.63|5 6 6.38"), ("target", "8.5"), ("ends", "1")])).unwrap();
        assert!(s.contains("stroke-dasharray=\"3 3\"") && s.contains(">8.5<") && s.contains(">7.63<") && s.contains(">6.38<"));
        assert_eq!(fmt(1905.0), "1,905");
        assert_eq!(fmt(0.5), "0.5");
    }

    #[test]
    fn bars_rings_dots_and_sparks_expand() {
        assert!(expand("bars", &at(&[("items", "Opus:5.79|Sol:6.38"), ("max", "10")])).unwrap().contains("width:57.9%"));
        assert!(expand("ring", &at(&[("done", "1"), ("total", "4"), ("label", "25%")])).unwrap().contains(">25%<"));
        assert_eq!(expand("dots", &at(&[("done", "2"), ("total", "4"), ("now", "3")])).unwrap().matches("<i ").count(), 4);
        assert!(expand("spark", &at(&[("values", "1 2 3")])).unwrap().contains("<polyline"));
        assert!(expand("nope", &[]).is_none());
        assert_eq!(colour("#ff453a"), "#FF453A");
        assert_eq!(colour("#BF5AF2"), INK2);
        assert_eq!(colour("url(x)"), INK);
    }

    /// neutrals by default, accents only when named; two series apart by weight, dash and end names.
    #[test]
    fn charts_are_neutral_unless_an_accent_is_named() {
        for hue in ["purple", "cyan", "pink", "teal", "indigo"] { assert_eq!(colour(hue), INK2, "{hue}") }
        let two = expand("line", &at(&[("series", "1 2|2 3"), ("names", "Opus|Sol"), ("ends", "1")])).unwrap();
        assert!(two.contains(&format!("stroke=\"{INK}\" stroke-width=\"2\" stroke-linejoin")), "{two}");
        assert!(two.contains(&format!("stroke=\"{INK2}\" stroke-width=\"1.6\" stroke-dasharray=\"5 3\"")), "{two}");
        assert!(two.contains(">Opus </tspan>2<") && two.contains(">Sol </tspan>3<"), "{two}");
        let lead = expand("line", &at(&[("series", "1 2|2 3"), ("colors", "|blue")])).unwrap();
        assert!(lead.contains(&format!("stroke=\"{INK}\"")) && lead.contains("stroke=\"#0A84FF\" stroke-width=\"1.6\" stroke-dasharray"), "{lead}");
        let two_accents = expand("line", &at(&[("series", "1 2|2 3"), ("colors", "blue|green")])).unwrap();
        assert!(two_accents.contains("#0A84FF") && !two_accents.contains("#30D158"), "{two_accents}");
        let gap = expand("line", &at(&[("series", "|1 2|2 3"), ("names", "None|Opus|Sol"), ("ends", "1")])).unwrap();
        assert!(gap.contains(">Opus </tspan>2<") && gap.contains(">Sol </tspan>3<"), "{gap}");
        let bars = expand("bars", &at(&[("items", "A:1|B:2"), ("hl", "B")])).unwrap();
        assert!(bars.contains(&format!("background:{INK2}\"")) && bars.contains("background:#0A84FF\"") && !bars.contains("opacity"), "{bars}");
        assert!(expand("ring", &at(&[("value", "1")])).unwrap().contains("#30D158"));
        for (tag, attrs) in [("ring", vec![("value", ".3")]), ("dots", vec![("done", "1"), ("total", "3")]), ("spark", vec![("values", "1 2")])] {
            let s = expand(tag, &at(&attrs)).unwrap();
            assert!(s.contains(INK) && !s.contains("#0A84FF"), "{tag}: {s}");
        }
    }
}
