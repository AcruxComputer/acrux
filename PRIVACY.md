# Privacy: what Acrux protects and what it does not

This page states the privacy properties of Acrux as the code implements them, so users and
integrators can tell the guarantees from the limits. It is based on reading the repository at
`198f622`. File references are given so each statement can be checked. It makes no claim about
anything deployed that the repository does not show.

Short version: Acrux encrypts transactions until their order is fixed, to protect against
front-running. It is not a confidential-transaction chain. Once a slot executes, state and
amounts are public.

## What is protected

**Threshold-encrypted transactions** (`node/crates/acrux-execution/src/threshold.rs`,
`sdk/src/threshold.ts`).

- The transaction payload (instruction data, amounts) is sealed with ChaCha20-Poly1305, split into
  Reed-Solomon shards, and the key is split with Shamir sharing over GF(256), with `2 <= t <= n <= 3`.
- Fewer than `t` fragments do not reveal the payload. A tampered shard, key share or nonce fails
  authentication. The header is signed by the fee payer, so a changed header is rejected.
- Fragments expire (`fragment_expiry_slots`).
- Rust and TypeScript are tested for byte compatibility in both directions
  (`sdk/test/vectors/`).

**Slot-level encryption at consensus** (`node/crates/acrux-crypto/src/lib.rs`). Keys are released in
attestations only after commitments, so ordering cannot depend on payload contents.

Effect: a party holding fewer than `t` fragments cannot read or front-run a transaction before it is
ordered. This holds only if fewer than `t` of the chosen receivers collude.

## What still leaks

1. **The transaction header is in the clear.** Fee payer, writable accounts, priority fee per compute
   unit, compute limit and target slot are authenticated data, not encrypted. Who pays and which
   accounts are touched is visible before the slot commits. This is needed for fee reservation, but
   it is a privacy limit.
2. **Execution is public.** After a slot runs, balances, transfers and program state are readable by
   anyone. There are no confidential balances and no anonymity set.
3. **Timing and size.** When fragments are sent, to whom, and how large they are, are visible to the
   receivers and to network observers.
4. **Receiver collusion.** `t` colluding receivers can decrypt. With `n <= 3` the set is small.
5. **The bridge links wallets to recipients** (`chain/programs/acrux_bridge/src/lib.rs`). Every deposit
   writes a `DepositRecord` and a `Deposited` event with the depositor, the Acrux recipient, the mint,
   the amount and the slot. Records are numbered, so all deposits can be listed. Using a fresh
   recipient helps only if the depositor wallet is not otherwise tied to you. The amount and slot
   remain public either way.
6. **The bridge trust anchor.** `initialize` makes its payer the authority that seeds the first validator
   set of the light client. Until that set is replaced by hand-offs, trust rests on that seed.

## Suggested next steps, smallest first

1. Link this page from `README.md` and the SDK threshold section, so the limits are stated where the
   feature is advertised.
2. In `chain/scripts/bridge-deposit.mjs`, default to a fresh Acrux recipient and warn when the
   recipient equals an address already linked to the depositor.
3. Add an adapter so the bridge's depositor is a program account rather than the user's wallet
   (a draft is in a separate pull request). This removes the wallet from the bridge records only. It
   does not hide the transaction, the amount or the timing.
4. Consider encrypting header fields that do not need to be public, or documenting which ones the
   scheduler needs.
5. Longer term: a relayer or batching for bridge deposits, and confidential balances if the goal is
   anonymity rather than front-running protection.

Corrections welcome. If any statement above does not match the code, please say so and I will fix it.
