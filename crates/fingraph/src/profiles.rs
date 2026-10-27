//! Bank Profiles: the five built-in Cayman institutions plus your own. Edit how each is recognised
//! and laid out, test any PDF against them, or start a new profile from a sample statement.

use adw::prelude::*;
use fingraph_core::parser::profiles::{BankProfile, HeaderAnchors, LayoutKind};
use fingraph_core::parser::{ColumnRole, MultiPassScanner, ScanResult};
use gtk::{gio, glib};
use std::path::PathBuf;
use std::rc::Rc;

use crate::{App, pill};

pub fn build(app: &Rc<App>) -> (gtk::Widget, crate::Refresh) {
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Bank Profiles")
        .description("Fingerprints pick the bank (page-1 matches count triple); header words and noise patterns tune its tables. Built-ins can be reset at any time.")
        .build();
    let buttons = gtk::Box::builder().spacing(6).build();
    let test = gtk::Button::builder().label("Test a PDF…").css_classes(["flat"]).build();
    let new = gtk::Button::builder().label("New from Sample…").css_classes(["flat"]).build();
    buttons.append(&test);
    buttons.append(&new);
    group.set_header_suffix(Some(&buttons));
    page.add(&group);
    let rows: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Default::default();
    {
        let w = Rc::downgrade(app);
        test.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                pick_pdf(&a, |a, path| run_scan(a, path, None, |a, r| test_result(a, &r, None)));
            }
        });
    }
    {
        let w = Rc::downgrade(app);
        new.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                pick_pdf(&a, |a, path| run_scan(a, path, None, |a, r| editor(a, profile_from_sample(&r))));
            }
        });
    }
    let refresh: crate::Refresh = {
        let w = Rc::downgrade(app);
        Rc::new(move || {
            let Some(a) = w.upgrade() else { return };
            for r in rows.borrow_mut().drain(..) {
                group.remove(&r);
            }
            for p in a.db.profiles().unwrap_or_default() {
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&p.display_name))
                    .subtitle(glib::markup_escape_text(&format!("{} · {} · dates {} · {}", p.slug, p.column_layout_kind.as_str(), p.default_date_format, p.routing_identifiers.join(", "))))
                    .activatable(true)
                    .build();
                r.add_prefix(&pill(&p.short_name, "bank"));
                if p.is_builtin {
                    r.add_suffix(&pill("Built-in", "meh"));
                }
                r.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (w, p) = (Rc::downgrade(&a), p.clone());
                r.connect_activated(move |_| {
                    if let Some(a) = w.upgrade() {
                        editor(&a, p.clone());
                    }
                });
                group.add(&r);
                rows.borrow_mut().push(r);
            }
        })
    };
    (page.upcast(), refresh)
}

fn pick_pdf(app: &Rc<App>, then: impl Fn(&Rc<App>, PathBuf) + 'static) {
    let filter = gtk::FileFilter::new();
    filter.add_suffix("pdf");
    filter.set_name(Some("PDF statements"));
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&filter);
    let fd = gtk::FileDialog::builder().title("Choose a Sample Statement").modal(true).filters(&filters).build();
    let w = Rc::downgrade(app);
    fd.open(Some(&app.win), gio::Cancellable::NONE, move |res| {
        if let (Some(a), Some(p)) = (w.upgrade(), res.ok().and_then(|f| f.path())) {
            then(&a, p);
        }
    });
}

/// Scan off the main thread; `only` restricts the scan to one profile.
fn run_scan(app: &Rc<App>, path: PathBuf, only: Option<BankProfile>, then: impl Fn(&Rc<App>, ScanResult) + 'static) {
    let profiles = match only {
        Some(p) => vec![p],
        None => app.db.profiles().unwrap_or_default(),
    };
    let w = Rc::downgrade(app);
    glib::spawn_future_local(async move {
        let r = gio::spawn_blocking(move || MultiPassScanner::new(&profiles).scan_pdf(&path).map_err(|e| e.to_string())).await.unwrap_or_else(|_| Err("scanner crashed".into()));
        let Some(a) = w.upgrade() else { return };
        match r {
            Ok(r) => then(&a, r),
            Err(e) => a.toast(&e),
        }
    });
}

