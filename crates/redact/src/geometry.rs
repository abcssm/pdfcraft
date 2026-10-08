//! Cutting redaction regions out of vector paths.
//!
//! A path partly under a mark used to be kept whole behind a clip, so its coordinates (text
//! converted to outlines, a drawn signature) stayed in the file and came back by deleting the
//! clip. Instead the path is flattened (curves become short lines) in page space and the regions
//! are subtracted from it:
//! - fills: each subpath is a polygon, clipped against the four strips around each region (left,
//!   right, below, above). The strips are convex and don't overlap, so Sutherland–Hodgman
//!   clipping keeps the fill's winding and even-odd parity everywhere outside the regions;
//! - strokes: each line segment loses the part inside a region (Liang–Barsky).
//!
//! What comes back has no point inside any region, so nothing under a mark can be recovered.

use pdfcraft_content::{Matrix, Op, num};
use pdfcraft_cos::Object;

pub(crate) type Pt = (f64, f64);

/// One subpath in page space.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Subpath {
    pub pts: Vec<Pt>,
    pub closed: bool,
}

/// Paths with more points than this (after flattening) are removed rather than cut.
const MAX_POINTS: usize = 200_000;
/// Fills that split into more polygons than this are removed rather than cut.
const MAX_POLYGONS: usize = 4096;

/// The path built by `body` (`m l c v y h re`; `W`/`W*` are ignored) in page space, after
/// `ctm`. `None` when it is too large or has non-finite coordinates: callers remove it.
pub(crate) fn flatten(body: &[Op], ctm: &Matrix) -> Option<Vec<Subpath>> {
    let mut subs: Vec<Subpath> = Vec::new();
    let dev = |x: f64, y: f64| ctm.apply(x, y);
    // The current point and the start of the current subpath, in page space.
    let mut cur: Option<Pt> = None;
    let mut start: Option<Pt> = None;
    // After `h`, the next segment starts a new subpath at the closed one's start.
    let mut reopen = false;
    let mut total = 0usize;
    for op in body {
        let n: Vec<f64> = op.operands.iter().filter_map(Object::as_f64).collect();
        // Operators only append, to the last subpath or new ones: count what this one added.
        let last = subs.len().saturating_sub(1);
        let before: usize = subs.iter().skip(last).map(|s| s.pts.len()).sum();
        match (op.op.as_slice(), n.as_slice()) {
            (b"m", &[x, y]) => {
                let p = dev(x, y);
                begin(&mut subs, p);
                (cur, start, reopen) = (Some(p), Some(p), false);
            }
            (b"re", &[x, y, w, h]) => {
                let pts = vec![dev(x, y), dev(x + w, y), dev(x + w, y + h), dev(x, y + h)];
                subs.push(Subpath { pts, closed: true });
                (cur, start, reopen) = (Some(dev(x, y)), Some(dev(x, y)), true);
            }
            (b"h", _) => {
                if let Some(s) = subs.last_mut() {
                    s.closed = true;
                }
                cur = start;
                reopen = true;
            }
            (b"l", &[x, y]) => {
                let p = dev(x, y);
                let from = segment_start(&mut subs, &mut cur, &mut reopen, p);
                if from.is_some()
                    && let Some(s) = subs.last_mut()
                {
                    s.pts.push(p);
                }
                cur = Some(p);
            }
            (b"c", &[x1, y1, x2, y2, x3, y3]) => curve(&mut subs, &mut cur, &mut reopen, [Some(dev(x1, y1)), Some(dev(x2, y2))], dev(x3, y3)),
            // `v`: the first control point is the current point.
            (b"v", &[x2, y2, x3, y3]) => curve(&mut subs, &mut cur, &mut reopen, [None, Some(dev(x2, y2))], dev(x3, y3)),
            // `y`: the second control point is the end point.
            (b"y", &[x1, y1, x3, y3]) => curve(&mut subs, &mut cur, &mut reopen, [Some(dev(x1, y1)), None], dev(x3, y3)),
            // Malformed operands: a viewer ignores the operator, and so do we.
            _ => {}
        }
        let after: usize = subs.iter().skip(last).map(|s| s.pts.len()).sum();
        total = total.saturating_add(after.saturating_sub(before));
        if total > MAX_POINTS {
            return None;
        }
    }
    let finite = subs.iter().flat_map(|s| &s.pts).all(|p| p.0.is_finite() && p.1.is_finite());
    (finite && total <= MAX_POINTS).then_some(subs)
}

