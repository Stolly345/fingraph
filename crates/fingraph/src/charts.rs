//! Native charts: a `GtkDrawingArea` painted with Cairo. Hovering shows a tooltip and highlights
//! the element; clicking reports its index (the donut uses it to drill into merchants).
//! Values arrive as f64 for geometry only; every number a person reads comes pre-formatted from Decimal.

use gtk::{cairo, gdk, glib};
use gtk::prelude::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::f64::consts::PI;
use std::rc::Rc;

pub type Rgb = (f64, f64, f64);

/// libadwaita palette, readable on light and dark.
pub const BLUE: Rgb = (0.21, 0.52, 0.89);
pub const GREEN: Rgb = (0.18, 0.76, 0.49);
pub const RED: Rgb = (0.88, 0.11, 0.14);
pub const ORANGE: Rgb = (1.0, 0.47, 0.0);
pub const PURPLE: Rgb = (0.57, 0.25, 0.67);
pub const YELLOW: Rgb = (0.96, 0.76, 0.07);
pub const TEAL: Rgb = (0.13, 0.56, 0.64);
pub const PINK: Rgb = (0.86, 0.38, 0.66);
pub const BROWN: Rgb = (0.6, 0.42, 0.25);
pub const SLATE: Rgb = (0.45, 0.5, 0.56);
pub const PALETTE: [Rgb; 10] = [BLUE, ORANGE, GREEN, PURPLE, RED, TEAL, YELLOW, PINK, BROWN, SLATE];

pub struct Series {
    pub name: String,
    pub values: Vec<f64>,
    pub color: Rgb,
}

pub struct LineSeries {
    pub name: String,
    pub points: Vec<(f64, f64)>,
    pub color: Rgb,
    pub dashed: bool,
}

/// (low, high) polygon edges for a shaded band.
pub type Band = (Vec<(f64, f64)>, Vec<(f64, f64)>);
pub type Formatter = Rc<dyn Fn(f64) -> String>;
type ClickFn = Rc<dyn Fn(usize)>;

pub enum Data {
    Empty(String),
    /// Grouped bars per label, plus an optional line on the same axis; one tooltip per group.
    Bars { labels: Vec<String>, series: Vec<Series>, line: Option<Series>, tips: Vec<String> },
    /// One stacked bar per label; `series` bottom to top. The Trends tab's breakdown is its legend.
    Stacked { labels: Vec<String>, series: Vec<Series>, tips: Vec<String> },
    Donut { items: Vec<(String, f64)>, tips: Vec<String>, center: String },
    HBars { items: Vec<(String, f64)>, values: Vec<String>, tips: Vec<String> },
    /// x is numeric; `band` = (low, high) polygon; `stops` = hover points (x, tooltip).
    Lines { x_labels: Vec<(f64, String)>, series: Vec<LineSeries>, band: Option<Band>, stops: Vec<(f64, String)> },
    /// Bars descending plus the cumulative share (0–100) on a right-hand axis.
    Pareto { items: Vec<(String, f64)>, cum_pct: Vec<f64>, tips: Vec<String> },
    /// Where money came from (left) and went (right), through one bar in the middle; bands are
    /// sized by amount. Both sides should add up to the same total. Hits: left 0…, then right.
    Flow { left: Vec<(String, f64, Rgb)>, right: Vec<(String, f64, Rgb)>, tips: Vec<String> },
}

enum Shape {
    Rect(f64, f64, f64, f64),
    Arc { cx: f64, cy: f64, r0: f64, r1: f64, a0: f64, a1: f64 },
    XBand(f64, f64),
}

struct Hit {
    shape: Shape,
    index: usize,
    tip: String,
}

#[derive(Clone)]
pub struct Chart {
    pub widget: gtk::DrawingArea,
    data: Rc<RefCell<Data>>,
    hits: Rc<RefCell<Vec<Hit>>>,
    hover: Rc<Cell<Option<usize>>>,
    hover_x: Rc<Cell<f64>>,
    click: Rc<RefCell<Option<ClickFn>>>,
    /// Axis label formatter (compact money).
    pub axis: Rc<RefCell<Formatter>>,
}

