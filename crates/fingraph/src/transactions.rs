//! Transactions: a virtualised grid with instant search, account/category filters, inline editing
//! (category, payee, tag), bulk actions, and a toast that offers to remember each manual edit as a rule.

use adw::prelude::*;
use fingraph_core::db::{Category, TxFilter, TxRow};
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind, merchant_stem};
use fingraph_core::ReviewStatus;
use gtk::{gio, glib, pango};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use crate::App;

thread_local! {
    static SEARCH: RefCell<Option<gtk::SearchEntry>> = const { RefCell::new(None) };
    /// For cell buttons built in factory callbacks that have no app at hand.
    static APP: RefCell<Option<std::rc::Weak<App>>> = const { RefCell::new(None) };
    /// An account picked in the sidebar, applied to the account filter on the next refresh.
    static PENDING_ACCOUNT: std::cell::Cell<Option<Option<i64>>> = const { std::cell::Cell::new(None) };
}

/// The sidebar's account rows: this account's transactions.
pub fn show_account(app: &Rc<App>, id: i64) {
    *app.focus_ids.borrow_mut() = None;
    PENDING_ACCOUNT.with(|p| p.set(Some(Some(id))));
    app.show_view("transactions");
}

pub fn focus_search(_app: &Rc<App>) {
    SEARCH.with(|s| {
        if let Some(e) = s.borrow().as_ref() {
            e.grab_focus();
        }
    });
}

fn row_of(item: &gtk::ListItem) -> Option<TxRow> {
    item.item().and_downcast::<glib::BoxedAnyObject>().map(|o| o.borrow::<TxRow>().clone())
}

/// `[Uncategorized, Income, ↳ Salary, …]` with ids alongside.
fn category_choices(cats: &[Category]) -> (Vec<String>, Vec<Option<i64>>) {
    let mut names = vec!["Uncategorized".to_string()];
    let mut ids = vec![None];
    for c in cats {
        names.push(if c.parent_id.is_some() { format!("   {}", c.name) } else { c.name.clone() });
        ids.push(Some(c.id));
    }
    (names, ids)
}

