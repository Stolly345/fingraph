//! Monthly Close: the routine for when a month's statements are in. Check they're all there and
//! balance, sort what's new, settle the budget (cover overspending, see what rolls over), tick off
//! the regular bills, then close the month with a summary.

use adw::prelude::*;
use chrono::NaiveDate;
use fingraph_core::Decimal;
use fingraph_core::budget::{CloseSummary, Envelope};
use fingraph_core::db::TxFilter;
use fingraph_core::insights::{add_months, month_start};
use fingraph_core::money::fmt_money;
use fingraph_core::outlook::{self, BillState};
use fingraph_core::workflows::MonthState;
use gtk::glib;
use std::cell::Cell;
use std::rc::Rc;

use crate::budget::{envs, move_dialog, snap, step_page};
use crate::{App, dim, heading, today};

type StepFn = Rc<dyn Fn(usize)>;

const STEPS: [&str; 6] = ["Statements", "Sort What's New", "Budget", "Bills", "Goals", "Close the Month"];

fn list() -> gtk::ListBox {
    gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build()
}

fn row(title: &str, sub: &str, icon: &str, class: &str) -> adw::ActionRow {
    let r = adw::ActionRow::builder().title(glib::markup_escape_text(title)).subtitle(glib::markup_escape_text(sub)).build();
    let img = gtk::Image::from_icon_name(icon);
    if !class.is_empty() {
        img.add_css_class(class);
    }
    r.add_prefix(&img);
    r
}

/// The newest month that's over and not yet closed (last month, usually).
fn default_month(app: &App) -> NaiveDate {
    let closed = app.db.closes(&fingraph_core::budget::scope(&app.entity_ids())).unwrap_or_default();
    let last = add_months(month_start(today()), -1);
    (0..12).map(|i| add_months(last, -i)).find(|m| !closed.contains_key(m)).unwrap_or(last)
}

/// What a month came to: income, spending, saved, overspent categories, missed bills.
pub fn summary(app: &App, m: NaiveDate) -> CloseSummary {
    let s = snap(app);
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    let bar = fingraph_core::insights::monthly(&s.rows, m, 1, s.ccy, &s.fx).pop();
    let (income, spent) = bar.map_or((Decimal::ZERO, Decimal::ZERO), |b| (b.income, b.expenses));
    let rolls = app.db.rollovers(&s.scope);
    let over = envs(app, &s, m).iter().filter(|(id, e)| (!e.budget.is_zero() || rolls.contains_key(id)) && e.available().is_sign_negative()).count();
    let ids = app.entity_ids();
    let whole = app.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
    let docs = app.db.documents(&ids).unwrap_or_default();
    let missed = outlook::bills_for(&whole, &outlook::covered(&docs), m, s.ccy, &s.fx).iter().filter(|b| b.state == BillState::Missed).count();
    let aom = outlook::age_of_money(&whole, &docs, s.ccy, &s.fx).and_then(|a| a.by_month.iter().rev().find(|p| p.0 <= end).map(|p| p.1));
    CloseSummary { currency: Some(s.ccy), income, spent, saved: income - spent, age_of_money: aom, over, missed, net_worth: crate::wealth::net_worth_at(app, end) }
}