fn begin(subs: &mut Vec<Subpath>, p: Pt) {
    subs.push(Subpath { pts: vec![p], closed: false });
}

/// Where a segment ending at `to` starts: the current point (starting a new subpath after `h`),
/// or nowhere when there is no current point (the segment then begins a subpath at `to`).
fn segment_start(subs: &mut Vec<Subpath>, cur: &mut Option<Pt>, reopen: &mut bool, to: Pt) -> Option<Pt> {
    match *cur {
        None => {
            begin(subs, to);
            *cur = Some(to);
            None
        }
        Some(c) => {
            if *reopen || subs.is_empty() {
                begin(subs, c);
                *reopen = false;
            }
            Some(c)
        }
    }
}

/// Append a flattened cubic Bézier from the current point to `p3`. A missing first control
/// point is the current point (`v`); a missing second one is `p3` (`y`).
fn curve(subs: &mut Vec<Subpath>, cur: &mut Option<Pt>, reopen: &mut bool, controls: [Option<Pt>; 2], p3: Pt) {
    let Some(p0) = segment_start(subs, cur, reopen, p3) else {
        return;
    };
    let [c1, c2] = controls;
    let (p1, p2) = (c1.unwrap_or(p0), c2.unwrap_or(p3));
    let d = |a: Pt, b: Pt| ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
    let len = d(p0, p1) + d(p1, p2) + d(p2, p3);
    // About one line per 2 points of length, 4 to 64 per curve.
    let steps = if len.is_finite() { (len / 2.0).ceil().clamp(4.0, 64.0) as usize } else { 4 };
    if let Some(s) = subs.last_mut() {
        for i in 1..=steps {
            let t = i as f64 / steps as f64;
            let u = 1.0 - t;
            let (a, b, c, e) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
            s.pts.push((a * p0.0 + b * p1.0 + c * p2.0 + e * p3.0, a * p0.1 + b * p1.1 + c * p2.1 + e * p3.1));
        }
    }
    *cur = Some(p3);
}

/// One half-plane: `axis` 0 = x, 1 = y; keeps points with coordinate ≥ `bound` when `keep_ge`,
/// else ≤ `bound`.
#[derive(Clone, Copy)]
struct Half {
    axis: u8,
    bound: f64,
    keep_ge: bool,
}

impl Half {
    fn value(&self, p: Pt) -> f64 {
        if self.axis == 0 { p.0 } else { p.1 }
    }

    fn inside(&self, p: Pt) -> bool {
        if self.keep_ge { self.value(p) >= self.bound } else { self.value(p) <= self.bound }
    }

    /// Where segment `a`–`b` crosses the boundary (one end inside, the other outside).
    fn cross(&self, a: Pt, b: Pt) -> Pt {
        let (va, vb) = (self.value(a), self.value(b));
        let t = if (vb - va).abs() > f64::EPSILON { (self.bound - va) / (vb - va) } else { 0.0 };
        (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t)
    }
}

/// The four disjoint convex strips that make up everything outside `r`.
fn outside(r: [f64; 4]) -> [Vec<Half>; 4] {
    let h = |axis, bound, keep_ge| Half { axis, bound, keep_ge };
    [
        vec![h(0, r[0], false)],
        vec![h(0, r[2], true)],
        vec![h(0, r[0], true), h(0, r[2], false), h(1, r[1], false)],
        vec![h(0, r[0], true), h(0, r[2], false), h(1, r[3], true)],
    ]
}

