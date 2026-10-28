//! Simulated statements for all five Cayman banks through the full 5-pass scanner.

use chrono::NaiveDate;
use fingraph_core::demo::{self, Bank, DemoTx, StatementSpec};
use fingraph_core::parser::{MultiPassScanner, ScanResult, profiles};
use fingraph_core::{AccountType, Currency, Decimal, Reconciliation};

fn d(y: i32, m: u32, day: u32) -> NaiveDate {
    NaiveDate::from_ymd_opt(y, m, day).unwrap()
}
fn dec(s: &str) -> Decimal {
    s.parse().unwrap()
}
fn scan(specs: &[StatementSpec]) -> ScanResult {
    let p = profiles::builtins();
    MultiPassScanner::new(&p).scan_tokens(demo::render(specs)).unwrap()
}
/// Every section reconciled, and every transaction exactly as generated (date, ledger amount).
fn assert_exact(r: &ScanResult, specs: &[StatementSpec]) {
    assert_eq!(r.sections.len(), specs.len(), "sections");
    for (s, spec) in r.sections.iter().zip(specs) {
        assert_eq!(s.reconciliation, Reconciliation::Reconciled, "{:?} {:?}\n{:#?}", spec.bank, spec.account_type, s.report);
        assert_eq!(s.currency, spec.currency);
        assert_eq!(s.account_type, spec.account_type);
        assert_eq!(s.account_last4.as_deref(), Some(spec.last4.as_str()));
        let got: Vec<(NaiveDate, Decimal)> = s.transactions.iter().map(|t| (t.effective.unwrap_or(t.posted), t.amount)).collect();
        let want: Vec<(NaiveDate, Decimal)> = spec.txs.iter().map(|t| (t.date, t.amount)).collect();
        assert_eq!(got, want);
    }
}

#[test]
fn butterfield_chequing_ddmmyyyy_with_fx_memo() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 3), "SALARY ISLAND AIR", dec("2720.64")),
        DemoTx::new(d(2026, 8, 5), "POS FOSTER'S FOOD FAIR", dec("-84.12")),
        DemoTx::new(d(2026, 8, 7), "VISA PURCHASE AMAZON.COM", dec("-121.80")).memo("USD 145.00 @ 0.84000"),
        DemoTx::new(d(2026, 8, 12), "CUC WEB PMT", dec("-231.40")),
    ];
    let spec = StatementSpec::new(Bank::Butterfield, AccountType::Checking, Currency::KYD, "4821", dec("1500.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    let r = scan(std::slice::from_ref(&spec));
    assert_eq!(r.profile_slug.as_deref(), Some("butterfield_ky"));
    assert_exact(&r, &[spec]);
    let amazon = &r.sections[0].transactions[2];
    assert_eq!(amazon.memo, "USD 145.00 @ 0.84000");
    assert_eq!(amazon.fx_original, Some((Currency::USD, dec("145.00"))));
    assert!(r.sections[0].report.segments.iter().all(|s| s.ok) && r.sections[0].report.segments.len() == 4);
}

#[test]
fn butterfield_card_cr_suffix_and_posting_date() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 2), "HBO MAX HBOMAX.COM", dec("-15.99")),
        DemoTx::new(d(2026, 8, 9), "PAYMENT RECEIVED - THANK YOU", dec("500.00")),
        DemoTx::new(d(2026, 8, 14), "CRACKED CONCH WEST BAY", dec("-96.00")).memo("KYD 80.00 @ 1.2000"),
    ];
    let spec = StatementSpec::new(Bank::Butterfield, AccountType::CreditCard, Currency::USD, "7710", dec("612.40"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    let r = scan(std::slice::from_ref(&spec));
    assert_exact(&r, std::slice::from_ref(&spec));
    let t = &r.sections[0].transactions;
    assert_eq!(t[0].posted, d(2026, 8, 3), "ledger date is the posting date");
    assert_eq!(t[1].stmt_amount, dec("-500.00"), "CR lowers what's owed");
    assert_eq!(t[2].fx_original, Some((Currency::KYD, dec("80.00"))));
    assert_eq!(r.sections[0].closing, Some(dec("612.40") + dec("15.99") - dec("500.00") + dec("96.00")));
}

#[test]
fn scotiabank_dateless_rows_roll_over_december_to_january() {
    let txs = vec![
        DemoTx::new(d(2025, 12, 18), "POS PURCHASE", dec("-62.10")).memo("KIRK HOME CENTRE 0021"),
        DemoTx::new(d(2025, 12, 31), "ELECTRONIC CREDIT", dec("2720.64")).memo("ISLAND AIR DIR PAY CR SALARY"),
        DemoTx::new(d(2026, 1, 2), "CUC WEB PMT", dec("-210.00")).memo("CARIBBEAN UTILITIES CO"),
        DemoTx::new(d(2026, 1, 15), "SERVICE CHARGE", dec("-13.00")),
    ];
    let spec = StatementSpec::new(Bank::Scotiabank, AccountType::Checking, Currency::KYD, "2045", dec("800.00"), (d(2025, 12, 16), d(2026, 1, 15)), txs);
    let r = scan(std::slice::from_ref(&spec));
    assert_eq!(r.profile_slug.as_deref(), Some("scotiabank_ky"));
    assert_eq!(r.period, Some((d(2025, 12, 16), d(2026, 1, 15))));
    assert_exact(&r, &[spec]);
    assert_eq!(r.sections[0].transactions[0].memo, "KIRK HOME CENTRE 0021");
}

#[test]
fn scotiabank_card_two_line_fx_memo() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 20), "MARLEY JERK CENTRE GEORGE TOWN", dec("-18.36")).memo("KYD 15.00 @ 1.2240"),
        DemoTx::new(d(2026, 8, 21), "INTERNET - CARD PAYMENT", dec("1000.00")),
        DemoTx::new(d(2026, 9, 2), "C.U.C GEORGE TOWN", dec("-283.55")),
    ];
    let spec = StatementSpec::new(Bank::Scotiabank, AccountType::CreditCard, Currency::USD, "2197", dec("1250.00"), (d(2026, 8, 17), d(2026, 9, 15)), txs);
    let r = scan(std::slice::from_ref(&spec));
    assert_exact(&r, std::slice::from_ref(&spec));
    assert_eq!(r.sections[0].transactions[0].fx_original, Some((Currency::KYD, dec("15.00"))));
}

