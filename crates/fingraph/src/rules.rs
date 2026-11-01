//! Subscriptions & Rules: every detected subscription with its true yearly cost, and every rule
//! FinGraph has learned from you (edit, re-apply or forget).

use adw::prelude::*;
use fingraph_core::insights;
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind};
use gtk::glib;
use std::rc::Rc;

use crate::{App, today};

/// Which page: Subscriptions (checked often) or Rules & Categories (set up once).
#[derive(Clone, Copy, PartialEq)]
pub enum Part {
    Subscriptions,
    Rules,
}

pub fn build(app: &Rc<App>, part: Part) -> (gtk::Widget, crate::Refresh) {
    let page = adw::PreferencesPage::new();
    let subs = adw::PreferencesGroup::builder().title("Subscriptions").description("Recurring charges you've marked (or the wizard detected), costed in the display currency.").build();
    let rules = adw::PreferencesGroup::builder().title("Learned Rules").description("Applied to every new statement. Workspace rules beat global ones; newer beat older.").build();
    let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("New rule").css_classes(["flat"]).build();
    let suffix = gtk::Box::builder().spacing(6).build();
    suffix.append(&gtk::Button::builder().label("Reset & Re-triage…").css_classes(["flat"]).action_name("win.retriage").build());
    suffix.append(&gtk::Button::builder().label("Rescan…").css_classes(["flat"]).action_name("win.rescan").build());
    suffix.append(&add);
    rules.set_header_suffix(Some(&suffix));
    if part == Part::Subscriptions {
        page.add(&subs);
    } else {
        page.add(&rules);
    }
    let cats = adw::PreferencesGroup::builder().title("Categories").description("Shown A–Z everywhere. Add your own; sub-categories roll up into their parent on the dashboard.").build();
    let add_cat = adw::ButtonRow::builder().title("Add a Category…").start_icon_name("list-add-symbolic").build();
    cats.add(&add_cat);
    if part == Part::Rules {
        page.add(&cats);
    }
    {
        let w = Rc::downgrade(app);
        add_cat.connect_activated(move |_| {
            if let Some(a) = w.upgrade() {
                category_dialog(&a);
            }
        });
    }
    let sub_rows: Rc<std::cell::RefCell<Vec<gtk::Widget>>> = Default::default();
    let rule_rows: Rc<std::cell::RefCell<Vec<gtk::Widget>>> = Default::default();
    {
        let w = Rc::downgrade(app);
        add.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                rule_dialog(&a, None);
            }
        });
    }

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (subs, rules) = (subs.clone(), rules.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            for r in sub_rows.borrow_mut().drain(..) {
                subs.remove(&r);
            }
            for r in rule_rows.borrow_mut().drain(..) {
                rules.remove(&r);
            }
            if part == Part::Subscriptions {
                let rows = a.all_rows();
                let fx = a.db.fx().unwrap_or_default();
                let list = insights::subscriptions(&rows, a.display(), &fx, today());
                let total: fingraph_core::Decimal = list.iter().filter(|s| s.active).map(|s| s.annual).sum();
                subs.set_description(Some(&format!("{} active · {} a year", list.iter().filter(|s| s.active).count(), a.money(total))));
                if list.is_empty() {
                    let r = adw::ActionRow::builder().title("No subscriptions yet").subtitle("Smart Triage asks about merchants that charge on a schedule.").build();
                    subs.add(&r);
                    sub_rows.borrow_mut().push(r.upcast());
                }
                for s in list {
                    let every = match s.cadence_days {
                        7 => "weekly".to_string(),
                        14 => "fortnightly".into(),
                        30 => "monthly".into(),
                        91 => "quarterly".into(),
                        365 => "yearly".into(),
                        d => format!("every {d} days"),
                    };
                    let r = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&s.payee))
                        .subtitle(glib::markup_escape_text(&format!(
                            "{} {every}{} · {} · last {} · {} charge{}{}{}",
                            fmt_money(s.latest, s.currency),
                            if s.cadence_guessed { " (a guess: set the cycle)" } else { "" },
                            s.account,
                            s.last_charged.format("%d %b %Y"),
                            s.charges,
                            if s.charges == 1 { "" } else { "s" },
                            s.last_rise.map(|(o, n, _)| format!(" · ▲ price rose {o} → {n}")).unwrap_or_default(),
                            if s.active { "" } else { " · lapsed" }
                        )))
                        .subtitle_lines(3)
                        .build();
                    // the billing cycle: what you pick here beats any guess
                    const CYCLES: [(&str, i64); 5] = [("Weekly", 7), ("Fortnightly", 14), ("Monthly", 30), ("Quarterly", 91), ("Yearly", 365)];
                    // a menu built when opened: a DropDown per row cost ~4 ms each
                    let now = CYCLES.iter().min_by_key(|c| (c.1 - s.cadence_days).abs()).map_or("Monthly", |c| c.0);
                    let cycle = gtk::MenuButton::builder().label(now).valign(gtk::Align::Center).tooltip_text("How often it bills").build();
                    if s.cadence_guessed {
                        cycle.add_css_class("warning");
                    }
                    {
                        let (w, stem) = (Rc::downgrade(&a), s.stem.clone());
                        cycle.set_create_popup_func(move |mb| {
                            let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
                            let pop = gtk::Popover::builder().child(&col).build();
                            for (name, days) in CYCLES {
                                let b = gtk::Button::builder().label(name).css_classes(["flat"]).build();
                                let (w, stem, pop) = (w.clone(), stem.clone(), pop.clone());
                                b.connect_clicked(move |_| {
                                    pop.popdown();
                                    let Some(a) = w.upgrade() else { return };
                                    let mut p = a.db.prefs();
                                    p.subscription_cadence.insert(stem.clone(), days);
                                    if a.db.save_prefs(&p).is_ok() {
                                        a.refresh();
                                    }
                                });
                                col.append(&b);
                            }
                            mb.set_popover(Some(&pop));
                        });
                    }
                    r.add_suffix(&cycle);
                    r.add_suffix(&gtk::Label::builder().label(format!("{}/yr", a.money(s.annual))).css_classes(["tnum", "heading"]).build());
                    if let Some(url) = fingraph_core::merchants::manage_url(&format!("{} {}", s.payee, s.stem)) {
                        let manage = gtk::Button::builder().icon_name("adw-external-link-symbolic").tooltip_text(format!("Manage or cancel — opens {url}")).valign(gtk::Align::Center).css_classes(["flat", "circular"]).build();
                        let w = Rc::downgrade(&a);
                        manage.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                gtk::UriLauncher::new(url).launch(Some(&a.win), gtk::gio::Cancellable::NONE, |_| {});
                            }
                        });
                        r.add_suffix(&manage);
                    }
                    let not = gtk::Button::builder().icon_name("window-close-symbolic").tooltip_text("Not a subscription").valign(gtk::Align::Center).css_classes(["flat", "circular"]).build();
                    let (w, ids) = (Rc::downgrade(&a), s.tx_ids.clone());
                    not.connect_clicked(move |_| {
                        if let Some(a) = w.upgrade() {
                            a.saved(a.db.set_subscription(&ids, false));
                            a.refresh();
                        }
                    });
                    r.add_suffix(&not);
                    subs.add(&r);
                    sub_rows.borrow_mut().push(r.upcast());
                }

            }
            if part == Part::Rules {
                let cats = a.categories();
                let ents = a.entities.borrow().clone();
                let learned = a.db.rules().unwrap_or_default();
                if learned.is_empty() {
                    let r = adw::ActionRow::builder().title("Nothing learned yet").subtitle("Answer the Smart Triage cards, or edit a category in Transactions and choose “Remember”.").build();
                    rules.add(&r);
                    rule_rows.borrow_mut().push(r.upcast());
                }
                for rule in learned.into_iter().rev() {
                    let cat = rule.category_id.and_then(|id| cats.iter().find(|c| c.id == id)).map_or("—".to_string(), |c| c.name.clone());
                    let scope = rule.entity_id.and_then(|id| ents.iter().find(|e| e.id == id)).map_or("All workspaces".to_string(), |e| e.name.clone());
                    let mut bits = vec![format!("→ {cat}")];
                    if let Some(p) = &rule.clean_payee {
                        bits.push(format!("payee “{p}”"));
                    }
                    if rule.is_subscription {
                        bits.push("subscription".into());
                    }
                    if rule.is_transfer {
                        bits.push("internal transfer".into());
                    }
                    match (rule.amount_min, rule.amount_max) {
                        (Some(a), Some(b)) if a == b => bits.push(format!("only {a}")),
                        (Some(a), Some(b)) => bits.push(format!("{a}–{b} only")),
                        (Some(a), None) => bits.push(format!("{a} or more")),
                        (None, Some(b)) => bits.push(format!("up to {b}")),
                        (None, None) => {}
                    }
                    if let Some(b) = rule.business_entity_id {
                        bits.push(format!("business expense of {}", a.entity_name(b)));
                    }
                    bits.push(scope);
                    let r = adw::ActionRow::builder()
                        .title(glib::markup_escape_text(&format!("{}  “{}”", rule.kind.as_str(), rule.pattern)))
                        .subtitle(glib::markup_escape_text(&bits.join(" · ")))
                        .activatable(true)
                        .build();
                    let apply = gtk::Button::builder().label("Apply Now").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Re-apply to every matching transaction, including ones you confirmed").build();
                    let forget = gtk::Button::builder().icon_name("user-trash-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Forget this rule (categories already set stay)").build();
                    r.add_suffix(&apply);
                    r.add_suffix(&forget);
                    {
                        let (w, rule) = (Rc::downgrade(&a), rule.clone());
                        apply.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                let n = a.db.apply_rule(&rule, true).unwrap_or(0);
                                a.toast(&format!("Applied to {n} transactions"));
                                a.refresh();
                            }
                        });
                    }
                    {
                        let (w, id) = (Rc::downgrade(&a), rule.id);
                        forget.connect_clicked(move |_| {
                            if let Some(a) = w.upgrade() {
                                if a.saved(a.db.delete_rule(id)) {
                                    a.toast("Rule forgotten");
                                }
                                a.refresh();
                            }
                        });
                    }
                    {
                        let (w, rule) = (Rc::downgrade(&a), rule.clone());
                        r.connect_activated(move |_| {
                            if let Some(a) = w.upgrade() {
                                rule_dialog(&a, Some(rule.clone()));
                            }
                        });
                    }
                    rules.add(&r);
                    rule_rows.borrow_mut().push(r.upcast());
                }
            }
        })
    };
    (page.upcast(), refresh)
}

