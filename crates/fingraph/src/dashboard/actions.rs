//! Summary starts with decisions, while charts remain available under Detailed Analysis.
use super::*;
use gtk::glib;
use fingraph_core::money::fmt_money;

pub(super) fn fill(app: &Rc<App>, into: &gtk::Box) {
    crate::clear(into);
    into.append(&crate::heading("Next Steps"));
    let list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let add = |title: &str, detail: &str, label: &str, action: &str| {
        let row = adw::ActionRow::builder().title(glib::markup_escape_text(title)).subtitle(glib::markup_escape_text(detail)).build();
        row.add_suffix(&gtk::Button::builder().label(label).action_name(action).valign(gtk::Align::Center).build());
        list.append(&row);
    };
    let ids = app.entity_ids();
    let last = insights::add_months(insights::month_start(today()), -1);
    let coverage = app.db.coverage(&ids, 2, today()).unwrap_or_default();
    let incomplete = coverage.iter().filter(|c| c.months.iter().any(|(m, state)| *m == last && matches!(state,
        fingraph_core::workflows::MonthState::Missing | fingraph_core::workflows::MonthState::Partial | fingraph_core::workflows::MonthState::Discrepancy))).count();
    if incomplete > 0 {
        add(&format!("{incomplete} accounts need statements or review for {}", last.format("%B")),
            "The month's totals are incomplete. Import the remaining statements before judging spending or savings.", "Check Statements", "win.page-4");
    }
    let merchants = app.db.triage_count(&ids).unwrap_or(0);
    if merchants > 0 { add(&format!("{merchants} merchants need an answer"), "Review their categories before closing the month.", "Review", "win.show-review"); }
    let s = crate::budget::snap(app);
    let m = insights::month_start(crate::budget::data_through(app));
    let envelopes = crate::budget::envs(app, &s, m);
    let over = envelopes.values().filter(|e| e.funded() > Decimal::ZERO && e.available() < Decimal::ZERO).count();
    if over > 0 { add(&format!("{over} categories over budget in {}", m.format("%B")), "Review the charges and adjust your plan or move budget between categories.", "Open Budget", "win.page-1"); }
    if let Some(p) = &s.plan {
        let big = s.funds.iter().filter_map(|f| s.fx.convert(f.monthly(), f.currency, s.ccy)).sum();
        let left = fingraph_core::method::to_assign(p.monthly_income(), s.budgets.values().copied().sum(), big, p.loan_payments, p.debt_extra, p.savings_goal);
        if left < Decimal::ZERO { add("Your plan assigns more than your income", "Reduce an assignment or update your income before following the payday checklist.", "Edit Plan", "win.setup-budget"); }
    }
    if let Some(fund) = s.funds.iter().min_by_key(|f| f.due(today())) {
        let due = fund.due(today());
        if (due - today()).num_days() <= 31 {
            let detail = app.db.fund_allocation(fund.id, due)
                .map(|v| format!("Due {} · recorded allocation {} (not bank-verified). Check funding and payment.", due.format("%d %b"), fmt_money(v, fund.currency)))
                .unwrap_or_else(|| format!("Due {} · allocation not recorded. Check how much you have reserved.", due.format("%d %b")));
            add(&fund.name, &detail, "Open Budget", "win.page-1");
        }
    }
    let row = adw::ActionRow::builder().title("Review and close the month").subtitle("Check statement coverage, reconcile the budget and save your report card.").build();
    let close = gtk::Button::builder().label("Monthly Close…").valign(gtk::Align::Center).build();
    let weak = Rc::downgrade(app);
    close.connect_clicked(move |_| {
        if let Some(a) = weak.upgrade() {
            let weak = Rc::downgrade(&a);
            crate::close::open(&a, None, Rc::new(move || if let Some(a) = weak.upgrade() { a.refresh(); }));
        }
    });
    row.add_suffix(&close); list.append(&row);
    into.append(&list);
}
