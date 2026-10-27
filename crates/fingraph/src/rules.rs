//! Subscriptions & Rules: every detected subscription with its true yearly cost, and every rule
//! FinGraph has learned from you (edit, re-apply or forget).

use adw::prelude::*;
use fingraph_core::db::TxFilter;
use fingraph_core::insights;
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind};
use gtk::glib;
use std::rc::Rc;

use crate::{App, pill, today};

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let page = adw::PreferencesPage::new();
    let subs = adw::PreferencesGroup::builder().title("Subscriptions").description("Recurring charges you've marked (or the wizard detected), costed in the display currency.").build();
    let rules = adw::PreferencesGroup::builder().title("Learned Rules").description("Applied to every new statement. Workspace rules beat global ones; newer beat older.").build();
    let add = gtk::Button::builder().icon_name("list-add-symbolic").tooltip_text("New rule").css_classes(["flat"]).build();
    let suffix = gtk::Box::builder().spacing(6).build();
    suffix.append(&gtk::Button::builder().label("Reset & Re-triage…").css_classes(["flat"]).action_name("win.retriage").build());
    suffix.append(&gtk::Button::builder().label("Rescan…").css_classes(["flat"]).action_name("win.rescan").build());
    suffix.append(&add);
    rules.set_header_suffix(Some(&suffix));
    page.add(&subs);
    page.add(&rules);
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
            let rows = a.db.transactions(&TxFilter { entity_ids: a.entity_ids(), ..Default::default() }).unwrap_or_default();
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
                        "{} {every} on {} · last {} · {} charges",
                        fmt_money(s.latest, s.currency),
                        s.account,
                        s.last_charged.format("%d %b %Y"),
                        s.charges
                    )))
                    .build();
                if let Some((old, new, _)) = s.last_rise {
                    r.add_suffix(&pill(&format!("▲ {old} → {new}"), "bad"));
                }
                if !s.active {
                    r.add_suffix(&pill("Lapsed", "meh"));
                }
                r.add_suffix(&gtk::Label::builder().label(format!("{}/yr", a.money(s.annual))).css_classes(["tnum", "heading"]).build());
                let not = gtk::Button::builder().label("Not a Subscription").valign(gtk::Align::Center).css_classes(["flat"]).build();
                let (w, ids) = (Rc::downgrade(&a), s.tx_ids.clone());
                not.connect_clicked(move |_| {
                    if let Some(a) = w.upgrade() {
                        let _ = a.db.set_subscription(&ids, false);
                        a.refresh();
                    }
                });
                r.add_suffix(&not);
                subs.add(&r);
                sub_rows.borrow_mut().push(r.upcast());
            }

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
                            let _ = a.db.delete_rule(id);
                            a.toast("Rule forgotten");
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
        })
    };
    (page.upcast(), refresh)
}

/// New rule, or edit one (saved as a new rule replacing the old).
fn rule_dialog(app: &Rc<App>, existing: Option<Rule>) {
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
    if let Some(r) = &existing {
        kind.set_selected(RuleKind::ALL.iter().position(|k| *k == r.kind).unwrap_or(0) as u32);
        pattern.set_text(&r.pattern);
        payee.set_text(r.clean_payee.as_deref().unwrap_or(""));
        category.set_selected(r.category_id.and_then(|id| cats.iter().position(|c| c.id == id)).map_or(0, |i| i + 1) as u32);
        scope.set_selected(r.entity_id.and_then(|id| ents.iter().position(|e| e.id == id)).map_or(0, |i| i + 1) as u32);
        sub.set_active(r.is_subscription);
        transfer.set_active(r.is_transfer);
    }
    let g = adw::PreferencesGroup::new();
    for row in [kind.upcast_ref::<gtk::Widget>(), pattern.upcast_ref(), payee.upcast_ref(), category.upcast_ref(), scope.upcast_ref(), sub.upcast_ref(), transfer.upcast_ref()] {
        g.add(row);
    }
    let d = adw::AlertDialog::new(Some(if existing.is_some() { "Edit Rule" } else { "New Rule" }), None);
    d.set_extra_child(Some(&g));
    d.add_responses(&[("cancel", "Cancel"), ("ok", "Save & Apply")]);
    d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    let w = Rc::downgrade(app);
    d.connect_response(None, move |_, r| {
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
        };
        if let Some(old) = &existing {
            let _ = a.db.delete_rule(old.id);
        }
        match a.db.learn(&rule) {
            Ok(n) => a.toast(&format!("Rule saved · {n} transactions updated")),
            Err(e) => a.toast(&e.to_string()),
        }
        a.refresh();
    });
    d.present(Some(&app.win));
}