fn test_result(app: &Rc<App>, r: &ScanResult, against: Option<&BankProfile>) {
    let mut lines = vec![match (&r.profile_slug, against) {
        (Some(s), Some(p)) if *s == p.slug => format!("✓ Recognised as {}", p.display_name),
        (None, Some(p)) => format!("✗ None of {}'s identifiers were found", p.display_name),
        (Some(s), _) => format!("Detected: {s}"),
        (None, None) => "No profile matched — the generic layout rules were used".into(),
    }];
    lines.push(format!("Scores: {}", r.diagnostics.pass1.scores.iter().map(|(s, n)| format!("{s} {n}")).collect::<Vec<_>>().join(", ")));
    lines.push(format!("Period: {}", r.period.map(|(a, b)| format!("{a} → {b}")).unwrap_or("not found".into())));
    for s in &r.sections {
        lines.push(format!(
            "{} {} ••{}: {} transactions · {}",
            s.currency,
            s.account_type.label(),
            s.account_last4.as_deref().unwrap_or("????"),
            s.transactions.len(),
            match &s.reconciliation {
                fingraph_core::Reconciliation::Reconciled => "Reconciled to the cent".to_string(),
                fingraph_core::Reconciliation::Discrepancy(d) => format!("Discrepancy {d}"),
                fingraph_core::Reconciliation::Unverified => "Unverified (no balances printed)".into(),
            }
        ));
    }
    if let Some(pg) = r.diagnostics.pages.iter().find(|p| !p.header_words.is_empty()) {
        lines.push(format!("Columns: {}", pg.header_words.iter().map(|(r, w)| format!("{r:?} “{w}”")).collect::<Vec<_>>().join(" · ")));
    }
    let d = adw::AlertDialog::new(Some("Profile Test"), Some(&lines.join("\n")));
    d.add_response("ok", "Close");
    d.present(Some(&app.win));
}

/// The column-mapping helper: a new profile pre-filled from what the scanner saw in the sample.
fn profile_from_sample(r: &ScanResult) -> BankProfile {
    let p1 = &r.diagnostics.pass1;
    let name = p1.letterhead.first().cloned().unwrap_or_else(|| "My Bank".into());
    let mut anchors = HeaderAnchors::default();
    if let Some(pg) = r.diagnostics.pages.iter().find(|p| !p.header_words.is_empty()) {
        for (role, words) in &pg.header_words {
            let w = words.to_lowercase();
            match role {
                ColumnRole::Date => anchors.date.push(w),
                ColumnRole::PostDate => anchors.posting_date.push(w),
                ColumnRole::Description => anchors.description.push(w),
                ColumnRole::Reference => anchors.reference.push(w),
                ColumnRole::Debit => anchors.debit.push(w),
                ColumnRole::Credit => anchors.credit.push(w),
                ColumnRole::Amount => anchors.amount.push(w),
                ColumnRole::Balance => anchors.balance.push(w),
            }
        }
    }
    let layout = if !anchors.debit.is_empty() && !anchors.balance.is_empty() {
        LayoutKind::DebitCreditWithBalance
    } else if !anchors.debit.is_empty() {
        LayoutKind::SeparateDebitCredit
    } else if p1.account_type == Some(fingraph_core::AccountType::CreditCard) {
        LayoutKind::CreditCardDRCR
    } else {
        LayoutKind::SignedSingleAmount
    };
    BankProfile {
        id: 0,
        slug: name.to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect::<String>().trim_matches('_').chars().take(24).collect(),
        short_name: name.split_whitespace().next().unwrap_or("Bank").to_string(),
        display_name: name,
        routing_identifiers: p1.letterhead.iter().take(2).cloned().collect(),
        default_date_format: "DD/MM/YYYY".into(),
        column_layout_kind: layout,
        header_anchors: anchors,
        noise_patterns: Vec::new(),
        fx_note_regex: r"(?i)(?P<ccy>USD|KYD|US\$|CI\$)\s*(?P<amt>\d[\d,]*\.\d{2})\s*(?:@|at)\s*(?P<rate>\d+\.\d+)".into(),
        is_builtin: false,
    }
}

fn list_row(title: &str, v: &[String]) -> adw::EntryRow {
    adw::EntryRow::builder().title(title).text(v.join(", ")).build()
}

fn split_list(s: &str) -> Vec<String> {
    s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect()
}

