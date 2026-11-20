pub const STITCH_ATTRIBUTION: &str =
    "Stitch<sup>[^](https://github.com/thealternator89/stitch/#readme)</sup>";
pub const COPILOT_ATTRIBUTION: &str =
    "GitHub Copilot<sup>[^](https://github.com/features/copilot)</sup>";

pub fn get_attribution_statement(edited: bool) -> String {
    if edited {
        format!("> Assisted by {STITCH_ATTRIBUTION} and {COPILOT_ATTRIBUTION}.")
    } else {
        format!("> Generated with {STITCH_ATTRIBUTION} and {COPILOT_ATTRIBUTION}.")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attribution_statement_reflects_edited_flag() {
        let generated = get_attribution_statement(false);
        assert!(generated.starts_with("> Generated with "));
        assert!(generated.contains(STITCH_ATTRIBUTION));
        assert!(generated.contains(COPILOT_ATTRIBUTION));

        let assisted = get_attribution_statement(true);
        assert!(assisted.starts_with("> Assisted by "));
        assert!(assisted.contains(STITCH_ATTRIBUTION));
        assert!(assisted.contains(COPILOT_ATTRIBUTION));
    }
}
