//! The category picker: one popover used everywhere a category is chosen (To Review, a transaction's
//! category cell, bulk Categorize, the transaction editor). Type to find; before typing it offers the
//! suggestions, what you picked lately and what you use most, then every category under its parent.
//! A category that doesn't exist yet is one "Add…" away. Clicking or Enter picks; arrows only move.

use adw::prelude::*;
use fingraph_core::db::Category;
use gtk::glib;
use std::rc::Rc;

use crate::App;

/// What to offer besides the full list.
#[derive(Default, Clone)]
pub struct Pick {
    /// Offer "Uncategorized" (clearing a category); a Review answer never needs it.
    pub allow_none: bool,
    /// Shown first, best first (the directory's suggestion, your own guesses).
    pub suggested: Vec<i64>,
    /// Money going out (`Some(true)`) or coming in: which of your most-used categories to offer.
    pub outgoing: Option<bool>,
}

#[derive(Clone, Copy, PartialEq)]
enum Choice {
    None,
    Cat(i64),
    Add,
}

/// `cats` matching `q`, best first: a word of the name starting with it ("fee" → Account Fees), then
/// the children of a parent that does (Government & Fees → Taxes), then the name containing it
/// anywhere (Coffee). Tree order within each tier.
fn matches<'a>(cats: &'a [Category], q: &str) -> Vec<&'a Category> {
    let q = q.trim().to_lowercase();
    let word_start = |name: &str| name.to_lowercase().split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&q));
    let parent = |c: &Category| c.parent_id.and_then(|p| cats.iter().find(|x| x.id == p)).map(|p| p.name.clone()).unwrap_or_default();
    let tier = |c: &Category| {
        if word_start(&c.name) {
            Some(0)
        } else if word_start(&parent(c)) {
            Some(1)
        } else if c.name.to_lowercase().contains(&q) {
            Some(2)
        } else {
            None
        }
    };
    let mut out: Vec<(u8, &Category)> = cats.iter().filter_map(|c| tier(c).map(|t| (t, c))).collect();
    out.sort_by_key(|(t, _)| *t);
    out.into_iter().map(|(_, c)| c).collect()
}

/// The ids offered before typing, in order and without repeats: suggestions, recent, most used (six at most).
pub fn shortcuts(app: &App, opts: &Pick, cats: &[Category]) -> Vec<i64> {
    let mut out: Vec<i64> = Vec::new();
    let most = opts.outgoing.map(|o| app.db.top_categories(&app.entity_ids(), o, 6).unwrap_or_default()).unwrap_or_default();
    for id in opts.suggested.iter().chain(&app.db.recent_categories()).chain(&most) {
        if !out.contains(id) && cats.iter().any(|c| c.id == *id) {
            out.push(*id);
        }
    }
    out.truncate(6);
    out
}

