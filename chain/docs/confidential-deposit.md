# Confidential deposit adapter (phase 1)

Status: local code, unit tests only. Not deployed anywhere. The program id in `declare_id!` is a
placeholder and must be replaced by the key of the real deploy.

## What it does

A direct bridge deposit writes a `DepositRecord` and a `Deposited` event holding the depositor's
wallet next to the Acrux recipient. The adapter changes who the depositor is.

One instruction, `deposit(amount, recipient)`, in one Solana transaction:

1. the user pays the adapter PDA the lamports the bridge will charge its depositor (record rent,
   and the vault's rent on a mint's first deposit);
2. the user's tokens move into a per-mint escrow token account owned by the adapter PDA;
3. the adapter PDA, signing with its seeds, calls `acrux_bridge::deposit` with the escrow as
   source. The bridge records the adapter PDA as `depositor`.

The amount forwarded is what the escrow received (so Token-2022 transfer fees are handled), and the
instruction fails if the escrow does not return to its prior balance. Funds from other deposits that
sit in the escrow are never touched.

`recipient` should be a fresh Acrux key made for this deposit and never used before.

## What is protected

- Reading the bridge account data or the `Deposited` event does not show the user's wallet. The
  depositor field is the same adapter PDA for every user.
- The escrow address depends on the mint only, not on the user.

## What still leaks

This is not anonymity. Be exact about it.

- **Amount.** The transfer into the escrow and the bridge deposit both carry the amount, in clear.
- **Timing.** Both happen in the transaction's slot.
- **Wallet and recipient together.** The user's wallet signs the same transaction that carries the
  recipient as instruction data. Anyone reading the transaction, not just the bridge accounts, sees
  wallet, amount and recipient side by side. Indexers and explorers show transactions.
- **Fee payer and rent top-up.** The user's wallet pays the fee and funds the adapter PDA. That
  transfer is public.
- **Acrux side.** What an Acrux validator or observer can link between the credited recipient and its
  later activity is unchanged by this program.
- **The bridge deposit sequence.** Deposits are numbered. With low volume, amount plus timing can be
  matched between the Solana side and the recipient's first activity on Acrux.

So the adapter hides the wallet only from consumers of the bridge's records. It does not hide it from
someone who reads the transaction. Real unlinking needs the user's transaction to stop naming the
recipient: for example a relayer that submits deposits, or deposits made from a fresh funded wallet.
Those are later phases and are not in this code.

## Not covered by phase 1

- Token-2022 mints with extensions other than the transfer fee: the vault and escrow are sized as base
  accounts, and a mint needing more space fails at account creation rather than silently.
- No relayer, no batching, no delay, no denominations.
- No fee or latency measurements. None were taken, and none are claimed.

## Tests

`cargo test -p confidential_deposit --lib` runs 19 native tests: the rent top-up arithmetic, the net-amount
and escrow-drained checks (including a Token-2022 fee cutting the amount, a zero or falling balance, and
u64 extremes), the error codes, that the PDAs are off-curve, the discriminator, the
recipient checks, the PDA derivation, the record size against the bridge's own `INIT_SPACE`, and the
exact bridge instruction and account list the adapter builds (the depositor is the adapter PDA and a
signer, and the user's key appears nowhere in it).

The program has not been run on a validator: the sandbox this was written in cannot build SBF
binaries. An end-to-end test, `tests/confidential_deposit.test.mjs` (steps in `confidential-deposit-e2e.md`), is written but has never been run;
running it needs a machine with the Solana toolchain.
