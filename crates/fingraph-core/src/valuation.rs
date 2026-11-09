//! Preflight every monetary source before presenting a combined financial view.
use crate::{Currency, Result};
use crate::db::Db;

impl Db {
    pub fn missing_conversion_rates(&self, entity_ids: &[i64], display: Currency) -> Result<Vec<Currency>> {
        let fx = self.fx()?;
        let scope = crate::budget::scope(entity_ids);
        let mut currencies: Vec<_> = self.accounts()?.into_iter()
            .filter(|a| entity_ids.is_empty() || entity_ids.contains(&a.entity_id)).map(|a| a.currency).collect();
        // A business can include split expenses paid from a personal account in another
        // currency. Audit the rows that actually count here, not just owned accounts.
        let rows = self.transactions(&crate::db::TxFilter { entity_ids: entity_ids.to_vec(), split_lines: true, ..Default::default() })?;
        currencies.extend(crate::insights::missing_rates(&rows, display, &fx));
        currencies.extend(self.manual_items(entity_ids)?.iter().map(|i| i.currency));
        currencies.extend(self.goals(&scope)?.iter().map(|i| i.currency));
        if let Some(c) = self.plan(&scope).and_then(|p| p.currency) { currencies.push(c); }
        let mut st = self.conn.prepare("SELECT currency FROM budget_history WHERE scope = ?1
            UNION SELECT currency FROM budget_moves WHERE scope = ?1
            UNION SELECT currency FROM sinking_funds WHERE scope = ?1")?;
        let codes = st.query_map([&scope], |r| r.get::<_, String>(0))?.collect::<rusqlite::Result<Vec<_>>>()?;
        currencies.extend(codes.iter().filter_map(|c| Currency::parse(c)));
        currencies.retain(|c| fx.rate(*c, display).is_none());
        if !currencies.is_empty() && !fx.knows(display) { currencies.push(display); }
        currencies.sort(); currencies.dedup();
        Ok(currencies)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cross_workspace_expenses_require_their_original_currency_rate() {
        let db = Db::open_in_memory().unwrap(); db.load_demo().unwrap();
        let personal = db.entity_by_name("Personal").unwrap().unwrap();
        let business = db.entity_by_name("Business — Property").unwrap().unwrap();
        let tx = db.transactions(&crate::db::TxFilter { entity_ids: vec![personal], ..Default::default() }).unwrap().remove(0);
        db.conn.execute("UPDATE transactions SET currency='GBP', business_entity_id=?1 WHERE id=?2", rusqlite::params![business, tx.id]).unwrap();
        assert_eq!(db.missing_conversion_rates(&[business], Currency::KYD).unwrap(), vec![Currency::parse("GBP").unwrap()]);
    }

    #[test]
    fn missing_rates_in_planned_bills_are_detected_even_without_transactions() {
        let db = Db::open_in_memory().unwrap();
        let gbp = Currency::parse("GBP").unwrap();
        let f = crate::budget::Fund { id: 0, name: "Insurance".into(), amount: 1200.into(),
            currency: gbp, every_months: 12, next_due: "2026-12-01".parse().unwrap(), stem: String::new() };
        db.save_fund("", &f).unwrap();
        assert_eq!(db.missing_conversion_rates(&[], Currency::KYD).unwrap(), vec![gbp]);
        assert_eq!(db.floating_currencies().unwrap(), vec![gbp], "a planned currency must appear in Money preferences");
        let today = chrono::Local::now().date_naive();
        assert!(db.rates_needed(today).unwrap().iter().any(|(codes, _, _)| codes.contains(&gbp)), "the Update Rates action must be able to resolve a planned currency");
        assert!(db.missing_conversion_rates(&[], gbp).unwrap().is_empty());
    }
}