/// `month`: None = the newest month not yet closed. `done` runs after closing.
pub fn open(app: &Rc<App>, month: Option<NaiveDate>, done: crate::Refresh) {
    let month = Rc::new(Cell::new(month.unwrap_or_else(|| default_month(app))));
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let bodies: Vec<gtk::Box> = STEPS
        .iter()
        .enumerate()
        .map(|(i, t)| {
            let (page, b) = step_page(t, "");
            stack.add_named(&page, Some(&i.to_string()));
            b
        })
        .collect();
    let bodies = Rc::new(bodies);

    // the month, newest first; ✓ = closed
    let scope = fingraph_core::budget::scope(&app.entity_ids());
    let months: Vec<NaiveDate> = (1..=12).map(|i| add_months(month_start(today()), -i)).collect();
    let closed = app.db.closes(&scope).unwrap_or_default();
    let labels: Vec<String> = months.iter().map(|m| format!("{}{}", m.format("%B %Y"), if closed.contains_key(m) { "  ✓" } else { "" })).collect();
    let pick = gtk::DropDown::builder().model(&gtk::StringList::new(&labels.iter().map(String::as_str).collect::<Vec<_>>())).build();
    pick.set_selected(months.iter().position(|m| *m == month.get()).unwrap_or(0) as u32);

    let back = gtk::Button::builder().label("Back").sensitive(false).build();
    let fwd = gtk::Button::builder().label("Next").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::builder().title_widget(&pick).build();
    header.pack_start(&back);
    header.pack_end(&fwd);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&stack));
    let dlg = adw::Dialog::builder().title("Monthly Close").content_width(760).content_height(720).child(&tv).build();
    let step = Rc::new(Cell::new(0usize));

    // each step is drawn when shown, so it reflects what you just did (triage, a move)
    let show: StepFn = {
        let (w, stack, back, fwd, step, bodies, month, dlg) = (Rc::downgrade(app), stack.clone(), back.clone(), fwd.clone(), step.clone(), bodies.clone(), month.clone(), dlg.clone());
        let me: Rc<std::cell::RefCell<Option<StepFn>>> = Rc::default();
        let f: StepFn = {
            let me = me.clone();
            Rc::new(move |i: usize| {
                let Some(a) = w.upgrade() else { return };
                step.set(i);
                stack.set_visible_child_name(&i.to_string());
                back.set_sensitive(i > 0);
                fwd.set_label(if i == STEPS.len() - 1 { "Done" } else { "Next" });
                dlg.set_title(&format!("Monthly Close · {} of {}", i + 1, STEPS.len()));
                let again = me.borrow().clone();
                let redo: crate::Refresh = Rc::new(move || {
                    if let Some(f) = &again {
                        f(i);
                    }
                });
                fill(&a, i, month.get(), &bodies[i], redo);
            })
        };
        *me.borrow_mut() = Some(f.clone());
        f
    };
    {
        let (show, step, month, months) = (show.clone(), step.clone(), month.clone(), months.clone());
        pick.connect_selected_notify(move |p| {
            month.set(months[(p.selected() as usize).min(months.len() - 1)]);
            show(step.get());
        });
    }
    {
        let (show, step) = (show.clone(), step.clone());
        back.connect_clicked(move |_| show(step.get().saturating_sub(1)));
    }
    {
        let (show, step, dlg) = (show.clone(), step.clone(), dlg.clone());
        fwd.connect_clicked(move |_| {
            if step.get() + 1 < STEPS.len() {
                show(step.get() + 1);
            } else {
                dlg.close();
                done();
            }
        });
    }
    show(0);
    dlg.present(Some(&app.win));
}

