//! The Budget Plan and the monthly Report Card on paper (save as PDF, or print), and the calendar
//! file of paydays and bills. Both papers are laid out by one small engine: headings, paragraphs,
//! number tiles and tables, flowed onto Letter pages through GTK's print system, so saving and
//! printing draw exactly the same thing.

use adw::prelude::*;
use chrono::{Datelike, NaiveDate};
use fingraph_core::Decimal;
use fingraph_core::insights::{self, add_months, by_category, month_start};
use fingraph_core::method::{Method, share};
use fingraph_core::plan_doc::{self, CatPlan, PlanDoc, PlanInput, ReportCard};
use gtk::{cairo, gio};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use crate::budget::{OUTSIDE, data_through, snap, usual};
use crate::{App, today};

// ──────────────────────────────────────────────────────────────── the page engine

#[derive(Clone, Copy, PartialEq)]
enum Look {
    Normal,
    Bold,
    /// Over budget, behind, short: printed in red.
    Warn,
    Dim,
}

struct Table {
    /// Column widths as shares of the page width.
    widths: Vec<f64>,
    /// Right-align (numbers).
    right: Vec<bool>,
    head: Vec<String>,
    rows: Vec<(Vec<String>, Look)>,
    /// A tick box in front of each row (the payday checklist).
    checks: bool,
}

enum Block {
    Title(String, String),
    Heading(String),
    Para(String, Look),
    /// Big numbers side by side: (label, value, note).
    Tiles(Vec<(String, String, String)>),
    Table(Table),
    Break,
}

const INK: (f64, f64, f64) = (0.11, 0.13, 0.15);
const DIM: (f64, f64, f64) = (0.42, 0.45, 0.48);
const RED: (f64, f64, f64) = (0.72, 0.13, 0.13);
const ACCENT: (f64, f64, f64) = (0.18, 0.36, 0.31);
const RULE: (f64, f64, f64) = (0.82, 0.84, 0.86);
const SHADE: (f64, f64, f64) = (0.95, 0.96, 0.95);

fn colour(cr: &cairo::Context, c: (f64, f64, f64)) {
    cr.set_source_rgb(c.0, c.1, c.2);
}

fn font(cr: &cairo::Context, size: f64, bold: bool) {
    cr.select_font_face("Sans", cairo::FontSlant::Normal, if bold { cairo::FontWeight::Bold } else { cairo::FontWeight::Normal });
    cr.set_font_size(size);
}

fn width(cr: &cairo::Context, s: &str) -> f64 {
    cr.text_extents(s).map(|e| e.x_advance()).unwrap_or(0.0)
}

/// Cut to fit, with an ellipsis.
fn fit(cr: &cairo::Context, s: &str, max: f64) -> String {
    if width(cr, s) <= max {
        return s.to_string();
    }
    let mut v: Vec<char> = s.chars().collect();
    while !v.is_empty() && width(cr, &format!("{}…", v.iter().collect::<String>())) > max {
        v.pop();
    }
    format!("{}…", v.iter().collect::<String>())
}

fn wrap(cr: &cairo::Context, s: &str, max: f64) -> Vec<String> {
    let mut out = Vec::new();
    for para in s.split('\n') {
        let mut line = String::new();
        for w in para.split(' ') {
            let try_ = if line.is_empty() { w.to_string() } else { format!("{line} {w}") };
            if width(cr, &try_) > max && !line.is_empty() {
                out.push(std::mem::take(&mut line));
                line = w.to_string();
            } else {
                line = try_;
            }
        }
        out.push(line);
    }
    out
}

