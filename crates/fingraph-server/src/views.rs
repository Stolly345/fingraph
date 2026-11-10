//! What the phone's screens show, worked out on the server with the same core calculations the desktop
//! uses, and sent as plain JSON. Amounts are strings (`"123.45"`) so no precision is lost on the way.
use chrono::{Datelike, Local, NaiveDate};
use fingraph_core::budget::{self, Alert};
use fingraph_core::db::{CategoryKind, Db, TxFilter, TxRow};
use fingraph_core::insights::{self, add_months, month_start};
use fingraph_core::money::{Currency, Fx};
use fingraph_core::outlook::{self, BillState};
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize, Clone)]
pub struct Params {
    /// `2026-10`; this month when absent.
    pub month: Option<String>,
    /// One workspace; all of them when absent.
    pub workspace: Option<i64>,
    /// Search words (transactions).
    pub q: Option<String>,
    pub account: Option<i64>,
    pub category: Option<i64>,
    /// 0-based page of 50 (transactions).
    pub page: Option<usize>,
}

#[derive(Debug)]
pub enum ViewError {
    Unknown,
    Failed(String),
}

impl<E: std::fmt::Display> From<E> for ViewError {
    fn from(e: E) -> Self {
        ViewError::Failed(e.to_string())
    }
}

type R = Result<Value, ViewError>;

pub fn render(db: &Db, name: &str, p: &Params) -> R {
    let ctx = Ctx::new(db, p)?;
    match name {
        "summary" => summary(db, &ctx),
        "budget" => budget_view(db, &ctx),
        "transactions" => transactions(db, &ctx, p),
        "bills" => bills(db, &ctx),
        "subscriptions" => subscriptions(&ctx),
        "accounts" => accounts(db, &ctx),
        _ => Err(ViewError::Unknown),
    }
}

