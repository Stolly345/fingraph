//! The merchant directory (`data/merchants.tsv`: Cayman businesses and common online services) and
//! keyword rules. A directory match is confident enough to categorize on its own; a keyword ("MARKET",
//! "DIGITAL") is only a good guess, so it's offered as the suggestion in To Review.

use std::sync::LazyLock;

pub struct Merchant {
    keys: Vec<Vec<String>>,
    pub name: String,
    pub category: String,
    /// A payment platform (GOOGLE, APPLE.COM/BILL): used only when nothing more specific matches.
    pub fallback: bool,
}

impl Merchant {
    /// The words it matches on, as written in the directory (for showing, not matching).
    pub fn keys(&self) -> Vec<String> {
        self.keys.iter().map(|k| k.join(" ")).collect()
    }
}

/// Upper-case words with apostrophes dropped and other punctuation as spaces:
/// `A.L. THOMPSON'S, GEORGETOWN` → `A L THOMPSONS GEORGETOWN`.
pub fn tokens(s: &str) -> Vec<String> {
    s.to_uppercase()
        .replace(['\'', '’'], "")
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

pub static DIRECTORY: LazyLock<Vec<Merchant>> = LazyLock::new(|| {
    include_str!("../data/merchants.tsv")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let mut f = l.split('\t');
            let (keys, name, category) = (f.next()?, f.next()?, f.next()?);
            let fallback = f.next().is_some_and(|x| x.trim() == "fallback");
            Some(Merchant { keys: keys.split('|').map(tokens).filter(|k| !k.is_empty()).collect(), name: name.trim().into(), category: category.trim().into(), fallback })
        })
        .collect()
});

/// Where `key` first appears as consecutive whole words (the last may carry a plural/possessive S).
fn hit(words: &[String], key: &[String]) -> Option<usize> {
    if key.len() > words.len() {
        return None;
    }
    words.windows(key.len()).position(|w| {
        w.iter().zip(key).enumerate().all(|(i, (a, b))| a == b || (i == key.len() - 1 && a.len() == b.len() + 1 && a.starts_with(b.as_str()) && a.ends_with('S')))
    })
}

/// The directory entry naming this text. Statements print the merchant before any billing
/// descriptor (`AUDIBLE*E47R, AMZN.COM/BILL`), so the earliest match wins; at the same spot the
/// longest does (`FOSTERS PHARMACY` over `FOSTERS`, `FLOW TAXI` over `FLOW`).
pub fn lookup(text: &str) -> Option<&'static Merchant> {
    let words = tokens(text);
    let words = &words;
    DIRECTORY
        .iter()
        .flat_map(|m| m.keys.iter().filter_map(move |k| hit(words, k).map(|at| ((!m.fallback, std::cmp::Reverse(at), k.len(), k.iter().map(String::len).sum::<usize>()), m))))
        .max_by_key(|(score, _)| *score)
        .map(|(_, m)| m)
}

