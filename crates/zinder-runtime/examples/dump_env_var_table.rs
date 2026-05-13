//! One-off helper to dump the rendered env-var table.

#![allow(
    clippy::print_stdout,
    reason = "Local helper to render the canonical env-var table for insertion into docs."
)]

fn main() {
    print!("{}", zinder_runtime::render_environment_variable_table());
}