/// Lay the blocks out on pages `w` × `h` points; draw page `only` (or none, just counting).
/// Returns how many pages there are.
fn flow(cr: &cairo::Context, w: f64, h: f64, blocks: &[Block], footer: &str, only: Option<usize>) -> usize {
    let (top, bottom) = (8.0, h - 26.0);
    let mut page = 0usize;
    let mut y = top;
    let on = |p: usize| only == Some(p);
    // start a new page when `need` doesn't fit
    let room = |y: &mut f64, page: &mut usize, need: f64| {
        if *y + need > bottom && *y > top {
            *page += 1;
            *y = top;
            true
        } else {
            false
        }
    };
    for b in blocks {
        match b {
            Block::Break => {
                if y > top {
                    page += 1;
                    y = top;
                }
            }
            Block::Title(t, sub) => {
                room(&mut y, &mut page, 48.0);
                if on(page) {
                    colour(cr, ACCENT);
                    cr.rectangle(0.0, y, 5.0, 38.0);
                    let _ = cr.fill();
                    font(cr, 22.0, true);
                    colour(cr, INK);
                    cr.move_to(14.0, y + 20.0);
                    let _ = cr.show_text(t);
                    font(cr, 9.5, false);
                    colour(cr, DIM);
                    cr.move_to(14.0, y + 35.0);
                    let _ = cr.show_text(&fit(cr, sub, w - 14.0));
                }
                y += 54.0;
            }
            Block::Heading(t) => {
                // kept with what follows it
                room(&mut y, &mut page, 90.0);
                y += 8.0;
                if on(page) {
                    font(cr, 13.0, true);
                    colour(cr, ACCENT);
                    cr.move_to(0.0, y + 12.0);
                    let _ = cr.show_text(t);
                    colour(cr, RULE);
                    cr.rectangle(0.0, y + 17.0, w, 0.8);
                    let _ = cr.fill();
                }
                y += 25.0;
            }
            Block::Para(t, look) => {
                font(cr, 9.5, *look == Look::Bold);
                let lines = wrap(cr, t, w);
                for l in lines {
                    room(&mut y, &mut page, 13.0);
                    if on(page) {
                        font(cr, 9.5, *look == Look::Bold);
                        colour(cr, match look {
                            Look::Warn => RED,
                            Look::Dim => DIM,
                            _ => INK,
                        });
                        cr.move_to(0.0, y + 10.0);
                        let _ = cr.show_text(&l);
                    }
                    y += 13.0;
                }
                y += 4.0;
            }
            Block::Tiles(tiles) => {
                room(&mut y, &mut page, 62.0);
                let n = tiles.len().max(1) as f64;
                let gap = 8.0;
                let tw = (w - gap * (n - 1.0)) / n;
                if on(page) {
                    for (i, (label, value, note)) in tiles.iter().enumerate() {
                        let x = i as f64 * (tw + gap);
                        colour(cr, SHADE);
                        cr.rectangle(x, y, tw, 56.0);
                        let _ = cr.fill();
                        font(cr, 8.5, false);
                        colour(cr, DIM);
                        cr.move_to(x + 8.0, y + 14.0);
                        let _ = cr.show_text(&fit(cr, label, tw - 16.0));
                        font(cr, 16.0, true);
                        colour(cr, if note.starts_with('!') { RED } else { INK });
                        cr.move_to(x + 8.0, y + 34.0);
                        let _ = cr.show_text(&fit(cr, value, tw - 16.0));
                        font(cr, 8.0, false);
                        colour(cr, if note.starts_with('!') { RED } else { DIM });
                        cr.move_to(x + 8.0, y + 48.0);
                        let _ = cr.show_text(&fit(cr, note.trim_start_matches('!'), tw - 16.0));
                    }
                }
                y += 64.0;
            }
            Block::Table(t) => {
                let row_h = 15.0;
                let x0 = if t.checks { 16.0 } else { 0.0 };
                let tw = w - x0;
                let total: f64 = t.widths.iter().sum();
                let cols: Vec<(f64, f64)> = {
                    let mut x = x0;
                    t.widths
                        .iter()
                        .map(|c| {
                            let cw = tw * c / total;
                            let r = (x, cw);
                            x += cw;
                            r
                        })
                        .collect()
                };
                let cell = |cr: &cairo::Context, i: usize, s: &str, y: f64| {
                    let (x, cw) = cols[i];
                    let s = fit(cr, s, cw - 8.0);
                    let tx = if t.right[i] { x + cw - 4.0 - width(cr, &s) } else { x + 4.0 };
                    cr.move_to(tx, y + 11.0);
                    let _ = cr.show_text(&s);
                };
                let head = |cr: &cairo::Context, y: f64| {
                    font(cr, 8.5, true);
                    colour(cr, DIM);
                    for (i, hd) in t.head.iter().enumerate() {
                        cell(cr, i, hd, y);
                    }
                    colour(cr, RULE);
                    cr.rectangle(0.0, y + row_h - 1.0, w, 0.8);
                    let _ = cr.fill();
                };
                room(&mut y, &mut page, row_h * 3.0);
                if on(page) {
                    head(cr, y);
                }
                y += row_h + 2.0;
                for (k, (cells, look)) in t.rows.iter().enumerate() {
                    if room(&mut y, &mut page, row_h) {
                        if on(page) {
                            head(cr, y);
                        }
                        y += row_h + 2.0;
                    }
                    if on(page) {
                        if k % 2 == 1 {
                            colour(cr, SHADE);
                            cr.rectangle(0.0, y, w, row_h);
                            let _ = cr.fill();
                        }
                        if t.checks {
                            colour(cr, INK);
                            cr.set_line_width(0.9);
                            cr.rectangle(2.0, y + 2.5, 10.0, 10.0);
                            let _ = cr.stroke();
                        }
                        font(cr, 9.0, *look == Look::Bold);
                        colour(cr, match look {
                            Look::Warn => RED,
                            Look::Dim => DIM,
                            _ => INK,
                        });
                        for (i, c) in cells.iter().enumerate().take(cols.len()) {
                            cell(cr, i, c, y);
                        }
                    }
                    y += row_h;
                }
                y += 8.0;
            }
        }
    }
    let pages = page + 1;
    if let Some(p) = only {
        font(cr, 8.0, false);
        colour(cr, DIM);
        let s = format!("{footer} · page {} of {pages}", p + 1);
        cr.move_to(w - width(cr, &s), h - 6.0);
        let _ = cr.show_text(&s);
    }
    pages
}

/// Save as a PDF (`to` = a path) or open the print dialog (`to` = None).
fn output(app: &Rc<App>, job: &str, footer: String, blocks: Vec<Block>, to: Option<std::path::PathBuf>) {
    let op = gtk::PrintOperation::new();
    op.set_job_name(job);
    let setup = gtk::PageSetup::new();
    setup.set_paper_size(&gtk::PaperSize::new(Some(gtk::PAPER_NAME_LETTER)));
    setup.set_top_margin(0.6, gtk::Unit::Inch);
    setup.set_bottom_margin(0.5, gtk::Unit::Inch);
    setup.set_left_margin(0.65, gtk::Unit::Inch);
    setup.set_right_margin(0.65, gtk::Unit::Inch);
    op.set_default_page_setup(Some(&setup));
    op.set_unit(gtk::Unit::Points);
    let blocks = Rc::new(blocks);
    {
        let (blocks, footer) = (blocks.clone(), footer.clone());
        op.connect_begin_print(move |op, ctx| {
            let n = flow(&ctx.cairo_context(), ctx.width(), ctx.height(), &blocks, &footer, None);
            op.set_n_pages(n as i32);
        });
    }
    op.connect_draw_page(move |_, ctx, n| {
        flow(&ctx.cairo_context(), ctx.width(), ctx.height(), &blocks, &footer, Some(n as usize));
    });
    let action = match &to {
        Some(path) => {
            op.set_export_filename(path);
            gtk::PrintOperationAction::Export
        }
        None => gtk::PrintOperationAction::PrintDialog,
    };
    match op.run(action, Some(&app.win)) {
        Ok(_) => {
            if let Some(path) = to {
                let file = gio::File::for_path(&path);
                app.toast_action(&format!("Saved {}", path.display()), "Open", Rc::new(move |a: &Rc<App>| gtk::FileLauncher::new(Some(&file)).launch(Some(&a.win), gio::Cancellable::NONE, |_| {})));
            }
        }
        Err(e) => app.toast(&format!("Couldn't {}: {e}", if action == gtk::PrintOperationAction::Export { "save" } else { "print" })),
    }
}