pub fn open(app: &Rc<App>, anchor: &impl IsA<gtk::Widget>, opts: Pick, on_pick: impl Fn(Option<i64>) + 'static) {
    let on_pick: Rc<dyn Fn(Option<i64>)> = Rc::new(on_pick);
    let cats = app.categories();
    let short = shortcuts(app, &opts, &cats);

    let search = gtk::SearchEntry::builder().placeholder_text("Find a category").margin_start(6).margin_end(6).margin_top(6).build();
    let list = gtk::ListBox::builder().css_classes(["navigation-sidebar"]).selection_mode(gtk::SelectionMode::Single).build();
    let scroll = gtk::ScrolledWindow::builder().child(&list).hscrollbar_policy(gtk::PolicyType::Never).min_content_height(120).max_content_height(420).propagate_natural_height(true).width_request(320).build();
    let body = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    body.append(&search);
    body.append(&scroll);
    let pop = gtk::Popover::builder().child(&body).build();
    pop.set_parent(anchor);
    pop.connect_closed(|p| {
        let p = p.clone();
        glib::idle_add_local_once(move || p.unparent());
    });

    let choices: Rc<std::cell::RefCell<Vec<Option<Choice>>>> = Rc::default();
    let rebuild = {
        let (list, choices, cats) = (list.clone(), choices.clone(), cats.clone());
        let name_of = {
            let cats = cats.clone();
            move |id: i64| cats.iter().find(|c| c.id == id).cloned()
        };
        let parent_name = {
            let cats = cats.clone();
            move |c: &Category| c.parent_id.and_then(|p| cats.iter().find(|x| x.id == p)).map(|p| p.name.clone())
        };
        move |q: &str| {
            list.remove_all();
            let mut ch: Vec<Option<Choice>> = Vec::new();
            let header = |text: &str| {
                let r = gtk::ListBoxRow::builder().activatable(false).selectable(false).child(&gtk::Label::builder().label(text).xalign(0.0).css_classes(["dim-label", "caption-heading"]).margin_top(8).build()).build();
                list.append(&r);
            };
            // a category row: its name, and its parent beneath when it's a sub-category shown out of the tree
            let row = |c: &Category, with_parent: bool, indent: bool| {
                // one line per row, so the list keeps an even rhythm: the name, and its parent dimmed at the end
                let b = gtk::Box::builder().spacing(12).margin_start(if indent { 16 } else { 0 }).build();
                b.append(&gtk::Label::builder().label(&c.name).xalign(0.0).hexpand(true).ellipsize(gtk::pango::EllipsizeMode::End).css_classes(if c.parent_id.is_none() && !with_parent { vec!["heading"] } else { vec![] }).build());
                let parent = parent_name(c);
                if with_parent && let Some(p) = &parent {
                    b.append(&gtk::Label::builder().label(p).xalign(1.0).css_classes(["dim-label", "caption"]).build());
                }
                let r = gtk::ListBoxRow::builder().child(&b).build();
                // a screen reader hears the tree the indent shows
                let said = match &parent {
                    Some(p) => format!("{}, in {p}", c.name),
                    None => c.name.clone(),
                };
                r.update_property(&[gtk::accessible::Property::Label(&said)]);
                list.append(&r);
            };
            let q = q.trim();
            if q.is_empty() {
                if opts.allow_none {
                    ch.push(None);
                    header("No category");
                    ch.push(Some(Choice::None));
                    list.append(&gtk::ListBoxRow::builder().child(&gtk::Label::builder().label("Uncategorized").xalign(0.0).build()).build());
                }
                if !short.is_empty() {
                    ch.push(None);
                    header("Suggested & recent");
                    for id in &short {
                        if let Some(c) = name_of(*id) {
                            ch.push(Some(Choice::Cat(c.id)));
                            row(&c, true, false);
                        }
                    }
                }
                ch.push(None);
                header("All categories");
                for c in &cats {
                    ch.push(Some(Choice::Cat(c.id)));
                    row(c, false, c.parent_id.is_some());
                }
            } else {
                let found = matches(&cats, q);
                for c in &found {
                    ch.push(Some(Choice::Cat(c.id)));
                    row(c, true, false);
                }
                if opts.allow_none && "uncategorized".contains(&q.to_lowercase()) {
                    ch.push(Some(Choice::None));
                    list.append(&gtk::ListBoxRow::builder().child(&gtk::Label::builder().label("Uncategorized").xalign(0.0).build()).build());
                }
                // nothing by that exact name: make it, here
                if !cats.iter().any(|c| c.name.eq_ignore_ascii_case(q)) {
                    ch.push(Some(Choice::Add));
                    let add = adw::ButtonContent::builder().icon_name("list-add-symbolic").label(format!("Add “{q}”…")).halign(gtk::Align::Start).build();
                    list.append(&gtk::ListBoxRow::builder().child(&add).build());
                }
            }
            // the first real choice is what Enter picks
            if let Some(i) = ch.iter().position(Option::is_some) {
                list.select_row(list.row_at_index(i as i32).as_ref());
            }
            *choices.borrow_mut() = ch;
        }
    };
    let rebuild = Rc::new(rebuild);
    rebuild("");

    let pick = {
        let (app, pop, search, on_pick, choices) = (Rc::downgrade(app), pop.clone(), search.clone(), on_pick.clone(), choices.clone());
        Rc::new(move |i: usize| {
            let Some(Some(c)) = choices.borrow().get(i).copied() else { return };
            let Some(a) = app.upgrade() else { return };
            pop.popdown();
            match c {
                Choice::None => on_pick(None),
                Choice::Cat(id) => {
                    let _ = a.db.note_recent_category(id);
                    on_pick(Some(id));
                }
                Choice::Add => {
                    let f = on_pick.clone();
                    crate::rules::category_dialog_with(&a, search.text().trim(), move |a, id| {
                        let _ = a.db.note_recent_category(id);
                        f(Some(id));
                    });
                }
            }
        })
    };
    {
        let p = pick.clone();
        list.connect_row_activated(move |_, r| p(r.index() as usize));
    }
    {
        let r = rebuild.clone();
        search.connect_search_changed(move |s| r(&s.text()));
    }
    {
        let (p, list) = (pick.clone(), list.clone());
        search.connect_activate(move |_| {
            if let Some(r) = list.selected_row() {
                p(r.index() as usize);
            }
        });
    }
    // arrows move the highlight from the search box without leaving it; only Enter or a click picks
    {
        let (list, choices) = (list.clone(), choices.clone());
        let keys = gtk::EventControllerKey::new();
        keys.connect_key_pressed(move |_, key, _, _| {
            let step: i32 = match key {
                gtk::gdk::Key::Down => 1,
                gtk::gdk::Key::Up => -1,
                _ => return glib::Propagation::Proceed,
            };
            let ch = choices.borrow();
            let mut i = list.selected_row().map_or(-1, |r| r.index());
            loop {
                i += step;
                if i < 0 || i as usize >= ch.len() {
                    return glib::Propagation::Stop;
                }
                if ch[i as usize].is_some() {
                    break;
                }
            }
            if let Some(r) = list.row_at_index(i) {
                list.select_row(Some(&r));
            }
            glib::Propagation::Stop
        });
        search.add_controller(keys);
    }
    pop.popup();
    search.grab_focus();
}

