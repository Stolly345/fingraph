//! How a category looks: a symbolic icon and a named colour, the same on every screen. The built-in
//! categories have defaults, so existing ledgers look right with no setup; a choice of your own is
//! stored on the category. A sub-category without one wears its parent's.

use crate::db::Category;

/// (name, shade on dark, shade on light, chart colour).
pub type NamedColor = (&'static str, &'static str, &'static str, (f64, f64, f64));

pub const COLORS: [NamedColor; 10] = [
    ("blue", "#62a0ea", "#1c71d8", (0.21, 0.52, 0.89)),
    ("green", "#57e389", "#26a269", (0.18, 0.76, 0.49)),
    ("orange", "#ffa348", "#c64600", (1.0, 0.47, 0.0)),
    ("purple", "#c061cb", "#813d9c", (0.57, 0.25, 0.67)),
    ("red", "#ff7b63", "#c01c28", (0.88, 0.11, 0.14)),
    ("teal", "#5bc8af", "#0f8a78", (0.13, 0.56, 0.64)),
    ("yellow", "#f8e45c", "#9c6e03", (0.96, 0.76, 0.07)),
    ("pink", "#f0a0c8", "#b83280", (0.86, 0.38, 0.66)),
    ("brown", "#cdab8f", "#865e3c", (0.6, 0.42, 0.25)),
    ("slate", "#b0b8c1", "#5e6873", (0.45, 0.5, 0.56)),
];

/// What the icon chooser offers.
pub const ICONS: [&str; 20] = [
    "user-home-symbolic",
    "emoji-food-symbolic",
    "emoji-nature-symbolic",
    "emoji-objects-symbolic",
    "emoji-travel-symbolic",
    "airplane-mode-symbolic",
    "emoji-activities-symbolic",
    "emoji-people-symbolic",
    "face-smile-symbolic",
    "avatar-default-symbolic",
    "applications-multimedia-symbolic",
    "applications-engineering-symbolic",
    "battery-full-symbolic",
    "security-high-symbolic",
    "media-playlist-repeat-symbolic",
    "starred-symbolic",
    "folder-symbolic",
    "x-office-document-symbolic",
    "x-office-spreadsheet-symbolic",
    "emoji-symbols-symbolic",
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Look {
    pub icon: String,
    pub color: String,
}

/// The default for a top-level category; anything else gets the plain folder.
pub fn default_look(name: &str) -> (&'static str, &'static str) {
    match name {
        "Bank Fees & Interest" => ("x-office-spreadsheet-symbolic", "slate"),
        "Business Expense" => ("folder-symbolic", "brown"),
        "Cash Withdrawal" => ("emoji-symbols-symbolic", "slate"),
        "Clothing & Shoes" => ("emoji-people-symbolic", "pink"),
        "Dining & Takeaway" => ("emoji-food-symbolic", "orange"),
        "Education" => ("x-office-document-symbolic", "blue"),
        "Entertainment" => ("applications-multimedia-symbolic", "purple"),
        "Family Expense" => ("avatar-default-symbolic", "pink"),
        "Gifts & Donations" => ("starred-symbolic", "red"),
        "Government & Fees" => ("x-office-document-symbolic", "brown"),
        "Groceries" => ("emoji-nature-symbolic", "green"),
        "Health & Fitness" => ("emoji-activities-symbolic", "teal"),
        "Home & Garden" => ("user-home-symbolic", "green"),
        "Housing" => ("user-home-symbolic", "blue"),
        "Income" => ("starred-symbolic", "green"),
        "Insurance" => ("security-high-symbolic", "purple"),
        "Loan Payments" => ("x-office-spreadsheet-symbolic", "orange"),
        "Personal Care" => ("face-smile-symbolic", "pink"),
        "Pets" => ("emoji-nature-symbolic", "brown"),
        "Professional Services" => ("avatar-default-symbolic", "slate"),
        "Property Expense" => ("user-home-symbolic", "brown"),
        "Repairs & Maintenance" => ("applications-engineering-symbolic", "teal"),
        "Savings & Investments" => ("starred-symbolic", "green"),
        "Shopping" => ("emoji-objects-symbolic", "purple"),
        "Subscriptions & Digital" => ("media-playlist-repeat-symbolic", "pink"),
        "Transfers" => ("media-playlist-repeat-symbolic", "slate"),
        "Transport" => ("emoji-travel-symbolic", "yellow"),
        "Travel" => ("airplane-mode-symbolic", "teal"),
        "Utilities" => ("battery-full-symbolic", "blue"),
        _ => ("folder-symbolic", "slate"),
    }
}

/// What a category wears: its own choices, then its parent's, then the default of the top-level name.
pub fn look_of(cat: &Category, all: &[Category]) -> Look {
    let parent = cat.parent_id.and_then(|p| all.iter().find(|c| c.id == p));
    let top = parent.unwrap_or(cat);
    let (icon, color) = default_look(&top.name);
    Look {
        icon: cat.icon.clone().or_else(|| parent.and_then(|p| p.icon.clone())).unwrap_or_else(|| icon.to_string()),
        color: cat.color.clone().or_else(|| parent.and_then(|p| p.color.clone())).unwrap_or_else(|| color.to_string()),
    }
}

/// The chart colour of a named colour.
pub fn rgb_of(color: &str) -> (f64, f64, f64) {
    COLORS.iter().find(|c| c.0 == color).map_or(COLORS[9].3, |c| c.3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::CategoryKind;

    fn cat(id: i64, name: &str, parent: Option<i64>, icon: Option<&str>, color: Option<&str>) -> Category {
        Category { id, name: name.into(), parent_id: parent, kind: CategoryKind::Expense, icon: icon.map(Into::into), color: color.map(Into::into) }
    }

    #[test]
    fn subcategories_inherit_and_choices_win() {
        let top = cat(1, "Groceries", None, None, None);
        let sub = cat(2, "Liquor", Some(1), None, None);
        let all = vec![top.clone(), sub.clone()];
        assert_eq!(look_of(&sub, &all), Look { icon: "emoji-nature-symbolic".into(), color: "green".into() });
        let painted = vec![cat(1, "Groceries", None, None, Some("red")), sub.clone()];
        assert_eq!(look_of(&sub, &painted).color, "red");
        let own = cat(2, "Liquor", Some(1), Some("starred-symbolic"), None);
        assert_eq!(look_of(&own, &painted), Look { icon: "starred-symbolic".into(), color: "red".into() });
    }

    #[test]
    fn every_default_is_a_colour_and_an_offered_icon() {
        for name in ["Groceries", "Travel", "Utilities", "Something I Made Up"] {
            let (icon, color) = default_look(name);
            assert!(COLORS.iter().any(|c| c.0 == color));
            assert!(ICONS.contains(&icon) || icon == "airplane-mode-symbolic");
        }
    }
}
