# Acrux chain

The Solana programs of Acrux, in one Anchor 0.31.1 workspace.

| Program | Runs on | Id |
|---|---|---|
| `hub_registry` | Solana (local validator now, mainnet next) | `ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm` |
| `demo_pool` | Acrux Net (the node's embedded SVM) | `CUBR9LNdTwxwp5ARBQj1EopfuH8dr8u4i7DsXcb8HbHN` |

| Path | Holds |
|---|---|
| `programs/hub_registry/`, `programs/demo_pool/` | program sources |
| `genesis/acrux-net.json` | Acrux Net genesis accounts for the demo pool |
| `scripts/validator.sh`, `scripts/init-localnet.mjs` | the local validator and the registry initialized on it |
| `scripts/init-mainnet.mjs` | `initialize` on mainnet with the production values |
| `scripts/registry.mjs` | registry PDAs, the local configuration, `initialize` |
| `scripts/genesis.mjs`, `scripts/dev-keys.mjs` | the genesis writer and the development keys |
| `tests/` | the tests and their runner |

## Build

```
npm install
node scripts/dev-keys.mjs
mkdir -p target/deploy
solana-keygen new --no-bip39-passphrase -o keys/hub_registry-keypair.json   # once; or bring the deployed program's keypair
cp keys/hub_registry-keypair.json target/deploy/hub_registry-keypair.json
cp keys/dev/demo-pool-program.json target/deploy/demo_pool-keypair.json
anchor build --no-idl -- --arch v0
anchor idl build -p hub_registry -o target/idl/hub_registry.json -t target/types/hub_registry.ts
anchor idl build -p demo_pool -o target/idl/demo_pool.json -t target/types/demo_pool.ts
node scripts/genesis.mjs
```

Toolchain: anchor-cli 0.31.1, cargo-build-sbf 4.4.0 with platform-tools v1.57. The build writes the programs (SBPF v0) to `target/deploy/` and the IDLs and their TypeScript types to `target/idl/` and `target/types/`; the scripts, the tests and the host images read them from there.

## Test

```
npm test
```

`tests/run.mjs` starts `solana-test-validator` with 32-slot epochs, `hub_registry` as an upgradeable program, `demo_pool` and the accounts of `genesis/acrux-net.json` loaded, runs both test files against it, then stops the validator it started and removes `.ledger/`. One file alone: `node tests/run.mjs tests/demo_pool.test.mjs`.

## Local validator

```
bash scripts/validator.sh .ledger 75 &
node scripts/init-localnet.mjs
```

`init-localnet.mjs` waits for the validator, initializes the registry and prints the RPC URL, the program id, the registry and fee vault addresses and the configuration. Every start is a new chain (Agave 2.3.13, on 127.0.0.1).

| Port | Use |
|---|---|
| 4990 | JSON-RPC |
| 4991 | WebSocket (always RPC + 1) |
| 4992 | faucet |
| 4993 | gossip |
| 4994 to 5015 | dynamic range |

State is in `.ledger/`: `ledger/`, `validator.log` and `validator.pid`.

Epochs are 75 slots (about 30 s) with the second argument above, so a join turns effective and a hub turns releasable while someone watches. The registry is initialized with the local values: deposit 1 SOL, registration fee 500 bps, cooldown 1 epoch, inactivity 4 epochs, n_min 4, codes of 3 to 8 capitals.

Local SOL: the genesis lamports are on the `local-authority` development key, and the faucet answers `solana airdrop -u http://127.0.0.1:4990 <amount> <address>`.

## hub_registry

Epoch always means the Solana Clock epoch. Rent comes from the Rent sysvar.

### Accounts

| Account | Address | Fields |
|---|---|---|
| `Registry` | PDA `["registry"]` | `authority`, `fee_destination`, `fee_vault`, `deposit_lamports`, `registration_fee_bps` (of the deposit), `cooldown_epochs`, `inactivity_epochs`, `min_members` (n_min), `code_min_len`, `code_max_len`, `hubs_registered`, `bump` |
| `FeeVault` | PDA `["fee_vault"]` | `bump`. Fees sit in the account's lamports above its rent. No key signs for it. |
| `Hub` | PDA `["hub", code]` | `id`, `code` (8 bytes, zero padded), `location` (`x`, `y`, `z`: ECEF, WGS84, whole metres, `i32`), `registrant`, `deposit_lamports`, `members`, `joining`, `leaving`, `counted_epoch`, `created_epoch`, `last_active_epoch`, `bump`. The deposit sits in the account's lamports above its rent. |
| `Membership` | PDA `["member", identity]` | `identity`, `hub`, `hub_id`, `joined_epoch`, `effective_epoch`, `pending` (`None`, `Leave`, `Move`), `pending_hub`, `pending_hub_id`, `pending_epoch`, `cooldown_until_epoch`, `bump` |

### Instructions

| Instruction | Signers | What it does |
|---|---|---|
| `initialize(config)` | the program's upgrade authority | Creates `Registry` and `FeeVault` with the configuration. |
| `register_hub(code, location)` | registrant | Creates the hub. The deposit goes into the hub account, `deposit * registration_fee_bps / 10000` into the fee vault. Code: `A-Z`, length within the registry's range (`^[A-Z]{3,8}$` with the local values). Location: a point between 6,300 km and 6,400 km from the Earth's centre. |
| `join_hub()` | identity, payer | Creates the membership when it is the identity's first. The membership counts from the next epoch. |
| `leave_hub()` | identity | The membership ends at the next epoch boundary. Starts the cooldown. |
| `move_hub()` | identity | The member changes hub at the next epoch boundary. Starts the cooldown. |
| `release_hub()` | anyone | For a hub past its inactivity period: closes it and returns the deposit and the rent to the registrant. The code is free again. |
| `sweep_fees()` | anyone | Sends the fee vault's lamports above its rent to `fee_destination`. |

Every instruction but `initialize` emits an event by self CPI (`HubRegistered`, `MemberJoined`, `MemberLeaving`, `MemberMoving`, `HubReleased`, `FeesSwept`), so its accounts end with `event_authority` (PDA `["__event_authority"]`) and the program id. Errors are in the IDL.

### Reading the state

- **Members of a hub at epoch `e`:** `members` when `e == counted_epoch`, and `members + joining - leaving` when `e > counted_epoch`.
- **A membership at epoch `e`:** when `pending` is not `None` and `e >= pending_epoch`, the change is in effect: the member is in `pending_hub` (a move) or in no hub (a leave) since `pending_epoch`. Otherwise the member belongs to `hub`, counting from `effective_epoch`. The next instruction on the membership writes a change in effect into `hub`.
- **A membership whose hub was released:** its `hub_id` differs from the `id` of the hub account at that address, or the account is gone. It counts for no hub. The identity leaves or moves as usual.
- **A hub never active:** `last_active_epoch == created_epoch`.
- **Releasable:** `e > last_active_epoch + inactivity_epochs`, with `last_active_epoch` advanced as below.

### Decisions

- **Hub activity is the member floor, kept without a crank.** A hub is active in an epoch when at least `min_members` memberships are in effect in it. Joins and departures are booked on the hub as `joining` and `leaving` for the next epoch, so the hub's count in every epoch is known from its own account. Any instruction that touches the hub first brings the counts up to the current epoch and sets `last_active_epoch`: to the current epoch when the count reaches `min_members`, otherwise to the last counted epoch that did. No epoch can be missed, because between two instructions the count does not change. `release_hub` runs the same step before it checks the period, so nobody has to touch a hub to keep it alive or to make it releasable.
- **The stake floor (rho_min = 1% of total stake) is applied off chain.** Stake is Solana stake, which this program does not read. The schedule preview computes hub stake from a stake snapshot and gives windows only to hubs that pass both floors. On chain, `last_active_epoch` tracks the member floor.
- **The inactivity period of a new hub counts from its creation epoch,** so a hub has `inactivity_epochs` epochs to gather its members.
- **One change at a time.** A leave or move is accepted once the membership is in effect, nothing is pending and the cooldown is over. `cooldown_until_epoch` is the epoch the change takes effect plus `cooldown_epochs`: with a cooldown of 1, a move sent in epoch E is in effect from E+1 and the next change is accepted from E+2. A first join starts no cooldown.
- **A hub has an id.** `release_hub` does not wait for the last member to leave, and a released code can be registered again at the same address. The id tells a membership of the earlier hub from a member of the new one, so counts stay exact.
- **`initialize` is gated on the program's upgrade authority** (read from the ProgramData account), so the configuration cannot be set by whoever sends the first transaction after a deploy.
- **The configuration is set once.** There is no update instruction; a change ships as a program upgrade, like the buyback crank on the fee vault.
- **`sweep_fees` is permissionless** because its destination is fixed in the registry.
- **`join_hub` takes a separate payer,** so an operator wallet can pay the membership rent while the validator identity signs. Both may be the same key.
- **Events go out by self CPI,** which keeps them readable in busy or log-truncated transactions.

### Mainnet

The program id is the public key of `keys/hub_registry-keypair.json` and is built into the binary. Deploy with `solana program deploy --program-id keys/hub_registry-keypair.json target/deploy/hub_registry.so`, then `initialize` with `scripts/init-mainnet.mjs` (its header lists the variables).

## demo_pool

One constant-product pool per pair of mints (classic SPL Token). Two instructions.

### Accounts

| Account | Address |
|---|---|
| `Pool` | PDA `["pool", mint_a, mint_b]`: `mint_a`, `mint_b`, `vault_a`, `vault_b`, `fee_bps` (u16), `bump` |
| vault | PDA `["vault", pool, mint]`, a token account owned by the pool PDA |

The reserves are the vaults' token balances.

### `swap`

Instruction data, 25 bytes:

| Bytes | Field |
|---|---|
| 0..8 | discriminator `f8 c6 9e 91 e1 75 87 c8` |
| 8..16 | `amount_in`, u64 little endian |
| 16..24 | `minimum_out`, u64 little endian |
| 24 | `a_to_b`: 1 swaps mint A for mint B, 0 swaps B for A |

Accounts, in order:

| # | Account | Writable | Signer |
|---|---|---|---|
| 0 | `user` | yes | yes |
| 1 | `pool` | | |
| 2 | `mint_a` | | |
| 3 | `mint_b` | | |
| 4 | `vault_a` | yes | |
| 5 | `vault_b` | yes | |
| 6 | `user_a`, the user's associated token account for mint A | yes | |
| 7 | `user_b`, the user's associated token account for mint B | yes | |
| 8 | SPL Token program `TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA` | | |
| 9 | Associated Token program `ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL` | | |
| 10 | System program `11111111111111111111111111111111` | | |

Associated token account: PDA `[user, token_program, mint]` of the Associated Token program. The swap creates either of the user's two accounts when it is missing, paid by the user, so a swap is one instruction of this program and nothing else. The same layout is in `target/idl/demo_pool.json`.

Errors: `6000 InvalidFee`, `6001 ZeroAmount`, `6002 SlippageExceeded`.

### Quote

Integers throughout, `reserve_in` and `reserve_out` being the vault balances on the input and output side:

```
fee        = ceil(amount_in * fee_bps / 10000)
net        = amount_in - fee
amount_out = floor(reserve_out * net / (reserve_in + net))
minimum_out = floor(amount_out * (10000 - slippage_bps) / 10000)
```

The whole `amount_in` goes into the input vault, so the fee stays in the pool. With the genesis reserves, 1,000 AXA in gives fee 3,000,000 and `amount_out` 1,992,013,962 (1,992.013962 AXB).

### `initialize_pool(fee_bps, amount_a, amount_b)`

Creates the pool and its two vaults and moves the first reserves in from the payer's token accounts. Accounts: `payer` (signer), `mint_a`, `mint_b`, `pool`, `vault_a`, `vault_b`, `payer_a`, `payer_b`, Token program, System program.

## Acrux Net genesis

`genesis/acrux-net.json` is written by `node scripts/genesis.mjs` and is a pure function of the development seed. The node loads the program with `acrux net up --program CUBR9LNdTwxwp5ARBQj1EopfuH8dr8u4i7DsXcb8HbHN=<path to target/deploy/demo_pool.so>` and writes each entry of `accounts` into its SVM as it is.

| Field | Content |
|---|---|
| `seed`, `derivation` | the development seed and how keys derive from it |
| `programs` | `demo_pool`: address and the `.so` path relative to this folder |
| `tokens` | AXA and AXB: symbol, name, decimals (6), mint |
| `pool` | address, mints, vaults, `feeBps` (30), reserves (1,000,000 AXA and 2,000,000 AXB) |
| `mintAuthority` | the development key that can mint more of both |
| `faucet` | the faucet key, its lamports, its two token accounts and their balance |
| `accounts` | each account as `label`, `address`, `owner`, `lamports`, `data` (base64), `executable` |

The eight accounts: the two mints, the pool, its two vaults, the faucet's system account (100 ACRUX) and the faucet's two associated token accounts (1,000,000,000 AXA and 1,000,000,000 AXB). Each mint's supply equals its vault plus the faucet's balance. Lamports are the rent-exempt minimum under the SVM's default rent (3,480 lamports per byte-year, two years, 128 bytes of overhead).

**The faucet hands out demo tokens.** Its token accounts are in genesis for that: a grant is one transaction signed by the faucet key with an idempotent associated token account creation for the receiver and an SPL Token transfer, next to the ACRUX grant. A browser key that holds AXA or AXB then swaps with the single `swap` instruction above.

## Keys

All keys under `keys/` are local only and never committed.

- `keys/dev/*.json` derive from the published development seed `acrux-dev-seed-v1` (`keypair = ed25519 from sha256(seed + "/" + label)`, rewritten by `node scripts/dev-keys.mjs`). They only ever hold local test funds: the demo pool's program id, the two mints, the mint authority, the faucet, and the local validator's authority and fee destination.
- `keys/hub_registry-keypair.json` is the program keypair of `hub_registry`, generated at random and kept out of version control. Its public key is the program id on every cluster.
