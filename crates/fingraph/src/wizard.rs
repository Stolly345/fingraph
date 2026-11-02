//! Smart Triage: one conversational card per unfamiliar merchant; every answer is learned and
//! applied retroactively. (The first-run walkthrough is `guide`.)

use adw::prelude::*;
use fingraph_core::db::Category;
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind, TriageGroup, TriageKind};
use fingraph_core::{Currency, EntityKind};
use gtk::glib;
use std::rc::Rc;

use crate::App;

// ──────────────────────────────────────────────────────────────── Smart Triage

/// The one-tap answers, A–Z (the suggestion, when there is one, goes first).
const QUICK: [&str; 16] = [
    "Bank Fees & Interest",
    "Business Expense",
    "Dining & Takeaway",
    "Entertainment",
    "Family Expense",
    "Groceries",
    "Health & Fitness",
    "Home & Garden",
    "Housing",
    "Income",
    "Property Expense",
    "Repairs & Maintenance",
    "Shopping",
    "Subscriptions & Digital",
    "Transport",
    "Utilities",
];

fn total_text(g: &TriageGroup) -> String {
    g.totals.iter().map(|(c, v)| fmt_money(v.abs(), *c)).collect::<Vec<_>>().join(" + ")
}

/// "4 payments of CI$ 125.00 (CI$ 500.00 in all)", or for varying amounts "3 payments totaling … (latest …)".
fn amounts_text(g: &TriageGroup) -> String {
    let typical = fmt_money(g.typical_amount.abs(), g.currency);
    match (g.count, g.same_amount) {
        (1, _) => format!("1 payment of {typical}"),
        (n, true) => format!("{n} payments of {typical} each ({} in all)", total_text(g)),
        (n, false) => format!("{n} payments totaling {} (latest {typical})", total_text(g)),
    }
}

fn cadence_word(days: Option<i64>) -> &'static str {
    match days {
        Some(7) => "/wk",
        Some(14) => "/2wk",
        Some(30) => "/mo",
        Some(91) => "/qtr",
        Some(365) => "/yr",
        _ => "",
    }
}

/// Forget triage answers and manual categories in the workspaces on screen, then start triage again.
pub fn reset_and_retriage(app: &Rc<App>) {
    let d = adw::AlertDialog::new(
        Some("Reset Learning?"),
        Some("Every learned rule, category, payee name, subscription flag and business flag in the workspaces you're viewing is forgotten, and Smart Triage starts over. Transfers matched between your own accounts stay matched. Your statements aren't touched."),
    );
    d.add_responses(&[("cancel", "Cancel"), ("reset", "Reset & Re-triage")]);
    d.set_response_appearance("reset", adw::ResponseAppearance::Destructive);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        if r != "reset" {
            return;
        }
        if let Err(e) = a.db.auto_backup("before-reset") {
            return a.toast(&format!("Couldn't make the backup first, so nothing was reset: {e}"));
        }
        match a.db.reset_learning(&a.entity_ids()) {
            Ok(n) => {
                a.toast(&format!("Forgotten — {n} transactions are back in triage"));
                a.refresh();
                triage(&a);
            }
            Err(e) => a.toast(&e.to_string()),
        }
    }));
    d.present(Some(&app.win));
}

/// Smart Triage lives on the To Review page.
pub fn triage(app: &Rc<App>) {
    app.show_view("review");
}