/// (keyword, category). A keyword ending in `*` matches any word starting with it
/// (INSURANCE* catches INSURANCEBROKERS). Whole words only, so MKT never matches MKTPL.
pub const KEYWORDS: &[(&str, &str)] = &[
    ("DIGITAL|DGTL|DIGI|SOFTWARE|SAAS|APP STORE|APPS|SUBSCRIPTION|SUBSCR|STREAMING|VPN|HOSTING|DOMAINS", "Subscriptions & Digital"),
    ("MARKET|MKT|SUPERMARKET|SUPERMKT|SUPER MARKET|GROCERY|GROCERIES|GROCER|FOOD FAIR|SUPERCENTRE|SUPERCENTER|MINI MART|MINIMART", "Groceries"),
    ("PHARMACY|PHARM*|CHEMIST|DRUGSTORE|DRUG STORE|APOTHECARY", "Pharmacy"),
    ("LIQUOR|LIQUORS|WINE|WINES|SPIRITS|BREWERY|DISTILLERY", "Liquor"),
    ("COFFEE|CAFE|BAKERY|ICE CREAM|GELATO|DONUT|DONUTS|JUICE BAR|SMOOTHIE*|PASTRY|PASTRIES", "Coffee & Snacks"),
    ("RESTAURANT|RESTAURANTE|GRILL|BISTRO|PIZZA|PIZZERIA|KITCHEN|DINER|SUSHI|JERK|BBQ|STEAKHOUSE|SEAFOOD|TACO|TAQUERIA|BURGER|BURGERS|CANTINA|TAVERN|PUB|IZAKAYA|SMOKEHOUSE|ROTI|BRASSERIE", "Restaurants"),
    ("INSURANCE*|ASSURANCE|INS CO|UNDERWRITERS", "Insurance"),
    ("PROPANE|LPG", "Gas"),
    ("FUEL|PETROL|GAS STATION|SERVICE STATION|GASOLINE|PETROLEUM", "Fuel"),
    ("AUTO|AUTOMOTIVE|AUTOS|MOTORS|GARAGE|TYRE|TYRES|TIRE|TIRES|MECHANIC*|CAR WASH|AUTO PARTS|BODY SHOP|COLLISION|WASH SHACK|AUTOPARTS|DETAILING", "Vehicle Maintenance"),
    ("FREIGHT|SHIPPING|CARGO|COURIER|FORWARDING|FORWARDERS|LOGISTICS|CUSTOMS BROKER*", "Shipping & Freight"),
    ("RENT A CAR|CAR RENTAL|RENTACAR", "Car Rental"),
    ("TAXI|TAXIS|CAB|CABS|SHUTTLE", "Taxi & Rideshare"),
    ("PARKING|PARKADE|TOLL|TOLLS", "Parking"),
    ("SALON|BARBER*|NAILS|NAIL BAR|BEAUTY|LASH|LASHES|COSMETIC*", "Hair & Beauty"),
    ("SPA|MASSAGE", "Spa & Massage"),
    ("GYM|FITNESS|CROSSFIT|YOGA|PILATES|BARRE|MARTIAL ARTS", "Gym & Sports"),
    ("HARDWARE|LUMBER|BUILDING SUPPL*|PLUMBING|ELECTRICAL|PAINT|PAINTS|AIR CONDITIONING|HVAC|HANDYMAN|CONSTRUCTION|ROOFING", "Repairs & Maintenance"),
    ("HOTEL|HOTELS|RESORT|RESORTS|SUITES|INN|CONDOS", "Hotels"),
    ("AIRWAYS|AIRLINE|AIRLINES|AIR LINES", "Flights"),
    ("DENTAL|DENTIST|DENTISTRY|CLINIC|HOSPITAL|MEDICAL|DOCTOR|DOCTORS|PHYSIO*|LABORATORY|LABORATORIES|CHIROPRACT*|PAEDIATRIC*|PEDIATRIC*|ORTHODONT*|SURGERY|HEALTHCARE|URGENT CARE", "Doctor & Dental"),
    ("OPTICAL|OPTICIAN*|OPTOMETR*|EYE CARE|EYECARE|EYE CLINIC|EYEWEAR", "Optical"),
    ("SCHOOL|ACADEMY|PRESCHOOL|COLLEGE|UNIVERSITY|TUITION|MONTESSORI", "School Fees & Tuition"),
    ("DAYCARE|DAY CARE|CHILDCARE|CHILD CARE|NANNY|AFTERCARE|NURSERY SCHOOL", "Childcare & Nanny"),
    ("VET|VETS|VETERINARY|ANIMAL HOSPITAL", "Vet"),
    ("PET|PETS|PET SUPPLIES|KENNEL*", "Pet Food & Supplies"),
    ("CLEANING|JANITORIAL|MAID|MAIDS|LAUNDRY|DRY CLEAN*", "Cleaning"),
    ("PEST|TERMITE*|EXTERMINAT*", "Pest Control"),
    ("POOL|POOLS", "Pool Service"),
    ("LANDSCAP*|LAWN|GARDENING|NURSERY", "Landscaping"),
    ("CINEMA|CINEMAS|THEATRE|THEATER|TICKETS|TICKETING|MUSEUM|AQUARIUM", "Events & Tickets"),
    ("FURNITURE|MATTRESS|HOME DECOR|INTERIORS", "Furniture & Decor"),
    ("APPLIANCE|APPLIANCES", "Appliances"),
    ("ELECTRONICS|COMPUTER|COMPUTERS|CELLULAR", "Electronics"),
    ("SHOES|BOUTIQUE|CLOTHING|APPAREL|FASHION|SHOE SHOP|MENSWEAR|SWIMWEAR", "Clothing & Shoes"),
    ("BOOKS|BOOKSTORE|TOYS|GIFT SHOP|GIFTS|DUTY FREE|JEWELLERS|JEWELERS|DEPARTMENT STORE", "Shopping"),
    ("GOVERNMENT|GOVT|REGISTRY", "Government & Fees"),
    ("STAMP DUTY", "Stamp Duty"),
    ("TAX|TAXES|TAX NETWORK|IRS|USATAXPYMT|EFTPS|TURBOTAX|TAXACT|H&R BLOCK|CRA", "Taxes"),
    ("CHURCH|CHAPEL|TITHE|TITHES|MINISTRIES", "Church & Tithes"),
    ("CHARITY|DONATION|FOUNDATION", "Charity"),
    ("ACCOUNTING|ACCOUNTANTS|BOOKKEEPING|CPA", "Accounting"),
    ("LAW FIRM|ATTORNEYS|BARRISTERS|SOLICITORS", "Legal"),
    ("OFFICE SUPPL*|OFFICE PRODUCTS|STATIONERY", "Office Supplies"),
    ("PROPERTY MANAGEMENT|PROP MGMT", "Property Management"),
    ("STRATA|HOA|HOMEOWNERS ASSOC*", "Strata Fees"),
];