/// New rule, or edit one (saved as a new rule replacing the old).
/// Edit a rule, or (id 0) create one, optionally pre-filled from a transaction.
pub fn rule_dialog(app: &Rc<App>, existing: Option<Rule>) {
    let cats = app.categories();
    let ents = app.entities.borrow().clone();
    let kinds: Vec<&str> = RuleKind::ALL.iter().map(|k| k.as_str()).collect();
    let kind = adw::ComboRow::builder().title("Match").model(&gtk::StringList::new(&kinds)).build();
    let pattern = adw::EntryRow::builder().title("Pattern (merchant words, text or regex)").build();
    let payee = adw::EntryRow::builder().title("Clean payee (optional)").build();
    let mut cat_names = vec!["(leave category)".to_string()];
    cat_names.extend(cats.iter().map(|c| c.name.clone()));
    let category = adw::ComboRow::builder().title("Category").model(&gtk::StringList::new(&cat_names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let mut scope_names = vec!["All workspaces".to_string()];
    scope_names.extend(ents.iter().map(|e| e.name.clone()));
    let scope = adw::ComboRow::builder().title("Applies to").model(&gtk::StringList::new(&scope_names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let sub = adw::SwitchRow::builder().title("Subscription").build();
    let transfer = adw::SwitchRow::builder().title("Internal transfer").build();
    let biz_ents: Vec<_> = ents.iter().filter(|e| e.kind != fingraph_core::EntityKind::Personal).cloned().collect();
    let mut biz_names = vec!["No".to_string()];
    biz_names.extend(biz_ents.iter().map(|e| e.name.clone()));
    let business = adw::ComboRow::builder().title("Business expense of").model(&gtk::StringList::new(&biz_names.iter().map(String::as_str).collect::<Vec<_>>())).build();
    // "Adriana, CI$ 125.00 only": anything else from the same payee goes to To Review
    let amt_min = adw::EntryRow::builder().title("Only when the amount is at least (optional)").build();
    let amt_max = adw::EntryRow::builder().title("…and at most (same number = exactly)").build();
    if let Some(r) = &existing {
        amt_min.set_text(&r.amount_min.map(|d| d.to_string()).unwrap_or_default());
        amt_max.set_text(&r.amount_max.map(|d| d.to_string()).unwrap_or_default());
        kind.set_selected(RuleKind::ALL.iter().position(|k| *k == r.kind).unwrap_or(0) as u32);
        pattern.set_text(&r.pattern);
        payee.set_text(r.clean_payee.as_deref().unwrap_or(""));
        category.set_selected(r.category_id.and_then(|id| cats.iter().position(|c| c.id == id)).map_or(0, |i| i + 1) as u32);
        scope.set_selected(r.entity_id.and_then(|id| ents.iter().position(|e| e.id == id)).map_or(0, |i| i + 1) as u32);
        sub.set_active(r.is_subscription);
        transfer.set_active(r.is_transfer);
        business.set_selected(r.business_entity_id.and_then(|id| biz_ents.iter().position(|e| e.id == id)).map_or(0, |i| i + 1) as u32);
    }
    let g = adw::PreferencesGroup::new();
    for row in [kind.upcast_ref::<gtk::Widget>(), pattern.upcast_ref(), payee.upcast_ref(), category.upcast_ref(), scope.upcast_ref(), sub.upcast_ref(), transfer.upcast_ref(), business.upcast_ref(), amt_min.upcast_ref(), amt_max.upcast_ref()] {
        g.add(row);
    }
    let d = adw::AlertDialog::new(Some(if existing.as_ref().is_some_and(|r| r.id != 0) { "Edit Rule" } else { "New Rule" }), None);
    d.set_extra_child(Some(&g));
    d.add_responses(&[("cancel", "Cancel"), ("ok", "Save & Apply")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        if r != "ok" || pattern.text().trim().is_empty() {
            return;
        }
        let k = RuleKind::ALL[kind.selected() as usize];
        if k == RuleKind::Regex && !fingraph_core::rules::regex_ok(pattern.text().trim()) {
            return a.toast("That regular expression doesn't parse");
        }
        let rule = Rule {
            id: 0,
            kind: k,
            pattern: pattern.text().trim().to_string(),
            clean_payee: Some(payee.text().trim().to_string()).filter(|s| !s.is_empty()),
            category_id: (category.selected() as usize).checked_sub(1).and_then(|i| cats.get(i)).map(|c| c.id),
            is_subscription: sub.is_active(),
            is_transfer: transfer.is_active(),
            expected_cadence: if sub.is_active() { Some(30) } else { None },
            entity_id: (scope.selected() as usize).checked_sub(1).and_then(|i| ents.get(i)).map(|e| e.id),
            business_entity_id: (business.selected() as usize).checked_sub(1).and_then(|i| biz_ents.get(i)).map(|e| e.id),
            amount_min: fingraph_core::money::parse_user(&amt_min.text()).map(|d| d.abs()),
            amount_max: fingraph_core::money::parse_user(&amt_max.text()).map(|d| d.abs()),
        };
        if let Some(old) = existing.as_ref().filter(|o| o.id != 0) {
            a.saved(a.db.delete_rule(old.id));
        }
        match a.db.learn(&rule) {
            Ok(n) => a.toast(&format!("Rule saved · {n} transactions updated")),
            Err(e) => a.toast(&e.to_string()),
        }
        a.refresh();
    }));
    d.present(Some(&app.win));
}

fn category_dialog(app: &Rc<App>) {
    let tops: Vec<_> = app.categories().into_iter().filter(|c| c.parent_id.is_none()).collect();
    let name = adw::EntryRow::builder().title("Name").build();
    let mut parents = vec!["None — a top-level category".to_string()];
    parents.extend(tops.iter().map(|c| format!("Under {}", c.name)));
    let parent = adw::ComboRow::builder().title("Sits under").model(&gtk::StringList::new(&parents.iter().map(String::as_str).collect::<Vec<_>>())).build();
    let kind = adw::ComboRow::builder().title("Kind").subtitle("Transfers never count as spending").model(&gtk::StringList::new(&["Expense", "Income", "Transfer"])).build();
    let g = adw::PreferencesGroup::new();
    g.add(&name);
    g.add(&parent);
    g.add(&kind);
    let d = adw::AlertDialog::new(Some("New Category"), None);
    d.set_extra_child(Some(&g));
    d.add_responses(&[("cancel", "Cancel"), ("ok", "Add")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| crate::shielded(|| {
        let Some(a) = w.upgrade() else { return };
        let n = name.text().trim().to_string();
        if r != "ok" || n.is_empty() {
            return;
        }
        use fingraph_core::db::CategoryKind;
        // a sub-category takes its parent's kind
        let p = (parent.selected() as usize).checked_sub(1).and_then(|i| tops.get(i));
        let k = p.map(|p| p.kind).unwrap_or([CategoryKind::Expense, CategoryKind::Income, CategoryKind::Transfer][kind.selected() as usize]);
        match a.db.add_category(&n, p.map(|p| p.id), k) {
            Ok(_) => {
                a.toast(&format!("Added “{n}”"));
                a.refresh();
            }
            Err(_) => a.toast(&format!("“{n}” already exists there")),
        }
    }));
    d.present(Some(&app.win));
}