/// Sutherland–Hodgman: `poly` (implicitly closed) clipped to one half-plane.
fn clip_half(poly: &[Pt], half: Half) -> Vec<Pt> {
    let mut out = Vec::with_capacity(poly.len() + 2);
    let Some(&last) = poly.last() else { return out };
    let mut a = last;
    for &b in poly {
        match (half.inside(a), half.inside(b)) {
            (true, true) => out.push(b),
            (true, false) => out.push(half.cross(a, b)),
            (false, true) => {
                out.push(half.cross(a, b));
                out.push(b);
            }
            (false, false) => {}
        }
        a = b;
    }
    out
}

fn bbox(pts: &[Pt]) -> [f64; 4] {
    pts.iter().fold([f64::MAX, f64::MAX, f64::MIN, f64::MIN], |b, p| [b[0].min(p.0), b[1].min(p.1), b[2].max(p.0), b[3].max(p.1)])
}

fn overlaps(a: [f64; 4], b: [f64; 4]) -> bool {
    a[0] < b[2] && b[0] < a[2] && a[1] < b[3] && b[1] < a[3]
}

/// The fill area of `subs` minus the regions, as polygons (each implicitly closed). `None` when
/// it splits into too many pieces: the caller removes the fill.
pub(crate) fn cut_fill(subs: &[Subpath], rects: &[[f64; 4]]) -> Option<Vec<Vec<Pt>>> {
    let mut out: Vec<Vec<Pt>> = Vec::new();
    for s in subs.iter().filter(|s| s.pts.len() >= 3) {
        let mut polys = vec![s.pts.clone()];
        for &r in rects {
            let mut next = Vec::with_capacity(polys.len());
            for p in polys {
                if !overlaps(bbox(&p), r) {
                    next.push(p);
                    continue;
                }
                for strip in outside(r) {
                    let q = strip.iter().fold(p.clone(), |q, &h| clip_half(&q, h));
                    if q.len() >= 3 {
                        next.push(q);
                    }
                }
            }
            if next.len() > MAX_POLYGONS {
                return None;
            }
            polys = next;
        }
        out.extend(polys);
        if out.len() > MAX_POLYGONS {
            return None;
        }
    }
    Some(out)
}

/// The part of segment `a`–`b` inside `r`, as a parameter range (Liang–Barsky), if any.
fn inside_range(a: Pt, b: Pt, r: [f64; 4]) -> Option<(f64, f64)> {
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let (mut t0, mut t1) = (0.0f64, 1.0f64);
    for (p, q) in [(-dx, a.0 - r[0]), (dx, r[2] - a.0), (-dy, a.1 - r[1]), (dy, r[3] - a.1)] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else {
            let t = q / p;
            if p < 0.0 {
                t0 = t0.max(t);
            } else {
                t1 = t1.min(t);
            }
        }
    }
    (t0 <= t1).then_some((t0, t1))
}

/// The parameter ranges of `a`–`b` outside every region, in order.
fn kept_ranges(a: Pt, b: Pt, rects: &[[f64; 4]]) -> Vec<(f64, f64)> {
    let mut cut: Vec<(f64, f64)> = rects.iter().filter_map(|&r| inside_range(a, b, r)).collect();
    cut.sort_by(|x, y| x.0.total_cmp(&y.0));
    let mut kept = Vec::new();
    let mut at = 0.0;
    for (c0, c1) in cut {
        if c0 > at {
            kept.push((at, c0));
        }
        at = f64::max(at, c1);
    }
    if at < 1.0 {
        kept.push((at, 1.0));
    }
    kept
}

fn lerp(a: Pt, b: Pt, t: f64) -> Pt {
    if t <= 0.0 { a } else { (a.0 + (b.0 - a.0) * t, a.1 + (b.1 - a.1) * t) }
}