pub fn compact(v: f64) -> String {
    let a = v.abs();
    let s = if a >= 1_000_000.0 {
        format!("{:.1}M", a / 1_000_000.0)
    } else if a >= 10_000.0 {
        format!("{:.0}k", a / 1000.0)
    } else if a >= 1000.0 {
        format!("{:.1}k", a / 1000.0)
    } else {
        format!("{a:.0}")
    };
    if v < 0.0 { format!("-{s}") } else { s }
}

fn nice_step(range: f64, ticks: f64) -> f64 {
    if range <= 0.0 {
        return 1.0;
    }
    let raw = range / ticks;
    let mag = 10f64.powf(raw.log10().floor());
    let n = raw / mag;
    (if n < 1.5 { 1.0 } else if n < 3.0 { 2.0 } else if n < 7.0 { 5.0 } else { 10.0 }) * mag
}

impl Chart {
    pub fn new(height: i32) -> Chart {
        let widget = gtk::DrawingArea::builder().content_height(height).hexpand(true).has_tooltip(true).build();
        let c = Chart {
            widget,
            data: Rc::new(RefCell::new(Data::Empty("No data yet".into()))),
            hits: Rc::new(RefCell::new(Vec::new())),
            hover: Rc::new(Cell::new(None)),
            hover_x: Rc::new(Cell::new(-1.0)),
            click: Rc::new(RefCell::new(None)),
            axis: Rc::new(RefCell::new(Rc::new(compact))),
        };
        {
            let c2 = c.clone();
            c.widget.set_draw_func(move |w, cr, width, height| {
                let fg = w.color();
                let fg = (fg.red() as f64, fg.green() as f64, fg.blue() as f64);
                let mut hits = Vec::new();
                let axis = c2.axis.borrow().clone();
                draw(cr, width as f64, height as f64, fg, &c2.data.borrow(), c2.hover.get(), c2.hover_x.get(), &mut hits, &*axis);
                *c2.hits.borrow_mut() = hits;
            });
        }
        let motion = gtk::EventControllerMotion::new();
        {
            let c2 = c.clone();
            motion.connect_motion(move |_, x, y| {
                let hit = c2.hit(x, y);
                let idx = hit.as_ref().map(|h| h.0);
                c2.hover_x.set(x);
                if idx != c2.hover.get() || matches!(&*c2.data.borrow(), Data::Lines { .. }) {
                    c2.hover.set(idx);
                    c2.widget.set_tooltip_text(hit.map(|h| h.1).as_deref());
                    c2.widget.queue_draw();
                }
            });
        }
        {
            let c2 = c.clone();
            motion.connect_leave(move |_| {
                c2.hover.set(None);
                c2.hover_x.set(-1.0);
                c2.widget.set_tooltip_text(None);
                c2.widget.queue_draw();
            });
        }
        c.widget.add_controller(motion);
        let click = gtk::GestureClick::new();
        {
            let c2 = c.clone();
            click.connect_released(move |_, _, x, y| {
                if let Some((i, _)) = c2.hit(x, y) {
                    let f = c2.click.borrow().clone();
                    if let Some(f) = f {
                        f(i);
                    }
                }
            });
        }
        c.widget.add_controller(click);
        // the keyboard does what the mouse does: Tab in, ←/→ pick a bar or slice, Enter opens it
        let keys = gtk::EventControllerKey::new();
        {
            let c2 = c.clone();
            keys.connect_key_pressed(move |_, key, _, _| {
                let mut ids: Vec<usize> = c2.hits.borrow().iter().map(|h| h.index).collect();
                ids.sort_unstable();
                ids.dedup();
                let (Some(&first), Some(&last)) = (ids.first(), ids.last()) else { return glib::Propagation::Proceed };
                let now = c2.hover.get();
                let pick = match key {
                    gdk::Key::Right | gdk::Key::Down => Some(now.and_then(|n| ids.iter().copied().find(|i| *i > n)).unwrap_or(if now.is_some() { last } else { first })),
                    gdk::Key::Left | gdk::Key::Up => Some(now.and_then(|n| ids.iter().copied().rev().find(|i| *i < n)).unwrap_or(first)),
                    gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::space => {
                        let f = c2.click.borrow().clone();
                        if let (Some(f), Some(n)) = (f, now) {
                            f(n);
                        }
                        return glib::Propagation::Stop;
                    }
                    _ => None,
                };
                let Some(i) = pick else { return glib::Propagation::Proceed };
                c2.hover.set(Some(i));
                let tip = c2.hits.borrow().iter().find(|h| h.index == i).map(|h| h.tip.clone()).unwrap_or_default();
                c2.widget.set_tooltip_text(Some(&tip));
                c2.widget.update_property(&[gtk::accessible::Property::Description(&tip)]);
                c2.widget.queue_draw();
                glib::Propagation::Stop
            });
        }
        c.widget.add_controller(keys);
        c
    }