/// Ask where to save, then write.
fn save_as(app: &Rc<App>, name: &str, then: impl Fn(&Rc<App>, std::path::PathBuf) + 'static) {
    let fd = gtk::FileDialog::builder().title("Save").initial_name(name).modal(true).build();
    let w = Rc::downgrade(app);
    fd.save(Some(&app.win), gio::Cancellable::NONE, move |res| {
        if let (Some(a), Some(path)) = (w.upgrade(), res.ok().and_then(|f| f.path())) {
            then(&a, path);
        }
    });
}

// ──────────────────────────────────────────────────────────────── the Budget Plan

/// Your accounts by id, as named in FinGraph, plus "outside FinGraph" for savings kept elsewhere.
fn account_names(app: &App) -> BTreeMap<i64, String> {
    let mut m: BTreeMap<i64, String> = app.db.accounts().unwrap_or_default().into_iter().map(|a| (a.id, a.name)).collect();
    m.insert(OUTSIDE, "your savings account (outside FinGraph)".into());
    m
}

pub(crate) fn plan_doc(app: &App) -> Option<PlanDoc> {
    if !app.financial_ready() { return None; }
    let s = snap(app);
    let plan = s.plan.clone()?;
    let (usual_by, _, _) = usual(&s, month_start(today()));
    let cats: Vec<CatPlan> = s
        .cats
        .iter()
        .filter_map(|c| {
            // budgeted, or usually spent on (a want a method cut to nothing still shows, at 0)
            let b = s.budgets.get(&c.id).copied().unwrap_or_default();
            let u = usual_by.get(&c.name).copied().unwrap_or_default();
            (b > Decimal::ZERO || (u > Decimal::ZERO && s.plan.as_ref().is_some_and(|p| p.method.split().is_some()))).then(|| CatPlan { name: c.name.clone(), fixed: s.fixed.contains(&c.name), need: s.needs.contains(&c.name), budget: b, usual: usual_by.get(&c.name).copied() })
        })
        .collect();
    let accounts = account_names(app);
    let currency: BTreeMap<i64, fingraph_core::Currency> = app.db.accounts().unwrap_or_default().into_iter().map(|a| (a.id, a.currency)).collect();
    let docs = app.db.documents(&app.entity_ids()).unwrap_or_default();
    let balances: BTreeMap<i64, (NaiveDate, Decimal)> = insights::latest_balances(&s.rows, &docs)
        .into_iter()
        .map(|(id, (d, b))| (id, (d, currency.get(&id).map_or(b, |c| s.fx.convert(b, *c, s.ccy).unwrap_or_default()))))
        .collect();
    // one line per service, however many spellings the bank used
    // (its latest charge, what that costs a year)
    let mut by: BTreeMap<String, (NaiveDate, Decimal)> = BTreeMap::new();
    for x in insights::subscriptions(&s.rows, s.ccy, &s.fx, today()).into_iter().filter(|x| x.cancelled) {
        let e = by.entry(x.payee).or_insert((x.last_charged, x.annual));
        if x.last_charged > e.0 {
            *e = (x.last_charged, x.annual);
        }
    }
    let mut cancelled: Vec<(String, Decimal)> = by.into_iter().map(|(n, (_, v))| (n, v)).collect();
    cancelled.sort_by_key(|c| std::cmp::Reverse(c.1));
    let roles = plan.accounts.clone();
    plan_doc::plan_doc(PlanInput {
        plan: &plan,
        roles: &roles,
        ccy: s.ccy,
        fx: &s.fx,
        cats,
        funds: &s.funds,
        rows: &s.rows,
        accounts: &accounts,
        balances: &balances,
        owed: crate::budget::owed(app, &s).iter().map(|x| x.1).sum(),
        cancelled,
        fixed: s.fixed.clone(),
        parents: &s.parents,
        today: today(),
        data_through: data_through(app),
    })
}

/// Whole currency units, for tight spaces: CI$ 1,331.
fn whole(d: &PlanDoc, v: Decimal) -> String {
    let s = d.money(v.round_dp(0));
    s.strip_suffix(".00").map(str::to_string).unwrap_or(s)
}

fn day(d: NaiveDate) -> String {
    d.format("%a %d %b").to_string()
}

