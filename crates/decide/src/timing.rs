//! Capture provenance, independent of speaker labels.
use serde::{Deserialize, Serialize};
/// Capture identity, independent of inferred speaker identity. Unknown sources
/// are external.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionSource {
    Microphone,
    #[default]
    External,
}
