//! Transactions: a virtualised grid with instant search, account/category filters, inline editing
//! (category, payee, tag), bulk actions, and a toast that offers to remember each manual edit as a rule.

use adw::prelude::*;
use fingraph_core::db::{Category, TxFilter, TxRow};
use fingraph_core::money::fmt_money;
use fingraph_core::rules::{Rule, RuleKind, merchant_stem};
use fingraph_core::ReviewStatus;
use gtk::{gio, glib, pango};
use std::cell::RefCell;
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

/// Preserve bank, account type and currency: last-four digits alone can name several accounts.
pub fn short_account(name: &str) -> String {
    let normalized = name.split_whitespace().collect::<Vec<_>>().join(" ");
    normalized.replace("Credit Card", "Card").replace("Checking", "Chq").replace("Savings", "Sav")
}

fn row_of(item: &gtk::ListItem) -> Option<TxRow> {
    item.item().and_downcast::<glib::BoxedAnyObject>().map(|o| o.borrow::<TxRow>().clone())
}

/// A flat button showing `text`, for a cell that opens a picker or editor when clicked. Cheap to
/// build: a DropDown or EditableLabel per row cost ~4 ms each, and a refresh builds hundreds.
fn cell_button() -> gtk::Button {
    let l = gtk::Label::builder().xalign(0.0).ellipsize(pango::EllipsizeMode::End).build();
    gtk::Button::builder().child(&l).css_classes(["flat", "cell"]).build()
}

fn set_cell_text(b: &gtk::Button, text: &str) {
    if let Some(l) = b.child().and_downcast::<gtk::Label>() {
        l.set_label(text);
    }
}