fn plan_blocks(app: &App, d: &PlanDoc) -> Vec<Block> {
    let m = |v: Decimal| d.money(v);
    let save = d.saving();
    let short = d.goal > Decimal::ZERO && d.left_to_save < d.goal;
    let split = d.method.split();
    let pc = |v: Decimal| format!("{}%", share(v, d.base));
    let tiles = match split {
        Some(sp) => {
            let mut v = Vec::new();
            match (sp.needs, sp.wants) {
                (Some(n), Some(w)) => {
                    v.push(("Needs a month".to_string(), m(d.needs()), format!("{}{} of {} · target {n}%", if share(d.needs(), d.base) > Decimal::from(n) { "!" } else { "" }, pc(d.needs()), whole(d, d.base))));
                    v.push(("Wants a month".to_string(), m(d.wants()), format!("{}{} · target {w}%", if share(d.wants(), d.base) > Decimal::from(w) { "!" } else { "" }, pc(d.wants()))));
                }
                _ => v.push((
                    "Spending a month".to_string(),
                    m(d.needs() + d.wants()),
                    format!("{}{} of {} · target {}%", if share(d.needs() + d.wants(), d.base) > Decimal::from(sp.spend) { "!" } else { "" }, pc(d.needs() + d.wants()), whole(d, d.base), sp.spend),
                )),
            }
            let saved = save + d.pension;
            v.push(("Saving a month".to_string(), m(saved), format!("{}{} with pension · target {}%", if share(saved, d.base) < Decimal::from(sp.save) { "!" } else { "" }, pc(saved), sp.save)));
            if sp.debt > 0 {
                v.push(("Paying off debt".to_string(), m(d.debt), format!("{} · target {}%", pc(d.debt), sp.debt)));
            }
            v
        }
        None => vec![
            ("Take-home a payday".into(), m(d.take_home), format!("{} a month", m(d.income))),
            ("Spending budget a month".into(), m(d.fixed + d.variable), format!("{} fixed + {} var.", whole(d, d.fixed), whole(d, d.variable).replace(d.ccy.symbol(), "").trim())),
            ("Big bills a month".into(), m(d.big), format!("{} bills set aside for", d.big_bills.len())),
            (
                "Saving a month".into(),
                m(save),
                if short { format!("!short of the {} goal", m(d.goal)) } else if d.goal > Decimal::ZERO { "on track for your goal".into() } else { "what's left over".into() },
            ),
        ],
    };
    let mut b = vec![
        Block::Title(d.method.plan_title(), format!("Made {} · {} · paid {}", d.made.format("%d %B %Y"), app.db.statement_coverage(&app.entity_ids()).unwrap_or_default().label(), d.cycle.to_lowercase())),
        Block::Tiles(tiles),
    ];
    // when needs crowd out wants, say why and where the room would come from
    if let Some(sp) = split {
        let room = Decimal::from(sp.wants.unwrap_or(sp.spend)) * d.base / Decimal::ONE_HUNDRED - if sp.wants.is_some() { Decimal::ZERO } else { d.needs() };
        let wants_usual: Decimal = d.cats.iter().filter(|c| !c.need).filter_map(|c| c.usual).sum();
        if wants_usual > Decimal::ZERO && d.wants() < wants_usual / Decimal::from(2) {
            let mut big: Vec<(String, Decimal)> = d.cats.iter().filter(|c| c.need).map(|c| (c.name.clone(), c.budget)).collect();
            if d.big > Decimal::ZERO {
                big.push(("big yearly bills".into(), d.big));
            }
            big.sort_by_key(|x| std::cmp::Reverse(x.1));
            let top: Vec<String> = big.iter().take(3).map(|(n, v)| format!("{n} {}", whole(d, *v))).collect();
            b.push(Block::Para(
                format!(
                    "Needs take {}% of what comes in, so {} leaves {} a month for wants, which usually cost {}. Wants are cut to fit. To win them back, bring the biggest needs down: {}.",
                    share(d.needs(), d.base),
                    d.method.label(),
                    m(room.max(Decimal::ZERO).round_dp(0)),
                    m(wants_usual),
                    top.join(", ")
                ),
                Look::Warn,
            ));
        }
    }

    // ── each payday
    b.push(Block::Heading("Every Payday".into()));
    let next: Vec<String> = d.next_paydays.iter().map(|p| p.format("%d %b").to_string()).collect();
    b.push(Block::Para(
        format!(
            "Next paydays: {}. Take-home {} into {}{}. Do this each time:",
            if next.is_empty() { "not set (add your next payday in Set Up Budget)".into() } else { next.join(", ") },
            m(d.take_home),
            d.pay_account,
            if d.other_income > Decimal::ZERO { format!(", plus other income of {} a month ({} a payday)", m(d.other_income), m(d.per_pay(d.other_income))) } else { String::new() }
        ),
        Look::Normal,
    ));
    b.push(Block::Table(Table {
        widths: vec![1.3, 4.6, 2.9],
        right: vec![true, false, false],
        head: vec!["Amount".into(), "What to do".into(), "For".into()],
        rows: d.checklist.iter().map(|s| (vec![m(s.amount), s.action.clone(), s.why.clone()], Look::Normal)).collect(),
        checks: true,
    }));
    b.push(if d.spare >= Decimal::ZERO {
        Block::Para(format!("Left over each payday: {}. Keep it as a buffer in {}, or add it to savings.", m(d.spare), d.pay_account), Look::Bold)
    } else {
        Block::Para(format!("The plan needs {} more than comes in each payday. Trim the variable categories or lower the savings goal.", m(-d.spare)), Look::Warn)
    });
    if d.per_year != Decimal::from(12) {
        b.push(Block::Para("Monthly figures are spread evenly over your paydays, so a month with an extra payday leaves you ahead.".into(), Look::Dim));
    }

    // ── bills in the next month and big bills in the next three
    b.push(Block::Heading("Bills Coming Up".into()));
    let until = today() + chrono::Duration::days(31);
    let mut rows: Vec<(NaiveDate, Vec<String>, Look)> = d
        .bills
        .iter()
        .filter(|x| x.next <= until)
        .map(|x| (x.next, vec![day(x.next), x.name.clone(), x.every().into(), m(x.amount), x.account.clone()], Look::Normal))
        .collect();
    let big_until = add_months(month_start(today()), 3);
    rows.extend(d.big_bills.iter().filter(|x| x.due < big_until).map(|x| (x.due, vec![day(x.due), x.name.clone(), "big bill".into(), m(x.amount), "the big-bill money".into()], Look::Bold)));
    rows.sort_by_key(|r| r.0);
    if rows.is_empty() {
        b.push(Block::Para("No regular bills found in your statements yet.".into(), Look::Dim));
    } else {
        b.push(Block::Table(Table {
            widths: vec![1.3, 3.0, 1.3, 1.3, 3.2],
            right: vec![false, false, false, true, false],
            head: vec!["Due".into(), "Bill".into(), "How often".into(), "Amount".into(), "Paid from".into()],
            rows: rows.into_iter().map(|r| (r.1, r.2)).collect(),
            checks: false,
        }));
        b.push(Block::Para("Dates are worked out from when each bill last came out; the calendar file puts them on your phone with a reminder three days before.".into(), Look::Dim));
    }

    // ── weekly limits (80/20: one allowance for everything variable)
    let variable: Vec<&CatPlan> = d.cats.iter().filter(|c| !c.fixed).collect();
    if d.method == Method::EightyTwenty {
        b.push(Block::Heading("Your Weekly Allowance".into()));
        let wk: Decimal = variable.iter().map(|c| c.weekly()).sum();
        b.push(Block::Tiles(vec![
            ("Spend up to, a week".into(), m(wk), "on everything that isn't a fixed bill".into()),
            ("A month".into(), m(d.variable), "groceries, eating out, shopping, the lot".into()),
        ]));
        b.push(Block::Para("Pay yourself first: the savings move out on payday, so what's left is yours to spend without tracking each category. Page 2 shows how it usually splits.".into(), Look::Dim));
    } else if !variable.is_empty() {
        b.push(Block::Heading("Weekly Spending Limits".into()));
        b.push(Block::Para("Easier to keep to than a monthly figure: check the week's spending against these.".into(), Look::Dim));
        let mut rows: Vec<(Vec<String>, Look)> = variable
            .iter()
            .map(|c| (vec![c.name.clone(), m(c.weekly()), m(c.budget), c.usual.map(m).unwrap_or_else(|| "—".into())], if c.at_risk() { Look::Warn } else { Look::Normal }))
            .collect();
        let wk: Decimal = variable.iter().map(|c| c.weekly()).sum();
        rows.push((vec!["All variable spending".into(), m(wk), m(d.variable), String::new()], Look::Bold));
        b.push(Block::Table(Table {
            widths: vec![3.4, 1.4, 1.4, 1.6],
            right: vec![false, true, true, true],
            head: vec!["Category".into(), "A week".into(), "A month".into(), "Usually a month".into()],
            rows,
            checks: false,
        }));
        if variable.iter().any(|c| c.at_risk()) {
            b.push(Block::Para("Red: you usually spend more than this, so this is where the plan is likeliest to slip.".into(), Look::Warn));
        }
    }

    // ── paying off debt
    if d.owed > Decimal::ZERO {
        b.push(Block::Heading("Paying Off Debt".into()));
        b.push(match (d.months_to_clear(), &d.debt_account) {
            (Some(n), acct) => Block::Para(
                format!(
                    "You owe {} on cards and loans. Paying {} a month on top of the minimums{} clears it in about {} month{}{}, before interest. Then that money goes to savings.",
                    m(d.owed),
                    m(d.debt),
                    acct.as_ref().map(|a| format!(" to {a}")).unwrap_or_default(),
                    n,
                    if n == 1 { "" } else { "s" },
                    if n > 0 { format!(" (around {})", add_months(month_start(d.made), n as i32).format("%B %Y")) } else { String::new() }
                ),
                Look::Bold,
            ),
            (None, _) => Block::Para(format!("You owe {} on cards and loans, and the plan pays nothing extra toward it. Card interest usually costs more than savings earn: consider the 70/20/10 method.", m(d.owed)), Look::Warn),
        });
    }

    // ── savings
    b.push(Block::Heading("Savings".into()));
    let (year_end, year, five) = d.projection();
    b.push(Block::Para(
        format!("Saving {} a month comes to {} by the end of {}, {} in a year and {} in five years, before any interest.", m(save), m(year_end), d.made.year(), m(year), m(five)),
        Look::Normal,
    ));
    if d.pension > Decimal::ZERO {
        b.push(Block::Para(format!("Your pension adds {} a month on top, saved for you before pay reaches the bank.", m(d.pension)), Look::Dim));
    }
    if short {
        b.push(Block::Para(format!("The budget leaves {} a month, {} short of your {} goal. Page 2 shows where it goes.", m(d.left_to_save), m(d.goal - d.left_to_save), m(d.goal)), Look::Warn));
    }

    // ── page 2: the detail
    b.push(Block::Break);
    b.push(Block::Title("Budget Plan: the Detail".into(), "Every category, every big bill, and the arithmetic".into()));
    b.push(Block::Heading("Every Category".into()));
    let mut rows: Vec<(Vec<String>, Look)> = d
        .cats
        .iter()
        .map(|c| {
            let diff = c.usual.map(|u| c.budget - u);
            (
                vec![c.name.clone(), format!("{}, {}", if c.need { "Need" } else { "Want" }, if c.fixed { "fixed" } else { "variable" }), m(c.budget), c.usual.map(m).unwrap_or_else(|| "—".into()), diff.map(|x| if x >= Decimal::ZERO { format!("+{}", m(x)) } else { m(x) }).unwrap_or_default()],
                if c.at_risk() { Look::Warn } else { Look::Normal },
            )
        })
        .collect();
    rows.push((vec!["Fixed".into(), String::new(), m(d.fixed), String::new(), String::new()], Look::Bold));
    rows.push((vec!["Variable".into(), String::new(), m(d.variable), String::new(), String::new()], Look::Bold));
    b.push(Block::Table(Table {
        widths: vec![3.0, 1.6, 1.5, 1.4, 1.4],
        right: vec![false, false, true, true, true],
        head: vec!["Category".into(), "Type".into(), "Budget a month".into(), "Usually".into(), "Room".into()],
        rows,
        checks: false,
    }));
    b.push(Block::Para("“Usually” is the median of your last six months, so a one-off doesn't count. “Room” is the budget minus that: red rows are budgeted below what they usually cost.".into(), Look::Dim));

    if !d.big_bills.is_empty() {
        b.push(Block::Heading("Big Yearly Bills".into()));
        let mut rows: Vec<(Vec<String>, Look)> = d
            .big_bills
            .iter()
            .map(|x| {
                let every = match x.every_months {
                    12 => "yearly".to_string(),
                    6 => "every 6 months".into(),
                    3 => "quarterly".into(),
                    24 => "every 2 years".into(),
                    n => format!("every {n} months"),
                };
                (vec![x.name.clone(), m(x.amount), every, m(x.monthly), x.due.format("%b %Y").to_string(), m(x.saved_by_now)], Look::Normal)
            })
            .collect();
        rows.push((vec!["Total".into(), String::new(), String::new(), m(d.big), String::new(), m(d.saved_by_now())], Look::Bold));
        b.push(Block::Table(Table {
            widths: vec![3.0, 1.2, 1.4, 1.2, 1.2, 1.6],
            right: vec![false, true, false, true, false, true],
            head: vec!["Bill".into(), "Costs".into(), "How often".into(), "A month".into(), "Next due".into(), "Target by now".into()],
            rows,
            checks: false,
        }));
        match &d.big_account {
            Some((name, when, bal)) => {
                let need = d.saved_by_now();
                b.push(Block::Para(
                    format!(
                        "{name} had {} on {}; the big bills need {} by now{}",
                        m(*bal),
                        when.format("%d %b %Y"),
                        m(need),
                        if *bal >= need { ". On track.".to_string() } else { format!(", so it's {} behind. Top it up to catch up.", m(need - *bal)) }
                    ),
                    if *bal >= need { Look::Bold } else { Look::Warn },
                ));
            }
            None => b.push(Block::Para(format!("“Target by now” is planned saving, not confirmed deposits: {} in all. Keeping it in its own account makes it easy to check.", m(d.saved_by_now())), Look::Dim)),
        }
    }

    b.push(Block::Heading("The Arithmetic".into()));
    let mut rows = vec![
        (vec!["Take-home a month".into(), m(d.income)], Look::Normal),
        (vec!["- Fixed bills".into(), m(d.fixed)], Look::Normal),
        (vec!["- Variable spending".into(), m(d.variable)], Look::Normal),
        (vec!["- Big bills set aside".into(), m(d.big)], Look::Normal),
        (vec!["- Loan payments".into(), m(d.loans)], Look::Normal),
        (vec!["- Paying off debt".into(), m(d.debt)], Look::Normal),
        (vec!["= Left to save".into(), m(d.left_to_save)], if d.left_to_save < Decimal::ZERO { Look::Warn } else { Look::Bold }),
    ];
    if d.goal > Decimal::ZERO {
        rows.push((vec!["Your savings goal".into(), m(d.goal)], Look::Normal));
        rows.push((
            vec![if short { "Short by".into() } else { "To spare".into() }, m((d.left_to_save - d.goal).abs())],
            if short { Look::Warn } else { Look::Bold },
        ));
    }
    b.push(Block::Table(Table { widths: vec![4.0, 1.6, 3.0], right: vec![false, true, false], head: vec!["".into(), "A month".into(), "".into()], rows: rows.into_iter().map(|(mut r, l)| { r.push(String::new()); (r, l) }).collect(), checks: false }));

    if let Some(sp) = split {
        b.push(Block::Heading(format!("Against {}", d.method.label())));
        b.push(Block::Para(format!("Shares of take-home plus pension ({} a month). Needs include big bills and loan minimums; saving includes the pension.", m(d.base)), Look::Dim));
        let row = |name: &str, v: Decimal, target: Option<u8>, over_is_bad: bool| {
            let p = share(v, d.base);
            let bad = target.is_some_and(|t| if over_is_bad { p > Decimal::from(t) } else { p < Decimal::from(t) });
            (vec![name.to_string(), m(v), format!("{p}%"), target.map(|t| format!("{t}%")).unwrap_or_else(|| "—".into())], if bad { Look::Warn } else { Look::Normal })
        };
        let mut rows = Vec::new();
        match (sp.needs, sp.wants) {
            (Some(n), Some(w)) => {
                rows.push(row("Needs", d.needs(), Some(n), true));
                rows.push(row("Wants", d.wants(), Some(w), true));
            }
            _ => rows.push(row("All spending", d.needs() + d.wants(), Some(sp.spend), true)),
        }
        rows.push(row("Saving (with pension)", d.saving() + d.pension, Some(sp.save), false));
        if sp.debt > 0 {
            rows.push(row("Paying off debt", d.debt, Some(sp.debt), false));
        }
        b.push(Block::Table(Table { widths: vec![3.0, 1.6, 1.0, 1.0, 2.0], right: vec![false, true, true, true, false], head: vec!["".into(), "A month".into(), "Share".into(), "Target".into(), "".into()], rows: rows.into_iter().map(|(mut r, l)| { r.push(String::new()); (r, l) }).collect(), checks: false }));
    }

    if !d.cancelled.is_empty() {
        b.push(Block::Heading("Cancelled".into()));
        let total: Decimal = d.cancelled.iter().map(|c| c.1).sum();
        let mut rows: Vec<(Vec<String>, Look)> = d.cancelled.iter().map(|(n, v)| (vec![n.clone(), m(*v)], Look::Normal)).collect();
        rows.push((vec!["Saved a year".into(), m(total)], Look::Bold));
        b.push(Block::Table(Table { widths: vec![4.0, 1.6, 3.0], right: vec![false, true, false], head: vec!["Subscription".into(), "A year".into(), "".into()], rows: rows.into_iter().map(|(mut r, l)| { r.push(String::new()); (r, l) }).collect(), checks: false }));
    }
    b
}