/// Learn the answer for a group: a rule over its merchant stem, applied to every match.
#[allow(clippy::too_many_arguments)]
fn answer(app: &Rc<App>, g: &TriageGroup, category_id: Option<i64>, payee: &str, subscription: bool, (scope, business, amount): (Option<i64>, Option<i64>, Option<fingraph_core::Decimal>), next: &Rc<dyn Fn()>, answered: &Rc<std::cell::Cell<usize>>) {
    let rule = Rule {
        id: 0,
        kind: RuleKind::NormalizedTokens,
        pattern: g.stem.clone(),
        clean_payee: Some(payee.trim().to_string()).filter(|s| !s.is_empty()),
        category_id,
        is_subscription: subscription,
        is_transfer: false,
        expected_cadence: if subscription { g.cadence.or(Some(30)) } else { None },
        entity_id: scope,
        business_entity_id: business,
        amount_min: amount,
        amount_max: amount,
    };
    // a learned rule reaches back over every past match, so the answer can be taken back
    app.undoable(|app| match app.db.learn(&rule) {
        Ok(n) => {
            // the wizard's own group is confirmed even where an older rule had guessed
            app.saved(app.db.set_category(&g.tx_ids, category_id));
            if subscription {
                app.saved(app.db.set_subscription(&g.tx_ids, true));
            }
            if business.is_some() {
                app.saved(app.db.set_business(&g.tx_ids, business));
            }
            answered.set(answered.get() + 1);
            Some(format!("Learned “{}” · updated {} transaction{}", g.suggested_payee, n.max(g.count), if n.max(g.count) == 1 { "" } else { "s" }))
        }
        Err(e) => {
            app.toast(&e.to_string());
            None
        }
    });
    next();
}