#[test]
fn cnb_dr_cr_balances_kyd_and_usd() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 1), "RENT RECEIVED UNIT A", dec("2400.00")),
        DemoTx::new(d(2026, 8, 6), "A.L. THOMPSON'S", dec("-2600.00")),
        DemoTx::new(d(2026, 8, 9), "MONTHLY ACCOUNT FEE", dec("-15.00")),
        DemoTx::new(d(2026, 8, 20), "RENT RECEIVED UNIT B", dec("2150.00")),
    ];
    // goes overdrawn (DR) after the second row
    let kyd = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::KYD, "6120", dec("100.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs.clone());
    let r = scan(std::slice::from_ref(&kyd));
    assert_eq!(r.profile_slug.as_deref(), Some("cnb_ky"));
    assert_exact(&r, std::slice::from_ref(&kyd));
    assert_eq!(r.sections[0].transactions[1].balance, Some(dec("-100.00")));
    let usd = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::USD, "6121", dec("3100.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    assert_exact(&scan(std::slice::from_ref(&usd)), &[usd]);
}

#[test]
fn rbc_rows_inherit_the_date_above_and_sparse_balances_verify() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 3), "TRANSFER FROM SCOTIABANK", dec("500.00")),
        DemoTx::new(d(2026, 8, 3), "BRANCH DEPOSIT", dec("40.00")),
        DemoTx::new(d(2026, 8, 3), "BILL PAYMENT HOME GAS", dec("-88.10")),
        DemoTx::new(d(2026, 8, 30), "INTEREST PAID", dec("2.15")),
        DemoTx::new(d(2026, 8, 30), "MONTHLY FEE", dec("-5.00")),
    ];
    let spec = StatementSpec::new(Bank::Rbc, AccountType::Savings, Currency::KYD, "3390", dec("2500.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    let r = scan(std::slice::from_ref(&spec));
    assert_eq!(r.profile_slug.as_deref(), Some("rbc_ky"));
    assert_exact(&r, &[spec]);
    assert_eq!(r.diagnostics.pass3.inherited_dates, 3);
    // balances printed only on each day's last row: two stretches, plus the closing row
    assert!(r.sections[0].report.segments.iter().all(|s| s.ok));
}

#[test]
fn cicsa_share_and_loan_sections() {
    let shares = StatementSpec::new(Bank::Cicsa, AccountType::ShareAccount, Currency::KYD, "1002", dec("6200.00"), (d(2025, 12, 1), d(2025, 12, 31)), vec![
        DemoTx::new(d(2025, 12, 15), "PAYROLL DEDUCTION SHARES", dec("300.00")).memo("PAYROLL SPLIT ISLAND AIR"),
        DemoTx::new(d(2025, 12, 20), "DIVIDEND POSTING 2025", dec("279.00")),
    ]);
    let loan = StatementSpec::new(Bank::Cicsa, AccountType::Loan, Currency::KYD, "1003", dec("18450.00"), (d(2025, 12, 1), d(2025, 12, 31)), vec![
        DemoTx::new(d(2025, 12, 3), "LOAN PAYMENT - THANK YOU", dec("450.00")),
        DemoTx::new(d(2025, 12, 29), "LOAN INTEREST", dec("-138.38")),
    ]);
    let r = scan(&[shares.clone(), loan.clone()]);
    assert_eq!(r.profile_slug.as_deref(), Some("cicsa_cu_ky"));
    assert_exact(&r, &[shares, loan]);
    assert_eq!(r.sections[1].closing, Some(dec("18138.38")), "payments lower the loan balance");
}

#[test]
fn a_page_break_split_row_is_joined_across_the_header() {
    let mut txs: Vec<DemoTx> = (1..=60).map(|i| DemoTx::new(d(2026, 8, 1 + (i % 28)), &format!("PURCHASE {i}"), Decimal::new(-(1000 + i as i64 * 37), 2))).collect();
    txs.sort_by_key(|t| t.date);
    let mut spec = StatementSpec::new(Bank::Butterfield, AccountType::Checking, Currency::KYD, "4821", dec("9000.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    spec.split_row = Some(41);
    let r = scan(std::slice::from_ref(&spec));
    let s = &r.sections[0];
    // the dated half at the foot of one page waits for its amount under the next page's header (the
    // CNB rule), so Pass 3 joins it and Pass 5 has nothing left to recover
    assert_eq!(s.report.rescans, 0, "{:#?}", s.report);
    assert!(!s.transactions[41].recovered);
    assert_eq!(s.transactions[41].description, spec.txs[41].description);
    assert_exact(&r, &[spec]);
}

#[test]
fn pass5_recovers_an_amount_that_drifted_out_of_its_column() {
    let txs = vec![
        DemoTx::new(d(2026, 8, 3), "SALARY", dec("2720.64")),
        DemoTx::new(d(2026, 8, 5), "KIRK MARKET", dec("-84.12")),
        DemoTx::new(d(2026, 8, 9), "CUC WEB PMT", dec("-231.40")),
    ];
    let mut spec = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::KYD, "6120", dec("500.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    spec.drift_row = Some(1);
    let r = scan(std::slice::from_ref(&spec));
    let s = &r.sections[0];
    assert_eq!(s.report.recovered.len(), 1, "{:#?}", s.report);
    assert_exact(&r, &[spec]);
}

#[test]
fn unsigned_single_column_signs_are_healed_from_the_balance() {
    // a deposit statement whose one amount column prints no signs: Pass 4 decides from Bal[i] - Bal[i-1]
    let txs = vec![DemoTx::new(d(2026, 8, 3), "SALARY", dec("1000.00")), DemoTx::new(d(2026, 8, 5), "RENT", dec("-400.00"))];
    let spec = StatementSpec::new(Bank::Cnb, AccountType::Checking, Currency::KYD, "6120", dec("100.00"), (d(2026, 8, 1), d(2026, 8, 31)), txs);
    let mut tokens = demo::render(std::slice::from_ref(&spec));
    // collapse Debits/Credits into one unsigned "Amount" column
    for t in &mut tokens {
        if t.text == "Debits" {
            t.text = "Amount".into();
        }
        if t.text == "Credits" {
            t.text = "Ref.".into();
        }
        if t.page == 1 && t.y0 > 140.0 && (t.x1 - 485.0).abs() < 1.0 && t.text.contains('.') {
            let w = t.x1 - t.x0;
            t.x1 = 405.0;
            t.x0 = 405.0 - w;
        }
    }
    let p = profiles::builtins();
    let r = MultiPassScanner::new(&p).scan_tokens(tokens).unwrap();
    let s = &r.sections[0];
    assert_eq!(s.reconciliation, Reconciliation::Reconciled, "{:#?}", s.report);
    assert_eq!(s.transactions.iter().map(|t| t.amount).collect::<Vec<_>>(), vec![dec("1000.00"), dec("-400.00")]);
    assert_eq!(s.report.sign_heals.len(), 1);
}

#[test]
fn a_whole_demo_year_reconciles() {
    for doc in demo::demo_documents() {
        let p = profiles::builtins();
        let r = MultiPassScanner::new(&p).scan_tokens(doc.tokens.clone()).unwrap();
        if doc.specs[0].garble_row.is_some() {
            // unreadable amount: an honest discrepancy, with the line waiting in the inspector
            let s = &r.sections[0];
            let want = doc.specs[0].txs[0].amount;
            assert_eq!(s.reconciliation, Reconciliation::Discrepancy(want), "{:#?}", s.report);
            assert!(s.report.skipped.iter().any(|l| l.text.contains("1,") && l.date == Some(doc.specs[0].txs[0].date)), "{:#?}", s.report.skipped);
            continue;
        }
        assert_exact(&r, &doc.specs);
    }
}

/// Your own statements, if present (never copied into the repo): `cargo test -- --ignored real`.
#[test]
#[ignore]
fn real_statements_reconcile() {
    let dir = fingraph_core::data_dir().join("documents");
    let p = profiles::builtins();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().extension().is_some_and(|x| x == "pdf") && e.file_name().to_string_lossy().starts_with("statement_") {
            let r = MultiPassScanner::new(&p).scan_pdf(&e.path()).unwrap();
            assert!(r.is_reconciled(), "{}", e.path().display());
        }
    }
}

/// Cayman National's real layout (from a redacted Feb 2026 statement; names and amounts here are made up):
/// a typewriter font, `DD/MM` dates, one Amount column with a trailing minus for money out, the date on a
/// description's first line and the amount + balance on its last, a row split by the page break, and the
/// summary amounts on an `as of` line under their labels.
#[test]
fn cnb_real_layout_multiline_rows_and_page_split() {
    use fingraph_core::parser::Token;
    const CW: f64 = 4.8; // Courier at 8pt
    let mut toks = Vec::new();
    let mut put = |page: u32, y: f64, x: f64, text: &str, gap: f64| {
        let mut x = x;
        for w in text.split_whitespace() {
            let wd = w.chars().count() as f64 * CW;
            toks.push(Token::new(page, x, y, x + wd, y + 8.0, w));
            x += wd + gap * CW;
        }
    };
    let (date, desc, amt, bal) = (60.0, 160.0, 425.0, 520.0);
    let right = |text: &str, edge: f64| edge - text.chars().count() as f64 * CW;
    put(1, 40.0, 60.0, "CAYMAN NATIONAL", 1.0);
    put(1, 40.0, 390.0, "Account Statement", 1.0);
    put(1, 60.0, 390.0, "Cayman National Bank Ltd.", 1.0);
    put(1, 72.0, 390.0, "Tel +1 345 949 4655 | caymannational.com", 1.0);
    put(1, 180.0, 300.0, "PERIODIC STATEMENT", 1.0);
    put(1, 192.0, 300.0, "Date: 31/03/2026", 1.0);
    put(1, 204.0, 300.0, "Period: 01/03/2026 to 31/03/2026", 1.0);
    put(1, 214.0, 50.0, "CUSTOMER COPY", 1.0);
    put(1, 400.0, 95.0, "ACCOUNT #: SV - 0001234", 1.0);
    put(1, 420.0, 78.0, "Currency: KYD", 1.0);
    put(1, 500.0, 64.0, "Beginning Balance", 1.0);
    put(1, 514.0, 64.0, "as of 01/03/26", 1.0);
    put(1, 514.0, right("1,000.00", 288.0), "1,000.00", 1.0);
    put(1, 514.0, 335.0, "Interest Paid YTD", 1.0);
    put(1, 514.0, right("0.00", 518.0), "0.00", 1.0);
    // the typewriter spacing splits a label into several cells
    put(1, 526.0, 76.0, "Deposits & Other Cred", 2.0);
    put(1, 526.0, right("1,500.00", 288.0), "1,500.00", 1.0);
    put(1, 526.0, 335.0, "Interest Earned", 1.0);
    put(1, 526.0, right("0.01", 518.0), "0.01", 1.0);
    put(1, 538.0, 76.0, "Cheques & Other Debit", 2.0);
    put(1, 538.0, right("236.50", 288.0), "236.50", 1.0);
    put(1, 538.0, 335.0, "Average Balance", 1.0);
    put(1, 538.0, right("1,100.00", 518.0), "1,100.00", 1.0);
    put(1, 556.0, 64.0, "Ending Balance", 1.0);
    put(1, 570.0, 64.0, "as of 31/03/26", 1.0);
    put(1, 570.0, right("2,263.50", 288.0), "2,263.50", 1.0);
    put(1, 570.0, 325.0, "Annual Percentage", 1.0);
    put(1, 600.0, 57.0, "Transaction Information", 1.0);
    for (page, hy) in [(1, 620.0), (2, 232.0)] {
        put(page, hy, date, "Date", 1.0);
        put(page, hy, 95.0, "Cheque #", 1.0);
        put(page, hy, desc, "Description", 1.0);
        put(page, hy, 372.0, "Amount", 1.0);
        put(page, hy, 472.0, "Balance", 1.0);
    }
    // (page, date, description, amount + balance): the amount sits on a description's last line, and
    // the numbers line up with the minus hanging to their right
    let lines = [
        (1, "02/03", "Govt Stamp Duty Fees", Some(("10.75 -", "989.25"))),
        (1, "03/03", "POS Purchase SAMPLE GROCERY", None),
        (1, "", "GEORGE TOWN KY", Some(("45.50 -", "943.75"))),
        (1, "03/03", "POS Transaction Fee", Some(("0.25 -", "943.50"))),
        (1, "05/03", "EXAMPLE EMPLOYER LTD", None),
        (1, "", "Dir Pay Mar", None),
        (1, "", "26 Salary", Some(("1,500.00", "2,443.50"))),
        (1, "06/03", "POS Purchase", None),
        (2, "", "EXAMPLE STORE GRAND", Some(("180.00 -", "2,263.50"))),
    ];
    let (mut y, mut page) = (620.0, 1);
    for (pg, dt, text, money) in lines {
        if pg != page {
            put(1, 760.0, 280.0, "Page 1", 1.0);
            put(2, 40.0, 60.0, "CAYMAN NATIONAL", 1.0);
            put(2, 196.0, 345.0, "31/03/2026", 1.0);
            put(2, 214.0, 57.0, "Transaction Information", 1.0);
            (y, page) = (232.0, pg);
        }
        y += 11.8;
        if !dt.is_empty() {
            put(pg, y, date, dt, 1.0);
        }
        put(pg, y, desc, text, 1.0);
        if let Some((a, b)) = money {
            let edge = if a.ends_with('-') { amt } else { amt - 2.0 * CW };
            put(pg, y, right(a, edge), a, 1.0);
            put(pg, y, right(b, bal), b, 1.0);
        }
    }
    put(2, 760.0, 280.0, "Page 2", 1.0);

    let p = profiles::builtins();
    let r = MultiPassScanner::new(&p).scan_tokens(toks).unwrap();
    assert_eq!(r.profile_slug.as_deref(), Some("cnb_ky"));
    assert_eq!(r.period, Some((d(2026, 3, 1), d(2026, 3, 31))));
    let s = &r.sections[0];
    assert_eq!(s.reconciliation, Reconciliation::Reconciled, "{:#?}", s.report);
    assert_eq!(s.account_type, AccountType::Savings);
    assert_eq!((s.opening, s.closing), (Some(dec("1000.00")), Some(dec("2263.50"))));
    assert_eq!((s.total_debits, s.total_credits), (Some(dec("236.50")), Some(dec("1500.00"))));
    let got: Vec<(NaiveDate, Decimal, &str)> = s.transactions.iter().map(|t| (t.posted, t.amount, t.description.as_str())).collect();
    assert_eq!(
        got,
        vec![
            (d(2026, 3, 2), dec("-10.75"), "Govt Stamp Duty Fees"),
            (d(2026, 3, 3), dec("-45.50"), "POS Purchase SAMPLE GROCERY GEORGE TOWN KY"),
            (d(2026, 3, 3), dec("-0.25"), "POS Transaction Fee"),
            (d(2026, 3, 5), dec("1500.00"), "EXAMPLE EMPLOYER LTD Dir Pay Mar 26 Salary"),
            (d(2026, 3, 6), dec("-180.00"), "POS Purchase EXAMPLE STORE GRAND"),
        ]
    );
    assert!(s.report.skipped.is_empty(), "{:#?}", s.report.skipped);
}