#[cfg(test)]
mod tests {
    use super::*;
    use fingraph_core::db::CategoryKind;

    fn cat(id: i64, name: &str, parent: Option<i64>) -> Category {
        Category { id, name: name.into(), parent_id: parent, kind: CategoryKind::Expense }
    }

    #[test]
    fn finds_by_name_or_parent() {
        let cats = vec![cat(1, "Government & Fees", None), cat(2, "Stamp Duty", Some(1)), cat(3, "Taxes", Some(1)), cat(4, "Transport", None), cat(5, "Taxi & Rideshare", Some(4))];
        let names = |q: &str| matches(&cats, q).iter().map(|c| c.name.clone()).collect::<Vec<_>>();
        assert_eq!(names("tax"), vec!["Taxes", "Taxi & Rideshare"]);
        assert_eq!(names("government"), vec!["Government & Fees", "Stamp Duty", "Taxes"], "a parent's name finds its children");
        let cats2 = vec![cat(1, "Bank Fees & Interest", None), cat(2, "Loan Interest", Some(1)), cat(3, "Dining & Takeaway", None), cat(4, "Coffee & Snacks", Some(3)), cat(5, "Account Fees", Some(1))];
        let names2: Vec<String> = matches(&cats2, "fee").iter().map(|c| c.name.clone()).collect();
        assert_eq!(names2, vec!["Bank Fees & Interest", "Account Fees", "Loan Interest", "Coffee & Snacks"], "word starts, then children, then anywhere");
        assert_eq!(names("  STAMP "), vec!["Stamp Duty"]);
        assert!(names("zzz").is_empty());
    }
}
