//! Conformance testing harness for comparing calc(ify) output against Chrome.
//!
//! The conformance test protocol:
//! 1. Chrome baseline: Run x86CSS in Chrome, capture state after each tick via
//!    getComputedStyle(). Save as JSON snapshots.
//! 2. Engine comparison: Run the same CSS through calc(ify) for the same number
//!    of ticks. Compare state after each tick against Chrome snapshots.
//! 3. Divergence: Binary-search to find the first divergent tick and property.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::state::State;

/// A snapshot of the machine state at a specific tick.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StateSnapshot {
    pub tick: u32,
    pub registers: RegisterSnapshot,
    /// Changed memory addresses and their values (sparse — only non-zero or changed).
    #[serde(default)]
    pub memory: Vec<MemoryEntry>,
}

/// Register values at a point in time (dynamically extracted from State).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RegisterSnapshot {
    #[serde(flatten)]
    pub values: BTreeMap<String, i32>,
}

/// A single memory address and its value.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MemoryEntry {
    pub address: u32,
    pub value: u8,
}

/// Result of comparing two snapshots.
#[derive(Debug, Clone)]
pub struct SnapshotDiff {
    pub tick: u32,
    pub register_diffs: Vec<RegisterDiff>,
    pub memory_diffs: Vec<MemoryDiff>,
}

#[derive(Debug, Clone)]
pub struct RegisterDiff {
    pub name: String,
    pub expected: i32,
    pub actual: i32,
}

#[derive(Debug, Clone)]
pub struct MemoryDiff {
    pub address: u32,
    pub expected: u8,
    pub actual: u8,
}

impl StateSnapshot {
    /// Capture a snapshot from the current engine state.
    pub fn from_state(state: &State, tick: u32) -> Self {
        Self {
            tick,
            registers: RegisterSnapshot::from_state(state),
            memory: Vec::new(), // Populated by capture_memory if needed
        }
    }

    /// Capture a snapshot including all non-zero memory.
    pub fn from_state_with_memory(state: &State, tick: u32) -> Self {
        let memory = state
            .memory
            .iter()
            .enumerate()
            .filter(|(_, &v)| v != 0)
            .map(|(addr, &value)| MemoryEntry {
                address: addr as u32,
                value,
            })
            .collect();

        Self {
            tick,
            registers: RegisterSnapshot::from_state(state),
            memory,
        }
    }

    /// Compare this snapshot against another, returning differences.
    pub fn diff(&self, other: &StateSnapshot) -> Option<SnapshotDiff> {
        let register_diffs = self.registers.diff(&other.registers);
        let memory_diffs = diff_memory(&self.memory, &other.memory);

        if register_diffs.is_empty() && memory_diffs.is_empty() {
            None
        } else {
            Some(SnapshotDiff {
                tick: self.tick,
                register_diffs,
                memory_diffs,
            })
        }
    }
}

impl RegisterSnapshot {
    /// Create a snapshot from the current State's state variables.
    pub fn from_state(state: &State) -> Self {
        let mut values = BTreeMap::new();
        for (i, name) in state.state_var_names.iter().enumerate() {
            values.insert(name.clone(), state.state_vars[i]);
        }
        Self { values }
    }

    /// Apply this snapshot to a State, restoring all captured variables.
    pub fn apply_to(&self, state: &mut State) {
        for (name, &value) in &self.values {
            state.set_var(name, value);
        }
    }

    /// Compare two snapshots and return a list of differences.
    fn diff(&self, other: &RegisterSnapshot) -> Vec<RegisterDiff> {
        let mut diffs = Vec::new();
        for (name, &expected) in &self.values {
            let actual = other.values.get(name).copied().unwrap_or(0);
            if expected != actual {
                diffs.push(RegisterDiff {
                    name: name.clone(),
                    expected,
                    actual,
                });
            }
        }
        diffs
    }
}