fn editor(app: &Rc<App>, p: BankProfile) {
    let page = adw::PreferencesPage::new();
    let g = adw::PreferencesGroup::builder().title("Institution").build();
    let display = adw::EntryRow::builder().title("Name").text(&p.display_name).build();
    let short = adw::EntryRow::builder().title("Badge").text(&p.short_name).build();
    let ids = list_row("Fingerprints (comma separated: names, SWIFT/BIC, web addresses)", &p.routing_identifiers);
    let date = adw::EntryRow::builder().title("Date format (DD/MM/YYYY, MM/DD/YYYY, DDMMM, DD MMM…)").text(&p.default_date_format).build();
    let kinds: Vec<&str> = LayoutKind::ALL.iter().map(|k| k.as_str()).collect();
    let layout = adw::ComboRow::builder().title("Column layout").model(&gtk::StringList::new(&kinds)).selected(LayoutKind::ALL.iter().position(|k| *k == p.column_layout_kind).unwrap_or(0) as u32).build();
    for r in [display.upcast_ref::<gtk::Widget>(), short.upcast_ref(), ids.upcast_ref(), date.upcast_ref(), layout.upcast_ref()] {
        g.add(r);
    }
    page.add(&g);
    let h = &p.header_anchors;
    let ga = adw::PreferencesGroup::builder().title("Column header words").description("Extra words that name each column on this bank's statements, on top of the ones every scan knows (date, description, debit, credit, balance…).").build();
    let a_date = list_row("Date", &h.date);
    let a_post = list_row("Posting date", &h.posting_date);
    let a_desc = list_row("Description", &h.description);
    let a_ref = list_row("Reference", &h.reference);
    let a_debit = list_row("Debit / withdrawal", &h.debit);
    let a_credit = list_row("Credit / deposit", &h.credit);
    let a_amount = list_row("Single signed amount", &h.amount);
    let a_bal = list_row("Balance", &h.balance);
    for r in [&a_date, &a_post, &a_desc, &a_ref, &a_debit, &a_credit, &a_amount, &a_bal] {
        ga.add(r);
    }
    page.add(&ga);
    let gn = adw::PreferencesGroup::builder().title("Noise &amp; FX").build();
    let noise = adw::EntryRow::builder().title("Noise patterns (regexes separated by “ ;; ”)").text(p.noise_patterns.join(" ;; ")).build();
    let fx = adw::EntryRow::builder().title("FX line regex (groups amt, ccy, optional rate)").text(&p.fx_note_regex).build();
    gn.add(&noise);
    gn.add(&fx);
    page.add(&gn);

    let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
    let test = gtk::Button::builder().label("Test PDF…").build();
    let reset = gtk::Button::builder().label(if p.is_builtin { "Reset to Built-in" } else { "Delete" }).css_classes(["destructive-action"]).visible(p.id != 0).build();
    let header = adw::HeaderBar::builder().title_widget(&adw::WindowTitle::new(&p.display_name, &p.slug)).build();
    header.pack_end(&save);
    header.pack_start(&test);
    header.pack_start(&reset);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&page));
    let dlg = adw::Dialog::builder().content_width(760).content_height(820).child(&tv).build();

    let collect = {
        let p = p.clone();
        move || -> BankProfile {
            BankProfile {
                display_name: display.text().trim().to_string(),
                short_name: short.text().trim().to_string(),
                routing_identifiers: split_list(&ids.text()),
                default_date_format: date.text().trim().to_string(),
                column_layout_kind: LayoutKind::ALL[layout.selected() as usize],
                header_anchors: HeaderAnchors {
                    date: split_list(&a_date.text()),
                    posting_date: split_list(&a_post.text()),
                    description: split_list(&a_desc.text()),
                    reference: split_list(&a_ref.text()),
                    debit: split_list(&a_debit.text()),
                    credit: split_list(&a_credit.text()),
                    amount: split_list(&a_amount.text()),
                    balance: split_list(&a_bal.text()),
                },
                noise_patterns: noise.text().split(";;").map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                fx_note_regex: fx.text().trim().to_string(),
                ..p.clone()
            }
        }
    };
    let collect = Rc::new(collect);
    {
        let (w, collect, dlg) = (Rc::downgrade(app), collect.clone(), dlg.clone());
        save.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let p = collect();
            if let Some(bad) = p.noise_patterns.iter().chain(std::iter::once(&p.fx_note_regex)).find(|r| !fingraph_core::rules::regex_ok(r)) {
                return a.toast(&format!("Not a valid regex: {bad}"));
            }
            if p.display_name.is_empty() || p.slug.is_empty() || p.routing_identifiers.is_empty() {
                return a.toast("A profile needs a name and at least one fingerprint");
            }
            match a.db.save_profile(&p) {
                Ok(_) => {
                    a.toast("Profile saved — it applies to the next import");
                    dlg.close();
                    a.refresh();
                }
                Err(e) => a.toast(&e.to_string()),
            }
        });
    }
    {
        let (w, collect) = (Rc::downgrade(app), collect.clone());
        test.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            let p = collect();
            pick_pdf(&a, move |a, path| {
                let p2 = p.clone();
                run_scan(a, path, Some(p.clone()), move |a, r| test_result(a, &r, Some(&p2)));
            });
        });
    }
    {
        let (w, id, dlg) = (Rc::downgrade(app), p.id, dlg.clone());
        reset.connect_clicked(move |_| {
            if let Some(a) = w.upgrade() {
                let _ = a.db.delete_or_reset_profile(id);
                dlg.close();
                a.refresh();
            }
        });
    }
    dlg.present(Some(&app.win));
}