pub fn card(app: &Rc<App>, content: &gtk::Box, g: &TriageGroup, next: Rc<dyn Fn()>, answered: Rc<std::cell::Cell<usize>>) {
    let cats: Vec<Category> = app.categories();
    let ents = app.entities.borrow().clone();
    let cat_id = |name: &str| cats.iter().find(|c| c.name == name).map(|c| c.id);
    let plural = if g.count == 1 { "charge" } else { "charges" };
    let typical = fmt_money(-g.typical_amount, g.currency);
    // "CHEQUE", "BRANCH DEPOSIT": the bank's label for many different payees, so one answer would be wrong
    let varied = g.kind == TriageKind::Merchant && g.count > 1 && fingraph_core::rules::varied_payee(&g.stem);

    let q = gtk::Label::builder().xalign(0.0).wrap(true).use_markup(true).css_classes(["title-3"]).build();
    let esc = |s: &str| glib::markup_escape_text(s).to_string();
    q.set_markup(&match g.kind {
        TriageKind::Subscription => format!("We found {} {plural} from <tt>{}</tt> ({typical}{}). How should we categorize <b>{}</b>?", g.count, esc(&g.stem), cadence_word(g.cadence), esc(&g.suggested_payee)),
        TriageKind::Transfer => format!("We found {} to <tt>{}</tt>. Is this an internal credit card payment or transfer between your accounts?", esc(&amounts_text(g)), esc(&g.stem)),
        TriageKind::Merchant if varied => format!("These {} entries all read <tt>{}</tt>. Your bank prints that for every one, so they're probably to or from different people.", g.count, esc(&g.stem)),
        TriageKind::Merchant => format!("We found {} from <tt>{}</tt>. How should we categorize <b>{}</b>?", esc(&amounts_text(g)), esc(&g.stem), esc(&g.suggested_payee)),
    });
    content.append(&q);
    // the charges themselves: date, amount, account and the statement's own wording
    let list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    const SHOWN: usize = 6;
    let row_for = |c: &fingraph_core::rules::TriageInput| {
        let detail = if c.memo.is_empty() { c.raw.clone() } else { format!("{} — {}", c.raw, c.memo) };
        let r = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&c.posted.format("%a %d %b %Y").to_string()))
            .subtitle(glib::markup_escape_text(&format!("{} · {detail}", c.account)))
            .subtitle_lines(2)
            .build();
        r.add_suffix(&gtk::Label::builder().label(fmt_money(c.amount, c.currency)).css_classes([if c.amount.is_sign_negative() { "amount-out" } else { "amount-in" }]).build());
        r
    };
    for c in g.charges.iter().take(SHOWN) {
        list.append(&row_for(c));
    }
    if g.charges.len() > SHOWN {
        let more = adw::ExpanderRow::builder().title(format!("{} earlier", g.charges.len() - SHOWN)).build();
        for c in &g.charges[SHOWN..] {
            more.add_row(&row_for(c));
        }
        list.append(&more);
    }
    content.append(&list);

    // payee + scope
    let opts = adw::PreferencesGroup::new();
    let payee = adw::EntryRow::builder().title("Clean payee").text(&g.suggested_payee).build();
    opts.add(&payee);
    // scope: the workspace(s) these charges came from, or every workspace
    let mut scope_ids: Vec<Option<i64>> = ents.iter().filter(|e| g.entity_ids.contains(&e.id)).map(|e| Some(e.id)).collect();
    let mut scope_names: Vec<String> = ents.iter().filter(|e| g.entity_ids.contains(&e.id)).map(|e| e.name.clone()).collect();
    scope_ids.push(None);
    scope_names.push("All workspaces".into());
    let scope_row = adw::ComboRow::builder()
        .title("Remember for")
        .model(&gtk::StringList::new(&scope_names.iter().map(String::as_str).collect::<Vec<_>>()))
        .selected(if g.entity_ids.len() == 1 && !app.db.prefs().rules_all_workspaces { 0 } else { scope_ids.len() as u32 - 1 })
        .build();
    opts.add(&scope_row);
    // "this was a business expense": books every matching payment to a business workspace
    let biz_row = adw::SwitchRow::builder().title("Business expense").subtitle("Count it in a business P&amp;L, not personal spending").build();
    opts.add(&biz_row);
    let biz: Rc<std::cell::Cell<Option<i64>>> = Rc::default();
    {
        let (w, biz, home) = (Rc::downgrade(app), biz.clone(), g.entity_ids.first().copied().unwrap_or_default());
        biz_row.connect_active_notify(move |row| {
            let Some(a) = w.upgrade() else { return };
            if !row.is_active() {
                biz.set(None);
                row.set_subtitle("Count it in a business P&amp;L, not personal spending");
                return;
            }
            if biz.get().is_some() {
                return;
            }
            let (biz, row2, row3) = (biz.clone(), row.clone(), row.clone());
            a.with_business(
                home,
                move |a, id| {
                    biz.set(Some(id));
                    row2.set_subtitle(&format!("Booked to {}", a.entity_name(id)));
                },
                move |_| row3.set_active(false),
            );
        });
    }

    let scope = {
        let r = scope_row.clone();
        move || scope_ids.get(r.selected() as usize).copied().flatten()
    };
    let scope = Rc::new(scope);
    // "Adriana, CI$ 125.00 only": a different amount from the same payee comes back to To Review
    let exact = adw::SwitchRow::builder()
        .title(glib::markup_escape_text(&format!("Only for {} payments", fmt_money(g.typical_amount.abs(), g.currency))))
        .subtitle("Any other amount from this payee (a bonus, a one-off job) comes back here to ask")
        .visible(g.same_amount)
        .build();
    opts.add(&exact);
    let scope = {
        let (s, biz, exact, amt) = (scope.clone(), biz.clone(), exact.clone(), g.typical_amount.abs());
        Rc::new(move || (s(), biz.get(), exact.is_active().then_some(amt)))
    };

    // recurring is a flag, not a category: on by default for a subscription card, and every chip honours it
    let sub_row = (g.kind == TriageKind::Subscription).then(|| {
        adw::SwitchRow::builder()
            .title("Count as a subscription")
            .subtitle(glib::markup_escape_text(&format!("Listed under Subscriptions and planned for every month ({typical}{})", cadence_word(g.cadence))))
            .active(true)
            .build()
    });
    let recurring = {
        let s = sub_row.clone();
        Rc::new(move || s.as_ref().is_some_and(|s| s.is_active()))
    };

    // category chips (behind "they're all the same" for a varied payee; a transfer card asks first)
    let chips_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).visible(g.kind != TriageKind::Transfer && !varied).build();
    let flow = gtk::FlowBox::builder().selection_mode(gtk::SelectionMode::None).max_children_per_line(3).min_children_per_line(3).column_spacing(6).row_spacing(6).build();
    // six answers at most, all about this merchant: the directory's suggestion, what your own
    // categorising suggests, then what you pick lately and most for money going this way
    let outgoing = g.typical_amount.is_sign_negative();
    let mut lead: Vec<i64> = g.suggested_category.and_then(cat_id).into_iter().collect();
    lead.extend(g.guesses.iter().filter_map(|n| cat_id(n)).filter(|id| !lead.contains(id)).collect::<Vec<_>>());
    let pick = crate::picker::Pick { allow_none: false, suggested: lead.clone(), outgoing: Some(outgoing) };
    let mut ids = crate::picker::shortcuts(app, &pick, &cats);
    // a new ledger has no habits yet: the usual ones, never Income for money going out
    for n in QUICK.iter().filter(|n| (**n == "Income") != outgoing) {
        if ids.len() >= 6 {
            break;
        }
        if let Some(id) = cat_id(n).filter(|id| !ids.contains(id)) {
            ids.push(id);
        }
    }
    for (i, id) in ids.iter().enumerate() {
        let Some(c) = cats.iter().find(|c| c.id == *id) else { continue };
        // one highlighted answer: the best guess
        let b = gtk::Button::builder().label(&c.name).css_classes(if i == 0 && !lead.is_empty() { vec!["chip", "suggested-action"] } else { vec!["chip"] }).build();
        let parent = c.parent_id.and_then(|p| cats.iter().find(|x| x.id == p)).map(|p| format!(" · {}", p.name)).unwrap_or_default();
        b.set_tooltip_text(Some(&format!("{}{parent} (key {})", c.name, i + 1)));
        let (w, g2, id, payee, scope, next, ans, rec) = (Rc::downgrade(app), g.clone(), Some(*id), payee.clone(), scope.clone(), next.clone(), answered.clone(), recurring.clone());
        b.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                answer(&a, &g2, id, &payee.text(), rec(), scope(), &next, &ans);
            }
        });
        flow.insert(&b, -1);
    }
    chips_box.append(&flow);
    // everything else: the same searchable picker as everywhere else in FinGraph
    let more = gtk::Button::builder()
        .child(&adw::ButtonContent::builder().icon_name("system-search-symbolic").label("Find Another Category…").build())
        .halign(gtk::Align::Start)
        .css_classes(["flat"])
        .build();
    {
        let (w, g2, payee, scope, next, ans, rec) = (Rc::downgrade(app), g.clone(), payee.clone(), scope.clone(), next.clone(), answered.clone(), recurring.clone());
        more.connect_clicked(move |b| {
            let Some(a) = w.upgrade() else { return };
            let (w, g2, payee, scope, next, ans, rec) = (w.clone(), g2.clone(), payee.clone(), scope.clone(), next.clone(), ans.clone(), rec.clone());
            crate::picker::open(&a, b, pick.clone(), move |id| {
                if let (Some(a), Some(id)) = (w.upgrade(), id) {
                    answer(&a, &g2, Some(id), &payee.text(), rec(), scope(), &next, &ans);
                }
            });
        });
    }
    chips_box.append(&more);
    // the keys, said once where they apply
    chips_box.append(&gtk::Label::builder().label("Keys: 1–6 pick a category · Enter takes the highlighted one · J and K move between merchants").xalign(0.0).wrap(true).css_classes(["dim-label", "caption"]).build());

    let buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(8).build();
    match g.kind {
        TriageKind::Subscription => {
            if let Some(s) = &sub_row {
                let l = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
                l.append(s);
                buttons.append(&l);
            }
        }
        TriageKind::Merchant if varied => {
            let each = gtk::Button::builder().label("Categorize Each One in Transactions").css_classes(["suggested-action", "pill"]).build();
            let same = gtk::Button::builder().label("They're All the Same — Categorize Together").css_classes(["pill"]).build();
            let (w, ids) = (Rc::downgrade(app), g.tx_ids.clone());
            each.connect_clicked(move |_| {
                if let Some(a) = w.upgrade() {
                    a.show_transactions(ids.clone());
                }
            });
            let (cb, op, b) = (chips_box.clone(), opts.clone(), buttons.clone());
            same.connect_clicked(move |_| {
                cb.set_visible(true);
                op.set_visible(true);
                b.set_visible(false);
            });
            opts.set_visible(false);
            buttons.append(&each);
            buttons.append(&same);
        }
        TriageKind::Transfer => {
            let yes = gtk::Button::builder().label("Yes, Internal Transfer (Exclude from Spending)").css_classes(["suggested-action", "pill"]).build();
            let no = gtk::Button::builder().label("No, Regular Expense").css_classes(["pill"]).build();
            let (w, g2, scope2, next2, ans) = (Rc::downgrade(app), g.clone(), scope.clone(), next.clone(), answered.clone());
            yes.connect_clicked(move |_| {
                let Some(a) = w.upgrade() else { return };
                a.undoable(|a| match a.db.mark_transfers(&g2.tx_ids, &g2.stem, scope2().0) {
                    Ok(n) => {
                        ans.set(ans.get() + 1);
                        Some(format!("Marked as transfers · {n} new pair{} matched across your accounts", if n == 1 { "" } else { "s" }))
                    }
                    Err(e) => {
                        a.toast(&e.to_string());
                        None
                    }
                });
                next2();
            });
            let cb = chips_box.clone();
            no.connect_clicked(move |_| cb.set_visible(true));
            buttons.append(&yes);
            buttons.append(&no);
        }
        TriageKind::Merchant => {}
    }
    content.append(&buttons);
    content.append(&chips_box);
    content.append(&opts);
}

