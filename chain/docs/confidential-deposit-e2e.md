# Running the adapter end to end

This has not been run yet. The author's environment had no Solana toolchain, so these steps and
`tests/confidential_deposit.test.mjs` are unverified. Expect to fix small things on the first run.
Everything is local: a validator on 127.0.0.1, no real funds, no mainnet.

## Machine

8+ cores, 16 GB RAM, Linux or macOS, about 20 GB of disk. Building SBF programs and Anchor from
source is heavy.

## Dependencies (versions the repo pins)

- Rust stable (rustup) and `cargo`.
- Solana CLI / Agave 2.x with `cargo-build-sbf` and `solana-test-validator`
  (`sh -c "$(curl -sSfL https://release.anza.xyz/stable/install)"`).
- Anchor 0.31.1 (see `Anchor.toml`): `cargo install --git https://github.com/coral-xyz/anchor avm --force`,
  then `avm install 0.31.1 && avm use 0.31.1`.
- Node 20+ and `npm`.

## Steps, from the `chain/` directory

1. Install JS deps: `npm ci`
2. Give the adapter a real program id (the one in the source is a placeholder):
   `anchor keys sync`
   This writes a keypair to `target/deploy/confidential_deposit-keypair.json` and rewrites
   `declare_id!` and `Anchor.toml`. Do not commit that keypair.
3. Build both programs and their IDLs:
   `anchor build -p acrux_bridge && anchor build -p confidential_deposit`
   Check that `target/deploy/acrux_bridge.so`, `target/deploy/confidential_deposit.so`,
   `target/idl/acrux_bridge.json` and `target/idl/confidential_deposit.json` exist.
4. Run the native unit tests (these do not need a validator):
   `cargo test -p confidential_deposit --lib`   (expect 19 passed)
5. Run the end-to-end test. `tests/run.mjs` starts the local validator with the bridge and the
   adapter loaded, runs the file, and stops the validator:
   `node tests/run.mjs tests/confidential_deposit.test.mjs`
6. Optional, to be sure the bridge itself is unchanged: `node tests/run.mjs tests/acrux_bridge.test.mjs`

## What a pass shows

- A deposit through the adapter writes a bridge `DepositRecord` whose `depositor` is the adapter PDA,
  with the right recipient, mint and amount.
- The bridge vault holds the tokens and the adapter's escrow is back to 0.
- Amount 0, bad recipients, a wrong-mint or foreign source account, a wrong record sequence and an
  over-balance amount are rejected and leave balances and the deposit counter unchanged.
- The user's wallet is still present in the transaction. That is expected: see
  `confidential-deposit.md`.

## Not covered

Token-2022 transfer fees through the adapter (the unit tests cover the net-amount arithmetic only),
a CPI failure inside the bridge beyond the cases above, and any measurement of fees or latency.
