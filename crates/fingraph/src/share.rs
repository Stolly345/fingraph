//! Share Merchant Hints: the merchant names and categories you'd like FinGraph's built-in directory to
//! learn, shown line by line to read and untick, then saved as a file you send yourself. FinGraph never
//! sends it: nothing here touches the network until you click Open GitHub, which only opens a page.

use adw::prelude::*;
use fingraph_core::hints::{Hint, merchant_hints, to_csv};
use gtk::{gio, glib};
use std::rc::Rc;

use crate::App;

const NEW_ISSUE: &str = "https://github.com/Stolly345/fingraph/issues/new?title=Merchant%20hints&body=Attach%20the%20merchant%20hints%20file%20FinGraph%20saved%20(drag%20it%20here).";

pub fn open(app: &Rc<App>) {
    let hints = merchant_hints(&app.all_rows(), &app.categories());
    let page = adw::PreferencesPage::new();
    let about = adw::PreferencesGroup::builder()
        .title("Help FinGraph Recognise More Shops")
        .description(
            "Each line is a shop's name as your statements print it, and the category you gave it. Nothing else: no amounts, dates, \
             accounts or balances, and no income, transfers or payments to people. Shops FinGraph already gets right are left out.\n\n\
             Untick anything you'd rather keep, then save the file. FinGraph never sends it; attach it to a new issue on GitHub \
             and the next release will know these shops.",
        )
        .build();
    page.add(&about);

    let checks: Vec<(gtk::CheckButton, Hint)> = hints
        .iter()
        .map(|h| (gtk::CheckButton::builder().active(true).valign(gtk::Align::Center).build(), h.clone()))
        .collect();
    let save = gtk::Button::builder().label("Save File…").css_classes(["suggested-action"]).sensitive(!checks.is_empty()).build();
    if checks.is_empty() {
        let none = adw::StatusPage::builder()
            .icon_name("object-select-symbolic")
            .title("Nothing to Share Yet")
            .description("When you categorize a shop FinGraph didn't know, or file it differently than it guessed, it shows up here.")
            .build();
        let g = adw::PreferencesGroup::new();
        g.add(&none);
        page.add(&g);
    } else {
        let list = adw::PreferencesGroup::builder().title(format!("{} Shop{}", checks.len(), if checks.len() == 1 { "" } else { "s" })).build();
        for (c, h) in &checks {
            let cat = match &h.parent {
                Some(p) => format!("{} · {p}", h.category),
                None => h.category.clone(),
            };
            let r = adw::ActionRow::builder()
                .title(glib::markup_escape_text(&h.merchant))
                .subtitle(glib::markup_escape_text(&format!("{cat} · {} time{}", h.times, if h.times == 1 { "" } else { "s" })))
                .activatable_widget(c)
                .build();
            r.add_prefix(c);
            list.add(&r);
        }
        page.add(&list);
        // the button says how many lines will go
        let update = {
            let (save, checks) = (save.clone(), checks.iter().map(|(c, _)| c.clone()).collect::<Vec<_>>());
            Rc::new(move || {
                let n = checks.iter().filter(|c| c.is_active()).count();
                save.set_sensitive(n > 0);
                save.set_label(&if n == checks.len() { "Save File…".to_string() } else { format!("Save {n}…") });
            })
        };
        for (c, _) in &checks {
            let u = update.clone();
            c.connect_toggled(move |_| u());
        }
    }

    let header = adw::HeaderBar::new();
    header.pack_end(&save);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().title("Share Merchant Hints").content_width(560).content_height(640).child(&tv).build();
    let (w, d) = (Rc::downgrade(app), dlg.clone());
    save.connect_clicked(move |_| {
        let Some(a) = w.upgrade() else { return };
        let chosen: Vec<Hint> = checks.iter().filter(|(c, _)| c.is_active()).map(|(_, h)| h.clone()).collect();
        let content = to_csv(&chosen, env!("CARGO_PKG_VERSION"));
        let name = format!("fingraph-merchant-hints-{}.csv", crate::today().format("%Y-%m-%d"));
        let fd = gtk::FileDialog::builder().title("Save Merchant Hints").initial_name(name).modal(true).build();
        let (w, d) = (Rc::downgrade(&a), d.clone());
        fd.save(Some(&a.win), gio::Cancellable::NONE, move |res| {
            let (Some(a), Some(path)) = (w.upgrade(), res.ok().and_then(|f| f.path())) else { return };
            match std::fs::write(&path, &content) {
                Ok(()) => {
                    d.close();
                    // opening the page is the user's call; the file goes only where they attach it
                    a.toast_action(
                        &format!("Saved {} shop{} to {}", chosen.len(), if chosen.len() == 1 { "" } else { "s" }, path.display()),
                        "Open GitHub",
                        Rc::new(|a: &Rc<App>| gtk::UriLauncher::new(NEW_ISSUE).launch(Some(&a.win), gio::Cancellable::NONE, |_| {})),
                    );
                }
                Err(e) => a.toast(&format!("Couldn't save: {e}")),
            }
        });
    });
    dlg.present(Some(&app.win));
}
