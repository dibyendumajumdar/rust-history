// unit-test: DataflowConstProp
// EMIT_MIR_FOR_EACH_PANIC_STRATEGY
// EMIT_MIR_FOR_EACH_BIT_WIDTH

// EMIT_MIR large_array_index.main.DataflowConstProp.diff
fn main() {
    // check that we don't propagate this, because it's too large
    let x: u8 = [0_u8; 5000][2];
}