// ──────────────────────────────────────────────────────────────── workspaces

pub fn workspaces_dialog(app: &Rc<App>) {
    let page = adw::PreferencesPage::new();
    let g = adw::PreferencesGroup::builder().title("Workspaces").description("Each keeps its own accounts and P&L. Changing a currency revalues its transactions.").build();
    let ents = app.entities.borrow().clone();
    let mut edits = Vec::new();
    for e in &ents {
        let name = adw::EntryRow::builder().title(format!("{} workspace", e.kind.as_str())).text(&e.name).build();
        let ccy = crate::preferences::currency_row(&format!("{} currency", e.name), e.base_currency);
        g.add(&name);
        g.add(&ccy);
        edits.push((e.id, name, ccy));
    }
    page.add(&g);
    let add = adw::PreferencesGroup::builder().title("Add a Workspace").build();
    let new_name = adw::EntryRow::builder().title("Name (e.g. Business 2, Seven Mile Condo)").build();
    let new_kind = adw::ComboRow::builder().title("Kind").model(&gtk::StringList::new(&["Personal", "Business", "Rental Property"])).selected(1).build();
    add.add(&new_name);
    add.add(&new_kind);
    page.add(&add);
    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().title("Workspaces").content_width(560).content_height(560).child(&tv).build();
    let (w, d) = (Rc::downgrade(app), dlg.clone());
    save.connect_clicked(move |_| {
        let Some(a) = w.upgrade() else { return };
        for (id, name, ccy) in &edits {
            let c = crate::preferences::currency_at(ccy.selected());
            if let Err(e) = a.db.update_entity(*id, name.text().trim(), c) {
                a.toast(&e.to_string());
            }
        }
        let n = new_name.text().trim().to_string();
        if !n.is_empty() {
            let kind = [EntityKind::Personal, EntityKind::Business, EntityKind::RentalProperty][new_kind.selected() as usize];
            if let Err(e) = a.db.create_entity(&n, kind, Currency::KYD) {
                a.toast(&e.to_string());
            }
        }
        a.reload_entities();
        d.close();
    });
    dlg.present(Some(&app.win));
}