pub fn save_plan(app: &Rc<App>) {
    with_plan(app, |a, d| {
        let name = format!("Budget Plan {}.pdf", d.made.format("%Y-%m-%d"));
        save_as(a, &name, move |a, path| output(a, "Budget Plan", footer(&d), plan_blocks(a, &d), Some(path)));
    });
}

pub fn print_plan(app: &Rc<App>) {
    with_plan(app, |a, d| output(a, "Budget Plan", footer(&d), plan_blocks(a, &d), None));
}

fn ics(app: &App, d: &PlanDoc) -> Option<String> {
    let plan = snap(app).plan?;
    Some(plan_doc::calendar(d, &plan, 12, chrono::Utc::now().naive_utc()))
}

/// Write the plan, a report card or the calendar straight to a file (the screenshot harness).
#[cfg(debug_assertions)]
pub fn export_debug(app: &Rc<App>, what: &str, path: std::path::PathBuf) {
    match what {
        "plan" => with_plan(app, move |a, d| output(a, "Budget Plan", footer(&d), plan_blocks(a, &d), Some(path.clone()))),
        "card" => {
            if let Some(c) = report_card(app, last_full_month(app)) {
                output(app, "Report Card", "FinGraph Report Card".into(), card_blocks(&c), Some(path));
            }
        }
        _ => with_plan(app, move |a, d| {
            let _ = std::fs::write(&path, ics(a, &d).unwrap_or_default());
        }),
    }
}