/// Offer to turn a manual edit into a learned rule for this merchant.
fn offer_rule(app: &Rc<App>, t: &TxRow, category_id: Option<i64>, payee: Option<String>, what: &str) {
    let stem = merchant_stem(&t.raw_description, &t.memo);
    let rule = Rule { id: 0, kind: RuleKind::NormalizedTokens, pattern: stem.clone(), clean_payee: payee, category_id, is_subscription: t.is_subscription, is_transfer: false, expected_cadence: None, entity_id: if app.db.prefs().rules_all_workspaces { None } else { Some(t.entity_id) }, business_entity_id: None, ..Default::default() };
    app.toast_action(
        &format!("Remember: {what} for every “{stem}”?"),
        "Remember",
        Rc::new(move |a: &Rc<App>| match a.db.learn(&rule) {
            Ok(n) => {
                a.toast(&format!("Rule saved and applied to {n} transaction{}", if n == 1 { "" } else { "s" }));
                a.refresh();
            }
            Err(e) => a.toast(&e.to_string()),
        }),
    );
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let store = gio::ListStore::new::<glib::BoxedAnyObject>();
    let selection = gtk::MultiSelection::new(Some(store.clone()));
    let table = gtk::ColumnView::builder().model(&selection).css_classes(["data-table"]).show_row_separators(true).reorderable(false).build();
    let search = gtk::SearchEntry::builder().placeholder_text("Search payee, description, memo, tag or amount (Ctrl+F)").hexpand(true).build();
    SEARCH.with(|s| *s.borrow_mut() = Some(search.clone()));
    APP.with(|x| *x.borrow_mut() = Some(Rc::downgrade(app)));
    let account_dd = gtk::DropDown::builder().tooltip_text("Account").build();
    let category_dd = gtk::DropDown::builder().tooltip_text("Category").build();
    let account_ids: Rc<RefCell<Vec<Option<i64>>>> = Rc::new(RefCell::new(vec![None]));
    let filter_cats: Rc<RefCell<Vec<Option<i64>>>> = Rc::new(RefCell::new(vec![None]));
    let summary = gtk::Label::builder().xalign(0.0).css_classes(["dim-label"]).build();
    let focus_bar = adw::Banner::builder().button_label("Show All").build();
    let tools = gtk::Box::builder().spacing(8).margin_top(10).margin_bottom(10).margin_start(12).margin_end(12).build();
    tools.append(&search);
    tools.append(&account_dd);
    tools.append(&category_dd);

    // bulk actions
    let bar = gtk::ActionBar::new();
    let sel_label = gtk::Label::new(None);
    bar.pack_start(&sel_label);
    let cat_menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    let cat_pop = gtk::Popover::builder().child(&gtk::ScrolledWindow::builder().child(&cat_menu_box).min_content_height(320).propagate_natural_width(true).build()).build();
    let bulk_cat = gtk::MenuButton::builder().label("Categorize").popover(&cat_pop).build();
    let bulk_tag = gtk::Button::with_label("Tag…");
    let bulk_sub = gtk::Button::builder().label("Subscription").tooltip_text("Mark as a subscription").build();
    let bulk_biz = gtk::Button::builder().label("Business").tooltip_text("Flag as a business expense paid from this account").build();
    let one_split = gtk::Button::builder().icon_name("edit-cut-symbolic").tooltip_text("Split one transaction across categories / workspaces").build();
    let one_attach = gtk::Button::builder().icon_name("mail-attachment-symbolic").tooltip_text("Attach a receipt or invoice (or drop a file on the row)").build();
    let one_rule = gtk::Button::builder().label("Rule…").tooltip_text("A rule from this transaction: by its words or reference number, optionally only for this amount").build();
    let bulk_unlink = gtk::Button::builder().label("Not Transfer").tooltip_text("Not a transfer between your own accounts").build();
    let bulk_del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete selected").css_classes(["destructive-action"]).build();
    bar.pack_end(&bulk_del);
    bar.pack_end(&bulk_unlink);
    bar.pack_end(&bulk_sub);
    bar.pack_end(&bulk_biz);
    bar.pack_end(&one_rule);
    bar.pack_end(&one_attach);
    bar.pack_end(&one_split);
    bar.pack_end(&bulk_tag);
    bar.pack_end(&bulk_cat);
    bar.pack_end(&summary);
    bar.set_revealed(false);

    let cats_model = gtk::StringList::new(&[]);
    let cat_ids: Rc<RefCell<Vec<Option<i64>>>> = Rc::new(RefCell::new(Vec::new()));
    let known: Rc<RefCell<HashMap<i64, Option<i64>>>> = Rc::new(RefCell::new(HashMap::new()));

    let selected_rows = {
        let (store, selection) = (store.clone(), selection.clone());
        move || -> Vec<TxRow> {
            let bits = selection.selection();
            (0..store.n_items()).filter(|i| bits.contains(*i)).filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>().map(|o| o.borrow::<TxRow>().clone())).collect()
        }
    };
    let selected_rows = Rc::new(selected_rows);

    type Setup = Box<dyn Fn(&gtk::ListItem)>;
    type Bind = Box<dyn Fn(&gtk::ListItem, &TxRow)>;
    let col = |title: &str, expand: bool, setup: Setup, bind: Bind| {
        let f = gtk::SignalListItemFactory::new();
        f.connect_setup(move |_, item| setup(item.downcast_ref::<gtk::ListItem>().unwrap()));
        f.connect_bind(move |_, item| {
            let item = item.downcast_ref::<gtk::ListItem>().unwrap();
            if let Some(t) = row_of(item) {
                bind(item, &t);
            }
        });
        let c = gtk::ColumnViewColumn::new(Some(title), Some(f));
        c.set_expand(expand);
        c.set_resizable(true);
        table.append_column(&c);
    };
    let label = || gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();

    col(
        "Date",
        false,
        Box::new(move |item| {
            let l = label();
            l.set_width_chars(11);
            item.set_child(Some(&l));
        }),
        Box::new(|item, t| {
            let l = item.child().and_downcast::<gtk::Label>().unwrap();
            l.set_label(&t.posted.format("%d %b %Y").to_string());
            l.set_tooltip_text(t.effective.map(|e| format!("Transaction date {e}")).as_deref());
        }),
    );
    {
        let w = Rc::downgrade(app);
        col(
            "Payee",
            true,
            Box::new(move |item| {
                let e = gtk::EditableLabel::new("");
                let w = w.clone();
                e.connect_editing_notify(move |e| {
                    let Some(a) = w.upgrade() else { return };
                    if e.is_editing() {
                        return;
                    }
                    let Ok(id) = e.widget_name().parse::<i64>() else { return };
                    let Ok(t) = a.db.transaction(id) else { return };
                    let text = e.text().trim().to_string();
                    if text.is_empty() || text == t.clean_payee {
                        return;
                    }
                    if a.db.set_payee(id, &text).is_ok() {
                        offer_rule(&a, &t, None, Some(text.clone()), &format!("call it “{text}”"));
                    }
                });
                item.set_child(Some(&e));
            }),
            Box::new(|item, t| {
                let e = item.child().and_downcast::<gtk::EditableLabel>().unwrap();
                e.set_widget_name(&t.id.to_string());
                e.set_text(&t.clean_payee);
            }),
        );
    }
    col(
        "Description",
        true,
        Box::new({
            let w = Rc::downgrade(app);
            move |item| {
                let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
                b.append(&label());
                let memo = label();
                memo.add_css_class("dim-label");
                memo.add_css_class("caption");
                b.append(&memo);
                // drop a receipt or invoice onto the row to attach it
                let drop = gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
                let (w, b2) = (w.clone(), b.clone());
                drop.connect_drop(move |_, v, _, _| {
                    let (Some(a), Ok(files), Ok(id)) = (w.upgrade(), v.get::<gtk::gdk::FileList>(), b2.widget_name().parse::<i64>()) else { return false };
                    crate::split::attach_paths(&a, id, files.files().iter().filter_map(|f| f.path()).collect());
                    true
                });
                b.add_controller(drop);
                item.set_child(Some(&b));
            }
        }),
        Box::new(|item, t| {
            let b = item.child().and_downcast::<gtk::Box>().unwrap();
            b.set_widget_name(&t.id.to_string());
            let l = b.first_child().and_downcast::<gtk::Label>().unwrap();
            let m = l.next_sibling().and_downcast::<gtk::Label>().unwrap();
            l.set_label(&t.raw_description);
            m.set_label(&t.memo);
            m.set_visible(!t.memo.is_empty());
            b.set_tooltip_text(Some(&format!("{}\n{}\nFITID {}", t.raw_description, t.memo, &t.fitid[..16])));
        }),
    );
    col(
        "Account",
        false,
        Box::new(move |item| item.set_child(Some(&label()))),
        Box::new(|item, t| {
            let l = item.child().and_downcast::<gtk::Label>().unwrap();
            l.set_label(&t.account_name);
        }),
    );
    {
        let (w, model, ids, kn) = (Rc::downgrade(app), cats_model.clone(), cat_ids.clone(), known.clone());
        col(
            "Category",
            false,
            Box::new(move |item| {
                let dd = gtk::DropDown::builder().model(&model).css_classes(["flat"]).build();
                let (w, ids, known) = (w.clone(), ids.clone(), kn.clone());
                dd.connect_selected_notify(move |dd| {
                    let Some(a) = w.upgrade() else { return };
                    let Ok(id) = dd.widget_name().parse::<i64>() else { return };
                    let new = ids.borrow().get(dd.selected() as usize).copied().flatten();
                    if known.borrow().get(&id) == Some(&new) {
                        return;
                    }
                    known.borrow_mut().insert(id, new);
                    let Ok(t) = a.db.transaction(id) else { return };
                    if a.db.set_category(&[id], new).is_ok() {
                        let name = a.categories().into_iter().find(|c| Some(c.id) == new).map_or("Uncategorized".to_string(), |c| c.name);
                        offer_rule(&a, &t, new, None, &format!("categorize as {name}"));
                        a.update_sidebar();
                    }
                });
                item.set_child(Some(&dd));
            }),
            {
                let (ids, known) = (cat_ids.clone(), known.clone());
                Box::new(move |item, t| {
                    let dd = item.child().and_downcast::<gtk::DropDown>().unwrap();
                    known.borrow_mut().insert(t.id, t.category_id);
                    dd.set_widget_name(&t.id.to_string());
                    let pos = ids.borrow().iter().position(|c| *c == t.category_id).unwrap_or(0);
                    dd.set_selected(pos as u32);
                })
            },
        );
    }
    {
        let w = Rc::downgrade(app);
        col(
            "Business",
            false,
            Box::new(move |item| {
                let cb = gtk::CheckButton::builder().halign(gtk::Align::Center).build();
                let w = w.clone();
                cb.connect_toggled(move |cb| {
                    let Some(a) = w.upgrade() else { return };
                    let Ok(id) = cb.widget_name().parse::<i64>() else { return };
                    let Ok(t) = a.db.transaction(id) else { return };
                    // binding sets the box too: only act on a real change
                    if cb.is_active() == t.business_entity_id.is_some() {
                        return;
                    }
                    if !cb.is_active() {
                        let _ = a.db.set_business(&[id], None);
                        a.toast(&format!("{} is personal again", t.clean_payee));
                        return a.refresh();
                    }
                    a.with_business(t.account_entity_id, move |a: &Rc<App>, biz| {
                        if a.db.set_business(&[t.id], Some(biz)).is_ok() {
                            let stem = merchant_stem(&t.raw_description, &t.memo);
                            let rule = Rule { kind: RuleKind::NormalizedTokens, pattern: stem.clone(), entity_id: Some(t.account_entity_id), business_entity_id: Some(biz), ..Default::default() };
                            a.toast_action(
                                &format!("Booked to {}. Always treat “{stem}” as a business expense?", a.entity_name(biz)),
                                "Always",
                                Rc::new(move |a: &Rc<App>| match a.db.learn(&rule) {
                                    Ok(n) => {
                                        a.toast(&format!("Rule saved · {n} transaction{} booked to the business", if n == 1 { "" } else { "s" }));
                                        a.refresh();
                                    }
                                    Err(e) => a.toast(&e.to_string()),
                                }),
                            );
                            a.refresh();
                        }
                    }, |a| a.refresh());
                });
                item.set_child(Some(&cb));
            }),
            {
                let w = Rc::downgrade(app);
                Box::new(move |item, t| {
                    let cb = item.child().and_downcast::<gtk::CheckButton>().unwrap();
                    cb.set_widget_name(&t.id.to_string());
                    cb.set_active(t.business_entity_id.is_some());
                    let tip = match (t.business_entity_id, w.upgrade()) {
                        (Some(b), Some(a)) => format!("Business expense of {} (paid from {})", a.entity_name(b), t.account_name),
                        _ => "Tick if this was a business expense".to_string(),
                    };
                    cb.set_tooltip_text(Some(&tip));
                })
            },
        );
    }
    {
        let w = Rc::downgrade(app);
        col(
            "Tag",
            false,
            Box::new(move |item| {
                let e = gtk::EditableLabel::builder().width_chars(8).build();
                let w = w.clone();
                e.connect_editing_notify(move |e| {
                    let Some(a) = w.upgrade() else { return };
                    if e.is_editing() {
                        return;
                    }
                    if let Ok(id) = e.widget_name().parse::<i64>() {
                        let _ = a.db.set_tag(&[id], &e.text());
                    }
                });
                item.set_child(Some(&e));
            }),
            Box::new(|item, t| {
                let e = item.child().and_downcast::<gtk::EditableLabel>().unwrap();
                e.set_widget_name(&t.id.to_string());
                e.set_text(&t.tag);
                e.set_tooltip_text(Some("Property, unit or project tag"));
            }),
        );
    }
    {
        let w = Rc::downgrade(app);
        col(
            "Amount",
            false,
            Box::new(move |item| item.set_child(Some(&gtk::Label::builder().xalign(1.0).build()))),
            Box::new(move |item, t| {
                let l = item.child().and_downcast::<gtk::Label>().unwrap();
                l.set_label(&fmt_money(t.amount, t.currency));
                l.set_css_classes(&[if t.amount.is_sign_negative() { "amount-out" } else { "amount-in" }]);
                let (disp, fx) = w.upgrade().map(|a| (a.display(), a.db.fx().unwrap_or_default())).unwrap_or((t.currency, Default::default()));
                let shown = if t.currency == disp { String::new() } else { format!("≈ {} in {}\n", fmt_money(fingraph_core::insights::value(t, disp, &fx), disp), disp) };
                l.set_tooltip_text(Some(&format!("{shown}Workspace amount {} {}{}", t.amount_entity, t.entity_currency, t.balance.map(|b| format!("\nBalance after: {b}")).unwrap_or_default())));
            }),
        );
    }
    col(
        "",
        false,
        Box::new(move |item| item.set_child(Some(&gtk::Box::builder().spacing(4).build()))),
        Box::new(|item, t| {
            let b = item.child().and_downcast::<gtk::Box>().unwrap();
            crate::clear(&b);
            let icon = |name: &str, tip: &str| gtk::Image::builder().icon_name(name).tooltip_text(tip).build();
            b.append(&match t.review {
                ReviewStatus::NeedsReview => icon("dialog-information-symbolic", "Needs review"),
                ReviewStatus::AutoCategorized => icon("emblem-system-symbolic", "Categorized by a learned rule"),
                ReviewStatus::Confirmed => icon("object-select-symbolic", "Confirmed"),
            });
            if t.transfer_peer.is_some() {
                b.append(&icon("media-playlist-repeat-symbolic", "Internal transfer: matched with your other account, excluded from spending"));
            }
            if t.is_subscription {
                b.append(&icon("starred-symbolic", "Subscription"));
            }
            if t.splits > 0 {
                b.append(&icon("edit-cut-symbolic", &format!("Split into {} lines (select it and press ✂ to change)", t.splits)));
            }
            if t.attachments > 0 {
                let clip = gtk::Button::builder().icon_name("mail-attachment-symbolic").tooltip_text(format!("{} receipt{} attached — click to open", t.attachments, if t.attachments == 1 { "" } else { "s" })).css_classes(["flat", "circular"]).build();
                let id = t.id;
                clip.connect_clicked(move |btn| {
                    if let Some(a) = APP.with(|x| x.borrow().as_ref().and_then(|w| w.upgrade())) {
                        crate::split::open_attachments(&a, id, btn.upcast_ref());
                    }
                });
                b.append(&clip);
            }
        }),
    );

    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    content.append(&focus_bar);
    content.append(&tools);
    content.append(&gtk::ScrolledWindow::builder().child(&table).vexpand(true).build());
    content.append(&bar);

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (store, account_dd, category_dd, search, account_ids, filter_cats, summary, focus_bar, cats_model, cat_ids, cat_menu_box, cat_pop) =
            (store.clone(), account_dd.clone(), category_dd.clone(), search.clone(), account_ids.clone(), filter_cats.clone(), summary.clone(), focus_bar.clone(), cats_model.clone(), cat_ids.clone(), cat_menu_box.clone(), cat_pop.clone());
        let selected_rows = selected_rows.clone();
        let busy = Rc::new(std::cell::Cell::new(false));
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            if busy.replace(true) {
                return;
            }
            // filter choices (keep the current pick when it still exists)
            let ents = a.entity_ids();
            let accts: Vec<_> = a.db.accounts().unwrap_or_default().into_iter().filter(|x| ents.contains(&x.entity_id) && !x.hidden).collect();
            let prev_acct = PENDING_ACCOUNT.with(|p| p.take()).unwrap_or_else(|| account_ids.borrow().get(account_dd.selected() as usize).copied().flatten());
            let mut names = vec!["All accounts".to_string()];
            names.extend(accts.iter().map(|x| x.name.clone()));
            *account_ids.borrow_mut() = std::iter::once(None).chain(accts.iter().map(|x| Some(x.id))).collect();
            account_dd.set_model(Some(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())));
            account_dd.set_selected(account_ids.borrow().iter().position(|x| *x == prev_acct).unwrap_or(0) as u32);

            let cats = a.categories();
            let (cn, ci) = category_choices(&cats);
            if cats_model.n_items() as usize != cn.len() {
                cats_model.splice(0, cats_model.n_items(), &cn.iter().map(String::as_str).collect::<Vec<_>>());
            }
            *cat_ids.borrow_mut() = ci.clone();
            let prev_cat = filter_cats.borrow().get(category_dd.selected() as usize).copied().flatten();
            let mut fnames = vec!["All categories".to_string(), "Uncategorized".to_string()];
            fnames.extend(cn.iter().skip(1).cloned());
            *filter_cats.borrow_mut() = [None, Some(-1)].into_iter().chain(ci.iter().skip(1).copied()).collect();
            category_dd.set_model(Some(&gtk::StringList::new(&fnames.iter().map(String::as_str).collect::<Vec<_>>())));
            category_dd.set_selected(filter_cats.borrow().iter().position(|x| *x == prev_cat).unwrap_or(0) as u32);

            // bulk "Categorize" menu
            crate::clear(&cat_menu_box);
            for (name, id) in cn.iter().zip(ci.iter()) {
                let b = gtk::Button::builder().label(name.trim()).css_classes(["flat"]).halign(gtk::Align::Fill).build();
                if let Some(l) = b.child().and_downcast::<gtk::Label>() {
                    l.set_xalign(0.0);
                }
                let (w, id, sr, pop) = (Rc::downgrade(&a), *id, selected_rows.clone(), cat_pop.clone());
                b.connect_clicked(move |_| {
                    pop.popdown();
                    let Some(a) = w.upgrade() else { return };
                    let rows = sr();
                    let ids: Vec<i64> = rows.iter().map(|t| t.id).collect();
                    if a.db.set_category(&ids, id).is_ok() {
                        a.toast(&format!("Categorized {} transactions", ids.len()));
                        a.refresh();
                    }
                });
                cat_menu_box.append(&b);
            }

            let cat_filter = filter_cats.borrow().get(category_dd.selected() as usize).copied().flatten();
            let mut rows = a
                .db
                .transactions(&TxFilter {
                    entity_ids: ents.clone(),
                    from: None,
                    to: None,
                    account_id: account_ids.borrow().get(account_dd.selected() as usize).copied().flatten(),
                    category_id: cat_filter.filter(|c| *c > 0),
                    uncategorized_only: cat_filter == Some(-1),
                    search: search.text().to_string(),
                    split_lines: false,
                })
                .unwrap_or_default();
            let focus = a.focus_ids.borrow().clone();
            match &focus {
                Some(ids) => {
                    rows.retain(|t| ids.contains(&t.id));
                    focus_bar.set_title(&format!("Showing the {} transactions behind a chart or insight", rows.len()));
                    focus_bar.set_revealed(true);
                }
                None => {
                    let (from, to) = a.range();
                    rows.retain(|t| t.posted >= from && t.posted <= to);
                    focus_bar.set_revealed(false);
                }
            }
            let fx = a.db.fx().unwrap_or_default();
            let total: fingraph_core::Decimal = rows.iter().map(|t| fingraph_core::insights::value(t, a.display(), &fx)).sum();
            summary.set_label(&format!("{} transactions · net {}", rows.len(), a.money(total)));
            let objs: Vec<glib::BoxedAnyObject> = rows.into_iter().map(glib::BoxedAnyObject::new).collect();
            store.splice(0, store.n_items(), &objs);
            busy.set(false);
        })
    };
    {
        let w = Rc::downgrade(app);
        focus_bar.connect_button_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                *a.focus_ids.borrow_mut() = None;
                a.refresh();
            }
        });
    }
    for dd in [&account_dd, &category_dd] {
        let r = refresh.clone();
        dd.connect_selected_notify(move |_| r());
    }
    {
        let r = refresh.clone();
        search.connect_search_changed(move |_| r());
    }
    {
        let (bar, sel_label, store) = (bar.clone(), sel_label.clone(), store.clone());
        selection.connect_selection_changed(move |s, _, _| {
            let n = s.selection().size();
            bar.set_revealed(true);
            sel_label.set_label(&if n > 0 { format!("{n} selected") } else { String::new() });
            let _ = store.n_items();
        });
    }
    bar.set_revealed(true);
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_biz.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let rows = sr();
            let Some(home) = rows.first().map(|t| t.account_entity_id) else { return };
            let ids: Vec<i64> = rows.iter().map(|t| t.id).collect();
            a.with_business(
                home,
                move |a, biz| {
                    if a.db.set_business(&ids, Some(biz)).is_ok() {
                        a.toast(&format!("{} transactions booked to {}", ids.len(), a.entity_name(biz)));
                        a.refresh();
                    }
                },
                |_| {},
            );
        });
    }
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_sub.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let ids: Vec<i64> = sr().iter().map(|t| t.id).collect();
            if !ids.is_empty() && a.db.set_subscription(&ids, true).is_ok() {
                a.toast(&format!("{} marked as subscription", ids.len()));
                a.refresh();
            }
        });
    }
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_unlink.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let rows = sr();
            for t in &rows {
                let _ = a.db.unlink_transfer(t.id);
            }
            let ids: Vec<i64> = rows.iter().filter(|t| t.category_name.as_deref() == Some("Transfers")).map(|t| t.id).collect();
            let _ = a.db.set_category(&ids, None);
            a.toast(&format!("{} unlinked — they count as spending/income again", rows.len()));
            a.refresh();
        });
    }
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_tag.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let ids: Vec<i64> = sr().iter().map(|t| t.id).collect();
            if ids.is_empty() {
                return;
            }
            let e = adw::EntryRow::builder().title("Property, unit or project").build();
            let g = adw::PreferencesGroup::new();
            g.add(&e);
            let d = adw::AlertDialog::new(Some(&format!("Tag {} Transactions", ids.len())), None);
            d.set_extra_child(Some(&g));
            d.add_responses(&[("cancel", "Cancel"), ("ok", "Tag")]);
            d.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
            let w = Rc::downgrade(&a);
            d.connect_response(None, move |_, r| {
                if let (Some(a), "ok") = (w.upgrade(), r) {
                    let _ = a.db.set_tag(&ids, &e.text());
                    a.refresh();
                }
            });
            d.present(Some(&a.win));
        });
    }
    // one-transaction actions
    let only_one = {
        let sr = selected_rows.clone();
        let w = Rc::downgrade(app);
        Rc::new(move || -> Option<(Rc<App>, TxRow)> {
            let a = w.upgrade()?;
            let rows = sr();
            if rows.len() != 1 {
                a.toast("Select one transaction");
                return None;
            }
            Some((a, rows[0].clone()))
        })
    };
    {
        let o = only_one.clone();
        one_split.connect_clicked(move |_| {
            if let Some((a, t)) = o() {
                crate::split::dialog(&a, t.id);
            }
        });
    }
    {
        let o = only_one.clone();
        one_attach.connect_clicked(move |_| {
            if let Some((a, t)) = o() {
                crate::split::choose_attachment(&a, t.id);
            }
        });
    }
    {
        let o = only_one.clone();
        one_rule.connect_clicked(move |_| {
            let Some((a, t)) = o() else { return };
            // start from the most specific text the row has: a reference number when there is one
            let words: Vec<&str> = t.memo.split(|c: char| c == '|' || c.is_whitespace()).collect();
            let reference = words.iter().find(|w| w.len() >= 6 && w.chars().all(|c| c.is_ascii_digit())).map(|s| s.to_string());
            let rule = Rule {
                kind: if reference.is_some() { RuleKind::Substring } else { RuleKind::NormalizedTokens },
                pattern: reference.unwrap_or_else(|| merchant_stem(&t.raw_description, &t.memo)),
                clean_payee: Some(t.clean_payee.clone()),
                category_id: t.category_id,
                amount_min: Some(t.amount.abs()),
                amount_max: Some(t.amount.abs()),
                entity_id: Some(t.entity_id),
                ..Default::default()
            };
            crate::rules::rule_dialog(&a, Some(rule));
        });
    }
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_del.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let ids: Vec<i64> = sr().iter().map(|t| t.id).collect();
            if ids.is_empty() {
                return;
            }
            let d = adw::AlertDialog::new(Some(&format!("Delete {} transactions?", ids.len())), Some("Their statements will show a discrepancy until they're re-imported."));
            d.add_responses(&[("cancel", "Cancel"), ("del", "Delete")]);
            d.set_response_appearance("del", adw::ResponseAppearance::Destructive);
            let w = Rc::downgrade(&a);
            d.connect_response(None, move |_, r| {
                if let (Some(a), "del") = (w.upgrade(), r) {
                    let _ = a.db.delete_transactions(&ids);
                    a.refresh();
                }
            });
            d.present(Some(&a.win));
        });
    }
    (content.upcast(), refresh)
}
