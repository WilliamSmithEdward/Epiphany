//! Map an object's DISPLAY name to a lowercase, filesystem-safe on-disk folder
//! name (ADR-0037).
//!
//! A cube's display name (e.g. "Sales") is preserved everywhere in the API and
//! UI; only its on-disk folder becomes a slug (e.g. "sales"). Decoupling the
//! folder name from the display name keeps the on-disk layout consistent across
//! platforms (notably Linux, where the filesystem is case-sensitive) and avoids
//! characters that are illegal or awkward in a path.

/// The fallback token used when a name slugs to the empty string (e.g. a name
/// made entirely of characters that are stripped, like "***"). Stable so the
/// same empty-slugging name always maps to the same folder.
const EMPTY_FALLBACK: &str = "unnamed";

/// Map a display `name` to a lowercase, filesystem-safe, non-empty folder name.
///
/// Rules:
/// 1. Lowercase every ASCII letter (`A`-`Z` -> `a`-`z`). Non-ASCII letters are
///    not in the allowed set and become `-` by rule 2, so they are not
///    lowercased here.
/// 2. Replace every character that is not in `[a-z0-9-_]` (after lowercasing)
///    with `-`. So spaces, punctuation, slashes, and any non-ASCII byte all
///    become `-`.
/// 3. Collapse any run of consecutive `-` into a single `-`. (Underscores are
///    kept verbatim and are not collapsed.)
/// 4. Trim leading and trailing `-`.
/// 5. If the result is empty, fall back to a stable non-empty token
///    (`"unnamed"`).
///
/// The result is always non-empty, lowercase, and contains only `[a-z0-9-_]`.
pub fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for ch in name.chars() {
        // Rule 1 + 2: lowercase, then keep allowed chars; everything else -> '-'.
        let lowered = ch.to_ascii_lowercase();
        // Rule 2: '-' is allowed; '_', lowercase letters, and digits are allowed
        // verbatim; everything else collapses to '-'.
        let mapped = if lowered.is_ascii_lowercase() || lowered.is_ascii_digit() || lowered == '_' {
            lowered
        } else {
            '-'
        };
        if mapped == '-' {
            // Rule 3: collapse runs of '-'.
            if last_was_dash {
                continue;
            }
            last_was_dash = true;
        } else {
            last_was_dash = false;
        }
        out.push(mapped);
    }
    // Rule 4: trim leading/trailing '-'.
    let trimmed = out.trim_matches('-');
    // Rule 5: non-empty fallback.
    if trimmed.is_empty() {
        EMPTY_FALLBACK.to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lowercases() {
        assert_eq!(slug("Sales"), "sales");
        assert_eq!(slug("BUDGET"), "budget");
    }

    #[test]
    fn replaces_spaces_and_unsafe_chars_with_dash() {
        assert_eq!(slug("Sales Plan"), "sales-plan");
        assert_eq!(slug("Q1/Q2 Forecast"), "q1-q2-forecast");
        assert_eq!(slug("a:b*c?d"), "a-b-c-d");
        assert_eq!(slug("price ($)"), "price");
    }

    #[test]
    fn collapses_repeats_and_trims_edges() {
        assert_eq!(slug("  Sales   Plan  "), "sales-plan");
        assert_eq!(slug("--Sales--"), "sales");
        assert_eq!(
            slug("a___b"),
            "a___b",
            "underscores are kept, not collapsed"
        );
        assert_eq!(slug("a---b"), "a-b");
    }

    #[test]
    fn keeps_digits_dashes_underscores() {
        assert_eq!(slug("cube_2024-v1"), "cube_2024-v1");
        assert_eq!(slug("123"), "123");
    }

    #[test]
    fn empty_and_all_unsafe_fall_back_to_a_stable_token() {
        assert_eq!(slug(""), EMPTY_FALLBACK);
        assert_eq!(slug("***"), EMPTY_FALLBACK);
        assert_eq!(slug("   "), EMPTY_FALLBACK);
        assert_eq!(slug("---"), EMPTY_FALLBACK);
        // Stable: the same empty-slugging input always maps to the same token.
        assert_eq!(slug("!!!"), slug("///"));
    }

    #[test]
    fn non_ascii_becomes_dash() {
        // Non-ASCII letters are not in [a-z0-9-_]; they map to '-' and collapse.
        assert_eq!(slug("Café"), "caf");
        assert_eq!(slug("naïve plan"), "na-ve-plan");
    }

    #[test]
    fn is_idempotent_on_an_already_slugged_name() {
        for s in ["sales", "sales-plan", "cube_2024-v1", "unnamed"] {
            assert_eq!(slug(s), s, "slug of an already-slugged name is itself");
        }
    }

    #[test]
    fn case_collision_slugs_to_the_same_folder() {
        assert_eq!(slug("Sales"), slug("sales"));
        assert_eq!(slug("Sales"), slug("SALES"));
    }
}