/// The stroked lines of `subs` minus the regions, as open polylines. `close_all`: `s`/`b`/`b*`
/// close every subpath before stroking.
pub(crate) fn cut_stroke(subs: &[Subpath], rects: &[[f64; 4]], close_all: bool) -> Vec<Vec<Pt>> {
    let mut lines: Vec<Vec<Pt>> = Vec::new();
    let mut flush = |cur: &mut Vec<Pt>| {
        let line = std::mem::take(cur);
        if line.len() >= 2 {
            lines.push(line);
        }
    };
    for s in subs {
        let mut pts = s.pts.clone();
        if (s.closed || close_all)
            && pts.len() > 1
            && let Some(&first) = pts.first()
        {
            pts.push(first);
        }
        // `cur` is the polyline being built; it always ends at the start of the next segment.
        let mut cur: Vec<Pt> = Vec::new();
        for w in pts.windows(2) {
            let &[a, b] = w else { continue };
            let mut reached_end = false;
            for (t0, t1) in kept_ranges(a, b, rects) {
                if t0 > 0.0 || cur.is_empty() {
                    flush(&mut cur);
                    cur.push(lerp(a, b, t0));
                }
                reached_end = t1 >= 1.0;
                cur.push(if reached_end { b } else { lerp(a, b, t1) });
                if !reached_end {
                    flush(&mut cur);
                }
            }
            if !reached_end {
                flush(&mut cur);
            }
        }
        flush(&mut cur);
    }
    lines
}

/// Does any painted line of `subs` (strokes, or fill edges) run inside a region for more than a
/// hundredth of a point? `rects` should be shrunk slightly, so geometry cut exactly along a
/// region's edge doesn't count.
pub(crate) fn crosses(subs: &[Subpath], rects: &[[f64; 4]]) -> bool {
    subs.iter().any(|s| {
        let mut pts = s.pts.clone();
        if pts.len() > 2
            && let Some(&first) = pts.first()
        {
            pts.push(first);
        }
        pts.windows(2).any(|w| {
            let &[a, b] = w else { return false };
            let len = ((b.0 - a.0).powi(2) + (b.1 - a.1).powi(2)).sqrt();
            rects.iter().filter_map(|&r| inside_range(a, b, r)).any(|(t0, t1)| (t1 - t0) * len > 0.01)
        })
    })
}

