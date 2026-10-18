//! Stable tab identities shared by mouse controls and hardware navigation.
use crate::state::DesktopSession;
use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tab {
    #[default]
    Attention,
    Codex,
    Others,
}
pub const DEFAULT_ORDER: [Tab; 3] = [Tab::Attention, Tab::Codex, Tab::Others];
pub fn validate_order(order: &[Tab]) -> Result<[Tab; 3], String> {
    if order.len() != 3
        || DEFAULT_ORDER
            .iter()
            .any(|tab| order.iter().filter(|item| *item == tab).count() != 1)
    {
        return Err("tab order must contain attention, codex and others exactly once".into());
    }
    Ok([order[0], order[1], order[2]])
}
pub fn is_codex(row: &DesktopSession) -> bool {
    matches!(row.agent_id.as_str(), "codex" | "chatgpt")
}
pub fn visible_order(order: &[Tab; 3], rows: &[DesktopSession]) -> Vec<Tab> {
    order
        .iter()
        .copied()
        .filter(|tab| {
            *tab != Tab::Attention
                || rows.iter().any(|row| {
                    row.identity.is_some()
                        && !row.stale
                        && row.status != crate::state::AgentStatus::Offline
                        && row.attention.is_some()
                })
        })
        .collect()
}
pub fn fallback_tab(order: &[Tab; 3], rows: &[DesktopSession]) -> Tab {
    let visible = visible_order(order, rows);
    visible
        .iter()
        .copied()
        .find(|tab| {
            rows.iter().any(|row| {
                row.identity.is_some()
                    && !row.stale
                    && row.status != crate::state::AgentStatus::Offline
                    && tab.matches(row)
            })
        })
        .unwrap_or(visible[0])
}
impl Tab {
    pub fn matches(self, row: &DesktopSession) -> bool {
        match self {
            Self::Attention => row.attention.is_some(),
            Self::Codex => is_codex(row),
            Self::Others => !is_codex(row),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_requires_all_three_unique_ids_and_roundtrips() {
        assert_eq!(
            validate_order(&[Tab::Attention, Tab::Others, Tab::Codex]).unwrap(),
            [Tab::Attention, Tab::Others, Tab::Codex]
        );
        assert!(validate_order(&[Tab::Attention, Tab::Codex]).is_err());
        assert!(validate_order(&[Tab::Attention, Tab::Codex, Tab::Codex]).is_err());
        let json = serde_json::to_string(&[Tab::Codex, Tab::Attention, Tab::Others]).unwrap();
        assert_eq!(
            serde_json::from_str::<[Tab; 3]>(&json).unwrap(),
            [Tab::Codex, Tab::Attention, Tab::Others]
        );
        assert!(serde_json::from_str::<Tab>("\"all\"").is_err());
    }
}
