//! Prints the address derived from the regtest broadcast cycle's test seed.
//!
//! Operators copy the address into Zebra's `ZEBRA_MINING__MINER_ADDRESS`
//! environment variable (or `[mining] miner_address` in `zebrad.toml`) so
//! the live broadcast and reorg tests find a coinbase output they can
//! spend.

use zinder_testkit::TransparentTestKey;

#[allow(
    clippy::print_stdout,
    reason = "operator-facing binary that emits one line of structured config data"
)]
fn main() -> eyre::Result<()> {
    let test_seed = [0x42_u8; 32];
    let key = TransparentTestKey::from_seed(&test_seed)
        .map_err(|error| eyre::eyre!("could not derive test key: {error}"))?;
    println!("{}", key.address_base58());
    Ok(())
}
