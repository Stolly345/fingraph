//! Reads the 0.2.13 review found wrong but "proven" (Reconciled, no notes), and inputs that panicked. Each must now
//! be held (a note or a mismatch) or read right, and none may panic.
use fingraph_core::parser::{MultiPassScanner, ScanResult, Token};

const CW: f64 = 5.2;
// lines: each line is a list of (x, right_aligned, text)
fn page(lines: &[Vec<(f64, bool, &str)>], pg: u32, y0: f64) -> Vec<Token> {
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let y = y0 + i as f64 * 11.0;
        for &(x, right, text) in l {
            let ws: Vec<&str> = text.split_whitespace().collect();
            let total: f64 = ws.iter().map(|w| w.chars().count() as f64 * CW).sum::<f64>() + CW * 0.6 * ws.len().saturating_sub(1) as f64;
            let mut x = if right { x - total } else { x };
            for w in ws {
                let wd = w.chars().count() as f64 * CW;
                out.push(Token::new(pg, x, y, x + wd, y + 8.0, w));
                x += wd + CW * 0.6;
            }
        }
    }
    out
}

fn scan(t: Vec<Token>) -> ScanResult {
    MultiPassScanner::new(&[]).scan_tokens(t).unwrap()
}

fn show(r: &ScanResult) {
    println!("notes: {:?}", r.diagnostics.pass3.layout_notes);
    for s in &r.sections {
        println!("{:?} {:?} {:?} open {:?} close {:?} heals {:?} misread {:?}", s.reconciliation, s.account_type, s.currency, s.opening, s.closing, s.report.sign_heals, s.report.misread_balances);
        for t in &s.transactions { println!("  {} {} bal {:?} {}", t.posted, t.amount, t.balance, t.description); }
        for l in &s.report.skipped { println!("  SKIP {:?} {:?} {} [{}]", l.date, l.amount, l.text, l.reason); }
    }
}

// a plain savings statement; rows: (date, desc, withdrawal, deposit, balance)
fn stmt(rows: &[(&str, &str, &str, &str, &str)], open: &str, close: &str) -> Vec<Token> {
    let mut lines: Vec<Vec<(f64, bool, &str)>> = vec![
        vec![(40.0, false, "First Test Bank Ltd")],
        vec![(40.0, false, "Savings Account Statement")],
        vec![(40.0, false, "Statement Period: 01/08/2026 to 31/08/2026")],
        vec![(40.0, false, "Currency: USD")],
        vec![],
        vec![(40.0, false, "Date"), (110.0, false, "Description"), (380.0, true, "Withdrawals"), (460.0, true, "Deposits"), (540.0, true, "Balance")],
        vec![(40.0, false, "01/08/2026"), (110.0, false, "Opening Balance"), (540.0, true, open)],
    ];
    for r in rows {
        let mut l = vec![(40.0, false, r.0), (110.0, false, r.1)];
        if !r.2.is_empty() { l.push((380.0, true, r.2)); }
        if !r.3.is_empty() { l.push((460.0, true, r.3)); }
        if !r.4.is_empty() { l.push((540.0, true, r.4)); }
        lines.push(l);
    }
    lines.push(vec![(40.0, false, "31/08/2026"), (110.0, false, "Closing Balance"), (540.0, true, close)]);
    page(&lines, 1, 60.0)
}


/// What the fuzz harness calls proven: it would import without a hold.
fn proven(r: &ScanResult) -> bool {
    r.is_reconciled() && r.diagnostics.pass3.layout_notes.is_empty()
}

#[test]
fn a_plain_statement_still_reads() {
    let r = scan(stmt(&[("03/08/2026", "GROCERY STORE", "20.00", "", "980.00"), ("07/08/2026", "SALARY", "", "500.00", "1,480.00"), ("12/08/2026", "ATM WITHDRAWAL", "100.00", "", "1,380.00")], "1,000.00", "1,380.00"));
    show(&r);
    assert!(proven(&r));
}

/// Two rows' amounts on each other's lines look exactly like one misprinted balance: never proven.
#[test]
fn swapped_amounts_are_held() {
    let r = scan(stmt(&[("03/08/2026", "GROCERY STORE", "", "500.00", "980.00"), ("07/08/2026", "SALARY", "20.00", "", "1,480.00"), ("12/08/2026", "ATM WITHDRAWAL", "100.00", "", "1,380.00")], "1,000.00", "1,380.00"));
    show(&r);
    assert!(!proven(&r));
}

/// A deposit whose amount was lost must not be covered for by flipping the next row's sign.
#[test]
fn a_sign_heal_never_stands_in_for_a_lost_row() {
    let r = scan(stmt(&[("04/08/2026", "TRANSFER IN", "", "", ""), ("05/08/2026", "GROCERY STORE", "20.00", "", "1,020.00"), ("12/08/2026", "ATM WITHDRAWAL", "100.00", "", "920.00")], "1,000.00", "920.00"));
    show(&r);
    assert!(!proven(&r));
}

