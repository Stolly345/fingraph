//! Splitting one charge: CI$ 450 at A.L. Thompson's becomes CI$ 300 of repairs for the rental and
//! CI$ 150 of household supplies. The statement row is untouched; reports read the lines.

use adw::prelude::*;
use fingraph_core::Decimal;
use fingraph_core::money::{fmt_money, parse_user};
use fingraph_core::workflows::SplitLine;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::App;

struct LineUi {
    row: gtk::Box,
    amount: gtk::Entry,
    category: gtk::DropDown,
    workspace: gtk::DropDown,
    tag: gtk::Entry,
}

pub fn dialog(app: &Rc<App>, tx_id: i64) {
    let Ok(t) = app.db.transaction(tx_id) else { return };
    if t.transfer_peer.is_some() {
        return app.toast("A transfer between your own accounts can't be split");
    }
    let cats = app.categories();
    let ents = app.entities.borrow().clone();
    let mut cat_names = vec!["Uncategorized".to_string()];
    cat_names.extend(cats.iter().map(|c| if c.parent_id.is_some() { format!("   {}", c.name) } else { c.name.clone() }));
    let ws_names: Vec<String> = ents.iter().map(|e| if e.id == t.account_entity_id { format!("{} (paid from)", e.name) } else { e.name.clone() }).collect();
    let total = t.amount.abs();
    let sign = if t.amount.is_sign_negative() { -Decimal::ONE } else { Decimal::ONE };

    let lines_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    let remaining = gtk::Label::builder().xalign(0.0).css_classes(["heading", "tnum"]).build();
    let lines: Rc<RefCell<Vec<LineUi>>> = Rc::default();
    let save = gtk::Button::builder().label("Save Split").css_classes(["suggested-action"]).build();

    let update: Rc<dyn Fn()> = {
        let (lines, remaining, save) = (lines.clone(), remaining.clone(), save.clone());
        let ccy = t.currency;
        Rc::new(move || {
            let sum: Decimal = lines.borrow().iter().filter_map(|l| parse_user(&l.amount.text()).map(|d| d.abs())).sum();
            let left = total - sum;
            remaining.set_label(&if left.is_zero() { format!("Adds up to {}", fmt_money(total, ccy)) } else { format!("{} left to assign of {}", fmt_money(left, ccy), fmt_money(total, ccy)) });
            remaining.set_css_classes(&["heading", "tnum", if left.is_zero() { "amount-in" } else { "warning" }]);
            save.set_sensitive(left.is_zero() && lines.borrow().len() >= 2);
        })
    };

    type AddLine = Rc<dyn Fn(Option<&SplitLine>)>;
    let add_line: AddLine = {
        let (lines, lines_box, update, cats, ents, cat_names, ws_names) = (lines.clone(), lines_box.clone(), update.clone(), cats.clone(), ents.clone(), cat_names.clone(), ws_names.clone());
        let (home, ccy) = (t.account_entity_id, t.currency);
        // one size group per column so the rows line up
        let cols: Vec<gtk::SizeGroup> = (0..4).map(|_| gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal)).collect();
        Rc::new(move |l: Option<&SplitLine>| {
            let row = gtk::Box::builder().spacing(6).build();
            let amount = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).input_purpose(gtk::InputPurpose::Number).build();
            let category = gtk::DropDown::builder().model(&gtk::StringList::new(&cat_names.iter().map(String::as_str).collect::<Vec<_>>())).hexpand(true).build();
            let workspace = gtk::DropDown::builder().model(&gtk::StringList::new(&ws_names.iter().map(String::as_str).collect::<Vec<_>>())).build();
            let tag = gtk::Entry::builder().placeholder_text("Project / tag").width_chars(12).build();
            let remove = gtk::Button::builder().icon_name("list-remove-symbolic").tooltip_text("Remove this line").css_classes(["flat"]).build();
            match l {
                Some(l) => {
                    amount.set_text(&l.amount.abs().to_string());
                    category.set_selected(l.category_id.and_then(|id| cats.iter().position(|c| c.id == id)).map_or(0, |i| i + 1) as u32);
                    tag.set_text(&l.tag);
                    workspace.set_selected(ents.iter().position(|e| Some(e.id) == l.business_entity_id.or(Some(home))).unwrap_or(0) as u32);
                }
                None => {
                    // a new line takes whatever is left
                    let sum: Decimal = lines.borrow().iter().filter_map(|x| parse_user(&x.amount.text()).map(|d| d.abs())).sum();
                    if total > sum {
                        amount.set_text(&(total - sum).to_string());
                    }
                    workspace.set_selected(ents.iter().position(|e| e.id == home).unwrap_or(0) as u32);
                }
            }
            for (i, w) in [amount.upcast_ref::<gtk::Widget>(), category.upcast_ref(), workspace.upcast_ref(), tag.upcast_ref(), remove.upcast_ref()].into_iter().enumerate() {
                if let Some(g) = cols.get(i) {
                    g.add_widget(w);
                }
                row.append(w);
            }
            {
                let u = update.clone();
                amount.connect_changed(move |_| u());
            }
            {
                let (lines, lines_box, u, row2) = (lines.clone(), lines_box.clone(), update.clone(), row.clone());
                remove.connect_clicked(move |_| {
                    lines.borrow_mut().retain(|x| x.row != row2);
                    lines_box.remove(&row2);
                    u();
                });
            }
            lines_box.append(&row);
            lines.borrow_mut().push(LineUi { row, amount, category, workspace, tag });
            update();
        })
    };

    let existing = app.db.splits(tx_id).unwrap_or_default();
    if existing.is_empty() {
        add_line(Some(&SplitLine { amount: total, category_id: t.category_id, ..Default::default() }));
        if let Some(first) = lines.borrow().first() {
            first.amount.set_text("");
        }
        add_line(None);
    } else {
        for l in &existing {
            add_line(Some(l));
        }
    }

    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    body.append(&gtk::Label::builder().label(format!("{} · {} · {}", t.clean_payee, t.posted.format("%d %b %Y"), fmt_money(t.amount, t.currency))).xalign(0.0).css_classes(["title-4"]).wrap(true).build());
    body.append(&crate::dim("Give each part its own category and workspace — e.g. the rental's share of a hardware-store receipt to the business, the rest personal. The bank's row stays as printed; reports, P&L and “business owes you” use the lines."));
    body.append(&lines_box);
    let add = gtk::Button::builder().label("Add a Line").halign(gtk::Align::Start).css_classes(["flat"]).build();
    body.append(&add);
    body.append(&remaining);
    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let unsplit = gtk::Button::builder().label("Remove Split").css_classes(["destructive-action"]).visible(!existing.is_empty()).build();
    header.pack_start(&unsplit);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&gtk::ScrolledWindow::builder().child(&body).propagate_natural_height(true).hscrollbar_policy(gtk::PolicyType::Never).build()));
    let dlg = adw::Dialog::builder().title("Split Transaction").content_width(760).child(&tv).build();
    {
        let a = add_line.clone();
        add.connect_clicked(move |_| a(None));
    }
    {
        let (w, dlg) = (Rc::downgrade(app), dlg.clone());
        unsplit.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.set_splits(tx_id, &[]);
                a.toast("Split removed");
                dlg.close();
                a.refresh();
            }
        });
    }
    {
        let (w, dlg, lines) = (Rc::downgrade(app), dlg.clone(), lines.clone());
        save.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let out: Vec<SplitLine> = lines
                .borrow()
                .iter()
                .map(|l| SplitLine {
                    amount: parse_user(&l.amount.text()).map(|d| d.abs() * sign).unwrap_or_default(),
                    category_id: (l.category.selected() as usize).checked_sub(1).and_then(|i| cats.get(i)).map(|c| c.id),
                    business_entity_id: ents.get(l.workspace.selected() as usize).map(|e| e.id),
                    tag: l.tag.text().to_string(),
                    memo: String::new(),
                })
                .collect();
            match a.db.set_splits(tx_id, &out) {
                Ok(()) => {
                    a.toast(&format!("Split into {} lines", out.len()));
                    dlg.close();
                    a.refresh();
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    dlg.present(Some(&app.win));
}

/// Open a transaction's receipts: one opens directly, several get a menu (with Remove).
pub fn open_attachments(app: &Rc<App>, tx_id: i64, anchor: &gtk::Widget) {
    let list = app.db.attachments(tx_id).unwrap_or_default();
    let open = |a: &Rc<App>, p: &std::path::Path| gtk::FileLauncher::new(Some(&gio::File::for_path(p))).launch(Some(&a.win), gio::Cancellable::NONE, |_| {});
    let col = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).margin_top(6).margin_bottom(6).margin_start(6).margin_end(6).build();
    let pop = gtk::Popover::builder().child(&col).build();
    pop.set_parent(anchor);
    for at in list {
        let row = gtk::Box::builder().spacing(6).build();
        let b = gtk::Button::builder().label(&at.filename).css_classes(["flat"]).hexpand(true).build();
        let del = gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text("Remove this attachment").css_classes(["flat"]).build();
        {
            let (w, p, pop) = (Rc::downgrade(app), at.path.clone(), pop.clone());
            b.connect_clicked(move |_| {
                pop.popdown();
                if let Some(a) = w.upgrade() {
                    open(&a, &p);
                }
            });
        }
        {
            let (w, id, pop) = (Rc::downgrade(app), at.id, pop.clone());
            del.connect_clicked(move |_| {
                pop.popdown();
                if let Some(a) = w.upgrade() {
                    let _ = a.db.detach(id);
                    a.refresh();
                }
            });
        }
        row.append(&b);
        row.append(&del);
        col.append(&row);
    }
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });
    pop.popup();
}

/// Choose receipt files for a transaction.
pub fn choose_attachment(app: &Rc<App>, tx_id: i64) {
    let fd = gtk::FileDialog::builder().title("Attach Receipts or Invoices").modal(true).build();
    let w = Rc::downgrade(app);
    fd.open_multiple(Some(&app.win), gio::Cancellable::NONE, move |res| {
        let (Some(a), Ok(files)) = (w.upgrade(), res) else { return };
        attach_paths(&a, tx_id, (0..files.n_items()).filter_map(|i| files.item(i).and_downcast::<gio::File>().and_then(|f| f.path())).collect());
    });
}

pub fn attach_paths(app: &Rc<App>, tx_id: i64, paths: Vec<std::path::PathBuf>) {
    let mut n = 0;
    for p in paths {
        match app.db.attach(tx_id, &p) {
            Ok(()) => n += 1,
            Err(e) => app.toast(&format!("{}: {e}", p.display())),
        }
    }
    if n > 0 {
        app.toast(&format!("Attached {n} file{}", if n == 1 { "" } else { "s" }));
        app.refresh();
    }
}