fn fill(app: &Rc<App>, i: usize, m: NaiveDate, b: &gtk::Box, redo: crate::Refresh) {
    // keep the step's title (first child), replace the rest
    while let Some(c) = b.first_child().and_then(|t| t.next_sibling()) {
        b.remove(&c);
    }
    let s = snap(app);
    let (ccy, fx) = (s.ccy, &s.fx);
    let money = |d: Decimal| fmt_money(d, ccy);
    let end = add_months(m, 1).pred_opt().unwrap_or(m);
    let ids = app.entity_ids();
    match i {
        0 => {
            b.append(&dim(&format!("Every account should have a statement covering {} that balances to the cent. Statements come a month at a time; download them and drop them on the Statements page.", m.format("%B"))));
            let cov = app.db.coverage(&ids, 13, today()).unwrap_or_default();
            let l = list();
            let mut missing = 0;
            for c in &cov {
                let st = c.months.iter().find(|x| x.0 == m).map(|x| x.1);
                let (sub, icon, class) = match st {
                    Some(MonthState::Verified) => ("Statement in and balanced", "emblem-ok-symbolic", "success"),
                    Some(MonthState::Discrepancy) => ("Statement in, but it doesn't balance — open it on the Statements page", "dialog-warning-symbolic", "warning"),
                    Some(MonthState::Missing) => ("No statement yet", "dialog-question-symbolic", "error"),
                    Some(MonthState::Pending) => ("Not due yet", "content-loading-symbolic", "dim-label"),
                    Some(MonthState::Before) | None => ("Opened later", "action-unavailable-symbolic", "dim-label"),
                };
                if matches!(st, Some(MonthState::Missing | MonthState::Discrepancy)) {
                    missing += 1;
                }
                l.append(&row(&c.account, sub, icon, class));
            }
            b.append(&l);
            if missing > 0 {
                let go = gtk::Button::builder().label("Open Statements").halign(gtk::Align::Start).css_classes(["pill"]).build();
                let w = Rc::downgrade(app);
                go.connect_clicked(move |btn| {
                    if let Some(a) = w.upgrade() {
                        if let Some(d) = btn.ancestor(adw::Dialog::static_type()).and_downcast::<adw::Dialog>() {
                            d.close();
                        }
                        a.show_view("statements");
                    }
                });
                b.append(&go);
                b.append(&dim("You can carry on without them; what's missing just won't count yet."));
            }
        }
        1 => {
            let rows = app.db.transactions(&TxFilter { entity_ids: ids.clone(), from: Some(m), to: Some(end), uncategorized_only: true, ..Default::default() }).unwrap_or_default();
            if rows.is_empty() {
                b.append(&row("Everything in this month has a category", "", "emblem-ok-symbolic", "success"));
            } else {
                let total: Decimal = rows.iter().filter(|t| t.amount.is_sign_negative()).map(|t| -fx.convert(t.amount, t.currency, ccy)).sum();
                b.append(&row(&format!("{} transactions without a category", rows.len()), &format!("{} of spending that can't count against the budget yet", money(total)), "mail-unread-symbolic", "warning"));
                let go = gtk::Button::builder().label("Start Smart Triage").halign(gtk::Align::Start).css_classes(["suggested-action", "pill"]).build();
                let w = Rc::downgrade(app);
                go.connect_clicked(move |_| {
                    if let Some(a) = w.upgrade() {
                        crate::wizard::triage(&a);
                    }
                });
                b.append(&go);
                let again = gtk::Button::builder().label("Check Again").halign(gtk::Align::Start).css_classes(["flat"]).build();
                again.connect_clicked(move |_| redo());
                b.append(&again);
            }
        }
        2 => {
            let env = envs(app, &s, m);
            let rolls = app.db.rollovers(&s.scope);
            let mut cats: Vec<_> = s.cats.iter().filter(|c| env.get(&c.id).is_some_and(|e| !e.budget.is_zero() || rolls.contains_key(&c.id))).collect();
            if cats.is_empty() {
                b.append(&dim("No budget yet — set one up on the Budget page."));
                return;
            }
            cats.sort_by_key(|c| env.get(&c.id).map(Envelope::available).unwrap_or_default());
            b.append(&dim("Overspent categories first. Cover one by moving money from a category with some left; categories set to roll over carry what's left (or overspent) into next month."));
            let names: Vec<(i64, String, Decimal)> = s.cats.iter().map(|c| (c.id, c.name.clone(), env.get(&c.id).map(Envelope::available).unwrap_or_default())).collect();
            let l = list();
            for c in cats {
                let e = env[&c.id];
                let avail = e.available();
                let sub = format!("{} spent of {}{}", money(e.spent), money(e.funded()), if rolls.contains_key(&c.id) { " · rolls over" } else { "" });
                let r = if avail.is_sign_negative() { row(&c.name, &sub, "dialog-warning-symbolic", "error") } else { row(&c.name, &sub, "emblem-ok-symbolic", "success") };
                r.add_suffix(&gtk::Label::builder().label(if avail.is_sign_negative() { format!("over by {}", money(-avail)) } else { format!("{} left", money(avail)) }).css_classes(["tnum", if avail.is_sign_negative() { "amount-out" } else { "dim-label" }]).build());
                if avail.is_sign_negative() {
                    let cover = gtk::Button::builder().label("Cover…").valign(gtk::Align::Center).build();
                    let (w, sc, id, names, redo) = (Rc::downgrade(app), s.scope.clone(), c.id, names.clone(), redo.clone());
                    cover.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            move_dialog(&a, &sc, m, id, true, &names, ccy, redo.clone());
                        }
                    });
                    r.add_suffix(&cover);
                }
                l.append(&r);
            }
            b.append(&l);
        }
        3 => {
            let whole = app.db.transactions(&TxFilter { entity_ids: ids.clone(), ..Default::default() }).unwrap_or_default();
            let docs = app.db.documents(&ids).unwrap_or_default();
            let bills = outlook::bills_for(&whole, &outlook::covered(&docs), m, ccy, fx);
            if bills.is_empty() {
                b.append(&dim("No regular bills found yet — they show up once a payment has repeated on a steady rhythm."));
                return;
            }
            let missed = bills.iter().filter(|x| x.state == BillState::Missed).count();
            b.append(&dim(&format!(
                "Regular payments {} should have had. {}",
                m.format("%B"),
                if missed > 0 { format!("{missed} didn't come — paid another way, cancelled, or a problem worth a call.") } else { "Nothing missing.".into() }
            )));
            let l = list();
            for x in &bills {
                let (sub, icon, class) = match &x.state {
                    BillState::Paid(d) => (format!("Paid {} · {}", d.iter().map(|d| d.format("%d %b").to_string()).collect::<Vec<_>>().join(", "), x.account), "emblem-ok-symbolic", "success"),
                    BillState::Missed => (format!("Due around {} — not on the statement · {}", x.due.format("%d %b"), x.account), "dialog-warning-symbolic", "error"),
                    BillState::NotYetSeen => (format!("Due around {} — that statement isn't in yet · {}", x.due.format("%d %b"), x.account), "content-loading-symbolic", "dim-label"),
                    BillState::New => (format!("New this month, first on {} · {}", x.due.format("%d %b"), x.account), "starred-symbolic", "accent"),
                };
                let r = row(&x.payee, &sub, icon, class);
                r.add_suffix(&gtk::Label::builder().label(money(x.amount)).css_classes(["tnum"]).build());
                l.append(&r);
            }
            b.append(&l);
        }
        4 => crate::wealth::fill_close_goals(app, m, b, redo),
        _ => {
            let sum = summary(app, m);
            let closed = app.db.closes(&s.scope).unwrap_or_default();
            let l = list();
            let line = |t: &str, v: String| {
                let r = adw::ActionRow::builder().title(t).build();
                r.add_suffix(&gtk::Label::builder().label(v).css_classes(["tnum", "heading"]).build());
                l.append(&r);
            };
            line("Money in", money(sum.income));
            line("Spending", money(sum.spent));
            let rate = if sum.income.is_zero() { String::new() } else { format!(" ({}% of income)", (sum.saved / sum.income * Decimal::ONE_HUNDRED).round_dp(0)) };
            line("Saved", format!("{}{rate}", money(sum.saved)));
            if let Some(a) = sum.age_of_money {
                line("Age of Money", format!("{a} days"));
            }
            if let Some(n) = sum.net_worth {
                line("Net worth", money(n));
            }
            line("Over budget", if sum.over == 0 { "none".into() } else { format!("{} categories", sum.over) });
            line("Bills that didn't come", sum.missed.to_string());
            b.append(&heading(&format!("{} in Brief", m.format("%B %Y"))));
            b.append(&l);
            // earlier closes, to compare
            let prev: Vec<_> = closed.iter().rev().filter(|(k, _)| **k < m).take(3).collect();
            if !prev.is_empty() {
                b.append(&dim(&format!(
                    "Earlier: {}",
                    prev.iter().map(|(k, (v, _))| format!("{} saved {}", k.format("%b"), fmt_money(v.saved, v.currency.unwrap_or(ccy)))).collect::<Vec<_>>().join(" · ")
                )));
            }
            let is_closed = closed.contains_key(&m);
            let btn = gtk::Button::builder().label(if is_closed { format!("Close {} Again (Update the Summary)", m.format("%B")) } else { format!("Close {}", m.format("%B")) }).halign(gtk::Align::Start).css_classes(["suggested-action", "pill"]).build();
            let (w, sc, sum2) = (Rc::downgrade(app), s.scope.clone(), sum.clone());
            let r2 = redo.clone();
            btn.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    if a.saved(a.db.close_month(&sc, m, &sum2)) {
                        crate::outlook::remember_forecast(&a);
                        a.toast(&format!("{} closed", m.format("%B %Y")));
                    }
                    r2();
                }
            });
            b.append(&btn);
            if is_closed {
                let reopen = gtk::Button::builder().label("Reopen").halign(gtk::Align::Start).css_classes(["flat"]).build();
                let (w, sc) = (Rc::downgrade(app), s.scope.clone());
                reopen.connect_clicked(move |_| {
                    if let Some(a) = w.upgrade() {
                        a.saved(a.db.reopen_month(&sc, m));
                        redo();
                    }
                });
                b.append(&reopen);
            }
        }
    }
}
