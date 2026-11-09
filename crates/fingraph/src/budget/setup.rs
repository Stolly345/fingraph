//! Budget setup wizard and its input controls. Persistence is in fingraph_core::budget_setup.
use super::*;

/// How often a non-monthly bill comes.
const MONTH_NAMES: [&str; 12] = ["Due Jan", "Due Feb", "Due Mar", "Due Apr", "Due May", "Due Jun", "Due Jul", "Due Aug", "Due Sep", "Due Oct", "Due Nov", "Due Dec"];

pub(super) const EVERY: [(i32, &str); 5] = [(12, "Once a year"), (6, "Every 6 months"), (4, "Every 4 months (termly)"), (3, "Every 3 months"), (24, "Every 2 years")];

pub(super) fn every_index(months: i32) -> u32 {
    EVERY.iter().position(|e| e.0 == months).unwrap_or(0) as u32
}

/// Big bills most households have, offered on the wizard's yearly-bills step:
/// (name, hint, how often, due month if it's usually fixed).
const BIG_BILLS: [(&str, &str, i32, Option<u32>); 9] = [
    ("Car insurance", "One per vehicle: use + to add a motorcycle or another car", 12, None),
    ("Home or contents insurance", "Including hurricane cover", 12, None),
    ("HOA or strata fees", "When they're billed yearly or quarterly", 12, None),
    ("Vehicle licence & registration", "Yearly licensing and inspection, one per vehicle (+ for another)", 12, None),
    ("Health insurance", "Only if you pay it yourself, not through payroll", 12, None),
    ("School fees", "Per term", 4, None),
    ("Memberships & annual plans", "Gym, clubs, Costco, Amazon Prime, software", 12, None),
    ("Travel", "A yearly trip, or flights home", 12, None),
    ("Christmas & birthdays", "Gifts and celebrations", 12, Some(12)),
];

/// One bill on the wizard's yearly-bills step. An empty amount means you don't have it.
struct BillLine {
    /// The saved bill it came from (0 = new).
    id: i64,
    stem: String,
    currency: Currency,
    /// Its name: fixed for a listed bill, typed for one you add.
    title: String,
    name: Option<gtk::Entry>,
    amount: gtk::Entry,
    every: gtk::DropDown,
    month: gtk::DropDown,
    /// The saved due date, kept while its month isn't changed.
    due: Option<NaiveDate>,
    /// You said you cancelled it: dropped from the budget, and not suggested again.
    cancelled: Rc<Cell<bool>>,
    /// You said it's really monthly: not a big bill at all.
    monthly: Rc<Cell<bool>>,
    /// Setup asked whether it's monthly; saving without saying so is a no.
    asked: bool,
    /// The subscription it is, if it's one: where a monthly answer is recorded.
    sub_stem: Option<String>,
    /// How often it comes is a guess, so a "no" also records the cycle.
    guessed: bool,
    /// The category its charges are in, when seen in the statements.
    category: Option<String>,
}

impl BillLine {
    fn amount(&self) -> Option<Decimal> {
        if self.cancelled.get() || self.monthly.get() {
            return None;
        }
        parse_user(&self.amount.text()).map(|d| d.abs()).filter(|d| !d.is_zero())
    }
    fn every(&self) -> i32 {
        EVERY[(self.every.selected() as usize).min(EVERY.len() - 1)].0
    }
    fn name(&self) -> String {
        let typed = self.name.as_ref().map(|e| e.text().trim().to_string()).unwrap_or_default();
        if self.name.is_some() { if typed.is_empty() { "Bill".into() } else { typed } } else { self.title.clone() }
    }
    fn next_due(&self) -> NaiveDate {
        let m = self.month.selected() + 1;
        match self.due {
            Some(d) if d.month() == m && month_start(d) >= month_start(today()) => d,
            _ => core::due_in_month(m, today()),
        }
    }
}

// ──────────────────────────────────────────────────────────────── the setup wizard

pub(crate) fn parse_date(t: &str) -> Option<NaiveDate> {
    let t = t.trim();
    ["%d %b %Y", "%d %B %Y", "%Y-%m-%d", "%d/%m/%Y", "%d-%m-%Y", "%b %d %Y", "%B %d %Y"].iter().find_map(|f| NaiveDate::parse_from_str(&t.replace(',', ""), f).ok())
}

/// An amount, or "5%" (a percent).
fn parse_amount(t: &str) -> Option<(Decimal, bool)> {
    let pct = t.trim().ends_with('%');
    parse_user(t.trim().trim_end_matches('%')).map(|d| (d.abs(), pct))
}

pub(crate) fn amount_text(d: Decimal) -> String {
    if d.is_zero() { String::new() } else { d.round_dp(2).normalize().to_string() }
}

/// One category on the wizard's spending step.
struct CatLine {
    cat: Category,
    entry: gtk::Entry,
    row: adw::ActionRow,
    /// Usually, a month.
    usual: Option<Decimal>,
    /// Shown fortnightly: (the text we put in, the monthly figure it came from).
    memo: RefCell<Option<(String, String)>>,
    /// A need rather than a want (the budgeting methods split by it).
    need: gtk::ToggleButton,
    /// The big bills standing in for this category (insurance paid yearly, say).
    covered_by: RefCell<Vec<String>>,
    /// What the amount was before they did, to put back if they go.
    held: RefCell<Option<String>>,
}

impl CatLine {
    /// The monthly budget typed (or converted from fortnightly); `None` = no budget.
    fn monthly(&self, fortnightly: bool) -> Option<Decimal> {
        let text = self.entry.text();
        let from_memo = self.memo.borrow().as_ref().filter(|(shown, _)| fortnightly && *shown == text.as_str()).and_then(|(_, m)| parse_user(m));
        let v = from_memo.or_else(|| parse_user(&text).map(|d| if fortnightly { (d.abs() * Decimal::from(26) / Decimal::from(12)).round_dp(2) } else { d.abs() }))?;
        (!v.is_zero()).then_some(v)
    }
}

pub(crate) fn step_page(title: &str, intro: &str) -> (gtk::ScrolledWindow, gtk::Box) {
    let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(14).margin_top(12).margin_bottom(18).margin_start(24).margin_end(24).build();
    let h = heading(title);
    h.add_css_class("title-2");
    b.append(&h);
    b.append(&dim(intro));
    (gtk::ScrolledWindow::builder().child(&b).hscrollbar_policy(gtk::PolicyType::Never).vexpand(true).build(), b)
}

/// "A savings account outside FinGraph" in the account roles.
pub(crate) const OUTSIDE: i64 = -1;

/// What's owed on each card and loan (latest statement balance), in the display currency, most first.
pub(crate) fn owed(app: &App, s: &Snap) -> Vec<(i64, Decimal)> {
    let docs = app.db.documents(&app.entity_ids()).unwrap_or_default();
    let accts = app.db.accounts().unwrap_or_default();
    let mut v: Vec<(i64, Decimal)> = fingraph_core::insights::latest_balances(&s.rows, &docs)
        .into_iter()
        .filter_map(|(id, (_, b))| {
            let a = accts.iter().find(|a| a.id == id && !a.hidden)?;
            (a.account_type.is_liability() && b < Decimal::ZERO).then(|| (id, s.fx.convert(-b, a.currency, s.ccy).unwrap_or_default().round_dp(2)))
        })
        .collect();
    v.sort_by_key(|x| std::cmp::Reverse(x.1));
    v
}

/// An account as the setup's menus show it: its number first, so a long name can't cut it off.
fn account_label(name: &str, masked: &str) -> String {
    let digits: String = masked.chars().filter(char::is_ascii_digit).collect();
    let last4 = &digits[digits.len().saturating_sub(4)..];
    if last4.is_empty() {
        return name.to_string();
    }
    let short = name.split("••").next().unwrap_or(name).trim();
    format!("••{last4} · {short}")
}

/// First guesses at which account does what: pay lands where most income does; everyday spending
/// comes from the account with the most spending charges, when that's another one (a card).
fn guess_roles(s: &Snap, accts: &[(i64, String)]) -> Roles {
    let since = add_months(month_start(today()), -4);
    let mut income: BTreeMap<i64, Decimal> = BTreeMap::new();
    let mut charges: BTreeMap<i64, usize> = BTreeMap::new();
    for t in s.rows.iter().filter(|t| t.posted >= since && !t.is_transfer() && accts.iter().any(|a| a.0 == t.account_id)) {
        match t.category_kind {
            Some(fingraph_core::db::CategoryKind::Income) if t.amount.is_sign_positive() => *income.entry(t.account_id).or_default() += t.amount,
            Some(fingraph_core::db::CategoryKind::Expense) if t.amount.is_sign_negative() => *charges.entry(t.account_id).or_default() += 1,
            _ => {}
        }
    }
    let pay = income.iter().max_by_key(|x| *x.1).map(|x| *x.0);
    let spending = charges.iter().max_by_key(|x| *x.1).map(|x| *x.0).filter(|a| Some(*a) != pay);
    // bills: the account most regular-bill money comes out of
    let names: BTreeMap<String, i64> = accts.iter().map(|(id, n)| (n.clone(), *id)).collect();
    let mut by: BTreeMap<i64, Decimal> = BTreeMap::new();
    for b in fingraph_core::plan_doc::running_bills(&s.rows, &s.fixed, &s.parents, s.ccy, &s.fx, today(), today()) {
        if let Some(id) = names.get(&b.account) {
            *by.entry(*id).or_default() += b.amount;
        }
    }
    let bills = by.iter().max_by_key(|x| *x.1).map(|x| *x.0).filter(|a| Some(*a) != pay);
    Roles { pay, bills, spending, ..Default::default() }
}