pub fn save_calendar(app: &Rc<App>) {
    with_plan(app, |a, d| {
        if snap(a).plan.is_some_and(|p| p.next_pay.is_none()) {
            a.toast("Add your next payday in Set Up Budget first, so the calendar knows your paydays");
        }
        let Some(ics) = ics(a, &d) else { return };
        save_as(a, "FinGraph Budget.ics", move |a, path| match std::fs::write(&path, &ics) {
            Ok(()) => {
                let file = gio::File::for_path(&path);
                a.toast_action("Saved: open it to add the paydays and bills to your calendar", "Open", Rc::new(move |a: &Rc<App>| gtk::FileLauncher::new(Some(&file)).launch(Some(&a.win), gio::Cancellable::NONE, |_| {})));
            }
            Err(e) => a.toast(&format!("Couldn't save: {e}")),
        });
    });
}

fn footer(d: &PlanDoc) -> String {
    format!("FinGraph Budget Plan of {}", d.made.format("%d %b %Y"))
}

fn with_plan(app: &Rc<App>, then: impl Fn(&Rc<App>, PlanDoc) + 'static) {
    match plan_doc(app) {
        Some(d) => then(app, d),
        None => app.toast("Set up your budget first (Budget → Set Up Budget…)"),
    }
}

// ──────────────────────────────────────────────────────────────── the report card

