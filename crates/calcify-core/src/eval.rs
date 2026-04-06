//! The evaluator — runs compiled ticks against the flat state representation.
//!
//! This is the hot loop. Each tick:
//! 1. Decode the opcode at IP
//! 2. Parse ModRM if needed
//! 3. Resolve instruction arguments
//! 4. Execute (compute destination address + value)
//! 5. Write result to state
//! 6. Advance IP

use crate::state::State;
use crate::types::CompiledProgram;

/// The main evaluator that runs compiled CSS programs.
#[derive(Debug)]
pub struct Evaluator {
    pub program: CompiledProgram,
}

/// The result of running a batch of ticks — the set of properties that changed.
#[derive(Debug, Clone, Default)]
pub struct TickResult {
    /// Property changes as (name, value) pairs for DOM application.
    pub changes: Vec<(String, String)>,
    /// Number of ticks executed in this batch.
    pub ticks_executed: u32,
}

impl Evaluator {
    pub fn new(program: CompiledProgram) -> Self {
        Self { program }
    }

    /// Run a single tick of the emulator.
    pub fn tick(&self, _state: &mut State) -> TickResult {
        // TODO(phase-2): Implement the tick loop.
        //
        // 1. let opcode = state.read_mem(state.registers[IP])
        // 2. let inst_id = self.program.decode_table[&opcode]
        // 3. Parse ModRM if inst.has_modrm
        // 4. Resolve args
        // 5. Execute instruction
        // 6. Write result
        // 7. Advance IP
        todo!("Phase 2: Tick evaluation")
    }

    /// Run a batch of ticks and return the combined result.
    pub fn run_batch(&self, state: &mut State, count: u32) -> TickResult {
        let mut result = TickResult::default();
        for _ in 0..count {
            let tick_result = self.tick(state);
            result.changes = tick_result.changes;
            result.ticks_executed += 1;
        }
        result
    }
}
