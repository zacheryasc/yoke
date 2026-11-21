use std::collections::BTreeMap;

/// Aggregated, persistable view of one agent invocation's stream.
///
/// Produced after the child's stdout closes. Carries everything the metrics
/// layer wants: cost, agent-reported wall time, thinking total, and per-tool
/// durations + counts.
#[derive(Debug, Clone, Default)]
pub struct StreamSummary {
    pub cost_usd: f64,
    /// Wall clock as reported by the agent's stream, when available.
    pub agent_reported_secs: Option<f64>,
    pub num_turns: u32,
    pub thinking_secs: f64,
    pub tool_counts: BTreeMap<String, u32>,
    pub tool_durations_secs: BTreeMap<String, f64>,
}