/// The last month your statements cover in full.
pub fn last_full_month(app: &App) -> NaiveDate {
    let through = data_through(app);
    let next = through + chrono::Duration::days(1);
    if next.month() != through.month() { month_start(through) } else { add_months(month_start(through), -1) }
}

pub(crate) fn report_card(app: &App, month: NaiveDate) -> Option<ReportCard> {
    if !app.financial_ready() { return None; }
    let s = snap(app);
    let plan = s.plan.clone()?;
    let m = month_start(month);
    let end = add_months(m, 1) - chrono::Duration::days(1);
    let then = app.db.budgets_in(&s.scope, m).unwrap_or_default();
    let budgets: Vec<(String, bool, Decimal)> = s
        .cats
        .iter()
        .filter_map(|c| {
            let (v, k) = then.get(&c.id)?;
            let v = s.fx.convert(*v, *k, s.ccy).unwrap_or_default();
            (v > Decimal::ZERO).then(|| (c.name.clone(), s.fixed.contains(&c.name), v))
        })
        .collect();
    let spent: BTreeMap<String, Decimal> = by_category(&s.rows, m, end, s.ccy, &s.fx, &s.parents).into_iter().collect();
    let big = plan.accounts.big_bills.filter(|a| Some(*a) != plan.accounts.pay).and_then(|a| {
        let docs = app.db.documents(&app.entity_ids()).unwrap_or_default();
        let rows: Vec<_> = s.rows.iter().filter(|t| t.posted <= end).cloned().collect();
        let (_, bal) = *insights::latest_balances(&rows, &docs).get(&a)?;
        let acct = app.db.accounts().ok()?.into_iter().find(|x| x.id == a)?;
        Some((acct.name, s.fx.convert(bal, acct.currency, s.ccy).unwrap_or_default()))
    });
    plan_doc::report_card(m, &plan, &budgets, &spent, &s.funds, &s.rows, &s.parents, s.ccy, &s.fx, big, &s.needs)
}