    fn hit(&self, x: f64, y: f64) -> Option<(usize, String)> {
        self.hits.borrow().iter().find(|h| match h.shape {
            Shape::Rect(rx, ry, rw, rh) => x >= rx && x <= rx + rw && y >= ry && y <= ry + rh,
            Shape::XBand(a, b) => x >= a && x < b,
            Shape::Arc { cx, cy, r0, r1, a0, a1 } => {
                let (dx, dy) = (x - cx, y - cy);
                let r = (dx * dx + dy * dy).sqrt();
                let mut a = dy.atan2(dx);
                while a < a0 {
                    a += 2.0 * PI;
                }
                r >= r0 && r <= r1 && a <= a1
            }
        }).map(|h| (h.index, h.tip.clone()))
    }

    pub fn set(&self, d: Data) {
        // what a screen reader says on reaching the chart, before any arrow key: the same numbers as the tooltips
        self.widget.update_property(&[gtk::accessible::Property::Description(&summary(&d))]);
        *self.data.borrow_mut() = d;
        self.hover.set(None);
        self.widget.set_cursor_from_name(if self.click.borrow().is_some() { Some("pointer") } else { None });
        self.widget.queue_draw();
    }

    pub fn on_click(&self, f: impl Fn(usize) + 'static) {
        *self.click.borrow_mut() = Some(Rc::new(f));
        self.widget.set_cursor_from_name(Some("pointer"));
        self.widget.set_focusable(true);
        self.widget.set_focus_on_click(false);
    }
}

/// The chart in words, from its tooltips (one line each, the first dozen). Arrow keys read the rest.
fn summary(d: &Data) -> String {
    let tips: Vec<String> = match d {
        Data::Empty(msg) => return msg.clone(),
        Data::Bars { tips, .. } | Data::Stacked { tips, .. } | Data::Donut { tips, .. } | Data::HBars { tips, .. } | Data::Pareto { tips, .. } | Data::Flow { tips, .. } => tips.clone(),
        Data::Lines { stops, .. } => stops.iter().map(|s| s.1.clone()).collect(),
    };
    let lines: Vec<String> = tips.iter().take(12).map(|t| t.lines().filter(|l| !l.starts_with("Click")).collect::<Vec<_>>().join(", ")).collect();
    let more = if tips.len() > 12 { format!(". {} more: use the arrow keys", tips.len() - 12) } else { String::new() };
    format!("{}{more}", lines.join(". "))
}

/// A category's colour: its place in the palette, except Uncategorized, which is missing data rather
/// than a kind of spending, so it's a faint wash of the text colour and never the loudest slice.
fn swatch(name: &str, i: usize, fg: Rgb) -> (Rgb, f64) {
    if name == "Uncategorized" {
        return (fg, 0.3);
    }
    (category_color(name).unwrap_or(PALETTE[i % PALETTE.len()]), 1.0)
}

thread_local! {
    static CATEGORY_COLORS: RefCell<HashMap<String, Rgb>> = RefCell::default();
}

/// The colour each category wears, so it's the same slice, bar and tile on every screen.
pub fn set_category_colors(m: HashMap<String, Rgb>) {
    CATEGORY_COLORS.with(|c| *c.borrow_mut() = m);
}

/// A category's own colour, if `name` is one.
pub fn category_color(name: &str) -> Option<Rgb> {
    CATEGORY_COLORS.with(|c| c.borrow().get(name).copied())
}

fn rgb(cr: &cairo::Context, c: Rgb, a: f64) {
    cr.set_source_rgba(c.0, c.1, c.2, a);
}

