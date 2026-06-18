//! JSON rendering of the board (schema_version 0, unstable). No network, pure serialization.

use crate::model::Board;

/// Serialize the board as pretty JSON with a trailing newline.
pub fn render(board: &Board) -> anyhow::Result<String> {
    Ok(format!("{}\n", serde_json::to_string_pretty(board)?))
}
