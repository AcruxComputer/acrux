# Privacy: what Acrux protects and what it does not

This page states the privacy properties of Acrux as the code implements them, so users and
integrators can tell the guarantees from the limits. It is based on reading the repository at
`1349b98` (node 0.4.9). File references are given so each statement can be checked. It makes no claim about
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

## Zero-knowledge proofs: what Acrux uses today, what it does not

**Today: none.** The repository contains no zero-knowledge proof system (no SNARK, STARK, Bulletproof
or similar) in the node, the SDK or the Solana programs. What can look like proof checking is not ZK:

- **Bridge withdrawals** (`chain/programs/acrux_bridge/src/lib.rs`, `withdraw` and `release`). On Acrux,
  `withdraw` burns the token and records a numbered withdrawal. A cblock's outbox root commits to it.
  On Solana, `release` checks that the cblock is certified, rebuilds the withdrawal entry, and checks
  a **SHA-256 Merkle inclusion proof** against the cblock's `outbox_root`. It then pays from the vault
  once per sequence.
- **Light client.** A cblock is certified when validators holding 60% of the stake signed its Finalize
  vote (80% for Notar), checked through Solana's ed25519 precompile. This is signature-threshold
  verification, so it reveals everything it proves. The trust assumption is the validator set, not a
  cryptographic proof of execution.
- **Threshold encryption** (above) is symmetric encryption plus secret sharing. It is not ZK.

**Gaps.** There are no private balances, no private transfers, no shielded state, and no proof that
hides sender, recipient or amount. A withdrawal's recipient and amount are public in the outbox.

## Possible adoption path

Each step says whether it can be built now or depends on something outside this repository.
These are suggestions, not commitments, and none of them is implemented here.

| Step | What | Can start now? |
| --- | --- | --- |
| 1 | Confidential deposit adapter: the bridge's depositor becomes a program account instead of the user's wallet. A draft is in #2. It does not hide amount, timing or the transaction, and it is not ZK. | Yes. Needs an on-chain test run. |
| 2 | Private balances on the Solana side through an external ZK system. Helius's Solana Privacy Rings (open-source `helius-labs/zolana`) describe a Default Ring with private asset and amount but public sender and recipient, and an anonymous ring that hides sender and recipient too. Per their public docs the anonymous ring has not shipped yet, and Helius's target was Q4 2026. Check https://www.helius.dev/docs/privacy for the current status before relying on it. I have not verified which proof system it uses or how it would compose with the bridge. | Partly. The confidential level can be evaluated now. The anonymous level waits for the external release. |
| 3 | Beyond that: ZK compression for state, and proof aggregation so the bridge verifies one proof rather than many signatures. These are open design questions here: nothing in the code needs them yet, and the cost on Solana would have to be measured. | No. Design work first. |

The order matters: step 1 removes the wallet from bridge records at small cost, step 2 is what would
make amounts private, and step 3 is optimization that only makes sense once something needs it.

Corrections welcome. If any statement above does not match the code, please say so and I will fix it.
