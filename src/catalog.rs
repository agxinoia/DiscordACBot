//! Built-in catalogue of known EAC deployments.
//!
//! These pairs are published by others and have **not** been verified against
//! the CDN by this project, so nothing here is trusted blindly: adding a
//! catalogue entry probes it exactly like a hand-entered one, and an entry that
//! publishes nothing is refused rather than stored.
//!
//! Deployments do get rotated, so an entry that stops resolving is expected
//! eventually rather than a bug.

use crate::config::Game;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KnownGame {
    pub name: &'static str,
    pub product_id: &'static str,
    pub deployment_id: &'static str,
}

/// Known deployments, alphabetical. Discord select menus hold 25 options, so
/// keep this under that or `browse` will need paging.
pub const KNOWN: &[KnownGame] = &[
    KnownGame {
        name: "Apex Legends",
        product_id: "5dcd88f4e2094a698ebffa43438edc33",
        deployment_id: "47a5a1b2e0f64748a96777920ad97fbd",
    },
    KnownGame {
        name: "ARC Raiders",
        product_id: "9e8b37541e614575b4de303d2c2e44cf",
        deployment_id: "35e06571d8ab4de4b98519b624125459",
    },
    KnownGame {
        name: "Fortnite",
        product_id: "prod-fn",
        deployment_id: "62a9473a2dca46b29ccf17577fcf42d7",
    },
    KnownGame {
        name: "Rust",
        product_id: "429c2212ad284866aee071454c2125b5",
        deployment_id: "76796531e86443548754600511f42e9e",
    },
];

impl KnownGame {
    /// Platforms are left empty: they are filled in by probing, because which
    /// ones a deployment publishes cannot be known from the id pair.
    pub fn to_game(self) -> Game {
        Game {
            name: self.name.to_string(),
            product_id: self.product_id.to_string(),
            deployment_id: self.deployment_id.to_string(),
            platforms: Vec::new(),
        }
    }
}

pub fn find(name: &str) -> Option<&'static KnownGame> {
    let needle = name.trim();
    KNOWN.iter().find(|g| g.name.eq_ignore_ascii_case(needle))
}

/// Catalogue entries whose id pair is not already tracked by this guild.
pub fn not_yet_tracked(tracked: &[Game]) -> Vec<&'static KnownGame> {
    KNOWN
        .iter()
        .filter(|known| {
            !tracked.iter().any(|game| {
                game.product_id == known.product_id && game.deployment_id == known.deployment_id
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings;

    #[test]
    fn every_entry_would_survive_validation() {
        // A catalogue entry must be addable through the same checks as any
        // hand-entered one, or browse would offer something /eac add refuses.
        for game in KNOWN {
            settings::validate_name(game.name).expect(game.name);
            settings::validate_id("product_id", game.product_id).expect(game.name);
            settings::validate_id("deployment_id", game.deployment_id).expect(game.name);
        }
    }

    #[test]
    fn entries_are_unique_and_sorted() {
        let mut names: Vec<&str> = KNOWN.iter().map(|g| g.name).collect();
        let original = names.clone();
        names.sort_by_key(|n| n.to_lowercase());
        assert_eq!(names, original, "keep the catalogue alphabetical");

        let mut pairs: Vec<(&str, &str)> = KNOWN
            .iter()
            .map(|g| (g.product_id, g.deployment_id))
            .collect();
        pairs.sort_unstable();
        let count = pairs.len();
        pairs.dedup();
        assert_eq!(pairs.len(), count, "duplicate deployment in the catalogue");
    }

    #[test]
    fn fits_in_one_select_menu() {
        assert!(KNOWN.len() <= 25, "a select menu holds 25 options");
    }

    #[test]
    fn lookup_is_case_insensitive() {
        assert_eq!(find("rust").map(|g| g.name), Some("Rust"));
        assert_eq!(find("  ARC RAIDERS  ").map(|g| g.name), Some("ARC Raiders"));
        assert!(find("Not A Game").is_none());
    }

    #[test]
    fn a_tracked_deployment_is_filtered_out_of_the_offer() {
        let tracked = vec![find("Rust").unwrap().to_game()];
        let offered = not_yet_tracked(&tracked);

        assert_eq!(offered.len(), KNOWN.len() - 1);
        assert!(!offered.iter().any(|g| g.name == "Rust"));
    }

    #[test]
    fn matching_is_by_ids_not_by_name() {
        // A game renamed locally is still the same deployment.
        let mut renamed = find("Rust").unwrap().to_game();
        renamed.name = "my rust server".into();
        assert!(!not_yet_tracked(&[renamed]).iter().any(|g| g.name == "Rust"));
    }

    #[test]
    fn a_catalogue_entry_carries_no_platforms() {
        // Platforms come from probing; baking in a guess is what broke before.
        assert!(find("Rust").unwrap().to_game().platforms.is_empty());
    }
}
