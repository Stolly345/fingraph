//! Export & Reports: CSV, QIF, OFX 1.0.2 and QuickBooks QBO for the selected range, plus a P&L.

use adw::prelude::*;
use fingraph_core::db::{Account, TxFilter, TxRow};
use fingraph_core::export;
use fingraph_core::insights;
use gtk::{gio, glib};
use std::cell::RefCell;
use std::rc::Rc;

use crate::App;

#[derive(Clone, Copy)]
enum Format {
    Csv,
    Qif,
    Ofx,
    Qbo,
    Pnl,
}

fn save(app: &Rc<App>, name: &str, content: String) {
    let fd = gtk::FileDialog::builder().title("Export").initial_name(name).modal(true).build();
    let w = Rc::downgrade(app);
    fd.save(Some(&app.win), gio::Cancellable::NONE, move |res| {
        let (Some(a), Some(path)) = (w.upgrade(), res.ok().and_then(|f| f.path())) else { return };
        match std::fs::write(&path, content) {
            Ok(()) => a.toast(&format!("Saved {}", path.display())),
            Err(e) => a.toast(&format!("Couldn't save: {e}")),
        }
    });
}

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let page = adw::PreferencesPage::new();
    let what = adw::PreferencesGroup::builder().title("What to Export").build();
    let account = adw::ComboRow::builder().title("Account").subtitle("OFX and QBO hold one account each").build();
    let range = adw::ActionRow::builder().title("Date range").subtitle("Set in the header bar").build();
    what.add(&account);
    what.add(&range);
    page.add(&what);

    let fmt = adw::PreferencesGroup::builder().title("Formats").build();
    let mk = |title: &str, sub: &str| {
        let r = adw::ActionRow::builder().title(title).subtitle(sub).activatable(true).build();
        r.add_suffix(&gtk::Image::from_icon_name("document-save-symbolic"));
        fmt.add(&r);
        r
    };
    let r_csv = mk("CSV", "Every column, any spreadsheet");
    let r_qif = mk("QIF", "Quicken Interchange Format, one block per account");
    let r_ofx = mk("OFX 1.0.2", "SGML statement for Quicken, GnuCash, Moneydance");
    let r_qbo = mk("QuickBooks QBO", "Web Connect file with INTU.BID");
    page.add(&fmt);

    let ident = adw::PreferencesGroup::builder().title("OFX Identity").description("QuickBooks only accepts a QBO whose bank id (INTU.BID) it knows; 3000 is a widely accepted generic id.").build();
    let bankid = adw::EntryRow::builder().title("OFX bank id (routing / transit)").build();
    let bid = adw::EntryRow::builder().title("INTU.BID").build();
    let save_ident = gtk::Button::builder().label("Save").valign(gtk::Align::Center).css_classes(["flat"]).build();
    ident.set_header_suffix(Some(&save_ident));
    ident.add(&bankid);
    ident.add(&bid);
    page.add(&ident);

    let reports = adw::PreferencesGroup::builder().title("Profit &amp; Loss").build();
    let r_pnl = adw::ActionRow::builder().title("P&amp;L by month and category (CSV)").subtitle("Transfers between your accounts excluded").activatable(true).build();
    r_pnl.add_suffix(&gtk::Image::from_icon_name("x-office-spreadsheet-symbolic"));
    reports.add(&r_pnl);
    page.add(&reports);
    let summary = adw::PreferencesGroup::builder().title("Summary for the Range").build();
    page.add(&summary);
    let summary_rows: Rc<RefCell<Vec<adw::ActionRow>>> = Default::default();

    let accounts: Rc<RefCell<Vec<Account>>> = Default::default();
    let selected = {
        let (accounts, account) = (accounts.clone(), account.clone());
        Rc::new(move || -> Option<Account> { (account.selected() as usize).checked_sub(1).and_then(|i| accounts.borrow().get(i).cloned()) })
    };
    let rows_for = {
        let selected = selected.clone();
        Rc::new(move |a: &Rc<App>| -> Vec<TxRow> {
            let (from, to) = a.range();
            a.db
                .transactions(&TxFilter { entity_ids: a.entity_ids(), from: Some(from), to: Some(to), account_id: selected().map(|x| x.id), ..Default::default() })
                .unwrap_or_default()
                .into_iter()
                .rev()
                .collect()
        })
    };

    let go = {
        let w = Rc::downgrade(app);
        let (selected, rows_for) = (selected.clone(), rows_for.clone());
        Rc::new(move |f: Format| {
            let Some(a) = w.upgrade() else { return };
            let rows = rows_for(&a);
            if rows.is_empty() {
                return a.toast("Nothing in this range to export");
            }
            let acct = selected();
            let stem = acct.as_ref().map_or("fingraph".to_string(), |x| x.name.replace(['•', ' '], "_"));
            let (from, to) = a.range();
            let stem = format!("{stem}_{from}_{to}");
            match (f, acct) {
                (Format::Csv, _) => save(&a, &format!("{stem}.csv"), export::csv(&rows)),
                (Format::Qif, _) => save(&a, &format!("{stem}.qif"), export::qif(&rows)),
                (Format::Pnl, _) => save(&a, &format!("{stem}_pnl.csv"), export::pnl_csv(&rows, a.display(), &a.db.fx().unwrap_or_default())),
                (Format::Ofx, Some(x)) => save(&a, &format!("{stem}.ofx"), export::ofx(&x, &rows)),
                (Format::Qbo, Some(x)) => save(&a, &format!("{stem}.qbo"), export::qbo(&x, &rows)),
                (_, None) => a.toast("Pick one account for OFX or QBO"),
            }
        })
    };
    for (r, f) in [(&r_csv, Format::Csv), (&r_qif, Format::Qif), (&r_ofx, Format::Ofx), (&r_qbo, Format::Qbo), (&r_pnl, Format::Pnl)] {
        let go = go.clone();
        r.connect_activated(move |_| go(f));
    }
    {
        let (w, selected, bankid, bid) = (Rc::downgrade(app), selected.clone(), bankid.clone(), bid.clone());
        save_ident.connect_clicked(move |_| {
            let (Some(a), Some(mut x)) = (w.upgrade(), selected()) else { return };
            x.ofx_bank_id = bankid.text().trim().to_string();
            x.ofx_intu_bid = bid.text().trim().to_string();
            match a.db.update_account(&x) {
                Ok(()) => a.toast("Saved"),
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }

    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        let (accounts, account, range, ident, bankid, bid, selected, rows_for, summary) = (accounts.clone(), account.clone(), range.clone(), ident.clone(), bankid.clone(), bid.clone(), selected.clone(), rows_for.clone(), summary.clone());
        let r_ofx = r_ofx.clone();
        let r_qbo = r_qbo.clone();
        let busy = Rc::new(std::cell::Cell::new(false));
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            // setting the account list re-fires its notify, which calls back in here
            if busy.replace(true) {
                return;
            }
            let ents = a.entity_ids();
            let prev = selected().map(|x| x.id);
            let list: Vec<Account> = a.db.accounts().unwrap_or_default().into_iter().filter(|x| ents.contains(&x.entity_id)).collect();
            let mut names = vec!["All accounts in view".to_string()];
            names.extend(list.iter().map(|x| x.name.clone()));
            let pos = prev.and_then(|p| list.iter().position(|x| x.id == p)).map_or(0, |i| i + 1);
            *accounts.borrow_mut() = list;
            account.set_model(Some(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())));
            account.set_selected(pos as u32);
            range.set_subtitle(&a.range_label());
            let one = selected();
            r_ofx.set_sensitive(one.is_some());
            r_qbo.set_sensitive(one.is_some());
            ident.set_visible(one.is_some());
            if let Some(x) = &one {
                bankid.set_text(&x.ofx_bank_id);
                bid.set_text(&x.ofx_intu_bid);
            }
            for r in summary_rows.borrow_mut().drain(..) {
                summary.remove(&r);
            }
            let rows = rows_for(&a);
            let fx = a.db.fx().unwrap_or_default();
            let (from, to) = a.range();
            let k = insights::kpis(&rows, &[], from, to, a.display(), &fx, crate::today());
            let mut lines = vec![("Income".to_string(), a.money(k.income)), ("Expenses".to_string(), a.money(k.expenses)), ("Net".to_string(), a.money(k.net))];
            for (c, v) in insights::by_category(&rows, from, to, a.display(), &fx, &a.parents()).into_iter().take(8) {
                lines.push((format!("  {c}"), a.money(v)));
            }
            for (t, v) in lines {
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(&t)).build();
                r.add_suffix(&gtk::Label::builder().label(v).css_classes(["tnum"]).build());
                summary.add(&r);
                summary_rows.borrow_mut().push(r);
            }
            busy.set(false);
        })
    };
    {
        let r = refresh.clone();
        account.connect_selected_notify(move |_| r());
    }
    (page.upcast(), refresh)
}
