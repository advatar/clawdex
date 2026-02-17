use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum RiskLevel {
    Low,
    Medium,
    High,
    Critical,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ActionIntent {
    pub plugin: String,
    pub tool: String,
    pub summary: String,
}

pub fn requires_checkpoint(intent: &ActionIntent) -> bool {
    matches!(intent.tool.as_str(), "shell" | "network" | "file_delete")
}

pub fn log_checkpoint(intent: &ActionIntent) {
    println!("Checkpoint required: {:?}", intent);
}

#[cfg(test)]
mod tests {
    use super::{requires_checkpoint, ActionIntent};

    #[test]
    fn requires_checkpoint_for_high_risk_tools() {
        let shell = ActionIntent {
            plugin: "sample".to_string(),
            tool: "shell".to_string(),
            summary: "run command".to_string(),
        };
        assert!(requires_checkpoint(&shell));

        let read = ActionIntent {
            plugin: "sample".to_string(),
            tool: "read".to_string(),
            summary: "read file".to_string(),
        };
        assert!(!requires_checkpoint(&read));
    }
}