fn text(cr: &cairo::Context, s: &str, x: f64, y: f64, align: f64) {
    if let Ok(e) = cr.text_extents(s) {
        cr.move_to(x - e.width() * align - e.x_bearing(), y);
        let _ = cr.show_text(s);
    }
}

fn ellipsize(cr: &cairo::Context, s: &str, max: f64) -> String {
    if cr.text_extents(s).is_ok_and(|e| e.width() <= max) {
        return s.to_string();
    }
    let mut t: String = s.to_string();
    while t.chars().count() > 1 {
        t.pop();
        let c = format!("{t}…");
        if cr.text_extents(&c).is_ok_and(|e| e.width() <= max) {
            return c;
        }
    }
    t
}

/// The plot area: top, bottom, left, right.
type Area = (f64, f64, f64, f64);

/// Y axis with grid lines; returns the value→y mapping.
fn y_axis(cr: &cairo::Context, fg: Rgb, lo: f64, hi: f64, (top, bottom, left, right): Area, fmt: &dyn Fn(f64) -> String) -> impl Fn(f64) -> f64 {
    let step = nice_step(hi - lo, 4.0);
    let lo = (lo / step).floor() * step;
    let hi = ((hi / step).ceil() * step).max(lo + step);
    let map = move |v: f64| bottom - (v - lo) / (hi - lo) * (bottom - top);
    cr.set_line_width(1.0);
    let mut v = lo;
    while v <= hi + step * 0.01 {
        let y = map(v).round() + 0.5;
        rgb(cr, fg, if v.abs() < step * 0.01 { 0.35 } else { 0.10 });
        cr.move_to(left, y);
        cr.line_to(right, y);
        let _ = cr.stroke();
        rgb(cr, fg, 0.6);
        text(cr, &fmt(v), left - 6.0, y + 4.0, 1.0);
        v += step;
    }
    map
}

fn legend(cr: &cairo::Context, fg: Rgb, items: &[(&str, Rgb)], right: f64, y: f64) {
    let width = |s: &str| cr.text_extents(s).map_or(40.0, |e| e.x_advance());
    // every line keeps its key: out of room, each name is shortened to an even share instead of dropped
    let room = right - 60.0;
    let need: f64 = items.iter().map(|(n, _)| width(n) + 28.0).sum();
    let cap = if need > room && !items.is_empty() { (room / items.len() as f64 - 28.0).max(24.0) } else { f64::MAX };
    let mut x = right;
    for (name, c) in items.iter().rev() {
        let name = if width(name) > cap { ellipsize(cr, name, cap) } else { name.to_string() };
        let w = width(&name);
        x -= w;
        rgb(cr, fg, 0.75);
        text(cr, &name, x, y + 4.0, 0.0);
        x -= 14.0;
        rgb(cr, *c, 1.0);
        cr.rectangle(x, y - 4.0, 9.0, 9.0);
        let _ = cr.fill();
        x -= 14.0;
    }
}

