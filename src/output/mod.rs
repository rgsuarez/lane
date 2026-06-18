//! Rendering the board for humans (`human`) and machines (`json`).

pub mod human;
pub mod json;

use crate::model::Board;

/// Output format for `lane board`.
pub enum OutputFormat {
    Human,
    Json,
}

/// Render a board in the chosen format.
pub fn render(board: &Board, fmt: OutputFormat) -> anyhow::Result<String> {
    match fmt {
        OutputFormat::Human => Ok(human::render(board)),
        OutputFormat::Json => json::render(board),
    }
}
