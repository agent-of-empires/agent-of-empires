//! Age of Empires 2 civilization names for random session titles

use rand::seq::IndexedRandom;

pub const CIVILIZATIONS: &[&str] = &[
    "Armenians",
    "Aztecs",
    "Bengalis",
    "Berbers",
    "Bohemians",
    "Britons",
    "Bulgarians",
    "Burgundians",
    "Burmese",
    "Byzantines",
    "Celts",
    "Chinese",
    "Cumans",
    "Dravidians",
    "Ethiopians",
    "Franks",
    "Georgians",
    "Goths",
    "Gurjaras",
    "Hindustanis",
    "Huns",
    "Incas",
    "Italians",
    "Japanese",
    "Jurchens",
    "Khitans",
    "Khmer",
    "Koreans",
    "Lithuanians",
    "Magyars",
    "Malay",
    "Malians",
    "Mayans",
    "Mongols",
    "Persians",
    "Poles",
    "Portuguese",
    "Romans",
    "Saracens",
    "Shu",
    "Sicilians",
    "Slavs",
    "Spanish",
    "Tatars",
    "Teutons",
    "Turks",
    "Vietnamese",
    "Vikings",
    "Wei",
    "Wu",
];

fn to_roman(n: u32) -> String {
    let numerals = [
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];

    let mut result = String::new();
    let mut remaining = n;

    for (value, numeral) in numerals {
        while remaining >= value {
            result.push_str(numeral);
            remaining -= value;
        }
    }

    result
}

pub fn generate_random_title(existing_titles: &[&str]) -> String {
    generate_random_title_filtered(existing_titles, |_| false).unwrap_or_else(|| {
        let timestamp = chrono::Utc::now().timestamp();
        for n in timestamp.. {
            let candidate = format!("Session {}", n);
            if !existing_titles.contains(&candidate.as_str()) {
                return candidate;
            }
        }
        unreachable!("an unbounded suffix range always yields a free candidate")
    })
}

pub fn generate_random_title_filtered<F>(
    existing_titles: &[&str],
    is_unavailable: F,
) -> Option<String>
where
    F: Fn(&str) -> bool,
{
    let mut rng = rand::rng();

    let available: Vec<&str> = CIVILIZATIONS
        .iter()
        .filter(|civ| !existing_titles.contains(*civ) && !is_unavailable(civ))
        .copied()
        .collect();

    if let Some(&civ) = available.choose(&mut rng) {
        return Some(civ.to_string());
    }

    let base = CIVILIZATIONS.choose(&mut rng).unwrap_or(&"Session");
    for n in 2..=1000 {
        let candidate = format!("{} {}", base, to_roman(n));
        if !existing_titles.contains(&candidate.as_str()) && !is_unavailable(&candidate) {
            return Some(candidate);
        }
    }

    let timestamp = chrono::Utc::now().timestamp();
    for n in timestamp..timestamp + 1000 {
        let candidate = format!("{} {}", base, n);
        if !existing_titles.contains(&candidate.as_str()) && !is_unavailable(&candidate) {
            return Some(candidate);
        }
    }

    None
}

/// True when `title` is one this module could have produced via [`generate_random_title`]: a bare
/// civilization name, or a civilization followed by a Roman-numeral suffix (`Britons II`) or a
/// numeric timestamp suffix (`Britons 1700000000`).
pub fn is_default_civ_name(title: &str) -> bool {
    let t = title.trim();
    if CIVILIZATIONS.contains(&t) {
        return true;
    }
    if let Some((base, suffix)) = t.rsplit_once(' ') {
        if CIVILIZATIONS.contains(&base) && !suffix.is_empty() {
            // Match only the exact suffixes generate_random_title emits: a Roman numeral in
            // 2..=1000, or a timestamp (Unix seconds, >= 9 digits).
            let is_roman = (2..=1000).any(|n| to_roman(n) == suffix);
            let is_timestamp = suffix.len() >= 9 && suffix.chars().all(|c| c.is_ascii_digit());
            return is_roman || is_timestamp;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_roman_cases() {
        for (n, want) in [
            (1, "I"),
            (4, "IV"),
            (9, "IX"),
            (49, "XLIX"),
            (50, "L"),
            (100, "C"),
            (500, "D"),
            (1000, "M"),
        ] {
            assert_eq!(to_roman(n), want);
        }
    }

    #[test]
    fn generated_titles_avoid_taken_names_then_fall_back_to_suffixes() {
        let title = generate_random_title(&[]);
        assert!(CIVILIZATIONS.contains(&title.as_str()));

        let existing = vec!["Britons", "Franks", "Vikings"];
        assert!(!existing.contains(&generate_random_title(&existing).as_str()));

        let all: Vec<&str> = CIVILIZATIONS.to_vec();
        assert!(generate_random_title(&all).contains(" II"));

        let all_but_tatars: Vec<&str> = CIVILIZATIONS
            .iter()
            .copied()
            .filter(|civ| *civ != "Tatars")
            .collect();
        let filtered = generate_random_title_filtered(&all_but_tatars, |c| c == "Tatars")
            .expect("a suffixed civilization is still available");
        assert_ne!(filtered, "Tatars");
        assert!(filtered.contains(" II"), "got: {filtered}");

        assert!(generate_random_title_filtered(&[], |_| true).is_none());
    }

    #[test]
    fn is_default_civ_name_cases() {
        assert!(is_default_civ_name("Vikings"));
        assert!(is_default_civ_name("  Vikings  "));
        assert!(is_default_civ_name("Britons II"));
        assert!(is_default_civ_name("Franks XLIX"));
        assert!(is_default_civ_name("Mongols 1700000000"));
        assert!(!is_default_civ_name("Fix login bug"));
        assert!(!is_default_civ_name("My Vikings"));
        assert!(!is_default_civ_name("Vikings raid plan"));
        assert!(!is_default_civ_name("Vikings 2.0"));
        assert!(!is_default_civ_name("Notaciv II"));
        assert!(!is_default_civ_name("Britons IM"));
        assert!(!is_default_civ_name("Vikings 2"));
        assert!(!is_default_civ_name(""));
    }
}