#[allow(clippy::too_many_arguments)]
fn draw(cr: &cairo::Context, w: f64, h: f64, fg: Rgb, data: &Data, hover: Option<usize>, hover_x: f64, hits: &mut Vec<Hit>, fmt: &dyn Fn(f64) -> String) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Normal);
    cr.set_font_size(11.0);
    let (left, right, top, bottom) = (58.0, w - 14.0, 26.0, h - 26.0);
    match data {
        Data::Empty(msg) => {
            rgb(cr, fg, 0.5);
            text(cr, msg, w / 2.0, h / 2.0, 0.5);
        }
        Data::Bars { labels, series, line, tips } => {
            let n = labels.len().max(1) as f64;
            let all = series.iter().flat_map(|s| s.values.iter()).chain(line.iter().flat_map(|l| l.values.iter())).copied();
            let (lo, hi) = all.fold((0.0f64, 0.0f64), |(a, b), v| (a.min(v), b.max(v)));
            let y = y_axis(cr, fg, lo, if hi == lo { lo + 1.0 } else { hi }, (top, bottom, left, right), fmt);
            let gw = (right - left) / n;
            let bw = (gw * 0.72 / series.len().max(1) as f64).min(28.0);
            let zero = y(0.0);
            for (gi, label) in labels.iter().enumerate() {
                let gx = left + gi as f64 * gw;
                if hover == Some(gi) {
                    rgb(cr, fg, 0.06);
                    cr.rectangle(gx, top, gw, bottom - top);
                    let _ = cr.fill();
                }
                let total_w = bw * series.len() as f64;
                for (si, s) in series.iter().enumerate() {
                    let v = s.values.get(gi).copied().unwrap_or(0.0);
                    let x = gx + (gw - total_w) / 2.0 + si as f64 * bw;
                    let yv = y(v);
                    rgb(cr, s.color, if hover.is_none() || hover == Some(gi) { 0.9 } else { 0.55 });
                    cr.rectangle(x + 1.0, yv.min(zero), bw - 2.0, (yv - zero).abs().max(0.5));
                    let _ = cr.fill();
                }
                rgb(cr, fg, 0.6);
                if n <= 14.0 || gi % 2 == 0 {
                    text(cr, label, gx + gw / 2.0, bottom + 16.0, 0.5);
                }
                hits.push(Hit { shape: Shape::XBand(gx, gx + gw), index: gi, tip: tips.get(gi).cloned().unwrap_or_default() });
            }
            if let Some(l) = line {
                rgb(cr, l.color, 1.0);
                cr.set_line_width(2.5);
                for (i, v) in l.values.iter().enumerate() {
                    let (x, yy) = (left + (i as f64 + 0.5) * gw, y(*v));
                    if i == 0 { cr.move_to(x, yy) } else { cr.line_to(x, yy) }
                }
                let _ = cr.stroke();
                for (i, v) in l.values.iter().enumerate() {
                    cr.arc(left + (i as f64 + 0.5) * gw, y(*v), if hover == Some(i) { 5.0 } else { 3.5 }, 0.0, 2.0 * PI);
                    let _ = cr.fill();
                }
            }
            let mut leg: Vec<(&str, Rgb)> = series.iter().map(|s| (s.name.as_str(), s.color)).collect();
            if let Some(l) = line {
                leg.push((l.name.as_str(), l.color));
            }
            legend(cr, fg, &leg, right, 10.0);
        }
        Data::Stacked { labels, series, tips } => {
            let n = labels.len().max(1) as f64;
            let totals: Vec<f64> = (0..labels.len()).map(|i| series.iter().map(|s| s.values.get(i).copied().unwrap_or(0.0)).sum()).collect();
            let hi = totals.iter().copied().fold(0.0, f64::max);
            let y = y_axis(cr, fg, 0.0, if hi > 0.0 { hi } else { 1.0 }, (top, bottom, left, right), fmt);
            let gw = (right - left) / n;
            let bw = (gw * 0.68).min(46.0);
            for (gi, label) in labels.iter().enumerate() {
                let gx = left + gi as f64 * gw;
                if hover == Some(gi) {
                    rgb(cr, fg, 0.06);
                    cr.rectangle(gx, top, gw, bottom - top);
                    let _ = cr.fill();
                }
                let mut base = 0.0;
                for s in series {
                    let v = s.values.get(gi).copied().unwrap_or(0.0);
                    if v <= 0.0 {
                        continue;
                    }
                    let (y0, y1) = (y(base), y(base + v));
                    rgb(cr, s.color, if hover.is_none() || hover == Some(gi) { 0.95 } else { 0.6 });
                    cr.rectangle(gx + (gw - bw) / 2.0, y1, bw, (y0 - y1 - 1.0).max(0.5));
                    let _ = cr.fill();
                    base += v;
                }
                rgb(cr, fg, 0.6);
                text(cr, label, gx + gw / 2.0, bottom + 16.0, 0.5);
                hits.push(Hit { shape: Shape::XBand(gx, gx + gw), index: gi, tip: tips.get(gi).cloned().unwrap_or_default() });
            }
        }
        Data::Donut { items, tips, center } => {
            let total: f64 = items.iter().map(|i| i.1).sum();
            let (cx, cy) = (h / 2.0 + 8.0, h / 2.0);
            let r1 = h / 2.0 - 10.0;
            let r0 = r1 * 0.58;
            let mut a = -PI / 2.0;
            for (i, (name, v)) in items.iter().enumerate() {
                let da = if total > 0.0 { v / total * 2.0 * PI } else { 0.0 };
                let grow = if hover == Some(i) { 6.0 } else { 0.0 };
                let (c, k) = swatch(name, i, fg);
                rgb(cr, c, k * if hover.is_none() || hover == Some(i) { 1.0 } else { 0.6 });
                cr.new_path();
                cr.arc(cx, cy, r1 + grow, a, a + da);
                cr.arc_negative(cx, cy, r0, a + da, a);
                cr.close_path();
                let _ = cr.fill();
                hits.push(Hit { shape: Shape::Arc { cx, cy, r0, r1: r1 + 6.0, a0: a, a1: a + da }, index: i, tip: tips.get(i).cloned().unwrap_or_default() });
                a += da;
            }
            rgb(cr, fg, 0.85);
            cr.set_font_size(13.0);
            text(cr, center, cx, cy + 5.0, 0.5);
            cr.set_font_size(11.0);
            // legend beside the ring
            let lx = cx + r1 + 24.0;
            for (i, (name, v)) in items.iter().enumerate().take(((h - 10.0) / 18.0) as usize) {
                let ly = 16.0 + i as f64 * 18.0;
                let (c, k) = swatch(name, i, fg);
                rgb(cr, c, k);
                cr.rectangle(lx, ly - 8.0, 10.0, 10.0);
                let _ = cr.fill();
                rgb(cr, fg, if hover == Some(i) { 1.0 } else { 0.75 });
                let pct = if total > 0.0 { v / total * 100.0 } else { 0.0 };
                text(cr, &ellipsize(cr, &format!("{name}  {pct:.0}%"), right - lx - 18.0), lx + 16.0, ly + 1.0, 0.0);
                hits.push(Hit { shape: Shape::Rect(lx, ly - 10.0, right - lx, 16.0), index: i, tip: tips.get(i).cloned().unwrap_or_default() });
            }
        }
        Data::HBars { items, values, tips } => {
            let max = items.iter().map(|i| i.1).fold(0.0, f64::max);
            let rh = 22.0;
            let label_w = (w * 0.34).min(190.0);
            for (i, (name, v)) in items.iter().enumerate() {
                let y0 = 6.0 + i as f64 * rh;
                if y0 + rh > h {
                    break;
                }
                if hover == Some(i) {
                    rgb(cr, fg, 0.06);
                    cr.rectangle(0.0, y0, w, rh);
                    let _ = cr.fill();
                }
                rgb(cr, fg, 0.8);
                text(cr, &ellipsize(cr, name, label_w - 8.0), 4.0, y0 + 15.0, 0.0);
                let bx = label_w;
                let bwmax = w - label_w - 90.0;
                let bw = if max > 0.0 { v / max * bwmax } else { 0.0 };
                let (c, k) = swatch(name, i, fg);
                rgb(cr, c, k * 0.85);
                cr.rectangle(bx, y0 + 5.0, bw.max(1.0), rh - 10.0);
                let _ = cr.fill();
                rgb(cr, fg, 0.7);
                text(cr, values.get(i).map_or("", String::as_str), w - 4.0, y0 + 15.0, 1.0);
                hits.push(Hit { shape: Shape::Rect(0.0, y0, w, rh), index: i, tip: tips.get(i).cloned().unwrap_or_default() });
            }
        }
        Data::Lines { x_labels, series, band, stops } => {
            let xs = series.iter().flat_map(|s| s.points.iter().map(|p| p.0)).chain(band.iter().flat_map(|b| b.0.iter().chain(&b.1).map(|p| p.0)));
            let (x0, x1) = xs.fold((f64::MAX, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
            if x0 > x1 {
                rgb(cr, fg, 0.5);
                text(cr, "No balances yet", w / 2.0, h / 2.0, 0.5);
                return;
            }
            let ys = series.iter().flat_map(|s| s.points.iter().map(|p| p.1)).chain(band.iter().flat_map(|b| b.0.iter().chain(&b.1).map(|p| p.1)));
            let (lo, hi) = ys.fold((0.0f64, f64::MIN), |(a, b), v| (a.min(v), b.max(v)));
            let y = y_axis(cr, fg, lo, if hi <= lo { lo + 1.0 } else { hi }, (top, bottom, left, right), fmt);
            let x = |v: f64| left + if x1 > x0 { (v - x0) / (x1 - x0) * (right - left) } else { 0.0 };
            if let Some((low, high)) = band {
                rgb(cr, SLATE, 0.18);
                for (i, p) in high.iter().enumerate() {
                    if i == 0 { cr.move_to(x(p.0), y(p.1)) } else { cr.line_to(x(p.0), y(p.1)) }
                }
                for p in low.iter().rev() {
                    cr.line_to(x(p.0), y(p.1));
                }
                cr.close_path();
                let _ = cr.fill();
            }
            for s in series {
                rgb(cr, s.color, 1.0);
                cr.set_line_width(if s.dashed { 1.8 } else { 2.4 });
                cr.set_dash(if s.dashed { &[6.0, 4.0] } else { &[] }, 0.0);
                for (i, p) in s.points.iter().enumerate() {
                    if i == 0 { cr.move_to(x(p.0), y(p.1)) } else { cr.line_to(x(p.0), y(p.1)) }
                }
                let _ = cr.stroke();
            }
            cr.set_dash(&[], 0.0);
            rgb(cr, fg, 0.6);
            let mut last_end = f64::MIN;
            for (v, l) in x_labels {
                let px = x(*v);
                let wl = cr.text_extents(l).map_or(30.0, |e| e.width());
                if px - wl / 2.0 > last_end + 6.0 {
                    text(cr, l, px, bottom + 16.0, 0.5);
                    last_end = px + wl / 2.0;
                }
            }
            // hover: the nearest stop gets a guide line
            if !stops.is_empty() {
                let mut sorted: Vec<(f64, usize)> = stops.iter().enumerate().map(|(i, s)| (x(s.0), i)).collect();
                sorted.sort_by(|a, b| a.0.total_cmp(&b.0));
                for (k, &(px, i)) in sorted.iter().enumerate() {
                    let a = if k == 0 { left } else { (sorted[k - 1].0 + px) / 2.0 };
                    let b = sorted.get(k + 1).map_or(right + 1.0, |n| (px + n.0) / 2.0);
                    hits.push(Hit { shape: Shape::XBand(a, b), index: i, tip: stops[i].1.clone() });
                    if hover_x >= a && hover_x < b {
                        rgb(cr, fg, 0.3);
                        cr.set_line_width(1.0);
                        cr.move_to(px.round() + 0.5, top);
                        cr.line_to(px.round() + 0.5, bottom);
                        let _ = cr.stroke();
                    }
                }
            }
            let leg: Vec<(&str, Rgb)> = series.iter().take(6).map(|s| (s.name.as_str(), s.color)).collect();
            legend(cr, fg, &leg, right, 10.0);
        }
        Data::Flow { left, right, tips } => {
            let (sl, sr): (f64, f64) = (left.iter().map(|x| x.1).sum(), right.iter().map(|x| x.1).sum());
            let total = sl.max(sr);
            if total <= 0.0 {
                rgb(cr, fg, 0.5);
                text(cr, "Nothing in or out this month", w / 2.0, h / 2.0, 0.5);
                return;
            }
            let (top, bottom) = (8.0, h - 8.0);
            let gap = 5.0;
            let label_w = (w * 0.24).min(210.0);
            let (lx, mx, rx, bw) = (label_w, w / 2.0 - 6.0, w - label_w - 12.0, 12.0);
            // the scale leaves room for the gaps on the busier side
            let k = (bottom - top - gap * (left.len().max(right.len()).max(1) - 1) as f64) / total;
            let mid_h = total * k;
            let mid_top = top + (bottom - top - mid_h) / 2.0;
            let band = |x0: f64, a0: f64, a1: f64, x1: f64, b0: f64, b1: f64| {
                let c = (x0 + x1) / 2.0;
                cr.move_to(x0, a0);
                cr.curve_to(c, a0, c, b0, x1, b0);
                cr.line_to(x1, b1);
                cr.curve_to(c, b1, c, a1, x0, a1);
                cr.close_path();
            };
            let mut idx = 0;
            for (side, items) in [(0, left), (1, right)] {
                let n = items.len();
                let used: f64 = items.iter().map(|x| x.1).sum::<f64>() * k + gap * n.saturating_sub(1) as f64;
                let mut y = top + (bottom - top - used) / 2.0;
                let mut my = mid_top;
                // small bands share their space: a label only where it won't touch the one above
                let mut last_label = f64::MIN;
                for (name, v, c) in items.iter() {
                    let hgt = (v * k).max(1.0);
                    let lit = hover.is_none() || hover == Some(idx);
                    rgb(cr, *c, if lit { 0.38 } else { 0.16 });
                    if side == 0 { band(lx + bw, y, y + hgt, mx, my, my + hgt) } else { band(mx + bw, my, my + hgt, rx, y, y + hgt) }
                    let _ = cr.fill();
                    rgb(cr, *c, 0.95);
                    cr.rectangle(if side == 0 { lx } else { rx }, y, bw, hgt);
                    let _ = cr.fill();
                    rgb(cr, fg, 0.8);
                    let ty = (y + hgt / 2.0 + 4.0).max(last_label + 14.0);
                    if ty <= bottom + 4.0 && ty - (y + hgt) < 10.0 {
                        if side == 0 { text(cr, &ellipsize(cr, name, label_w - 10.0), lx - 6.0, ty, 1.0) } else { text(cr, &ellipsize(cr, name, label_w - 4.0), rx + bw + 6.0, ty, 0.0) }
                        last_label = ty;
                    }
                    let hit_x = if side == 0 { 0.0 } else { rx };
                    hits.push(Hit { shape: Shape::Rect(hit_x, y - gap / 2.0, if side == 0 { lx + bw } else { w - rx }, hgt + gap), index: idx, tip: tips.get(idx).cloned().unwrap_or_default() });
                    y += hgt + gap;
                    my += hgt;
                    idx += 1;
                }
            }
            rgb(cr, fg, 0.55);
            cr.rectangle(mx, mid_top, bw, mid_h);
            let _ = cr.fill();
        }
        Data::Pareto { items, cum_pct, tips } => {
            let max = items.iter().map(|i| i.1).fold(0.0, f64::max);
            let rr = right - 34.0;
            let y = y_axis(cr, fg, 0.0, if max > 0.0 { max } else { 1.0 }, (top, bottom, left, rr), fmt);
            let n = items.len().max(1) as f64;
            let gw = (rr - left) / n;
            let pct_y = |p: f64| bottom - p / 100.0 * (bottom - top);
            rgb(cr, fg, 0.6);
            for p in [0.0, 50.0, 100.0] {
                text(cr, &format!("{p:.0}%"), right, pct_y(p) + 4.0, 1.0);
            }
            rgb(cr, RED, 0.5);
            cr.set_dash(&[4.0, 4.0], 0.0);
            cr.set_line_width(1.0);
            cr.move_to(left, pct_y(80.0));
            cr.line_to(rr, pct_y(80.0));
            let _ = cr.stroke();
            cr.set_dash(&[], 0.0);
            for (i, (name, v)) in items.iter().enumerate() {
                let gx = left + i as f64 * gw;
                rgb(cr, BLUE, if hover.is_none() || hover == Some(i) { 0.9 } else { 0.55 });
                let yv = y(*v);
                cr.rectangle(gx + gw * 0.15, yv, gw * 0.7, bottom - yv);
                let _ = cr.fill();
                rgb(cr, fg, 0.65);
                text(cr, &ellipsize(cr, name, gw - 4.0), gx + gw / 2.0, bottom + 16.0, 0.5);
                hits.push(Hit { shape: Shape::XBand(gx, gx + gw), index: i, tip: tips.get(i).cloned().unwrap_or_default() });
            }
            rgb(cr, ORANGE, 1.0);
            cr.set_line_width(2.2);
            for (i, p) in cum_pct.iter().enumerate() {
                let (x, yy) = (left + (i as f64 + 0.5) * gw, pct_y(*p));
                if i == 0 { cr.move_to(x, yy) } else { cr.line_to(x, yy) }
            }
            let _ = cr.stroke();
            for (i, p) in cum_pct.iter().enumerate() {
                cr.arc(left + (i as f64 + 0.5) * gw, pct_y(*p), 3.0, 0.0, 2.0 * PI);
                let _ = cr.fill();
            }
            legend(cr, fg, &[("Outflow", BLUE), ("Cumulative %", ORANGE)], right, 10.0);
        }
    }
}
