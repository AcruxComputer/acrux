# Security Review: Acrux bridge, node, RPC and faucet

**Date:** 2026-11-18
**Reviewer:** @newerasolanamaxi (community review, not a professional audit)
**Code under review:** `AcruxComputer/acrux` at HEAD `7e41b54`
**Method:** reading the source, running the repo's own native Rust tests, and read-only JSON-RPC calls against the public gateway (`https://rpc.acrux.computer`). No fuzzing against live services, no transactions sent to any live network, no on-chain deployment of the bridge was tested. Findings marked "pre-deploy" were confirmed by code reading only; the bridge program `EDm6HxGArNEgGfWRZwvj42f4euc9oPQWVUwUbkK3PEe5` is not deployed on Solana mainnet or devnet at the time of writing, so no user funds are at risk today.

This file collects the findings reported as issues #4 through #12, maps the codebase against the failure modes behind the six largest bridge exploits, and lists concrete recommendations. Each issue contains the full technical detail, reproduction steps and suggested fixes.

## 1. Findings

| Issue | Severity | Component | Summary | Status |
|---|---|---|---|---|
| [#5](https://github.com/AcruxComputer/acrux/issues/5) | Critical (pre-deploy) | `acrux_bridge` | `initialize` is permissionless and the authority can seed a validator set for any epoch. Whoever initializes first controls the light client's trust root. | Fix proposed in [PR #10](https://github.com/AcruxComputer/acrux/pull/10) (open) |
| [#12](https://github.com/AcruxComputer/acrux/issues/12) | Medium (pre-deploy) | `acrux_bridge` | The program upgrade authority is single-key custody of every vault. A standard upgradeable deploy means one key can replace the code and drain all vaults, with no votes or proofs. | Open; deployment decision |
| [#7](https://github.com/AcruxComputer/acrux/issues/7) | Medium (pre-deploy) | `acrux_bridge` | `deposit` accepts any mint, including Token-2022 mints with permanent delegate, freeze authority or transfer hooks. A hostile mint can move or freeze vaulted funds after claims are minted. | Open |
| [#6](https://github.com/AcruxComputer/acrux/issues/6) | Low (pre-deploy) | `acrux_bridge` | A validator set with total stake 0 certifies any cblock without signatures; the outbox Merkle tree has no leaf/node domain separation. | Open |
| [#4](https://github.com/AcruxComputer/acrux/issues/4) | Low | RPC gateway (live) | `dataSlice` is ignored by `getProgramAccounts` and other account reads; standard clients download far more data than requested. | Open |
| [#8](https://github.com/AcruxComputer/acrux/issues/8) | Low | Faucet (live) | Per-address and per-IP limits can be bypassed with concurrent requests; the check and the grant record are not atomic, and the grant is sent before it is recorded. | Open |
| [#9](https://github.com/AcruxComputer/acrux/issues/9) | Low | Threshold transactions | One bad fragment makes a t-of-n transaction undecodable: no per-fragment verification, no subset retry, first-wins cache rejects the correct fragment. | Open |
| [#11](https://github.com/AcruxComputer/acrux/issues/11) | Low / Info | RPC gateway (live) | `getEpochInfo` reports Solana mainnet's fixed 432000-slot epoch while the chain's validator-set epoch is 1536 slots; `getEpochSchedule` is not implemented. | Open |

Earlier contributions in the same review effort: [PR #1](https://github.com/AcruxComputer/acrux/pull/1) (threshold test vectors and negative tests), [PR #2](https://github.com/AcruxComputer/acrux/pull/2) (confidential deposit adapter, draft), [PR #3](https://github.com/AcruxComputer/acrux/pull/3) (PRIVACY.md), [PR #10](https://github.com/AcruxComputer/acrux/pull/10) (fix for #5).

## 2. Lessons from the big bridge exploits, mapped onto Acrux

The six largest bridge hacks together cost over $2B. Each maps to a small number of root causes. This section checks each root cause against the Acrux bridge code as written today.

| Exploit | Loss | Root cause | Status in Acrux |
|---|---|---|---|
| Wormhole (Feb 2022) | ~$326M | Signature verification bypass: a fake SignatureSet account was accepted as verified. | **Not vulnerable.** `attest_cblock` reads the instructions sysvar with a checked address, uses `load_*_checked`, requires the signature, pubkey and message to live inside the ed25519 instruction itself, checks the ed25519 program ID, and the vote message binds validator index, slot and cblock hash (no replay across cblocks). |
| Nomad (Aug 2022) | ~$190M | Initialization bug: a zero root counted as confirmed, so any message could be forged. | **Same pattern class found and reported.** No zero-root acceptance exists in `hand_off` (proofs must match the outbox root), but the "a zero/empty value is accepted" pattern appears in #5 (permissionless initialize) and #6 (zero-stake validator set auto-certifies). Both are pre-deploy. |
| Poly Network (Aug 2021) | ~$611M | Cross-chain manager let an attacker replace the keeper set through an unprotected privileged call. | **No equivalent gap found.** Privileged bridge operations are gated on the authority and on certified cblocks. The closest exposure is key custody, see #12. |
| BNB Token Hub (Oct 2022) | ~$586M | Forged Merkle proof accepted by the IAVL verification path. | **Not vulnerable as written.** The outbox tree is a plain SHA-256 Merkle tree, proofs are checked against the root committed in a certified cblock, and replay protection is present. The missing leaf/node domain separation (#6) is a hardening item, not a practical collision today. |
| Ronin (Mar 2022) | ~$625M | Key compromise: 5 of 9 validator keys, including an allowlisted RPC node, were enough to sign anything. | **Same failure class as #12.** Consensus-level votes in Acrux are stake-weighted and verified, but the program upgrade authority is a single key with total control over every vault. Multisig plus timelock, or an immutable deploy, removes the class. |
| Multichain (Jul 2023) | ~$125M | MPC keys held by one person; single-party custody of the whole bridge. | **Same failure class as #12.** This is the one pattern where Acrux, if deployed as a standard upgradeable program, would match the exploited designs exactly. |

**Bottom line:** the cryptographic verification core of the bridge (signature checks, Merkle proofs, replay protection) is in good shape relative to the bridges that were exploited. The real exposure is operational: who controls `initialize` (#5, fixed in PR #10) and who holds the upgrade authority at deploy time (#12).

## 3. Recommendations

Ordered by importance:

1. **Merge PR #10 before any deployment.** It gates `initialize` to the upgrade authority and makes validator-set seeding one-shot. Without it, the first caller after deploy owns the bridge (#5, Critical).
2. **Decide key custody before deploying (#12).** Either put the upgrade authority behind a documented multisig (for example Squads) with a timelock on upgrades that touch vault logic, or deploy the program immutable. Note that PR #10 makes the upgrade authority the seed authority, which makes this decision more important, not less.
3. **Reject or gate risky mints (#7).** Add an allowlist, or reject Token-2022 mints that carry permanent delegate, freeze authority or transfer hooks.
4. **Require `total > 0` for validator sets and add Merkle domain separation (#6).** Both are small code changes: `require!(total > 0)` in `init_validator_set` and `hand_off`, and `0x00`/`0x01` prefixes for leaves and inner nodes.
5. **Honor `dataSlice` (and `filters`) in account reads (#4).** Apply offset/length in the shared `account_json` path so standard Solana clients behave as documented.
6. **Make faucet limiting atomic (#8).** Count and record the grant in one transaction, record before paying, and keep the faucet from spending the built-in traffic senders' funds.
7. **Verify fragments before caching them (#9).** Cover fragment bytes and key shares with a signature or commitment, and let `reconstruct` retry with a different subset when authentication fails.
8. **Report the real epoch schedule (#11).** Either derive `getEpochInfo` from the configured `windows_per_epoch` or implement `getEpochSchedule` so clients can discover the validator-set epoch length.

## 4. Scope and honest limits

- This is a volunteer code review, not a professional audit. Absence of a finding here does not mean absence of bugs.
- All bridge findings (#5, #6, #7, #12) come from reading the source at HEAD `7e41b54`. The program is not deployed on mainnet or devnet, so nothing was executed against live funds and severities are stated for the code as written if deployed unchanged.
- Live findings (#4, #11) were verified with read-only RPC calls. The faucet finding (#8) and the threshold finding (#9) come from code reading only; no write calls were made against live services.
- The consensus loop, P2P networking and the full transaction execution path have been read but not exercised in a multi-validator setting; a testnet deployment with adversarial validators would be the natural next step.
- Feedback and corrections are welcome on the individual issues. Happy to adjust or retract anything that misreads the code.