fn diff_memory(expected: &[MemoryEntry], actual: &[MemoryEntry]) -> Vec<MemoryDiff> {
    use std::collections::HashMap;

    let expected_map: HashMap<u32, u8> = expected.iter().map(|e| (e.address, e.value)).collect();
    let actual_map: HashMap<u32, u8> = actual.iter().map(|e| (e.address, e.value)).collect();

    let mut diffs = Vec::new();
    for (&addr, &exp_val) in &expected_map {
        let act_val = actual_map.get(&addr).copied().unwrap_or(0);
        if exp_val != act_val {
            diffs.push(MemoryDiff {
                address: addr,
                expected: exp_val,
                actual: act_val,
            });
        }
    }
    for (&addr, &act_val) in &actual_map {
        if !expected_map.contains_key(&addr) && act_val != 0 {
            diffs.push(MemoryDiff {
                address: addr,
                expected: 0,
                actual: act_val,
            });
        }
    }

    diffs.sort_by_key(|d| d.address);
    diffs
}

/// A collection of state snapshots for conformance testing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConformanceTrace {
    /// Source CSS file hash (for cache validation).
    pub css_hash: String,
    /// Chrome version used for baseline capture.
    pub chrome_version: String,
    /// Snapshots at each tick.
    pub snapshots: Vec<StateSnapshot>,
}

impl ConformanceTrace {
    /// Load a trace from a JSON file.
    pub fn load(path: &str) -> crate::Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&content)?)
    }

    /// Save a trace to a JSON file.
    pub fn save(&self, path: &str) -> crate::Result<()> {
        let content = serde_json::to_string_pretty(self)?;
        std::fs::write(path, content)?;
        Ok(())
    }
}

impl std::fmt::Display for SnapshotDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Tick {} diverged:", self.tick)?;
        for d in &self.register_diffs {
            writeln!(
                f,
                "  {} expected={} actual={}",
                d.name, d.expected, d.actual
            )?;
        }
        for d in &self.memory_diffs {
            writeln!(
                f,
                "  mem[{}] expected={} actual={}",
                d.address, d.expected, d.actual
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a state with standard x86 state variables for testing.
    fn test_state() -> State {
        let mut state = State::new(1536);
        // Manually add the standard x86 state variables that CSS would define.
        // The state_var_index is private, so we use the public API if available.
        // For testing, we populate state_vars and state_var_names directly
        // (the index will be checked when set_var is called).

        for name in ["AX", "CX", "DX", "BX", "SP", "BP", "SI", "DI", "IP", "ES", "CS", "SS", "DS", "flags"].iter() {
            state.state_vars.push(0);
            state.state_var_names.push(name.to_string());
        }
        state
    }

    #[test]
    fn snapshot_roundtrip() {
        let mut state = test_state();
        // Set values using direct mutation since state_var_index is private
        state.state_vars[0] = 0x1234; // AX
        state.state_vars[8] = 0x100;  // IP
        state.memory[0] = 0xFF;
        state.memory[1] = 0x42;

        let snapshot = StateSnapshot::from_state_with_memory(&state, 0);
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored: StateSnapshot = serde_json::from_str(&json).unwrap();

        assert_eq!(snapshot, restored);
        assert_eq!(restored.registers.values.get("AX").copied(), Some(0x1234));
        assert_eq!(restored.memory.len(), 2);
    }

    #[test]
    fn snapshot_diff_detects_changes() {
        let mut state1 = test_state();
        state1.state_vars[0] = 100; // AX

        let mut state2 = test_state();
        state2.state_vars[0] = 200; // AX

        let snap1 = StateSnapshot::from_state(&state1, 0);
        let snap2 = StateSnapshot::from_state(&state2, 0);

        let diff = snap1.diff(&snap2).expect("should differ");
        assert_eq!(diff.register_diffs.len(), 1);
        assert_eq!(diff.register_diffs[0].name, "AX");
        assert_eq!(diff.register_diffs[0].expected, 100);
        assert_eq!(diff.register_diffs[0].actual, 200);
    }

    #[test]
    fn identical_snapshots_no_diff() {
        let state = test_state();
        let snap1 = StateSnapshot::from_state(&state, 0);
        let snap2 = StateSnapshot::from_state(&state, 0);

        assert!(snap1.diff(&snap2).is_none());
    }
}