fn money(d: Decimal) -> String {
    d.round_dp(2).to_string()
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

fn month_end(m: NaiveDate) -> NaiveDate {
    add_months(month_start(m), 1).pred_opt().unwrap_or(m)
}

/// What every view starts from: the workspaces, the month, the display currency, the rows.
struct Ctx {
    ids: Vec<i64>,
    workspaces: Vec<Value>,
    month: NaiveDate,
    ccy: Currency,
    fx: Fx,
    parents: BTreeMap<i64, String>,
    rows: Vec<TxRow>,
}

impl Ctx {
    fn new(db: &Db, p: &Params) -> Result<Ctx, ViewError> {
        let entities = db.entities()?;
        let ids: Vec<i64> = match p.workspace {
            Some(id) if entities.iter().any(|e| e.id == id) => vec![id],
            _ => entities.iter().map(|e| e.id).collect(),
        };
        let month = p
            .month
            .as_deref()
            .and_then(|m| NaiveDate::parse_from_str(&format!("{m}-01"), "%Y-%m-%d").ok())
            .unwrap_or_else(|| month_start(today()));
        let ccy = db
            .setting("display_currency")
            .and_then(|c| Currency::parse(&c))
            .or_else(|| entities.iter().find(|e| ids.contains(&e.id)).map(|e| e.base_currency))
            .unwrap_or(Currency::USD);
        let cats = db.categories()?;
        let parents = cats.iter().filter_map(|c| Some((c.id, cats.iter().find(|p| Some(p.id) == c.parent_id)?.name.clone()))).collect();
        let rows = db.transactions(&TxFilter { entity_ids: ids.clone(), split_lines: true, ..Default::default() })?;
        // categories the owner leaves out of spending (Preferences) are left out here too
        let prefs = db.prefs();
        let rows = if prefs.excluded_categories.is_empty() {
            rows
        } else {
            let top = |t: &TxRow, parents: &BTreeMap<i64, String>| t.category_id.and_then(|id| parents.get(&id).cloned()).or_else(|| t.category_name.clone());
            rows.into_iter().filter(|t| !(t.amount.is_sign_negative() && top(t, &parents).is_some_and(|c| prefs.excluded_categories.contains(&c)))).collect()
        };
        let workspaces = entities.iter().map(|e| json!({ "id": e.id, "name": e.name, "currency": e.base_currency.code() })).collect();
        Ok(Ctx { ids, workspaces, month, ccy, fx: db.fx().unwrap_or_default(), parents, rows })
    }

    fn head(&self) -> serde_json::Map<String, Value> {
        let mut m = serde_json::Map::new();
        m.insert("month".into(), json!(self.month.format("%Y-%m").to_string()));
        m.insert("month_label".into(), json!(self.month.format("%B %Y").to_string()));
        m.insert("currency".into(), json!(self.ccy.code()));
        m.insert("symbol".into(), json!(self.ccy.symbol()));
        m.insert("workspaces".into(), json!(self.workspaces));
        m.insert("showing".into(), json!(self.ids));
        m
    }

    fn value(&self, t: &TxRow) -> Decimal {
        insights::value(t, self.ccy, &self.fx)
    }
}

/// How far through `m` the statements reach (1 = all of it), as the desktop's budget page works it out.
fn elapsed(db: &Db, ids: &[i64], m: NaiveDate) -> (Decimal, Option<NaiveDate>) {
    let cov = db.statement_coverage(ids).unwrap_or_default();
    let through = cov.common_end().or_else(|| cov.accounts.iter().filter_map(|a| a.verified_end).min());
    let end = month_end(m);
    let t = through.unwrap_or_else(today).min(today());
    let e = if t >= end {
        Decimal::ONE
    } else if t < m {
        Decimal::ZERO
    } else {
        Decimal::from(t.day()) / Decimal::from(end.day())
    };
    (e, through)
}

fn envelopes(db: &Db, ctx: &Ctx) -> Result<Vec<Value>, ViewError> {
    let cats: Vec<(i64, String)> = db.categories()?.into_iter().filter(|c| c.parent_id.is_none() && c.kind == CategoryKind::Expense).map(|c| (c.id, c.name)).collect();
    let scope = budget::scope(&ctx.ids);
    let env = budget::envelopes(db, &scope, &ctx.rows, &cats, ctx.month, ctx.ccy, &ctx.fx, &ctx.parents)?;
    let rolls = db.rollovers(&scope);
    let (done, _) = elapsed(db, &ctx.ids, ctx.month);
    let mut out: Vec<(Decimal, Value)> = cats
        .iter()
        .filter_map(|(id, name)| {
            let e = env.get(id)?;
            if e.budget.is_zero() && e.spent.is_zero() && !rolls.contains_key(id) {
                return None;
            }
            let funded = e.budget + e.moved + e.carried;
            let state = match budget::alert(e.spent, funded, done) {
                Some(Alert::Over) => "over",
                Some(Alert::Near) => "near",
                None if funded.is_zero() => "unbudgeted",
                None => "ok",
            };
            Some((
                e.spent,
                json!({ "id": id, "name": name, "budget": money(funded), "spent": money(e.spent), "left": money(e.available()), "state": state, "rolls_over": rolls.contains_key(id) }),
            ))
        })
        .collect();
    out.sort_by_key(|x| std::cmp::Reverse(x.0));
    Ok(out.into_iter().map(|(_, v)| v).collect())
}

fn budget_view(db: &Db, ctx: &Ctx) -> R {
    let cats = envelopes(db, ctx)?;
    let sum = |k: &str| cats.iter().filter_map(|c| c[k].as_str()?.parse::<Decimal>().ok()).sum::<Decimal>();
    let (done, through) = elapsed(db, &ctx.ids, ctx.month);
    let mut v = ctx.head();
    v.insert("budgeted".into(), json!(money(sum("budget"))));
    v.insert("spent".into(), json!(money(sum("spent"))));
    v.insert("left".into(), json!(money(sum("left"))));
    v.insert("elapsed".into(), json!(done.round_dp(3).to_string()));
    v.insert("data_through".into(), json!(through.map(|d| d.to_string())));
    v.insert("categories".into(), json!(cats));
    Ok(Value::Object(v))
}

fn latest_balances(db: &Db, ctx: &Ctx) -> Result<Vec<Value>, ViewError> {
    let docs = db.documents_all(&ctx.ids)?;
    let accounts = db.accounts()?;
    let by_account = db.transactions(&TxFilter { entity_ids: ctx.ids.clone(), by_account: true, ..Default::default() })?;
    let latest = insights::latest_balances(&by_account, &docs);
    Ok(accounts
        .iter()
        .filter(|a| !a.hidden && ctx.ids.contains(&a.entity_id))
        .map(|a| {
            let (as_of, bal) = latest.get(&a.id).map(|(d, b)| (Some(d.to_string()), Some(money(*b)))).unwrap_or((None, None));
            json!({ "id": a.id, "name": a.name, "bank": a.bank_short, "type": a.account_type.as_str(), "currency": a.currency.code(), "balance": bal, "as_of": as_of })
        })
        .collect())
}

fn accounts(db: &Db, ctx: &Ctx) -> R {
    let mut v = ctx.head();
    v.insert("accounts".into(), json!(latest_balances(db, ctx)?));
    Ok(Value::Object(v))
}

fn summary(db: &Db, ctx: &Ctx) -> R {
    let (m0, m1) = (ctx.month, month_end(ctx.month));
    let (income, spending) = insights::flows(&ctx.rows, m0, m1, ctx.ccy, &ctx.fx);
    let cats = envelopes(db, ctx)?;
    let budgeted: Decimal = cats.iter().filter_map(|c| c["budget"].as_str()?.parse::<Decimal>().ok()).sum();
    let spent: Decimal = cats.iter().filter_map(|c| c["spent"].as_str()?.parse::<Decimal>().ok()).sum();
    let (done, through) = elapsed(db, &ctx.ids, ctx.month);
    let cov = db.statement_coverage(&ctx.ids).unwrap_or_default();
    let mut v = ctx.head();
    v.insert("income".into(), json!(money(income)));
    v.insert("spending".into(), json!(money(spending.abs())));
    v.insert("budgeted".into(), json!(money(budgeted)));
    v.insert("spent".into(), json!(money(spent)));
    v.insert("left".into(), json!(money(budgeted - spent)));
    v.insert("elapsed".into(), json!(done.round_dp(3).to_string()));
    v.insert("data_through".into(), json!(through.map(|d| d.to_string())));
    v.insert("coverage".into(), json!(cov.label()));
    v.insert("alerts".into(), json!(cats.iter().filter(|c| c["state"] == "over" || c["state"] == "near").cloned().collect::<Vec<_>>()));
    v.insert("accounts".into(), json!(latest_balances(db, ctx)?));
    v.insert("next_steps".into(), json!(next_steps(db, ctx)?));
    Ok(Value::Object(v))
}

/// The few things worth doing next, most useful first. The desktop does them; the phone shows them.
fn next_steps(db: &Db, ctx: &Ctx) -> Result<Vec<Value>, ViewError> {
    let mut steps = Vec::new();
    let held: Vec<_> = db.documents_all(&ctx.ids)?.into_iter().filter(|d| !d.review.is_empty()).collect();
    if !held.is_empty() {
        let n = held.len();
        steps.push(json!({ "kind": "review_statements", "title": format!("{n} statement section{} waiting for review", if n == 1 { "" } else { "s" }),
            "detail": "They don't count until they're approved on the desktop (Statements → To Review)." }));
    }
    let uncategorized = ctx.rows.iter().filter(|t| t.category_id.is_none() && !t.is_transfer()).count();
    if uncategorized > 0 {
        steps.push(json!({ "kind": "categorize", "title": format!("{uncategorized} transaction{} to categorize", if uncategorized == 1 { "" } else { "s" }),
            "detail": "Answer them on the desktop (Categorize), and FinGraph learns the rules." }));
    }
    let docs = db.documents_all(&ctx.ids)?;
    let covered = outlook::covered(&docs);
    let missed: Vec<_> = outlook::bills_for(&ctx.rows, &covered, month_start(today()), ctx.ccy, &ctx.fx).into_iter().filter(|b| b.state == BillState::Missed).collect();
    for b in missed.iter().take(3) {
        steps.push(json!({ "kind": "missed_bill", "title": format!("{} didn't charge this month", b.payee), "detail": format!("Usually about {}{} from {}, due {}.", ctx.ccy.symbol(), money(b.amount), b.account, b.due.format("%-d %b")) }));
    }
    // statements due: an account whose latest statement ended more than 40 days ago
    let accounts = db.accounts()?;
    for a in accounts.iter().filter(|a| !a.hidden && ctx.ids.contains(&a.entity_id)) {
        if let Some(end) = covered.get(&a.id)
            && (today() - *end).num_days() > 40
        {
            steps.push(json!({ "kind": "statement_due", "title": format!("A new statement for {}", a.name), "detail": format!("The latest one ends {}. Drop the new PDF in the server's inbox folder.", end.format("%-d %b %Y")) }));
        }
    }
    let forecasts = outlook::forecast(&db.transactions(&TxFilter { entity_ids: ctx.ids.clone(), by_account: true, ..Default::default() })?, &docs, today() + chrono::Duration::days(30));
    for f in &forecasts {
        let min = db.min_balance(f.account_id);
        if let Some(s) = f.first_below(min, today(), today() + chrono::Duration::days(30)) {
            steps.push(json!({ "kind": "low_balance", "title": format!("{} may run low", f.account), "detail": s.label(f.currency) }));
        }
    }
    Ok(steps)
}

fn tx_json(t: &TxRow, ctx: &Ctx) -> Value {
    json!({
        "id": t.id, "date": t.posted.to_string(), "payee": if t.clean_payee.is_empty() { &t.raw_description } else { &t.clean_payee },
        "description": t.raw_description, "memo": t.memo, "amount": money(t.amount), "currency": t.currency.code(),
        "value": money(ctx.value(t)), "category": t.category_name, "category_id": t.category_id,
        "parent_category": t.category_id.and_then(|id| ctx.parents.get(&id).cloned()),
        "account": t.account_name, "account_id": t.account_id, "transfer": t.is_transfer(),
        "subscription": t.is_subscription, "recurring": t.is_recurring, "review": t.review.as_str(), "tag": t.tag,
        "attachments": t.attachments,
    })
}

const PAGE: usize = 50;

fn transactions(db: &Db, ctx: &Ctx, p: &Params) -> R {
    let search = p.q.clone().unwrap_or_default();
    // a search looks through everything; otherwise one month
    let (from, to) = if search.trim().is_empty() && p.account.is_none() && p.category.is_none() { (Some(ctx.month), Some(month_end(ctx.month))) } else { (None, None) };
    let mut rows = db.transactions(&TxFilter { entity_ids: ctx.ids.clone(), from, to, account_id: p.account, category_id: p.category, search: search.trim().to_string(), ..Default::default() })?;
    rows.sort_by(|a, b| b.posted.cmp(&a.posted).then(b.id.cmp(&a.id)));
    let page = p.page.unwrap_or(0);
    let total = rows.len();
    let items: Vec<Value> = rows.iter().skip(page * PAGE).take(PAGE).map(|t| tx_json(t, ctx)).collect();
    let mut v = ctx.head();
    v.insert("total".into(), json!(total));
    v.insert("page".into(), json!(page));
    v.insert("more".into(), json!((page + 1) * PAGE < total));
    v.insert("items".into(), json!(items));
    Ok(Value::Object(v))
}

fn bills(db: &Db, ctx: &Ctx) -> R {
    let docs = db.documents_all(&ctx.ids)?;
    let covered = outlook::covered(&docs);
    let list = outlook::bills_for(&ctx.rows, &covered, ctx.month, ctx.ccy, &ctx.fx);
    let items: Vec<Value> = list
        .iter()
        .map(|b| {
            let (state, paid) = match &b.state {
                BillState::Paid(d) => ("paid", d.iter().map(|d| d.to_string()).collect()),
                BillState::Missed => ("missed", Vec::new()),
                BillState::NotYetSeen => ("not_yet_seen", Vec::new()),
                BillState::New => ("new", Vec::new()),
            };
            json!({ "payee": b.payee, "account": b.account, "amount": money(b.amount), "cadence_days": b.cadence, "due": b.due.to_string(), "state": state, "paid_on": paid })
        })
        .collect();
    let total: Decimal = list.iter().map(|b| b.amount).sum();
    let mut v = ctx.head();
    v.insert("total".into(), json!(money(total)));
    v.insert("items".into(), json!(items));
    Ok(Value::Object(v))
}

fn cadence_label(days: i64) -> &'static str {
    match days {
        0..=10 => "weekly",
        11..=20 => "every two weeks",
        21..=45 => "monthly",
        46..=120 => "quarterly",
        _ => "yearly",
    }
}