fn card_blocks(c: &ReportCard) -> Vec<Block> {
    let m = |v: Decimal| c.money(v);
    let vs = |actual: Decimal, plan: Decimal, more_is_good: bool| {
        let d = actual - plan;
        let good = if more_is_good { d >= Decimal::ZERO } else { d <= Decimal::ZERO };
        format!("{}plan {} ({}{})", if good { "" } else { "!" }, m(plan), if d >= Decimal::ZERO { "+" } else { "" }, m(d))
    };
    let mut b = vec![
        Block::Title(
            format!("Report Card: {}", c.month.format("%B %Y")),
            format!("Your {} against what happened, from your statements", if c.method == Method::Own { "budget plan".to_string() } else { format!("{} plan", c.method.label()) }),
        ),
        Block::Tiles(vec![
            ("Saved".into(), m(c.saved_actual), vs(c.saved_actual, c.save_plan, true)),
            ("Spent".into(), m(c.spent_actual), vs(c.spent_actual, c.spent_plan, false)),
            ("Income".into(), m(c.income_actual), vs(c.income_actual, c.income_plan, true)),
        ]),
    ];
    let over = c.over();
    b.push(Block::Heading("What to Do Next".into()));
    let mut todo: Vec<String> = Vec::new();
    for l in over.iter().take(4) {
        todo.push(format!("{} went {} over its {} budget. Raise it to what it really costs, or spend less on it next month.", l.name, m(l.spent - l.budget), m(l.budget)));
    }
    if let Some((n, v)) = c.unbudgeted.first() {
        todo.push(format!("{} had {} of spending with no budget. Give it one so the plan includes it.", n, m(*v)));
    }
    if let Some((name, bal)) = &c.big_account
        && *bal < c.big_should_have
    {
        todo.push(format!("{name} is {} short of what the big bills need by now ({}). Top it up.", m(c.big_should_have - *bal), m(c.big_should_have)));
    }
    if c.save_plan > Decimal::ZERO && c.saved_actual < c.save_plan {
        todo.push(format!("Savings came in {} under the {} goal.", m(c.save_plan - c.saved_actual), m(c.save_plan)));
    }
    if todo.is_empty() {
        todo.push("Every budgeted category stayed within its budget. Nothing to change.".into());
    }
    for t in todo {
        b.push(Block::Para(format!("• {t}"), Look::Normal));
    }

    if let Some(sp) = c.method.split() {
        b.push(Block::Heading(format!("Against {}", c.method.label())));
        let saved = c.saved_actual + c.pension;
        let row = |name: &str, v: Decimal, t: u8, over_is_bad: bool| {
            let p = share(v, c.base);
            let bad = if over_is_bad { p > Decimal::from(t) } else { p < Decimal::from(t) };
            (vec![name.to_string(), m(v), format!("{p}%"), format!("{t}%"), String::new()], if bad { Look::Warn } else { Look::Normal })
        };
        let mut rows = Vec::new();
        match (sp.needs, sp.wants) {
            (Some(n), Some(w)) => {
                rows.push(row("Needs", c.needs_actual, n, true));
                rows.push(row("Wants", c.wants_actual, w, true));
            }
            _ => rows.push(row("All spending", c.needs_actual + c.wants_actual, sp.spend, true)),
        }
        rows.push(row("Saved (with pension)", saved, sp.save, false));
        b.push(Block::Table(Table { widths: vec![3.0, 1.6, 1.0, 1.0, 2.0], right: vec![false, true, true, true, false], head: vec!["".into(), "Actual".into(), "Share".into(), "Target".into(), "".into()], rows, checks: false }));
        b.push(Block::Para(format!("Shares of what came in plus your pension ({}). Big bills paid this month count in their categories.", m(c.base)), Look::Dim));
    }

    b.push(Block::Heading("Category by Category".into()));
    let mut rows: Vec<(Vec<String>, Look)> = c
        .lines
        .iter()
        .map(|l| {
            let left = l.budget - l.spent;
            (vec![l.name.clone(), if l.fixed { "Fixed" } else { "Variable" }.into(), m(l.budget), m(l.spent), if left >= Decimal::ZERO { m(left) } else { format!("over {}", m(-left)) }], if left < Decimal::ZERO { Look::Warn } else { Look::Normal })
        })
        .collect();
    let (bt, st): (Decimal, Decimal) = (c.lines.iter().map(|l| l.budget).sum(), c.lines.iter().map(|l| l.spent).sum());
    rows.push((vec!["Budgeted categories".into(), String::new(), m(bt), m(st), m(bt - st)], Look::Bold));
    b.push(Block::Table(Table {
        widths: vec![3.2, 1.2, 1.5, 1.5, 1.6],
        right: vec![false, false, true, true, true],
        head: vec!["Category".into(), "Type".into(), "Budget".into(), "Spent".into(), "Left".into()],
        rows,
        checks: false,
    }));
    if !c.unbudgeted.is_empty() {
        b.push(Block::Heading("Spending Without a Budget".into()));
        b.push(Block::Table(Table {
            widths: vec![4.0, 1.6, 3.0],
            right: vec![false, true, false],
            head: vec!["Category".into(), "Spent".into(), "".into()],
            rows: c.unbudgeted.iter().map(|(n, v)| (vec![n.clone(), m(*v), String::new()], Look::Normal)).collect(),
            checks: false,
        }));
    }
    b.push(Block::Heading("The Month's Money".into()));
    let rows = vec![
        (vec!["Income".into(), m(c.income_plan), m(c.income_actual)], Look::Normal),
        (vec!["- Spending".into(), m(c.spent_plan), m(c.spent_actual)], Look::Normal),
        (vec!["- Loan payments".into(), m(c.loans_plan), m(c.loans_actual)], Look::Normal),
        (vec!["Big bills to set aside".into(), m(c.big_plan), "—".into()], Look::Dim),
        (vec!["= Saved".into(), m(c.save_plan), m(c.saved_actual)], if c.saved_actual < c.save_plan { Look::Warn } else { Look::Bold }),
    ];
    b.push(Block::Table(Table { widths: vec![3.6, 1.6, 1.6, 2.0], right: vec![false, true, true, false], head: vec!["".into(), "Plan".into(), "Actual".into(), "".into()], rows: rows.into_iter().map(|(mut r, l)| { r.push(String::new()); (r, l) }).collect(), checks: false }));
    b.push(Block::Para("Saved is income minus spending and loan payments, transfers between your own accounts left out. The big-bill share is part of what's saved until the bill comes.".into(), Look::Dim));
    b
}

pub fn save_card(app: &Rc<App>, month: NaiveDate) {
    match report_card(app, month) {
        Some(c) => {
            let name = format!("Report Card {}.pdf", c.month.format("%Y-%m"));
            let c = Rc::new(RefCell::new(Some(c)));
            save_as(app, &name, move |a, path| {
                if let Some(c) = c.borrow().as_ref() {
                    output(a, "Report Card", format!("FinGraph Report Card, {}", c.month.format("%B %Y")), card_blocks(c), Some(path));
                }
            });
        }
        None => app.toast("Set up your budget first (Budget → Set Up Budget…)"),
    }
}

pub fn print_card(app: &Rc<App>, month: NaiveDate) {
    match report_card(app, month) {
        Some(c) => output(app, "Report Card", format!("FinGraph Report Card, {}", c.month.format("%B %Y")), card_blocks(&c), None),
        None => app.toast("Set up your budget first (Budget → Set Up Budget…)"),
    }
}

/// The Budget page's "Budget Plan" menu: the plan (save, print), the calendar, the report card.
pub fn menu_button(app: &Rc<App>) -> gtk::MenuButton {
    let mb = gtk::MenuButton::builder().label("Budget Plan").tooltip_text("Take your budget with you: a printable plan, a calendar of paydays and bills, and a monthly report card").build();
    let w = Rc::downgrade(app);
    mb.set_create_popup_func(move |mb| {
        let Some(a) = w.upgrade() else { return };
        let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        let pop = gtk::Popover::builder().child(&col).build();
        let card_month = last_full_month(&a);
        let items: Vec<(String, crate::AppFn)> = vec![
            ("Save Budget Plan as PDF…".into(), Rc::new(save_plan)),
            ("Print Budget Plan…".into(), Rc::new(print_plan)),
            ("Save Calendar of Paydays & Bills…".into(), Rc::new(save_calendar)),
            (format!("Save {} Report Card as PDF…", card_month.format("%B")), Rc::new(move |a: &Rc<App>| save_card(a, card_month))),
            (format!("Print {} Report Card…", card_month.format("%B")), Rc::new(move |a: &Rc<App>| print_card(a, card_month))),
        ];
        for (label, f) in items {
            let b = gtk::Button::builder().label(label).css_classes(["flat"]).build();
            if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                l.set_xalign(0.0);
            }
            let (w, pop) = (Rc::downgrade(&a), pop.clone());
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Some(a) = w.upgrade() {
                    f(&a);
                }
            });
            col.append(&b);
        }
        mb.set_popover(Some(&pop));
    });
    mb
}