/// Each rule's alternatives, as words.
type Keys = Vec<Vec<String>>;

static KEYWORD_TOKENS: LazyLock<Vec<(Keys, &'static str)>> = LazyLock::new(|| {
    KEYWORDS
        .iter()
        .map(|(ks, cat)| (ks.split('|').map(|k| k.split(' ').map(str::to_string).collect()).collect(), *cat))
        .collect()
});

fn keyword_hit(words: &[String], key: &[String]) -> bool {
    key.len() <= words.len()
        && words.windows(key.len()).any(|w| {
            w.iter().zip(key).all(|(a, b)| match b.strip_suffix('*') {
                Some(prefix) => a.starts_with(prefix),
                None => a == b,
            })
        })
}

/// A category guessed from words in the merchant's name; the first rule that matches wins.
pub fn keyword_category(text: &str) -> Option<&'static str> {
    let words = tokens(text);
    KEYWORD_TOKENS.iter().find(|(keys, _)| keys.iter().any(|k| keyword_hit(&words, k))).map(|(_, c)| *c)
}

/// Where to manage or cancel a subscription, by a word in its name (checked in order, so a
/// service beats the store that billed it).
const MANAGE: &[(&str, &str)] = &[
    ("NETFLIX", "https://www.netflix.com/cancelplan"),
    ("SPOTIFY", "https://www.spotify.com/account/subscription/"),
    ("DISNEY", "https://www.disneyplus.com/account"),
    ("HULU", "https://secure.hulu.com/account"),
    ("PARAMOUNT", "https://www.paramountplus.com/account/"),
    ("PEACOCK", "https://www.peacocktv.com/account"),
    ("YOUTUBE", "https://www.youtube.com/paid_memberships"),
    ("AUDIBLE", "https://www.audible.com/account/overview"),
    ("KINDLE", "https://www.amazon.com/kindle-dbs/ku/ku-central"),
    ("PRIME", "https://www.amazon.com/mc"),
    ("ADOBE", "https://account.adobe.com/plans"),
    ("MICROSOFT", "https://account.microsoft.com/services"),
    ("DROPBOX", "https://www.dropbox.com/account/plan"),
    ("NORDVPN", "https://my.nordaccount.com/dashboard/"),
    ("PATREON", "https://www.patreon.com/settings/memberships"),
    ("CLAUDE", "https://claude.ai/settings/billing"),
    ("ANTHROPIC", "https://claude.ai/settings/billing"),
    ("GOOGLE PLAY", "https://play.google.com/store/account/subscriptions"),
    ("GOOGLE", "https://myaccount.google.com/payments-and-subscriptions"),
    ("APPLE", "https://apps.apple.com/account/subscriptions"),
    ("ITUNES", "https://apps.apple.com/account/subscriptions"),
    ("PAYPAL", "https://www.paypal.com/myaccount/autopay/"),
];

/// The page where a subscription can be managed or cancelled, when it's a service we know.
pub fn manage_url(text: &str) -> Option<&'static str> {
    let t = text.to_uppercase();
    MANAGE.iter().find(|(k, _)| t.contains(k)).map(|x| x.1)
}