/// Subscriptions: payments flagged as subscriptions, one line per service, with the latest price, the
/// rhythm, the next expected charge and what it costs in a year.
fn subscriptions(ctx: &Ctx) -> R {
    let upto = today();
    let flagged: Vec<TxRow> = ctx.rows.iter().filter(|t| t.is_subscription && t.amount.is_sign_negative()).cloned().collect();
    let regular = outlook::regulars(&flagged, upto);
    let since = add_months(month_start(upto), -13);
    let mut items: Vec<(Decimal, Value)> = regular
        .iter()
        .filter(|r| r.last >= since)
        .map(|r| {
            let amount = ctx.fx.convert(r.amount.abs(), r.currency, ctx.ccy).unwrap_or(r.amount.abs());
            // rounded per line, so the lines add up to the total shown
            let yearly = (amount * Decimal::from(365) / Decimal::from(r.cadence.max(1))).round_dp(2);
            let next = outlook::nth_due(r.last, r.cadence, 1);
            // stopped: no charge for twice its rhythm
            let active = (upto - r.last).num_days() <= r.cadence * 2;
            (
                if active { yearly } else { Decimal::ZERO },
                json!({ "payee": r.payee, "account": r.account, "amount": money(amount), "currency": ctx.ccy.code(), "cadence": cadence_label(r.cadence),
                    "last": r.last.to_string(), "next": if active { Some(next.to_string()) } else { None }, "yearly": money(yearly), "active": active }),
            )
        })
        .collect();
    items.sort_by_key(|x| std::cmp::Reverse(x.0));
    let yearly: Decimal = items.iter().map(|(y, _)| *y).sum();
    let mut v = ctx.head();
    v.insert("yearly".into(), json!(money(yearly)));
    v.insert("monthly".into(), json!(money(yearly / Decimal::from(12))));
    v.insert("items".into(), json!(items.into_iter().map(|(_, v)| v).collect::<Vec<_>>()));
    Ok(Value::Object(v))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.load_demo().unwrap();
        db
    }

    #[test]
    fn every_view_renders_from_the_demo_ledger() {
        let db = demo();
        let p = Params { month: Some("2026-09".into()), ..Default::default() };
        for name in ["summary", "budget", "transactions", "bills", "subscriptions", "accounts"] {
            let v = render(&db, name, &p).unwrap_or_else(|_| panic!("{name} failed"));
            assert_eq!(v["month"], "2026-09", "{name}");
        }
        assert!(matches!(render(&db, "nope", &p), Err(ViewError::Unknown)));
    }

    #[test]
    fn transactions_page_and_search() {
        let db = demo();
        let p = Params { month: Some("2026-09".into()), ..Default::default() };
        let v = render(&db, "transactions", &p).unwrap();
        assert!(v["total"].as_u64().unwrap() > 20);
        assert!(v["items"].as_array().unwrap().len() <= PAGE);
        let first = &v["items"][0];
        assert!(first["date"].as_str().unwrap().starts_with("2026-09"));
        let s = render(&db, "transactions", &Params { q: Some("netflix".into()), ..Default::default() }).unwrap();
        let items = s["items"].as_array().unwrap();
        assert!(!items.is_empty());
        assert!(items.iter().all(|t| t["description"].as_str().unwrap().to_lowercase().contains("netflix") || t["payee"].as_str().unwrap().to_lowercase().contains("netflix")));
    }

    #[test]
    fn subscriptions_add_up() {
        let db = demo();
        let v = render(&db, "subscriptions", &Params::default()).unwrap();
        let items = v["items"].as_array().unwrap();
        let sum: Decimal = items.iter().filter(|i| i["active"] == true).map(|i| i["yearly"].as_str().unwrap().parse::<Decimal>().unwrap()).sum();
        assert_eq!(money(sum), v["yearly"].as_str().unwrap());
    }

    #[test]
    fn a_workspace_can_be_picked() {
        let db = demo();
        let ws = db.entities().unwrap();
        let one = render(&db, "accounts", &Params { workspace: Some(ws[0].id), ..Default::default() }).unwrap();
        let all = render(&db, "accounts", &Params::default()).unwrap();
        assert!(one["accounts"].as_array().unwrap().len() < all["accounts"].as_array().unwrap().len());
        assert_eq!(one["showing"], json!([ws[0].id]));
    }
}