/// `advisable` isn't Visa and a `Principal Office` isn't a loan.
#[test]
fn account_type_words_are_whole_words() {
    for extra in ["Principal Office: 1 Main Street", "It is advisable to check this statement"] {
        let mut t = stmt(&[("03/08/2026", "GROCERY STORE", "20.00", "", "980.00"), ("07/08/2026", "TRANSFER", "", "500.00", "1,480.00")], "1,000.00", "1,480.00");
        t.retain(|x| !matches!(x.text.as_str(), "Savings" | "Account" | "Statement") || x.y0 > 80.0);
        t.extend(page(&[vec![(300.0, false, extra)]], 1, 60.0));
        let r = scan(t);
        show(&r);
        assert!(!r.diagnostics.pass1.account_type.unwrap().is_liability(), "{extra}");
        assert!(r.sections.iter().all(|s| s.report.sign_heals.is_empty()), "{extra}");
    }
}

/// `Dear Ron,` on a `$`-only statement isn't the Romanian leu.
#[test]
fn a_name_is_not_a_currency() {
    let mut t = stmt(&[("03/08/2026", "GROCERY STORE", "$20.00", "", "$980.00")], "$1,000.00", "$980.00");
    t.retain(|x| x.text != "Currency:" && x.text != "USD");
    t.extend(page(&[vec![(300.0, false, "Ron Smith")], vec![(300.0, false, "Dear Ron,")]], 1, 20.0));
    let r = scan(t);
    show(&r);
    assert!(r.sections.iter().all(|s| s.currency.pegged()));
    assert!(!proven(&r));
    // and in capitals it's still only a guess, so it's held
    let mut t = stmt(&[("03/08/2026", "GROCERY STORE", "$20.00", "", "$980.00")], "$1,000.00", "$980.00");
    t.retain(|x| x.text != "Currency:" && x.text != "USD");
    t.extend(page(&[vec![(300.0, false, "RON SMITH")], vec![(300.0, false, "RON SMITH")]], 1, 20.0));
    assert!(!proven(&scan(t)));
}

/// Old statements: `97` is 1997, not 2097.
#[test]
fn two_digit_years_pivot() {
    let lines: Vec<Vec<(f64, bool, &str)>> = vec![
        vec![(40.0, false, "Westpac Savings Account Statement")],
        vec![(40.0, false, "Currency: USD")],
        vec![(40.0, false, "Date"), (110.0, false, "Description"), (380.0, true, "Withdrawals"), (460.0, true, "Deposits"), (540.0, true, "Balance")],
        vec![(40.0, false, "01/08/97"), (110.0, false, "Opening Balance"), (540.0, true, "1,000.00")],
        vec![(40.0, false, "05/08/97"), (110.0, false, "GROCERY STORE"), (380.0, true, "20.00"), (540.0, true, "980.00")],
        vec![(40.0, false, "19/08/97"), (110.0, false, "ATM"), (380.0, true, "100.00"), (540.0, true, "880.00")],
        vec![(40.0, false, "31/08/97"), (110.0, false, "Closing Balance"), (540.0, true, "880.00")],
    ];
    let r = scan(page(&lines, 1, 60.0));
    show(&r);
    assert!(r.sections.iter().flat_map(|s| &s.transactions).all(|t| t.posted.to_string().starts_with("1997-")));
}

/// Full-width digits (a Japanese statement) and absurd amounts don't panic.
#[test]
fn odd_input_never_panics() {
    let mut t = stmt(&[("03/08/2026", "GROCERY STORE", "20.00", "", "980.00")], "1,000.00", "980.00");
    t.extend(page(&[vec![(40.0, false, "Savings Account １２３４５")]], 1, 131.5));
    scan(t);
    let big = "50,000,000,000,000,000,000,000,000,000.00";
    scan(stmt(&[("03/08/2026", "X", "", big, ""), ("04/08/2026", "Y", "", big, "")], "0.00", "1.00"));
    for big in ["99,999,999,999,999,999,999,999.99", "79,228,162,514,264,337,593,543,950,335.00"] {
        let r = scan(stmt(&[("03/08/2026", "X", "", big, big)], "0.00", big));
        assert!(!proven(&r), "{big}");
    }
    // digits other than 0-9 aren't dates (they once read as the year 0), and a letter after the year keeps it
    assert!(fingraph_core::parser::dates::find_dates("١٢/٠٨/٢٠٢٦ １２/０８/２０２６").is_empty());
    assert!(fingraph_core::parser::dates::find_dates("12/08/2026é").iter().all(|d| d.2.has_year()));
}