/// Seven steps: pay & deductions → other money in and out → big yearly bills → method → spending → savings → accounts
/// goal (with the maths, the yearly bills' monthly share included).
pub fn wizard(app: &Rc<App>, done: crate::Refresh) {
    if !app.financial_ready() { return; }
    let s = Rc::new(snap(app));
    let (ccy, m) = (s.ccy, month_start(today()));
    let money = move |d: Decimal| fmt_money(d, ccy);
    // pay is looked for up to the newest statement: they arrive a month late
    let detected = core::detect_salary(&s.rows, data_through(app));
    let existing = s.plan.clone();
    let start = existing.clone().unwrap_or_else(|| {
        let mut p = Plan { currency: Some(ccy), loan_payments: usual_loans(&s, m), ..Default::default() };
        if let Some(d) = &detected {
            p.cycle = d.cycle;
            // the first payday still to come
            p.next_pay = Some(d.cycle.next(d.last));
            p.next_pay = p.paydays(today(), 1).first().copied();
            p.gross = s.fx.convert(d.amount, d.currency, ccy).unwrap_or_default();
        }
        p
    });

    // ── 1. pay
    let (p1, b1) = step_page(
        "Your Pay",
        "How often you're paid, the gross amount on your payslip, and what comes off it. In Cayman that's usually your 5% pension contribution and your share of health insurance. Type a deduction as an amount or a percent of gross (5%).",
    );
    if let Some(d) = &detected {
        let l = gtk::Label::builder()
            .label(format!(
                "From your statements: {} from {} {}, last on {}. That's take-home — if it matches, leave gross as it is and add no deductions; otherwise put in the payslip's gross and the deductions.",
                money(s.fx.convert(d.amount, d.currency, ccy).unwrap_or_default()),
                d.payee,
                d.cycle.label().to_lowercase(),
                d.last.format("%d %b %Y")
            ))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["card", "caption"])
            .build();
        l.set_margin_top(4);
        b1.append(&l);
    }
    let g = adw::PreferencesGroup::new();
    let cycle = adw::ComboRow::builder().title("Paid").model(&gtk::StringList::new(&PayCycle::ALL.map(PayCycle::label))).build();
    cycle.set_selected(PayCycle::ALL.iter().position(|c| *c == start.cycle).unwrap_or(3) as u32);
    let next_pay = adw::EntryRow::builder().title("Next payday (e.g. 31 Oct 2026)").text(start.next_pay.map(|d| d.format("%d %b %Y").to_string()).unwrap_or_default()).build();
    let gross = adw::EntryRow::builder().title(format!("Gross pay per payday ({})", ccy.symbol())).text(amount_text(start.gross)).input_purpose(gtk::InputPurpose::Number).build();
    g.add(&cycle);
    g.add(&next_pay);
    g.add(&gross);
    b1.append(&g);
    let dg = adw::PreferencesGroup::builder().title("Deductions per payday").build();
    let deds: Rc<RefCell<Vec<(adw::EntryRow, adw::EntryRow, gtk::Box)>>> = Rc::default();
    let ded_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    dg.add(&ded_box);
    let presets = gtk::Box::builder().spacing(6).build();
    let (pension, health, other) = (gtk::Button::with_label("+ Pension 5%"), gtk::Button::with_label("+ Health Insurance"), gtk::Button::with_label("+ Other"));
    for b in [&pension, &health, &other] {
        b.add_css_class("pill");
        presets.append(b);
    }
    dg.add(&presets);
    b1.append(&dg);
    let take_home = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b1.append(&take_home);

    // ── 2. other money
    let (p2, b2) = step_page(
        "Other Money In and Out",
        "Anything else that comes in every month (rent from a property, side work) and loan or car instalments that go out. Instalments are transfers, so they're not in the spending budget — but they still come out of what you can save.",
    );
    let g2 = adw::PreferencesGroup::new();
    let other_in = adw::EntryRow::builder().title(format!("Other income per month ({})", ccy.symbol())).text(amount_text(start.other_income)).build();
    let loans = adw::EntryRow::builder().title(format!("Loan payments per month ({})", ccy.symbol())).text(amount_text(start.loan_payments)).build();
    g2.add(&other_in);
    g2.add(&loans);
    b2.append(&g2);
    b2.append(&dim(&format!("Loan payments are filled in from the median of your last {MONTHS} months of Loan Payments.")));

    // ── 3. big yearly bills
    let (p_y, b_y) = step_page(
        "Big Yearly Bills",
        "The costs that don't come every month — insurance, fees, licences, school terms — so a normal month never shows them and they land all at once. Put in what each costs and the month it's next due; FinGraph sets a share aside every month so the money is there when it comes. Leave any you don't have empty.",
    );
    let bill_list = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    b_y.append(&bill_list);
    let add_bill_btn = gtk::Button::builder().label("+ Add Another").halign(gtk::Align::Start).css_classes(["pill"]).build();
    b_y.append(&add_bill_btn);
    let bills_total = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b_y.append(&bills_total);
    let bills: Rc<RefCell<Vec<BillLine>>> = Rc::default();

    // ── 4. spending
    let (p3, b3) = step_page(
        "Your Spending",
        "Each category starts at what you usually spend (the median of your last six months), the budget you already have, or what your method gives it. Change any of them; leave one empty for no budget. Mark each a Need or a Want: methods keep needs whole and trim wants. Fixed expenses are marked (change which on the Budget page).",
    );
    // the method's figures again, after changes to needs and wants
    let refill = gtk::Button::builder().label("Fill Again by My Method").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).tooltip_text("Put every category back to what your method gives it").build();
    b3.append(&refill);
    let (mut usual, _, one_off) = usual(&s, m);
    // the field is exactly what the list under it adds up to, not a rounded estimate or an old budget
    let subs_m: Decimal = fingraph_core::insights::subscriptions(&s.rows, ccy, &s.fx, today()).iter().filter(|x| x.active && x.cadence_days < BILL_DAYS).map(|x| x.monthly).sum();
    if !(subs_m + one_off).is_zero() {
        usual.insert(SUBS.into(), subs_m + one_off);
    }
    // Monthly | Fortnightly: what the amounts below mean (saved as monthly either way)
    let units = gtk::Box::builder().css_classes(["linked"]).halign(gtk::Align::Start).build();
    let monthly_b = gtk::ToggleButton::builder().label("Monthly").active(true).build();
    let fortnight_b = gtk::ToggleButton::builder().label("Fortnightly").group(&monthly_b).build();
    units.append(&monthly_b);
    units.append(&fortnight_b);
    b3.append(&units);
    let fortnightly = Rc::new(Cell::new(false));
    let lb = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).build();
    let cat_entries: Rc<Vec<CatLine>> = Rc::new(
        s.cats
            .iter()
            .filter(|c| s.budgets.contains_key(&c.id) || usual.contains_key(&c.name))
            .map(|c| {
                let u = usual.get(&c.name).copied();
                let r = adw::ActionRow::builder().title(glib::markup_escape_text(&c.name)).build();
                let e = gtk::Entry::builder().placeholder_text(ccy.symbol()).width_chars(9).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).build();
                e.set_text(&amount_text(if c.name == SUBS { u.or(s.budgets.get(&c.id).copied()) } else { s.budgets.get(&c.id).copied().or(u) }.unwrap_or_default()));
                // need or want: the methods keep needs whole and trim wants
                let is_need = s.needs.contains(&c.name);
                let need = gtk::ToggleButton::builder().label(if is_need { "Need" } else { "Want" }).active(is_need).valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("A need is something you'd pay even in a lean month (rent, groceries); a want is what you could cut (eating out). Click to switch.").build();
                need.connect_toggled(|b| b.set_label(if b.is_active() { "Need" } else { "Want" }));
                r.add_suffix(&need);
                r.add_suffix(&e);
                lb.append(&r);
                CatLine { cat: c.clone(), entry: e, row: r, usual: u, memo: RefCell::default(), need, covered_by: RefCell::default(), held: RefCell::default() }
            })
            .collect(),
    );
    // each row's note, in the chosen unit
    let fixed_names: Rc<Vec<String>> = Rc::new(s.fixed.clone());
    let subtitles = {
        let (cat_entries, fortnightly, fixed_names) = (cat_entries.clone(), fortnightly.clone(), fixed_names.clone());
        move || {
            let k = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
            for l in cat_entries.iter() {
                let note = l.usual.map(|u| format!(" · {}", usual_text(&l.cat.name, (u * k).round_dp(2), (subs_m * k).round_dp(2), (one_off * k).round_dp(2), ccy))).unwrap_or_default();
                let by = l.covered_by.borrow();
                if by.is_empty() {
                    l.row.set_subtitle(&glib::markup_escape_text(&format!("{}{note}", if fixed_names.contains(&l.cat.name) { "Fixed expense" } else { "Variable" })));
                } else {
                    l.row.set_subtitle(&glib::markup_escape_text(&format!("On your big bills: {}. Type an amount only if you also pay monthly.", by.join(", "))));
                }
            }
        }
    };
    subtitles();
    b3.append(&lb);
    // monthly subscriptions, each one you can mark cancelled: it comes off Subscriptions & Digital
    let sub_list: Vec<fingraph_core::insights::Subscription> = fingraph_core::insights::subscriptions(&s.rows, ccy, &s.fx, today())
        .into_iter()
        .filter(|x| x.cadence_days < BILL_DAYS && (x.active || (x.cancelled && (today() - x.last_charged).num_days() <= 2 * x.cadence_days + 7)))
        .collect();
    let sub_group = adw::PreferencesGroup::builder()
        .title("Your Subscriptions")
        .description("These add up to Subscriptions & Digital above. Mark any you've cancelled and its cost comes off. If it charges again, FinGraph counts it again.")
        .build();
    let sub_count: Rc<Cell<usize>> = Rc::new(Cell::new(sub_list.len() + usize::from(!one_off.is_zero())));
    sub_group.set_visible(sub_count.get() > 0);
    b3.append(&sub_group);
    let sub_marks: Rc<Vec<(String, bool, gtk::ToggleButton)>> = Rc::new({
        let g = sub_group.clone();
        let marks = sub_list
            .iter()
            .map(|x| {
                let r = adw::ActionRow::builder()
                    .title(glib::markup_escape_text(&x.payee))
                    .subtitle(format!("{} a month · last charged {}", money(x.monthly), x.last_charged.format("%d %b %Y")))
                    .build();
                let b = gtk::ToggleButton::builder().label("Cancelled").active(x.cancelled).valign(gtk::Align::Center).tooltip_text("I've cancelled this").build();
                r.add_suffix(&b);
                g.add(&r);
                let (cat_entries, fortnightly, monthly, row) = (cat_entries.clone(), fortnightly.clone(), x.monthly, r.clone());
                b.connect_toggled(move |b| {
                    row.set_opacity(if b.is_active() { 0.55 } else { 1.0 });
                    let Some(l) = cat_entries.iter().find(|l| l.cat.name == SUBS) else { return };
                    let k = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
                    let now = parse_user(&l.entry.text()).unwrap_or_default().abs();
                    let step = (monthly * k).round_dp(2);
                    *l.memo.borrow_mut() = None;
                    l.entry.set_text(&amount_text(if b.is_active() { (now - step).max(Decimal::ZERO) } else { now + step }));
                });
                if x.cancelled {
                    r.set_opacity(0.55);
                }
                (x.stem.clone(), x.cancelled, b)
            })
            .collect();
        // what isn't a subscription but lands in the same category, so the list still adds up
        if !one_off.is_zero() {
            let r = adw::ActionRow::builder().title("Other digital purchases").subtitle(format!("{} a month · one-off apps, games and downloads, usually", money(one_off))).build();
            let b = gtk::ToggleButton::builder().label("Leave Out").valign(gtk::Align::Center).tooltip_text("Don't budget for these").build();
            let (cat_entries, fortnightly, row) = (cat_entries.clone(), fortnightly.clone(), r.clone());
            b.connect_toggled(move |b| {
                row.set_opacity(if b.is_active() { 0.55 } else { 1.0 });
                let Some(l) = cat_entries.iter().find(|l| l.cat.name == SUBS) else { return };
                let k = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
                let now = parse_user(&l.entry.text()).unwrap_or_default().abs();
                let step = (one_off * k).round_dp(2);
                *l.memo.borrow_mut() = None;
                l.entry.set_text(&amount_text(if b.is_active() { (now - step).max(Decimal::ZERO) } else { now + step }));
            });
            r.add_suffix(&b);
            g.add(&r);
        }
        marks
    });
    let spend_total = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["heading", "tnum"]).build();
    b3.append(&spend_total);

    // ── 4. the method
    let (p_m, b_m) = step_page(
        "Choose Your Method",
        "A budgeting method decides how your money is divided; FinGraph then fills in your budget from it, using what you really spend. Your pension counts toward saving. You can still change any amount on the next step.",
    );
    let method = Rc::new(Cell::new(start.method));
    let applied: Rc<Cell<Option<fingraph_core::method::Method>>> = Rc::new(Cell::new(Some(start.method)));
    let mg = adw::PreferencesGroup::new();
    let mut first: Option<gtk::CheckButton> = None;
    let mut method_checks: Vec<gtk::CheckButton> = Vec::new();
    for mth in fingraph_core::method::Method::ALL {
        let check = gtk::CheckButton::builder().active(mth == start.method).valign(gtk::Align::Center).build();
        if let Some(f) = &first {
            check.set_group(Some(f));
        } else {
            first = Some(check.clone());
        }
        let r = adw::ActionRow::builder().title(mth.label()).subtitle(mth.about()).subtitle_lines(4).activatable_widget(&check).build();
        r.add_prefix(&check);
        mg.add(&r);
        method_checks.push(check.clone());
        let method = method.clone();
        check.connect_toggled(move |c| {
            if c.is_active() {
                method.set(mth);
            }
        });
    }
    b_m.append(&mg);
    // the screenshot harness: FINGRAPH_METHOD=1 picks the second method
    #[cfg(debug_assertions)]
    if let Some(i) = std::env::var("FINGRAPH_METHOD").ok().and_then(|v| v.parse::<usize>().ok()) {
        if let Some(c) = method_checks.get(i) {
            c.set_active(true);
        }
        applied.set(None);
    }
    let preview = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    preview.set_margin_top(4);
    b_m.append(&preview);
    let method_notes = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b_m.append(&method_notes);
    let owed_list = owed(app, &s);
    let owed_total: Decimal = owed_list.iter().map(|x| x.1).sum();

    // ── 6. savings
    let (p4, b4) = step_page(
        "Your Savings Goal",
        "How much you want to put away each month — an amount, or a percent of take-home (20%). FinGraph does the arithmetic: take-home − spending − big yearly bills − loans = what's left, and if that's short of the goal, how much the variable categories need to shrink.",
    );
    let g4 = adw::PreferencesGroup::new();
    let goal = adw::EntryRow::builder().title(format!("Save per month ({} or %)", ccy.symbol())).text(amount_text(start.savings_goal)).build();
    g4.add(&goal);
    let debt = adw::EntryRow::builder().title(format!("Pay down debt per month, on top of the minimums ({})", ccy.symbol())).text(amount_text(start.debt_extra)).build();
    g4.add(&debt);
    if owed_total > Decimal::ZERO {
        b4.append(&dim(&format!("You owe {} on cards and loans (latest statements). Extra paid here clears it faster.", money(owed_total))));
    }
    b4.append(&g4);
    let choice_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
    b4.append(&choice_box);
    let maths = gtk::Label::builder().xalign(0.0).wrap(true).selectable(true).css_classes(["card", "monospace"]).build();
    maths.set_margin_top(4);
    b4.append(&maths);
    let scale = gtk::Button::builder().label("Trim Variable Categories to Reach It").halign(gtk::Align::Start).css_classes(["pill"]).visible(false).build();
    b4.append(&scale);
    // zero-based: whatever's not given a job yet
    let rest = gtk::Button::builder().label("Put the Rest in Savings").halign(gtk::Align::Start).css_classes(["pill", "suggested-action"]).visible(false).build();
    b4.append(&rest);
    let unassigned: Rc<Cell<Decimal>> = Rc::default();
    let ahead = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["dim-label"]).build();
    b4.append(&ahead);

    // read every field into a plan + budgets, and redo the sums
    // ── 7. where the money goes: which account does what, for the payday checklist
    let (p5, b5) = step_page(
        "Where Your Money Goes",
        "Which account your pay lands in and what each part of it is for. The Budget Plan turns this into a checklist for every payday: move this much here, leave this much there.",
    );
    let accts: Vec<(i64, String)> = app.db.accounts().unwrap_or_default().into_iter().filter(|x| !x.hidden && s.rows.iter().any(|t| t.account_id == x.id)).map(|x| (x.id, account_label(&x.name, &x.account_number_masked))).collect();
    let mut roles = if start.accounts == Default::default() { guess_roles(&s, &accts) } else { start.accounts.clone() };
    if roles.debt.is_none() {
        roles.debt = owed_list.first().map(|x| x.0);
    }
    let g5 = adw::PreferencesGroup::new();
    // (title, help, choices beyond the accounts, current)
    let role_row = |title: &str, sub: &str, same: &str, outside: bool, cur: Option<i64>| {
        let mut ids: Vec<Option<i64>> = vec![None];
        let mut names: Vec<String> = vec![same.to_string()];
        for (id, n) in &accts {
            ids.push(Some(*id));
            names.push(n.clone());
        }
        if outside {
            ids.push(Some(OUTSIDE));
            names.push("A savings account outside FinGraph".into());
        }
        let r = adw::ComboRow::builder().title(title).subtitle(sub).model(&gtk::StringList::new(&names.iter().map(String::as_str).collect::<Vec<_>>())).build();
        r.set_selected(ids.iter().position(|i| *i == cur).unwrap_or(0) as u32);
        g5.add(&r);
        (r, ids)
    };
    let r_pay = role_row("Pay goes into", "Where your salary is deposited", "Not set", false, roles.pay);
    let r_bills = role_row("Bills are paid from", "Fixed bills and loan payments", "The pay account", false, roles.bills);
    let r_spend = role_row("Everyday spending from", "Groceries, eating out and the other variable categories (a card, say)", "The pay account", false, roles.spending);
    let r_big = role_row("Big-bill money is kept in", "Where the monthly share for yearly bills builds up", "The pay account", true, roles.big_bills);
    let r_save = role_row("Savings go to", "Where the monthly savings go", "The pay account", true, roles.savings);
    let r_debt = role_row("Extra debt payments go to", "The card or loan to pay down first (chosen on the savings step)", "Not set", false, roles.debt);
    b5.append(&g5);
    b5.append(&dim("A card used for everyday spending gets the money to pay it off: the checklist moves that much to it each payday. Pick “the pay account” when you pay straight from there."));
    let read_roles = {
        let rows = [r_pay.clone(), r_bills.clone(), r_spend.clone(), r_big.clone(), r_save.clone(), r_debt.clone()];
        move || {
            let v: Vec<Option<i64>> = rows.iter().map(|(r, ids)| ids.get(r.selected() as usize).copied().flatten()).collect();
            Roles { pay: v[0], bills: v[1], spending: v[2], big_bills: v[3], savings: v[4], debt: v[5] }
        }
    };

    let avalanche = Rc::new(Cell::new(start.debt_avalanche));
    let read = {
        let avalanche = avalanche.clone();
        let read_roles = read_roles.clone();
        let (cycle, next_pay, gross, deds, other_in, loans, goal, cat_entries, fortnightly) = (cycle.clone(), next_pay.clone(), gross.clone(), deds.clone(), other_in.clone(), loans.clone(), goal.clone(), cat_entries.clone(), fortnightly.clone());
        let (method, debt) = (method.clone(), debt.clone());
        Rc::new(move || -> (Plan, Vec<(i64, Option<Decimal>)>) {
            let mut p = Plan {
                cycle: PayCycle::ALL[(cycle.selected() as usize).min(3)],
                next_pay: parse_date(&next_pay.text()),
                gross: parse_user(&gross.text()).unwrap_or_default().abs(),
                deductions: deds
                    .borrow()
                    .iter()
                    .filter_map(|(n, a, _)| {
                        let (amount, percent) = parse_amount(&a.text())?;
                        Some(Deduction { name: n.text().to_string(), amount, percent })
                    })
                    .collect(),
                other_income: parse_user(&other_in.text()).unwrap_or_default().abs(),
                loan_payments: parse_user(&loans.text()).unwrap_or_default().abs(),
                currency: Some(ccy),
                accounts: read_roles(),
                method: method.get(),
                debt_extra: parse_user(&debt.text()).unwrap_or_default().abs(),
                debt_avalanche: avalanche.get(),
                ..Default::default()
            };
            let budgets: Vec<(i64, Option<Decimal>)> = cat_entries.iter().map(|l| (l.cat.id, l.monthly(fortnightly.get()))).collect();
            p.savings_goal = match parse_amount(&goal.text()) {
                Some((v, true)) => (p.monthly_income() * v / Decimal::ONE_HUNDRED).round_dp(0),
                Some((v, false)) => v,
                None => Decimal::ZERO,
            };
            (p, budgets)
        })
    };
    // ── the rest: savings or debt, and which debt
    let debts = core::avalanche_order(crate::wealth::account_debts(app));
    if !debts.is_empty() {
        let rest_group = adw::PreferencesGroup::builder().title("What Should Happen With the Rest?").description("Whatever's left after spending, big bills and loans.").build();
        let to_save = gtk::CheckButton::builder().valign(gtk::Align::Center).active(start.debt_extra.is_zero() || !start.savings_goal.is_zero()).build();
        let to_debt = gtk::CheckButton::builder().valign(gtk::Align::Center).group(&to_save).active(!start.debt_extra.is_zero() && start.savings_goal.is_zero()).build();
        for (c, t, st) in [(&to_save, "Put It in Savings", "Build up what you have"), (&to_debt, "Put It Toward Debt", "Pay cards and loans off sooner, on top of the regular payments")] {
            let r = adw::ActionRow::builder().title(t).subtitle(st).activatable_widget(c).build();
            r.add_prefix(c);
            rest_group.add(&r);
        }
        choice_box.append(&rest_group);
        // 70/20/10 already fixes the split: only which debt is left to choose
        let is_seventy = |m: fingraph_core::method::Method| m == fingraph_core::method::Method::SeventyTwentyTen;
        rest_group.set_visible(!is_seventy(start.method));
        for (i, c) in method_checks.iter().enumerate() {
            let (g, m) = (rest_group.clone(), fingraph_core::method::Method::ALL[i]);
            c.connect_toggled(move |c| {
                if c.is_active() {
                    g.set_visible(!is_seventy(m));
                }
            });
        }
        let move_all = {
            let (read, unassigned, goal, debt) = (read.clone(), unassigned.clone(), goal.clone(), debt.clone());
            move |into_debt: bool| {
                let (p, _) = read();
                let all = p.savings_goal + p.debt_extra + unassigned.get().max(Decimal::ZERO);
                let text = amount_text(all);
                goal.set_text(if into_debt { "" } else { &text });
                debt.set_text(if into_debt { &text } else { "" });
            }
        };
        for (c, into_debt) in [(&to_save, false), (&to_debt, true)] {
            let m = move_all.clone();
            c.connect_toggled(move |c| {
                if c.is_active() {
                    m(into_debt);
                }
            });
        }
        // the screenshot harness: FINGRAPH_REST=debt puts the rest toward debt, once the sums are in
        #[cfg(debug_assertions)]
        if std::env::var("FINGRAPH_REST").as_deref() == Ok("debt") {
            let c = to_debt.clone();
            glib::idle_add_local_once(move || c.set_active(true));
        }
        let which = adw::PreferencesGroup::builder().title("Which Debt First?").build();
        let label_of = |id: i64| accts.iter().find(|a| a.0 == id).map_or_else(|| "Account".to_string(), |a| a.1.clone());
        let top = debts[0].0;
        let choose = {
            let (combo, ids) = r_debt.clone();
            move |id: i64| combo.set_selected(ids.iter().position(|i| *i == Some(id)).unwrap_or(0) as u32)
        };
        let rate = |r: Option<Decimal>| r.map_or("rate not known".to_string(), |v| format!("{}% a year", v.round_dp(1).normalize()));
        let auto = gtk::CheckButton::builder().valign(gtk::Align::Center).build();
        let r = adw::ActionRow::builder().title("Highest Interest First (Avalanche)").subtitle(glib::markup_escape_text(&format!("Costs the least overall. Starts with {} ({}).", label_of(top), rate(debts[0].2)))).activatable_widget(&auto).build();
        r.add_prefix(&auto);
        which.add(&r);
        {
            let (flag, choose) = (avalanche.clone(), choose.clone());
            auto.connect_toggled(move |c| {
                flag.set(c.is_active());
                if c.is_active() {
                    choose(top);
                }
            });
        }
        let picked = roles.debt.filter(|_| !start.debt_avalanche && start.accounts.debt.is_some());
        let mut any = false;
        for (id, owes, apr) in &debts {
            let c = gtk::CheckButton::builder().valign(gtk::Align::Center).group(&auto).build();
            let r = adw::ActionRow::builder().title(glib::markup_escape_text(&label_of(*id))).subtitle(format!("{} owed · {}", money(*owes), rate(*apr))).activatable_widget(&c).build();
            r.add_prefix(&c);
            which.add(&r);
            let (choose, id) = (choose.clone(), *id);
            c.connect_toggled(move |c| {
                if c.is_active() {
                    choose(id);
                }
            });
            if picked == Some(id) {
                c.set_active(true);
                any = true;
            }
        }
        if !any {
            auto.set_active(true);
        }
        // shown once any money is going to debt
        let shows = |t: &str| parse_user(t).is_some_and(|v| !v.is_zero());
        which.set_visible(shows(&debt.text()));
        {
            let w = which.clone();
            debt.connect_changed(move |e| w.set_visible(shows(&e.text())));
        }
        choice_box.append(&which);
    }
    let factor: Rc<Cell<Option<Decimal>>> = Rc::default();
    // the share of the big bills set aside each month, in the display currency
    let aside_of = {
        let (bills, fx) = (bills.clone(), s.fx.clone());
        Rc::new(move || -> Decimal { bills.borrow().iter().filter_map(|l| Some(fx.convert(l.amount()? / Decimal::from(l.every()), l.currency, ccy).unwrap_or_default())).sum::<Decimal>().round_dp(2) })
    };
    // each category as a method sees it: need or want, and what it usually costs (or its budget)
    let cat_ins = {
        let cat_entries = cat_entries.clone();
        Rc::new(move || -> Vec<fingraph_core::method::CatIn> {
            cat_entries.iter().map(|l| fingraph_core::method::CatIn { name: l.cat.name.clone(), need: l.need.is_active(), usual: if l.covered_by.borrow().is_empty() { l.usual.or_else(|| l.monthly(false)).unwrap_or_default() } else { Decimal::ZERO } }).collect()
        })
    };
    let recalc: Rc<dyn Fn()> = {
        let (read, take_home, spend_total, maths, ahead, scale, cat_entries, factor, fixed_names) = (read.clone(), take_home.clone(), spend_total.clone(), maths.clone(), ahead.clone(), scale.clone(), cat_entries.clone(), factor.clone(), fixed_names.clone());
        let (bills, bills_total, fx) = (bills.clone(), bills_total.clone(), s.fx.clone());
        let (aside_of, cat_ins, preview, method_notes, rest, unassigned, refill) = (aside_of.clone(), cat_ins.clone(), preview.clone(), method_notes.clone(), rest.clone(), unassigned.clone(), refill.clone());
        let notes = subtitles.clone();
        Rc::new(move || {
            // a yearly bill stands in for that category's monthly amount, and gives it back if it goes
            let mut moved = false;
            for c in cat_entries.iter() {
                let by: Vec<String> = bills
                    .borrow()
                    .iter()
                    .filter(|l| l.amount().is_some())
                    .filter(|l| {
                        let name = l.name();
                        let cat = l.category.as_deref().filter(|c| !c.is_empty()).or(core::category_for_bill(&name));
                        cat.and_then(core::lumpy_category) == Some(c.cat.name.as_str())
                    })
                    .map(|l| l.name())
                    .collect();
                let was = !c.covered_by.borrow().is_empty();
                if *c.covered_by.borrow() == by {
                    continue;
                }
                moved = true;
                let now = !by.is_empty();
                *c.covered_by.borrow_mut() = by;
                if now && !was {
                    *c.held.borrow_mut() = Some(c.entry.text().to_string());
                    *c.memo.borrow_mut() = None;
                    c.entry.set_text("");
                } else if !now && was {
                    let back = c.held.borrow_mut().take().unwrap_or_default();
                    *c.memo.borrow_mut() = None;
                    c.entry.set_text(&back);
                }
            }
            if moved {
                notes();
            }
            let (p, budgets) = read();
            let aside = aside_of();
            let yearly: Decimal = bills.borrow().iter().filter_map(|l| Some(fx.convert(l.amount()? * Decimal::from(12) / Decimal::from(l.every()), l.currency, ccy).unwrap_or_default())).sum::<Decimal>().round_dp(2);
            let n = bills.borrow().iter().filter(|l| l.amount().is_some()).count();
            bills_total.set_label(&if n == 0 {
                "No big bills yet — add any you have, or skip this step.".to_string()
            } else {
                format!("{n} bill{} · {} a year · set aside {} a month", if n == 1 { "" } else { "s" }, money(yearly), money(aside))
            });
            let deducted: Vec<String> = p.deductions.iter().map(|d| format!("{} {}", if d.name.is_empty() { "deduction" } else { &d.name }, money(p.deduction(d)))).collect();
            take_home.set_label(&format!(
                "Gross {} a payday = {} a month.\nTake-home {} a payday{} → {} a month ({} × {} ÷ 12)",
                money(p.gross),
                money((p.gross * p.cycle.per_year() / Decimal::from(12)).round_dp(2)),
                money(p.net_pay()),
                if deducted.is_empty() { String::new() } else { format!(" (gross {} − {})", money(p.gross), deducted.join(" − ")) },
                money(p.monthly_income() - p.other_income),
                money(p.net_pay()),
                p.cycle.per_year()
            ));
            let total: Decimal = budgets.iter().filter_map(|b| b.1).sum();
            let fixed: Decimal = cat_entries.iter().zip(&budgets).filter(|(l, _)| fixed_names.contains(&l.cat.name)).filter_map(|(_, b)| b.1).sum();
            let flexible = total - fixed;
            let fn_ = |d: Decimal| money((d * Decimal::from(12) / Decimal::from(26)).round_dp(2));
            spend_total.set_label(&format!(
                "Spending budget {} a month ({} a fortnight) — fixed {}, variable {} a month",
                money(total),
                fn_(total),
                money(fixed),
                money(flexible)
            ));
            let save = p.monthly_savings(total) - aside;
            let mut lines = vec![
                format!("  Take-home per month        {:>14}", money(p.monthly_income())),
                format!("− Spending budget            {:>14}", money(total)),
                format!("− Big yearly bills set aside {:>14}", money(aside)),
                format!("− Loan payments              {:>14}", money(p.loan_payments)),
                format!("− Paying off debt            {:>14}", money(p.debt_extra)),
                format!("= Left to save               {:>14}", money(save)),
            ];
            let pension = p.pension_monthly();
            if pension > Decimal::ZERO {
                lines.push(format!("  + Pension (saved for you)  {:>14}", money(pension)));
            }
            factor.set(None);
            scale.set_visible(false);
            if !p.savings_goal.is_zero() {
                lines.push(format!("  Goal                       {:>14}", money(p.savings_goal)));
                if save >= p.savings_goal {
                    lines.push(format!("\nOn track, with {} a month to spare.", money(save - p.savings_goal)));
                } else {
                    lines.push(format!("\nShort by {} a month.", money(p.savings_goal - save)));
                    // the set-aside is as fixed as rent: only variable spending is trimmed
                    match p.flexible_factor(fixed + aside, flexible) {
                        Some(k) => {
                            lines.push(format!(
                                "Solve take-home − loans − fixed − big bills − k × variable = goal:\nk = ({} − {} − {} − {} − {}) ÷ {} = {:.3}\nTrim every variable category by {}% ({} → {}).",
                                money(p.monthly_income()),
                                money(p.loan_payments),
                                money(fixed),
                                money(aside),
                                money(p.savings_goal),
                                money(flexible),
                                k,
                                ((Decimal::ONE - k) * Decimal::ONE_HUNDRED).round_dp(0),
                                money(flexible),
                                money((flexible * k).round_dp(2))
                            ));
                            factor.set(Some(k));
                            scale.set_visible(true);
                        }
                        None => lines.push("Even with no variable spending it doesn't add up — lower the goal or the fixed expenses.".into()),
                    }
                }
            }
            // zero-based: every dollar gets a job, so what's left after the goal must come to nothing
            let zero = p.method == fingraph_core::method::Method::ZeroBased;
            let left = save - p.savings_goal;
            unassigned.set(left);
            rest.set_visible(zero && left.abs() >= Decimal::ONE);
            if zero {
                lines.push(if left.abs() < Decimal::ONE {
                    "\nZero-based: every dollar has a job.".to_string()
                } else if left > Decimal::ZERO {
                    format!("\nZero-based: {} still has no job. Give it to a category or to savings.", money(left))
                } else {
                    format!("\nZero-based: {} more is given out than comes in. Take it from a category or the goal.", money(-left))
                });
            }
            maths.set_label(&lines.join("\n"));

            // the method step: what the chosen method means with your numbers
            let mth = p.method;
            refill.set_visible(mth.split().is_some());
            let base = p.method_base();
            let ins = cat_ins();
            let needs_u: Decimal = ins.iter().filter(|c| c.need).map(|c| c.usual).sum::<Decimal>() + aside + p.loan_payments;
            let wants_u: Decimal = ins.iter().filter(|c| !c.need).map(|c| c.usual).sum();
            let pc = |d: Decimal| fingraph_core::method::share(d, base);
            let mut pv = vec![
                format!("  Take-home + pension a month {:>14}", money(base)),
                format!("  Needs usually (with big bills, loans) {:>8}  {:>3}%", money(needs_u), pc(needs_u)),
                format!("  Wants usually               {:>14}  {:>3}%", money(wants_u), pc(wants_u)),
            ];
            match fingraph_core::method::allocate(mth, base, pension, aside, p.loan_payments, owed_total, &ins, &money) {
                Some(a) => {
                    let wants: Decimal = a.budgets.iter().zip(&ins).filter(|(_, c)| !c.need).map(|(b, _)| b.1).sum();
                    pv.push(format!("\n{} gives:", mth.label()));
                    if let (Some(n), Some(w)) = (a.target_needs, a.target_wants) {
                        pv.push(format!("  Needs up to                 {:>14}", money(n)));
                        pv.push(format!("  Wants up to                 {:>14}  → {}", money(w), money(wants)));
                    } else {
                        pv.push(format!("  All spending up to          {:>14}", money(a.target_spend)));
                        pv.push(format!("  Wants get                   {:>14}", money(wants)));
                    }
                    pv.push(format!("  Saving at least             {:>14}  → {} + pension {}", money(a.target_save), money(a.savings), money(pension)));
                    if a.target_debt > Decimal::ZERO {
                        pv.push(format!("  Paying off debt             {:>14}  → {}", money(a.target_debt), money(a.debt)));
                    }
                    method_notes.set_label(&a.notes.join("\n"));
                }
                None => {
                    method_notes.set_label(if mth == fingraph_core::method::Method::ZeroBased {
                        "Zero-based starts from what you usually spend; on the savings step, give every dollar left a job until it comes to zero."
                    } else {
                        "Your own numbers: each category starts at what it usually costs, and you set the savings goal."
                    });
                }
            }
            preview.set_label(&pv.join("\n"));
            let months_left = 12 - today().month() as i32;
            ahead.set_label(&format!(
                "At {} a month: {} by the end of {} ({} months), {} in a year, {} in five years (before any interest).",
                money(save),
                money(save * Decimal::from(months_left.max(0))),
                today().year(),
                months_left,
                money(save * Decimal::from(12)),
                money(save * Decimal::from(60))
            ));
        })
    };

    // deduction rows
    type AddDeduction = Rc<dyn Fn(&str, &str)>;
    let add_ded: AddDeduction = {
        let (deds, ded_box, recalc) = (deds.clone(), ded_box.clone(), recalc.clone());
        Rc::new(move |name: &str, amount: &str| {
            let row = gtk::Box::builder().spacing(6).build();
            let n = adw::EntryRow::builder().title("Name").text(name).hexpand(true).build();
            let a = adw::EntryRow::builder().title("Amount or %").text(amount).build();
            let rm = gtk::Button::builder().icon_name("list-remove-symbolic").valign(gtk::Align::Center).css_classes(["flat"]).tooltip_text("Remove").build();
            let l = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).hexpand(true).build();
            l.append(&n);
            let l2 = gtk::ListBox::builder().css_classes(["boxed-list"]).selection_mode(gtk::SelectionMode::None).width_request(170).build();
            l2.append(&a);
            row.append(&l);
            row.append(&l2);
            row.append(&rm);
            for e in [&n, &a] {
                let r = recalc.clone();
                e.connect_changed(move |_| r());
            }
            {
                let (deds, ded_box, row2, recalc) = (deds.clone(), ded_box.clone(), row.clone(), recalc.clone());
                rm.connect_clicked(move |_| {
                    deds.borrow_mut().retain(|x| x.2 != row2);
                    ded_box.remove(&row2);
                    recalc();
                });
            }
            ded_box.append(&row);
            deds.borrow_mut().push((n, a, row));
            recalc();
        })
    };
    // yearly-bill rows: what's saved, then what the statements show, then the usual big bills
    type AddBill = Rc<dyn Fn(BillLine, &str, Option<i32>)>;
    let adder: Rc<RefCell<Option<AddBill>>> = Rc::default();
    // the amount and both drop-downs line up from row to row
    let widths = [gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal), gtk::SizeGroup::new(gtk::SizeGroupMode::Horizontal)];
    let add_bill: AddBill = {
        let (bills, bill_list, recalc, adder) = (bills.clone(), bill_list.clone(), recalc.clone(), adder.clone());
        let (cat_entries, fortnightly, fx, sub_group, sub_count) = (cat_entries.clone(), fortnightly.clone(), s.fx.clone(), sub_group.clone(), sub_count.clone());
        Rc::new(move |l: BillLine, hint: &str, at: Option<i32>| {
            // a bill you have or that was seen in your statements gets a second line: what FinGraph
            // knows or is asking, and the buttons to answer
            let answers = l.id != 0 || !l.stem.is_empty();
            let r = adw::ActionRow::builder().title(glib::markup_escape_text(&l.title)).subtitle(if answers { String::new() } else { glib::markup_escape_text(hint).to_string() }).build();
            let note_label = gtk::Label::builder().label(hint).xalign(0.0).hexpand(true).wrap(true).css_classes(if l.asked { ["caption", "warning"] } else { ["caption", "dim-label"] }).build();
            let second = gtk::Box::builder().spacing(8).margin_top(4).margin_bottom(8).margin_start(12).margin_end(12).build();
            second.append(&note_label);
            if let Some(e) = &l.name {
                e.set_hexpand(true);
                e.set_valign(gtk::Align::Center);
                r.add_prefix(e);
            }
            for (w, g) in [l.amount.upcast_ref::<gtk::Widget>(), l.every.upcast_ref(), l.month.upcast_ref()].into_iter().zip(&widths) {
                g.add_widget(w);
                r.add_suffix(w);
            }
            for e in std::iter::once(&l.amount).chain(l.name.as_ref()) {
                let rc = recalc.clone();
                e.connect_changed(move |_| rc());
            }
            for d in [&l.every, &l.month] {
                let rc = recalc.clone();
                d.connect_selected_notify(move |_| rc());
            }
            // one seen in your statements may really be monthly: FinGraph can be wrong about that
            let monthly_btn = (!l.stem.is_empty()).then(|| {
                let m = gtk::ToggleButton::builder().label(if l.asked { "Yes, Monthly" } else { "Monthly" }).valign(gtk::Align::Center).tooltip_text("This is charged every month: count it with everyday spending and don't set money aside for it").build();
                let (flag, rc, label, hint, widgets) = (l.monthly.clone(), recalc.clone(), note_label.clone(), hint.to_string(), [l.amount.clone().upcast::<gtk::Widget>(), l.every.clone().upcast(), l.month.clone().upcast()]);
                let (amount, cur, is_sub, cats, fortnightly, fx) = (l.amount.clone(), l.currency, l.sub_stem.is_some(), cat_entries.clone(), fortnightly.clone(), fx.clone());
                // what this row put into Subscriptions & Digital, so pressing again takes out the same
                let added: Rc<Cell<Decimal>> = Rc::default();
                let (group, count, listed, title) = (sub_group.clone(), sub_count.clone(), Rc::new(RefCell::new(None::<adw::ActionRow>)), l.title.clone());
                m.connect_toggled(move |m| {
                    let on = m.is_active();
                    flag.set(on);
                    for w in &widgets {
                        w.set_sensitive(!on);
                    }
                    let subs = cats.iter().find(|c| c.cat.name == SUBS).filter(|_| is_sub);
                    if let Some(c) = subs {
                        let per = if fortnightly.get() { Decimal::from(12) / Decimal::from(26) } else { Decimal::ONE };
                        let now = parse_user(&c.entry.text()).unwrap_or_default();
                        let change = if on { (fx.convert(parse_user(&amount.text()).unwrap_or_default().abs(), cur, ccy).unwrap_or_default() * per).round_dp(2) } else { added.get() };
                        added.set(if on { change } else { Decimal::ZERO });
                        *c.memo.borrow_mut() = None;
                        c.entry.set_text(&amount_text(if on { now + change } else { (now - change).max(Decimal::ZERO) }));
                        if on {
                            let row = adw::ActionRow::builder().title(glib::markup_escape_text(&title)).subtitle(format!("{} a month · you marked this monthly on the Big Yearly Bills step", fmt_money((change / per).round_dp(2), ccy))).build();
                            group.add(&row);
                            *listed.borrow_mut() = Some(row);
                            count.set(count.get() + 1);
                        } else if let Some(row) = listed.borrow_mut().take() {
                            group.remove(&row);
                            count.set(count.get().saturating_sub(1));
                        }
                        group.set_visible(count.get() > 0);
                    }
                    let note = if !on {
                        hint.clone()
                    } else if subs.is_some() {
                        format!("Monthly: added to {SUBS} on the Your Spending step, and not set aside for")
                    } else {
                        "Monthly: counted with your everyday spending, and not set aside for".to_string()
                    };
                    label.set_label(&note);
                    rc();
                });
                second.append(&m);
                m
            });
            // one you had (saved, or seen in your statements) can be marked cancelled
            if l.id != 0 || !l.stem.is_empty() {
                let c = gtk::ToggleButton::builder().label("Cancelled").valign(gtk::Align::Center).tooltip_text("I've cancelled this: drop it from the budget and stop suggesting it (unless it charges again)").build();
                let (flag, rc, row, label, hint, widgets) = (l.cancelled.clone(), recalc.clone(), r.clone(), note_label.clone(), hint.to_string(), [l.amount.clone().upcast::<gtk::Widget>(), l.every.clone().upcast(), l.month.clone().upcast()]);
                c.connect_toggled(move |c| {
                    let on = c.is_active();
                    flag.set(on);
                    for w in &widgets {
                        w.set_sensitive(!on);
                    }
                    row.set_opacity(if on { 0.55 } else { 1.0 });
                    label.set_label(if on { "Cancelled: not budgeted, and not suggested again unless it charges again" } else { &hint });
                    rc();
                });
                if let Some(m) = &monthly_btn {
                    // one or the other, not both
                    m.bind_property("active", &c, "sensitive").invert_boolean().sync_create().build();
                    c.bind_property("active", m, "sensitive").invert_boolean().sync_create().build();
                }
                second.append(&c);
            }
            // another of the same kind: a second vehicle's insurance, another child's fees
            let more = gtk::Button::builder().icon_name("list-add-symbolic").valign(gtk::Align::Center).css_classes(["flat", "circular"]).tooltip_text("Add another like this (another vehicle, say)").build();
            {
                let (adder, row, title, cur, every) = (adder.clone(), r.clone(), l.name(), l.currency, l.every.clone());
                more.connect_clicked(move |_| {
                    let Some(add) = adder.borrow().clone() else { return };
                    let ev = EVERY[(every.selected() as usize).min(EVERY.len() - 1)].0;
                    let name = gtk::Entry::builder().placeholder_text("What is it? (e.g. Motorcycle insurance)").text(format!("{title} 2")).build();
                    let new = BillLine {
                        id: 0,
                        stem: String::new(),
                        currency: cur,
                        title: String::new(),
                        name: Some(name),
                        amount: gtk::Entry::builder().placeholder_text(cur.symbol()).width_chars(9).max_width_chars(9).hexpand(false).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).tooltip_text("What it costs each time").build(),
                        every: gtk::DropDown::builder().model(&gtk::StringList::new(&EVERY.map(|e| e.1))).valign(gtk::Align::Center).tooltip_text("How often it comes").selected(every_index(ev)).build(),
                        month: gtk::DropDown::builder().model(&gtk::StringList::new(&MONTH_NAMES)).valign(gtk::Align::Center).tooltip_text("The month it's next due").selected(add_months(month_start(today()), 1).month() - 1).build(),
                        due: None,
                        cancelled: Rc::default(),
                        monthly: Rc::default(),
                        asked: false,
                        sub_stem: None,
                        guessed: false,
                        category: None,
                    };
                    add(new, "Rename it to say which (Motorcycle insurance, Wife's SUV insurance)", Some(row.index() + if answers { 2 } else { 1 }));
                });
            }
            r.add_suffix(&more);
            match at {
                Some(i) => bill_list.insert(&r, i),
                None => bill_list.append(&r),
            }
            if answers {
                let row2 = gtk::ListBoxRow::builder().child(&second).activatable(false).selectable(false).build();
                match at {
                    Some(i) => bill_list.insert(&row2, i + 1),
                    None => bill_list.append(&row2),
                }
            }
            bills.borrow_mut().push(l);
            recalc();
        })
    };
    *adder.borrow_mut() = Some(add_bill.clone());
    let months = MONTH_NAMES;
    let line = move |id: i64, stem: &str, cur: Currency, title: &str, typed: bool, amount: Decimal, every: i32, due: Option<NaiveDate>, month: u32| {
        let a = gtk::Entry::builder().placeholder_text(cur.symbol()).width_chars(9).max_width_chars(9).hexpand(false).valign(gtk::Align::Center).input_purpose(gtk::InputPurpose::Number).text(amount_text(amount)).tooltip_text("What it costs each time").build();
        let ev = gtk::DropDown::builder().model(&gtk::StringList::new(&EVERY.map(|e| e.1))).valign(gtk::Align::Center).tooltip_text("How often it comes").build();
        ev.set_selected(every_index(every));
        let mo = gtk::DropDown::builder().model(&gtk::StringList::new(&months)).valign(gtk::Align::Center).tooltip_text("The month it's next due").build();
        mo.set_selected(month.clamp(1, 12) - 1);
        let name = typed.then(|| gtk::Entry::builder().placeholder_text("What is it?").text(title).build());
        BillLine { id, stem: stem.to_string(), currency: cur, title: title.to_string(), name, amount: a, every: ev, month: mo, due, cancelled: Rc::default(), monthly: Rc::default(), asked: false, sub_stem: None, guessed: false, category: None }
    };
    let next_month = add_months(month_start(today()), 1).month();
    // is it really a big bill? ask when the charge is small, or how often it comes is only a guess
    let all_subs = fingraph_core::insights::subscriptions(&s.rows, ccy, &s.fx, today());
    let answered = app.db.kept_big_bills();
    let question = |l: &mut BillLine, name: &str, amount: Decimal, stated: String| -> String {
        let sub = all_subs.iter().find(|x| x.stem == l.stem || fingraph_core::rules::payer_key(&x.stem, "") == l.stem || x.payee.eq_ignore_ascii_case(name));
        l.sub_stem = sub.map(|x| x.stem.clone());
        l.guessed = sub.is_some_and(|x| x.cadence_guessed);
        l.category = s.rows.iter().find(|t| fingraph_core::rules::payer_key(&t.raw_description, &t.memo) == l.stem && !l.stem.is_empty()).map(|t| t.category_id.and_then(|id| s.parents.get(&id).cloned()).or_else(|| t.category_name.clone()).unwrap_or_default());
        l.asked = !l.stem.is_empty() && core::ask_if_monthly(l.guessed, amount, answered.contains(&l.stem));
        match (l.asked, l.guessed) {
            (true, true) => format!("{stated}. Is this a monthly charge? FinGraph hasn't seen enough charges to tell"),
            (true, false) => format!("{stated}. Is this a monthly charge? It's small for a big bill"),
            _ => stated,
        }
    };
    for f in &s.funds {
        let d = f.due(today());
        let mut l = line(f.id, &f.stem, f.currency, &f.name, false, f.amount, f.every_months, Some(d), d.month());
        let hint = question(&mut l, &f.name, s.fx.convert(f.amount, f.currency, ccy).unwrap_or_default(), format!("Already planned · {}", fmt_money(f.amount, f.currency)));
        add_bill(l, &hint, None);
    }
    let ideas: Vec<core::FundIdea> = app.db.big_bill_ideas(&s.rows, ccy, &s.fx, today()).into_iter().filter(|i| !s.funds.iter().any(|f| f.stem == i.stem)).collect();
    for i in &ideas {
        let mut l = line(0, &i.stem, ccy, &i.payee, false, i.amount.round_dp(0), i.every_months, Some(i.next_due), i.next_due.month());
        let hint = question(&mut l, &i.payee, i.amount, format!("Seen in your statements: {} {}", money(i.amount), if i.every_months == 12 { "once a year" } else { "every 3 months" }));
        add_bill(l, &hint, None);
    }
    let taken: Vec<String> = s.funds.iter().map(|f| f.name.to_lowercase()).chain(ideas.iter().map(|i| i.payee.to_lowercase())).collect();
    for (name, hint, every, month) in BIG_BILLS {
        if !taken.contains(&name.to_lowercase()) {
            add_bill(line(0, "", ccy, name, false, Decimal::ZERO, every, None, month.unwrap_or(next_month)), hint, None);
        }
    }
    {
        let add = add_bill.clone();
        add_bill_btn.connect_clicked(move |_| add(line(0, "", ccy, "", true, Decimal::ZERO, 12, None, next_month), "", None));
    }
    for d in &start.deductions {
        add_ded(&d.name, &if d.percent { format!("{}%", d.amount.normalize()) } else { amount_text(d.amount) });
    }
    for (b, n, a) in [(&pension, "Pension", "5%"), (&health, "Health insurance", ""), (&other, "", "")] {
        let add = add_ded.clone();
        b.connect_clicked(move |_| add(n, a));
    }
    for e in [next_pay.upcast_ref::<gtk::Editable>(), gross.upcast_ref(), other_in.upcast_ref(), loans.upcast_ref(), goal.upcast_ref()] {
        let r = recalc.clone();
        e.connect_changed(move |_| r());
    }
    {
        let r = recalc.clone();
        cycle.connect_selected_notify(move |_| r());
    }
    for l in cat_entries.iter() {
        let r = recalc.clone();
        l.entry.connect_changed(move |_| r());
    }
    {
        let (cat_entries, fortnightly, recalc) = (cat_entries.clone(), fortnightly.clone(), recalc.clone());
        fortnight_b.connect_toggled(move |b| {
            if b.is_active() == fortnightly.get() {
                return;
            }
            // convert what's typed, remembering the monthly figure so switching back is exact
            let to_fn = b.is_active();
            for l in cat_entries.iter() {
                let monthly = l.monthly(!to_fn);
                let text = match (to_fn, monthly) {
                    (true, Some(v)) => {
                        let t = amount_text((v * Decimal::from(12) / Decimal::from(26)).round_dp(2));
                        *l.memo.borrow_mut() = Some((t.clone(), amount_text(v)));
                        t
                    }
                    (false, Some(v)) => amount_text(v),
                    (_, None) => String::new(),
                };
                l.entry.set_text(&text);
                if !to_fn {
                    *l.memo.borrow_mut() = None;
                }
            }
            fortnightly.set(to_fn);
            subtitles();
            recalc();
        });
    }
    {
        let (cat_entries, factor, fixed_names) = (cat_entries.clone(), factor.clone(), fixed_names.clone());
        scale.connect_clicked(move |_| {
            let Some(k) = factor.get() else { return };
            for l in cat_entries.iter().filter(|l| !fixed_names.contains(&l.cat.name)) {
                let e = &l.entry;
                if let Some(v) = parse_user(&e.text()) {
                    // down to the nearest 5
                    let five = Decimal::from(5);
                    e.set_text(&amount_text((v.abs() * k / five).floor() * five));
                }
            }
        });
    }
    recalc();

    // fill the budget by the chosen method (needs whole, wants scaled, savings and debt their share)
    let apply = {
        let (read, aside_of, cat_ins, cat_entries, goal, debt, monthly_b) = (read.clone(), aside_of.clone(), cat_ins.clone(), cat_entries.clone(), goal.clone(), debt.clone(), monthly_b.clone());
        Rc::new(move |m: fingraph_core::method::Method| {
            let (p, _) = read();
            let Some(a) = fingraph_core::method::allocate(m, p.method_base(), p.pension_monthly(), aside_of(), p.loan_payments, owed_total, &cat_ins(), &money) else { return };
            // the method's figures are monthly
            monthly_b.set_active(true);
            for (l, (_, b)) in cat_entries.iter().zip(&a.budgets) {
                *l.memo.borrow_mut() = None;
                l.entry.set_text(&amount_text(*b));
            }
            goal.set_text(&amount_text(a.savings));
            debt.set_text(&amount_text(a.debt));
        })
    };
    for c in &method_checks {
        let r = recalc.clone();
        c.connect_toggled(move |c| {
            if c.is_active() {
                r();
            }
        });
    }
    for l in cat_entries.iter() {
        let r = recalc.clone();
        l.need.connect_toggled(move |_| r());
    }
    {
        let r = recalc.clone();
        debt.connect_changed(move |_| r());
    }
    {
        let (apply, method) = (apply.clone(), method.clone());
        refill.connect_clicked(move |_| apply(method.get()));
    }
    {
        let (goal, unassigned) = (goal.clone(), unassigned.clone());
        rest.connect_clicked(move |_| {
            let now = parse_amount(&goal.text()).filter(|x| !x.1).map_or(Decimal::ZERO, |x| x.0);
            goal.set_text(&amount_text((now + unassigned.get()).max(Decimal::ZERO)));
        });
    }
    recalc();

    // ── the frame: Back / Next / Save
    let stack = gtk::Stack::builder().transition_type(gtk::StackTransitionType::SlideLeftRight).vexpand(true).build();
    let pages = [p1, p2, p_y, p_m, p3, p4, p5];
    // the method step and the savings step, by position
    let (method_step, savings_step) = (3usize, 5usize);
    let last = pages.len() - 1;
    for (i, p) in pages.iter().enumerate() {
        stack.add_named(p, Some(&i.to_string()));
    }
    let back = gtk::Button::builder().label("Back").sensitive(false).build();
    let fwd = gtk::Button::builder().label("Next").css_classes(["suggested-action"]).build();
    let step = Rc::new(Cell::new(0usize));
    let header = adw::HeaderBar::new();
    header.pack_start(&back);
    header.pack_end(&fwd);
    let tv = adw::ToolbarView::new();
    tv.add_top_bar(&header);
    tv.set_content(Some(&stack));
    let dlg = adw::Dialog::builder().title("Set Up Your Budget").content_width(720).content_height(720).child(&tv).build();
    let show = {
        let (stack, back, fwd, step, dlg) = (stack.clone(), back.clone(), fwd.clone(), step.clone(), dlg.clone());
        move |i: usize| {
            step.set(i);
            stack.set_visible_child_name(&i.to_string());
            back.set_sensitive(i > 0);
            fwd.set_label(if i == last { "Save Budget" } else { "Next" });
            dlg.set_title(&format!("Set Up Your Budget · {} of {}", i + 1, last + 1));
        }
    };
    show(0);
    {
        let (show, step) = (show.clone(), step.clone());
        back.connect_clicked(move |_| show(step.get().saturating_sub(1)));
    }
    {
        let (w, dlg, s) = (Rc::downgrade(app), dlg.clone(), s.clone());
        fwd.connect_clicked(move |_| {
            let Some(a) = w.upgrade() else { return };
            if step.get() == 0 && !next_pay.text().trim().is_empty() && parse_date(&next_pay.text()).is_none() {
                return a.toast("Type the payday like 31 Oct 2026");
            }
            // leaving the method step: fill the budget by it, once per choice
            if step.get() == method_step && applied.get() != Some(method.get()) {
                apply(method.get());
                applied.set(Some(method.get()));
            }
            if step.get() < last {
                return show(step.get() + 1);
            }
            if method.get() == fingraph_core::method::Method::ZeroBased && unassigned.get().abs() >= Decimal::ONE {
                show(savings_step);
                return a.toast(&format!("Zero-based: {} isn't given a job yet", fmt_money(unassigned.get(), ccy)));
            }
            let (p, budgets) = read();
            let needs = cat_entries.iter().filter(|l| l.need.is_active() != s.needs.contains(&l.cat.name))
                .map(|l| (l.cat.name.clone(), l.need.is_active())).collect();
            let funds = bills.borrow().iter().filter_map(|l| Some(Fund { id: l.id, name: l.name(), amount: l.amount()?, currency: l.currency, every_months: l.every(), next_due: l.next_due(), stem: l.stem.clone() })).collect();
            let deleted_funds = bills.borrow().iter().filter(|l| l.id != 0 && l.amount().is_none()).map(|l| l.id).collect();
            let mut big_bill_choices = Vec::new();
            // what you cancelled, remembered so it's not suggested or counted again
            let mut prefs = a.db.prefs();
            let before = (prefs.cancelled.clone(), prefs.subscription_cadence.clone());
            let day = today().format("%Y-%m-%d").to_string();
            for l in bills.borrow().iter().filter(|l| l.cancelled.get() && !l.stem.is_empty()) {
                prefs.cancelled.insert(l.stem.clone(), day.clone());
            }
            // your answer to "is it monthly?" replaces FinGraph's guess, here and on every later screen
            for l in bills.borrow().iter().filter(|l| !l.cancelled.get() && !l.stem.is_empty()) {
                if l.monthly.get() {
                    big_bill_choices.push((l.stem.clone(), false));
                    if let Some(st) = &l.sub_stem {
                        prefs.subscription_cadence.insert(st.clone(), 30);
                    }
                } else if l.asked && l.amount().is_some() {
                    big_bill_choices.push((l.stem.clone(), true));
                    if let (true, Some(st)) = (l.guessed, &l.sub_stem) {
                        prefs.subscription_cadence.insert(st.clone(), (l.every() * 3044 + 50) as i64 / 100);
                    }
                }
            }
            for (stem, was, b) in sub_marks.iter() {
                match (b.is_active(), *was) {
                    (true, false) => {
                        prefs.cancelled.insert(stem.clone(), day.clone());
                    }
                    (false, true) => {
                        prefs.cancelled.remove(stem);
                    }
                    _ => {}
                }
            }
            let preferences = ((prefs.cancelled.clone(), prefs.subscription_cadence.clone()) != before).then_some(prefs);
            let setup = fingraph_core::budget_setup::BudgetSetup {
                plan: p, needs, budgets: budgets.into_iter().map(|(id, amount)| (id, amount.map(|v| (v, ccy)))).collect(),
                funds, deleted_funds, big_bill_choices, preferences,
            };
            if !a.saved(a.db.save_budget_setup(&s.scope, &setup)) { return; }
            dlg.close();
            done();
            a.toast_action("Budget saved", "Save Budget Plan…", Rc::new(|a: &Rc<App>| crate::planpdf::save_plan(a)));
        });
    }
    dlg.present(Some(&app.win));
}