pub fn count() -> usize {
    DIRECTORY.len()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn directory() {
        let name = |t: &str| lookup(t).map(|m| (m.name.as_str(), m.category.as_str()));
        assert_eq!(name("A.L. THOMPSON'S, GEORGETOWN"), Some(("A.L. Thompson's", "Repairs & Maintenance")));
        assert_eq!(name("FOSTER'S CAMANA BAY, GRAND CAYMAN"), Some(("Foster's", "Groceries")));
        assert_eq!(name("FOSTERS PHARMACY SAVANNAH"), Some(("Foster's Pharmacy", "Pharmacy")));
        assert_eq!(name("TRF TO: CLEAN GAS LTD ROYAL BANK OF CANADA"), Some(("Clean Gas", "Gas")));
        assert_eq!(name("CLEAN GAS LTD"), Some(("Clean Gas", "Gas")));
        assert_eq!(name("LOGIC COMMUNICATIONS"), Some(("Logic", "Phone & Internet")));
        assert_eq!(name("C3 PURE FIBRE"), Some(("C3", "Phone & Internet")));
        assert_eq!(name("C.U.C, GEORGE TOWN"), Some(("Caribbean Utilities Company", "Electricity")));
        assert_eq!(name("TRF TO: BRITISH CAYMANIAN BANK OF BUTTERFIELD INTL"), Some(("British Caymanian Insurance", "Insurance")));
        // Amazon: the store is shopping, its digital services are subscriptions
        assert_eq!(name("AMAZON MKTPL*AB12CD34E, SEATTLE"), Some(("Amazon", "Online Shopping")));
        assert_eq!(name("AMAZON DIGITAL SVCS, AMZN.COM/BILL"), Some(("Amazon Digital", "Subscriptions & Digital")));
        assert_eq!(name("AMAZON DIGIT*XY98ZW76Q"), Some(("Amazon Digital", "Subscriptions & Digital")));
        // a generic word is never a match on its own
        assert_eq!(name("FLOWERS BY JANE"), None);
        assert_eq!(name("KIRK MARKET/BACK-UP 1"), Some(("Kirk Market", "Groceries")));
        assert_eq!(name("MARLEY JERK CENTRE"), Some(("Marley Jerk Centre", "Takeaway & Delivery")));
        // the merchant comes before the billing descriptor
        assert_eq!(name("AUDIBLE*A12B34567, AMZN.COM/BILL"), Some(("Audible", "Subscriptions & Digital")));
        assert_eq!(name("AMAZON MKTPLACE PMTS, AMZN.COM/BILL"), Some(("Amazon", "Online Shopping")));
        assert_eq!(name("WAL-MART SUPERCENTER 5854, MIAMI"), Some(("Walmart", "Shopping")));
        assert_eq!(name("TORTUGA GREENERY, GEORGE TOWN"), None);
        assert_eq!(manage_url("Netflix.com Los Gatos"), Some("https://www.netflix.com/cancelplan"));
        assert_eq!(manage_url("GOOGLE *YouTubePremium"), Some("https://www.youtube.com/paid_memberships"));
        assert_eq!(manage_url("Foster's"), None);
        // a platform gives way to the service it billed for
        assert_eq!(name("GOOGLE *YouTubePremium g.co/helppay#"), Some(("YouTube", "Subscriptions & Digital")));
        assert_eq!(name("PP*GOOGLE FLIGHTRADAR24 4029357733 US"), Some(("Flightradar24", "Subscriptions & Digital")));
        assert_eq!(name("GOOGLE *Google One"), Some(("Google One", "Subscriptions & Digital")));
        assert_eq!(name("GOOGLE *TEMPORARY HOLD"), Some(("Google", "Subscriptions & Digital")));
        assert_eq!(name("APPLE.COM/BILL 866-712-7753"), Some(("Apple", "Subscriptions & Digital")));
        assert_eq!(name("NORDVPN.COM AMSTERDAM"), Some(("NordVPN", "Subscriptions & Digital")));
        assert_eq!(name("PROTON AG GENEVA"), Some(("Proton", "Subscriptions & Digital")));
        assert_eq!(name("TARGET 00012345 MIAMI FL"), Some(("Target", "Shopping")));
        assert_eq!(name("BEST BUY 00123"), Some(("Best Buy", "Electronics")));
        assert_eq!(name("STEAM CLEANING CO"), None);
    }

    #[test]
    fn keywords() {
        assert_eq!(keyword_category("SOMEWHERE DIGITAL LTD"), Some("Subscriptions & Digital"));
        assert_eq!(keyword_category("ACME DGTL SVCS"), Some("Subscriptions & Digital"));
        assert_eq!(keyword_category("BODDEN TOWN MARKET"), Some("Groceries"));
        assert_eq!(keyword_category("SAVANNAH MKT"), Some("Groceries"));
        // MKTPL and MARKETPLACE are online stores, MARKETING is an agency: not groceries
        assert_eq!(keyword_category("SOMESHOP MKTPL"), None);
        assert_eq!(keyword_category("ISLAND MARKETING LTD"), None);
        assert_eq!(keyword_category("SEVEN MILE INSURANCEBROKERS"), Some("Insurance"));
        assert_eq!(keyword_category("JOE'S AUTO REPAIR"), Some("Vehicle Maintenance"));
        // a tax service has a home; Scotiabank's stamp duty line stays Stamp Duty; a taxi isn't a tax
        assert_eq!(keyword_category("TAX NETWORK USA"), Some("Taxes"));
        assert_eq!(keyword_category("IRS USATAXPYMT"), Some("Taxes"));
        assert_eq!(keyword_category("STAMP DUTY TAX"), Some("Stamp Duty"));
        assert_eq!(keyword_category("ISLAND TAXI"), Some("Taxi & Rideshare"));
    }
}