/// A popover under `anchor`, built on demand and gone once closed.
fn pop_under(anchor: &gtk::Button, child: &impl IsA<gtk::Widget>) -> gtk::Popover {
    let pop = gtk::Popover::builder().child(child).build();
    pop.set_parent(anchor);
    pop.connect_closed(|p| {
        let p = p.clone();
        // after the close animation has used it
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
    pop
}

/// Edit `text` in a small entry; `on_done` gets the new text on Enter.
fn edit_text(anchor: &gtk::Button, text: &str, placeholder: &str, on_done: impl Fn(String) + 'static) {
    let e = gtk::Entry::builder().text(text).placeholder_text(placeholder).width_chars(24).build();
    let pop = pop_under(anchor, &e);
    e.grab_focus();
    e.connect_activate(move |e| {
        pop.popdown();
        on_done(e.text().trim().to_string());
    });
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
    let category_dd = gtk::DropDown::builder().tooltip_text("Category").enable_search(true).expression(gtk::PropertyExpression::new(gtk::StringObject::static_type(), None::<gtk::Expression>, "string")).build();
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
    let bulk_cat = gtk::Button::builder().label("Categorize…").tooltip_text("Pick one category for every selected transaction").build();
    let bulk_tag = gtk::Button::with_label("Tag…");
    let bulk_sub = gtk::Button::builder().label("Subscription").tooltip_text("Mark as a subscription").build();
    let bulk_biz = gtk::Button::builder().label("Business").tooltip_text("Flag as a business expense paid from this account").build();
    // the less frequent actions live under More, so the bar shows four choices, not nine
    let flat_item = |label: &str, tip: &str| {
        let b = gtk::Button::builder().label(label).tooltip_text(tip).css_classes(["flat"]).build();
        if let Some(l) = b.child().and_downcast::<gtk::Label>() {
            l.set_xalign(0.0);
        }
        b
    };
    let one_split = flat_item("Split…", "Split one transaction across categories / workspaces");
    let one_attach = flat_item("Attach Receipt…", "Attach a receipt or invoice (or drop a file on the row)");
    let one_rule = flat_item("Make a Rule…", "A rule from this transaction: by its words or reference number, optionally only for this amount");
    let bulk_unlink = flat_item("Not a Transfer", "Not a transfer between your own accounts");
    let more_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    for b in [&one_split, &one_attach, &one_rule, &bulk_unlink] {
        more_box.append(b);
    }
    let more_pop = gtk::Popover::builder().child(&more_box).build();
    for b in [&one_split, &one_attach, &one_rule, &bulk_unlink] {
        let p = more_pop.clone();
        b.connect_clicked(move |_| p.popdown());
    }
    let more = gtk::MenuButton::builder().icon_name("view-more-symbolic").tooltip_text("More actions").popover(&more_pop).build();
    let bulk_del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Delete selected").css_classes(["destructive-action"]).build();
    bar.pack_end(&bulk_del);
    bar.pack_end(&more);
    bar.pack_end(&bulk_sub);
    bar.pack_end(&bulk_biz);
    bar.pack_end(&bulk_tag);
    bar.pack_end(&bulk_cat);
    bar.pack_end(&summary);
    // nothing selected, nothing to act on
    let acts: Vec<gtk::Widget> = vec![bulk_cat.clone().upcast(), bulk_tag.clone().upcast(), bulk_biz.clone().upcast(), bulk_sub.clone().upcast(), more.clone().upcast(), bulk_del.clone().upcast()];
    for w in &acts {
        w.set_sensitive(false);
    }
    bar.set_revealed(false);


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
    // payee on top, the bank's own wording under it: one column instead of two half-cut ones
    {
        let w = Rc::downgrade(app);
        col(
            "Payee",
            true,
            Box::new(move |item| {
                let cell = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
                let b = cell_button();
                b.set_tooltip_text(Some("Click to rename"));
                cell.append(&b);
                let detail = label();
                detail.set_css_classes(&["dim-label", "caption"]);
                detail.set_margin_start(6);
                cell.append(&detail);
                // drop a receipt or invoice onto the row to attach it
                let drop = gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
                let (w2, c2) = (w.clone(), cell.clone());
                drop.connect_drop(move |_, v, _, _| {
                    let (Some(a), Ok(files), Ok(id)) = (w2.upgrade(), v.get::<gtk::gdk::FileList>(), c2.widget_name().parse::<i64>()) else { return false };
                    crate::split::attach_paths(&a, id, files.files().iter().filter_map(|f| f.path()).collect());
                    true
                });
                cell.add_controller(drop);
                let w = w.clone();
                b.connect_clicked(move |b| {
                    let (Some(a), Ok(id)) = (w.upgrade(), b.widget_name().parse::<i64>()) else { return };
                    let Ok(t) = a.db.transaction(id) else { return };
                    let w = Rc::downgrade(&a);
                    edit_text(b, &t.clean_payee.clone(), "Payee", move |text| {
                        let Some(a) = w.upgrade() else { return };
                        if text.is_empty() || text == t.clean_payee {
                            return;
                        }
                        if a.db.set_payee(id, &text).is_ok() {
                            offer_rule(&a, &t, None, Some(text.clone()), &format!("call it “{text}”"));
                            a.refresh();
                        }
                    });
                });
                item.set_child(Some(&cell));
            }),
            Box::new(|item, t| {
                let cell = item.child().and_downcast::<gtk::Box>().unwrap();
                cell.set_widget_name(&t.id.to_string());
                let b = cell.first_child().and_downcast::<gtk::Button>().unwrap();
                b.set_widget_name(&t.id.to_string());
                set_cell_text(&b, &t.clean_payee);
                let detail = b.next_sibling().and_downcast::<gtk::Label>().unwrap();
                detail.set_label(&if t.memo.is_empty() { t.raw_description.clone() } else { format!("{} · {}", t.raw_description, t.memo) });
                detail.set_tooltip_text(Some(&format!("{}\n{}\nFITID {}", t.raw_description, t.memo, t.fitid.get(..16).unwrap_or(&t.fitid))));
            }),
        );
    }
    col(
        "Account",
        false,
        Box::new(move |item| item.set_child(Some(&label()))),
        Box::new(|item, t| {
            let l = item.child().and_downcast::<gtk::Label>().unwrap();
            l.set_label(&short_account(&t.account_name));
            l.set_tooltip_text(Some(&t.account_name));
        }),
    );
    {
        let w = Rc::downgrade(app);
        let tile_app = Rc::downgrade(app);
        col(
            "Category",
            false,
            Box::new(move |item| {
                let b = cell_button();
                b.set_width_request(150);
                // the category's tile, then its name
                let text = b.child().and_downcast::<gtk::Label>().unwrap();
                b.set_child(None::<&gtk::Widget>);
                let row = gtk::Box::builder().spacing(8).build();
                row.append(&gtk::Box::new(gtk::Orientation::Horizontal, 0));
                row.append(&text);
                b.set_child(Some(&row));
                let w = w.clone();
                b.connect_clicked(move |b| {
                    let (Some(a), Ok(id)) = (w.upgrade(), b.widget_name().parse::<i64>()) else { return };
                    let Ok(t) = a.db.transaction(id) else { return };
                    let w = Rc::downgrade(&a);
                    let opts = crate::picker::Pick { allow_none: true, suggested: Vec::new(), outgoing: Some(t.amount.is_sign_negative()) };
                    crate::picker::open(&a, b, opts, move |new| {
                        let Some(a) = w.upgrade() else { return };
                        if new == t.category_id {
                            return;
                        }
                        if a.db.set_category(&[id], new).is_ok() {
                            let name = a.categories().into_iter().find(|c| Some(c.id) == new).map_or("Uncategorized".to_string(), |c| c.name);
                            offer_rule(&a, &t, new, None, &format!("categorize as {name}"));
                            a.refresh();
                        }
                    });
                });
                item.set_child(Some(&b));
            }),
            Box::new(move |item, t| {
                let b = item.child().and_downcast::<gtk::Button>().unwrap();
                b.set_widget_name(&t.id.to_string());
                let name = t.category_name.as_deref().unwrap_or("Uncategorized");
                let row = b.child().and_downcast::<gtk::Box>().unwrap();
                let (old, text) = (row.first_child().unwrap(), row.last_child().and_downcast::<gtk::Label>().unwrap());
                text.set_label(name);
                row.remove(&old);
                if let Some(a) = tile_app.upgrade() {
                    row.prepend(&a.category_tile(name));
                }
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
                let shown = if t.currency == disp { String::new() } else { fingraph_core::insights::try_value(t, disp, &fx).map(|v| format!("≈ {} in {}\n", fmt_money(v, disp), disp)).unwrap_or_else(|| format!("{} amount unavailable: exchange rate needed\n", disp)) };
                l.set_tooltip_text(Some(&format!("{shown}Workspace amount {} {}{}", t.amount_entity.map(|v| v.to_string()).unwrap_or_else(|| "unavailable (missing exchange rate)".into()), t.entity_currency, t.balance.map(|b| format!("\nBalance after: {b}")).unwrap_or_default())));
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
            let app = APP.with(|x| x.borrow().as_ref().and_then(|w| w.upgrade()));
            // business and tag as badges on the row, each a click away from changing
            if let (Some(biz), Some(a)) = (t.business_entity_id, &app) {
                let pill = gtk::Button::builder().label(a.entity_name(biz)).tooltip_text(format!("Business expense of {} (paid from {}) — click to make it personal", a.entity_name(biz), t.account_name)).valign(gtk::Align::Center).css_classes(["flat", "row-badge", "business"]).build();
                let (id, payee) = (t.id, t.clean_payee.clone());
                pill.connect_clicked(move |_| {
                    if let Some(a) = APP.with(|x| x.borrow().as_ref().and_then(|w| w.upgrade())) {
                        a.undoable(|a| a.saved(a.db.set_business(&[id], None)).then(|| format!("{payee} is personal again")));
                        a.refresh();
                    }
                });
                b.append(&pill);
            }
            if !t.tag.is_empty() {
                let pill = gtk::Button::builder().label(&t.tag).tooltip_text("Property, unit or project tag — click to change").valign(gtk::Align::Center).css_classes(["flat", "row-badge"]).build();
                let (id, tag) = (t.id, t.tag.clone());
                pill.connect_clicked(move |p| {
                    let tag = tag.clone();
                    edit_text(p, &tag.clone(), "Property, unit or project", move |text| {
                        if let Some(a) = APP.with(|x| x.borrow().as_ref().and_then(|w| w.upgrade()))
                            && text != tag
                            && a.saved(a.db.set_tag(&[id], &text))
                        {
                            a.refresh();
                        }
                    });
                });
                b.append(&pill);
            }
            // the tooltip is also what a screen reader says
            let icon = |name: &str, tip: &str| {
                let i = gtk::Image::builder().icon_name(name).tooltip_text(tip).build();
                i.update_property(&[gtk::accessible::Property::Label(tip)]);
                i
            };
            // only what needs you gets a mark: a row a rule sorted, or you confirmed, is simply normal
            if t.review == ReviewStatus::NeedsReview {
                let l = gtk::Label::builder().label("Review").tooltip_text("Not sorted yet: pick a category, or answer it in To Review").valign(gtk::Align::Center).css_classes(["row-badge", "needs"]).build();
                b.append(&l);
            }
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

    // set while the page refreshes: its own dropdown rebuilds must not queue another refresh (0.2.14 looped forever)
    let busy = Rc::new(std::cell::Cell::new(false));
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let busy = busy.clone();
        let (store, account_dd, category_dd, search, account_ids, filter_cats, summary, focus_bar) =
            (store.clone(), account_dd.clone(), category_dd.clone(), search.clone(), account_ids.clone(), filter_cats.clone(), summary.clone(), focus_bar.clone());
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            if busy.get() {
                return;
            }
            let _busy = crate::Quiet::on(&busy);
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
            let prev_cat = filter_cats.borrow().get(category_dd.selected() as usize).copied().flatten();
            let mut fnames = vec!["All categories".to_string(), "Uncategorized".to_string()];
            fnames.extend(cn.iter().skip(1).cloned());
            *filter_cats.borrow_mut() = [None, Some(-1)].into_iter().chain(ci.iter().skip(1).copied()).collect();
            category_dd.set_model(Some(&gtk::StringList::new(&fnames.iter().map(String::as_str).collect::<Vec<_>>())));
            category_dd.set_selected(filter_cats.borrow().iter().position(|x| *x == prev_cat).unwrap_or(0) as u32);

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
                    by_account: false,
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
            // replace only the rows that changed, so an edit doesn't rebuild every visible row
            let old: Vec<TxRow> = (0..store.n_items()).filter_map(|i| store.item(i).and_downcast::<glib::BoxedAnyObject>().map(|o| o.borrow::<TxRow>().clone())).collect();
            let head = old.iter().zip(&rows).take_while(|(a, b)| a == b).count();
            let tail = old[head..].iter().rev().zip(rows[head..].iter().rev()).take_while(|(a, b)| a == b).count();
            let objs: Vec<glib::BoxedAnyObject> = rows[head..rows.len() - tail].iter().cloned().map(glib::BoxedAnyObject::new).collect();
            store.splice(head as u32, (old.len() - head - tail) as u32, &objs);
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
    // through the app's refresh, which keeps a bug in this page from closing the app
    for dd in [&account_dd, &category_dd] {
        let (w, busy) = (Rc::downgrade(app), busy.clone());
        dd.connect_selected_notify(move |_| {
            if !busy.get() {
                w.upgrade().iter().for_each(App::refresh);
            }
        });
    }
    {
        let w = Rc::downgrade(app);
        search.connect_search_changed(move |_| w.upgrade().iter().for_each(App::refresh));
    }
    {
        let (bar, sel_label, store) = (bar.clone(), sel_label.clone(), store.clone());
        selection.connect_selection_changed(move |s, _, _| {
            let n = s.selection().size();
            for w in &acts {
                w.set_sensitive(n > 0);
            }
            bar.set_revealed(true);
            sel_label.set_label(&if n > 0 { format!("{n} selected") } else { String::new() });
            let _ = store.n_items();
        });
    }
    bar.set_revealed(true);
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_cat.connect_clicked(move |b| {
            let Some(a) = w.upgrade() else { return };
            let ids: Vec<i64> = sr().iter().map(|t| t.id).collect();
            if ids.is_empty() {
                return;
            }
            let w = Rc::downgrade(&a);
            crate::picker::open(&a, b, crate::picker::Pick { allow_none: true, ..Default::default() }, move |id| {
                if let Some(a) = w.upgrade() {
                    a.undoable(|a| a.saved(a.db.set_category(&ids, id)).then(|| format!("Categorized {} transactions", ids.len())));
                    a.refresh();
                }
            });
        });
    }
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
                    // one transaction: offer to learn the merchant, as ticking a row used to; several: offer Undo
                    if let [t] = rows.as_slice() {
                        if a.saved(a.db.set_business(&[t.id], Some(biz))) {
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
                        }
                    } else {
                        a.undoable(|a| a.saved(a.db.set_business(&ids, Some(biz))).then(|| format!("{} transactions booked to {}", ids.len(), a.entity_name(biz))));
                    }
                    a.refresh();
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
            if ids.is_empty() {
                return;
            }
            a.undoable(|a| a.saved(a.db.set_subscription(&ids, true)).then(|| format!("{} marked as subscription", ids.len())));
            a.refresh();
        });
    }
    {
        let (w, sr) = (Rc::downgrade(app), selected_rows.clone());
        bulk_unlink.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let rows = sr();
            a.undoable(|a| {
                let ok = rows.iter().all(|t| a.saved(a.db.unlink_transfer(t.id)));
                let ids: Vec<i64> = rows.iter().filter(|t| t.category_name.as_deref() == Some("Transfers")).map(|t| t.id).collect();
                (ok && a.saved(a.db.set_category(&ids, None))).then(|| format!("{} unlinked — they count as spending/income again", rows.len()))
            });
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
            d.connect_response(None, move |_, r| crate::shielded(|| {
                if let (Some(a), "ok") = (w.upgrade(), r) {
                    a.undoable(|a| a.saved(a.db.set_tag(&ids, &e.text())).then(|| format!("Tagged {} transactions", ids.len())));
                    a.refresh();
                }
            }));
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
            d.connect_response(None, move |_, r| crate::shielded(|| {
                if let (Some(a), "del") = (w.upgrade(), r) {
                    a.saved(a.db.delete_transactions(&ids));
                    a.refresh();
                }
            }));
            d.present(Some(&a.win));
        });
    }
    (content.upcast(), refresh)
}
