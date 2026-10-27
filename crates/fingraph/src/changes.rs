//! What Changed: the insight cards, one per kind (price creep, bill spikes, double charges, fees,
//! category drift, P&L), each with an expandable list of what's behind it.

use adw::prelude::*;
use fingraph_core::db::TxRow;
use fingraph_core::insights::{self, Insight, InsightKind};
use gtk::glib;
use std::rc::Rc;

use crate::{App, dim, heading};

/// The rows the dashboard counts: money out in a left-out category doesn't count as spending.
pub fn counted_rows(app: &App, rows: Vec<TxRow>) -> Vec<TxRow> {
    let prefs = app.db.prefs();
    if prefs.excluded_categories.is_empty() {
        return rows;
    }
    let parents = app.parents();
    let top = |t: &TxRow| t.category_id.and_then(|id| parents.get(&id).cloned()).or_else(|| t.category_name.clone());
    rows.into_iter().filter(|t| !(t.amount.is_sign_negative() && top(t).is_some_and(|c| prefs.excluded_categories.contains(&c)))).collect()
}

/// The insights for what's on screen (workspaces, range, currency), minus kinds switched off.
pub fn current(app: &App) -> Vec<Insight> {
    let rows = counted_rows(app, app.db.transactions(&fingraph_core::db::TxFilter { entity_ids: app.entity_ids(), split_lines: true, ..Default::default() }).unwrap_or_default());
    let prefs = app.db.prefs();
    let (from, to) = app.range();
    let hidden: Vec<String> = app.db.setting("hidden_insights").unwrap_or_default().split(',').map(str::to_string).collect();
    let tune = insights::Tuning { duplicate_days: prefs.duplicate_days, spike_percent: prefs.spike_percent as f64, drift_percent: prefs.drift_percent as f64 };
    let ents = app.entities.borrow().clone();
    insights::insights(&rows, &ents, from, to, app.display(), &app.db.fx().unwrap_or_default(), &app.parents(), &tune)
        .into_iter()
        .filter(|i| !hidden.contains(&format!("{:?}", i.kind)))
        .collect()
}

/// How many cards the page shows (one per kind), for the sidebar.
pub fn count(app: &App) -> usize {
    let mut kinds: Vec<InsightKind> = current(app).iter().map(|i| i.kind).collect();
    kinds.dedup();
    kinds.len()
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(18).margin_bottom(24).margin_start(18).margin_end(18).build();
    let head = gtk::Box::builder().spacing(12).build();
    let title = gtk::Box::builder().orientation(gtk::Orientation::Vertical).hexpand(true).build();
    title.append(&heading("What Changed?"));
    title.append(&dim("Price rises, unusual bills, possible double charges and what the banks charged you, for the date range above."));
    head.append(&title);
    let choose = gtk::Button::builder().label("Choose Cards…").css_classes(["flat"]).valign(gtk::Align::Center).build();
    {
        let w = Rc::downgrade(app);
        choose.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                crate::preferences::show(&a);
                if let Some(d) = a.win.visible_dialog().and_downcast::<adw::PreferencesDialog>() {
                    d.set_visible_page_name("dashboard");
                }
            }
        });
    }
    head.append(&choose);
    body.append(&head);
    let cards = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).min_children_per_line(1).max_children_per_line(2).column_spacing(12).row_spacing(12).homogeneous(false).build();
    body.append(&cards);
    let page = gtk::ScrolledWindow::builder().child(&adw::Clamp::builder().maximum_size(1300).child(&body).build()).vexpand(true).build();

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            while let Some(c) = cards.first_child() {
                cards.remove(&c);
            }
            let ins = current(&a);
            let hidden: Vec<String> = Vec::new();
            let mut shown = 0;
            render(&a, &cards, &ins, &hidden, &mut shown);
            if shown == 0 {
                cards.insert(&dim("Nothing unusual in this range."), -1);
            }
        })
    };
    (page.upcast(), refresh)
}

fn render(a: &Rc<App>, cards: &gtk::FlowBox, ins: &[Insight], hidden: &[String], shown: &mut usize) {
        // one card per kind: a summary up front, every item behind an expander
        for kind in InsightKind::ALL.into_iter().filter(|k| !hidden.contains(&format!("{k:?}"))) {
            let of_kind: Vec<&insights::Insight> = ins.iter().filter(|i| i.kind == kind).collect();
            if of_kind.is_empty() {
                continue;
            }
            let (title, detail, rows): (String, String, Vec<insights::InsightPart>) = match of_kind.as_slice() {
                [one] => (one.title.clone(), one.detail.clone(), one.parts.clone()),
                many => {
                    let n = many.len();
                    let title = match kind {
                        InsightKind::Duplicate => format!("{n} possible double charges"),
                        InsightKind::Spike => format!("{n} bills above their usual"),
                        InsightKind::PriceCreep => format!("{n} subscriptions got dearer"),
                        InsightKind::CategoryDrift => format!("{n} categories trending up"),
                        InsightKind::TaxSummary => "Net income year to date".into(),
                        InsightKind::BankFees => format!("{n} fee summaries"),
                        InsightKind::Annual => format!("{n} annual renewals"),
                    };
                    let preview = many.iter().take(2).map(|i| i.title.as_str()).collect::<Vec<_>>().join(" · ");
                    let detail = if n > 2 { format!("{preview} · and {} more", n - 2) } else { preview };
                    (title, detail, many.iter().map(|i| (i.title.clone(), i.detail.clone(), i.tx_ids.clone())).collect())
                }
            };
            let all_ids: Vec<i64> = of_kind.iter().flat_map(|i| i.tx_ids.iter().copied()).collect();
            let show = |ids: Vec<i64>, label: String| {
                let b = gtk::Button::builder().label(label).css_classes(["flat"]).halign(gtk::Align::Start).valign(gtk::Align::Center).build();
                let w = Rc::downgrade(a);
                b.connect_clicked(move |_| {
                    if let Some(a) = w.upgrade() {
                        a.show_transactions(ids.clone());
                    }
                });
                b
            };
            let card = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).css_classes(["card", "chart-card"]).build();
            let top = gtk::Box::builder().spacing(8).build();
            top.append(&gtk::Image::from_icon_name(kind.icon()));
            top.append(&gtk::Label::builder().label(kind.label()).xalign(0.0).css_classes(["caption-heading", "dim-label"]).build());
            card.append(&top);
            card.append(&gtk::Label::builder().label(&title).xalign(0.0).wrap(true).css_classes(["heading"]).build());
            card.append(&dim(&detail));
            if !all_ids.is_empty() {
                let n = all_ids.len();
                card.append(&show(all_ids, format!("Show {n} transaction{}", if n == 1 { "" } else { "s" })));
            }
            if !rows.is_empty() {
                let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                for (t, d, ids) in &rows {
                    let r = adw::ActionRow::builder().title(glib::markup_escape_text(t)).subtitle(glib::markup_escape_text(d)).build();
                    if !ids.is_empty() {
                        r.add_suffix(&show(ids.clone(), "Show".into()));
                    }
                    list.append(&r);
                }
                let exp = gtk::Expander::builder().label(format!("Details ({})", rows.len())).child(&list).build();
                card.append(&exp);
            }
            cards.insert(&card, -1);
            *shown += 1;
        }
}