/// Operators that build `polys` (closed with `h` when `close`) in the user space of `inv` (the
/// inverse of the CTM).
pub(crate) fn path_ops(polys: &[Vec<Pt>], inv: &Matrix, close: bool) -> Vec<Op> {
    let mut ops = Vec::new();
    for poly in polys {
        for (i, &(x, y)) in poly.iter().enumerate() {
            let (u, v) = inv.apply(x, y);
            ops.push(Op::new(if i == 0 { "m" } else { "l" }, vec![num(u), num(v)]));
        }
        if close {
            ops.push(Op::new("h", vec![]));
        }
    }
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdfcraft_content::parse;

    fn subs(src: &str) -> Vec<Subpath> {
        flatten(&parse(src.as_bytes()).ops, &Matrix::IDENTITY).unwrap()
    }

    fn inside_any(p: Pt, rects: &[[f64; 4]]) -> bool {
        rects.iter().any(|r| p.0 > r[0] + 1e-9 && p.0 < r[2] - 1e-9 && p.1 > r[1] + 1e-9 && p.1 < r[3] - 1e-9)
    }

    #[test]
    fn a_rectangle_crossed_by_a_region_becomes_the_parts_outside_it() {
        // A 0–100 square with a 40–60 band through the middle (full height and beyond).
        let r = [[40.0, -10.0, 60.0, 110.0]];
        let polys = cut_fill(&subs("0 0 100 100 re"), &r).unwrap();
        assert_eq!(polys.len(), 2, "{polys:?}");
        let area = |p: &Vec<Pt>| {
            let mut a = 0.0;
            for (i, &(x0, y0)) in p.iter().enumerate() {
                let (x1, y1) = p[(i + 1) % p.len()];
                a += x0 * y1 - x1 * y0;
            }
            (a / 2.0).abs()
        };
        let total: f64 = polys.iter().map(area).sum();
        assert!((total - 8000.0).abs() < 1e-6, "{total}");
        assert!(polys.iter().flatten().all(|&p| !inside_any(p, &r)));
    }

    #[test]
    fn a_hole_in_the_middle_keeps_the_frame() {
        let r = [[40.0, 40.0, 60.0, 60.0]];
        let polys = cut_fill(&subs("0 0 100 100 re"), &r).unwrap();
        assert_eq!(polys.len(), 4, "left, right, below, above");
        assert!(polys.iter().flatten().all(|&p| !inside_any(p, &r)));
    }

    #[test]
    fn strokes_lose_exactly_the_covered_part() {
        let r = [[40.0, -1.0, 60.0, 1.0]];
        let lines = cut_stroke(&subs("0 0 m 100 0 l"), &r, false);
        let close = |a: &[Pt], b: &[Pt]| a.len() == b.len() && a.iter().zip(b).all(|(p, q)| (p.0 - q.0).abs() < 1e-9 && (p.1 - q.1).abs() < 1e-9);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(close(&lines[0], &[(0.0, 0.0), (40.0, 0.0)]) && close(&lines[1], &[(60.0, 0.0), (100.0, 0.0)]), "{lines:?}");
        // A polyline whose middle vertex is under the region, and one that never touches it.
        let lines = cut_stroke(&subs("0 0 m 50 0 l 100 0 l 0 50 m 100 50 l"), &r, false);
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(lines.iter().flatten().all(|&p| !inside_any(p, &r)));
    }

    #[test]
    fn curves_are_flattened_and_cut() {
        // A circle-ish closed curve around (50, 50), cut by a region over its right half.
        let s = subs("100 50 m 100 77.6 77.6 100 50 100 c 22.4 100 0 77.6 0 50 c 0 22.4 22.4 0 50 0 c 77.6 0 100 22.4 100 50 c h");
        assert_eq!(s.len(), 1);
        assert!(s[0].pts.len() > 16 && s[0].closed);
        let r = [[50.0, -10.0, 110.0, 110.0]];
        let fill = cut_fill(&s, &r).unwrap();
        assert!(!fill.is_empty() && fill.iter().flatten().all(|&p| !inside_any(p, &r)));
        let stroke = cut_stroke(&s, &r, false);
        assert!(!stroke.is_empty() && stroke.iter().flatten().all(|&p| !inside_any(p, &r)));
        assert!(!crosses(&[Subpath { pts: stroke.concat(), closed: false }], &r));
        assert!(crosses(&s, &r), "the uncut circle runs through the region");
    }

    #[test]
    fn hostile_paths_are_refused_not_cut() {
        // Coordinates that overflow to infinity in page space.
        assert!(flatten(&parse(b"0 0 m 1000 0 l").ops, &Matrix([1e307, 0.0, 0.0, 1e307, 0.0, 0.0])).is_none());
        let huge = "0 0 m 1 1 l ".repeat(MAX_POINTS);
        assert!(flatten(&parse(huge.as_bytes()).ops, &Matrix::IDENTITY).is_none());
        // Many regions over one fill: refused once it shatters into too many pieces.
        let rects: Vec<[f64; 4]> = (0..400).map(|i| [i as f64 * 0.25, 0.0, i as f64 * 0.25 + 0.1, 0.1]).collect();
        let s = subs("0 -10 100 120 re");
        assert!(cut_fill(&s, &rects).is_none_or(|p| p.len() <= MAX_POLYGONS));
    }

    #[test]
    fn malformed_operands_are_ignored() {
        let s = subs("0 0 m 5 l 10 10 20 c 10 10 l");
        assert_eq!(s, vec![Subpath { pts: vec![(0.0, 0.0), (10.0, 10.0)], closed: false }]);
    }
}
