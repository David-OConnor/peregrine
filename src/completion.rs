//! Input-row autocomplete. Tab completion and the greyed suggestion are the
//! shell library's (see [shell::complete_input], [shell::input_hint]), shared
//! with the CLI so both complete and suggest alike; this is only how the GUI
//! lists candidates.

use shell::CompletionResult;

/// The candidates of an ambiguous completion, for listing on a second Tab.
pub fn candidate_list(result: &CompletionResult) -> String {
    let names: Vec<&str> = result
        .candidates
        .iter()
        .map(|c| c.display.as_str())
        .collect();
    names.join("   ")
}
