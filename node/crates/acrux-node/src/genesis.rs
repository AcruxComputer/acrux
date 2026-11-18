//! Acrux Net's genesis state from `backend/chain/genesis/acrux-net.json`: the demo pool program,
//! its two mints and vaults, and the faucet with its token accounts.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use solana_sdk::{
    account::Account,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::{keypair::keypair_from_seed, Signer},
};
use std::{path::{Path, PathBuf}, str::FromStr};

pub const TOKEN_PROGRAM: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
pub const ATA_PROGRAM: &str = "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL";
/// Demo tokens per faucet grant, per mint, in whole tokens.
const GRANT_TOKENS: u64 = 100;

#[derive(Deserialize)]
struct File {
    seed: String,
    programs: Vec<ProgramEntry>,
    tokens: Vec<Token>,
    pool: Pool,
    faucet: Faucet,
    accounts: Vec<AccountEntry>,
}
#[derive(Deserialize)]
struct ProgramEntry {
    address: String,
    path: String,
}
#[derive(Deserialize, Clone)]
pub struct Token {
    pub symbol: String,
    pub name: String,
    pub decimals: u8,
    pub mint: String,
}
#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Pool {
    pub address: String,
    pub mint_a: String,
    pub mint_b: String,
    pub vault_a: String,
    pub vault_b: String,
    pub fee_bps: u64,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Faucet {
    address: String,
    token_accounts: Vec<String>,
}
#[derive(Deserialize)]
struct AccountEntry {
    address: String,
    owner: String,
    lamports: u64,
    data: String,
    executable: bool,
}

pub struct Genesis {
    pub programs: Vec<(String, PathBuf)>,
    pub accounts: Vec<(String, Account)>,
    pub tokens: Vec<Token>,
    pub pool: Pool,
    pub faucet: Keypair,
    faucet_token_accounts: Vec<String>,
    /// Programs placed at their mainnet addresses from a named slot (`chain/placements/`).
    pub placements: Vec<crate::execution::Placement>,
    /// The bridge from Solana, when the chain credits its deposits (`chain/bridge.json`).
    pub bridge: Option<BridgeConfig>,
}

#[derive(Deserialize)]
struct PlacementManifest {
    placements: Vec<ManifestEntry>,
}
#[derive(Deserialize)]
struct ManifestEntry {
    file: String,
    slot: u64,
}
#[derive(Deserialize)]
struct PlacementFile {
    name: String,
    accounts: Vec<AccountEntry>,
}

/// The bridge's rule: the Solana program whose deposits Acrux credits, and the first Acrux slot
/// whose blocks may carry credits.
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeConfig {
    #[serde(deserialize_with = "pubkey")]
    pub program: Pubkey,
    pub from_slot: u64,
    /// Solana light-client epochs, independent of the consensus schedule's epochs.
    #[serde(default = "bridge_epoch_slots")]
    pub epoch_slots: u64,
    /// The chain name whose SHA-256 prefixes votes from the bridge activation slot.
    #[serde(deserialize_with = "chain_id")]
    pub chain_id: String,
}
impl BridgeConfig {
    pub fn chain_tag(&self) -> [u8; 32] { crate::crypto::hash(self.chain_id.as_bytes()) }
}
fn bridge_epoch_slots() -> u64 { 432_000 }
fn chain_id<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    let name = String::deserialize(d)?;
    if name.trim().is_empty() {
        return Err(serde::de::Error::custom("bridge chainId must be a nonempty chain name"));
    }
    Ok(name)
}
fn pubkey<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Pubkey, D::Error> {
    let s = String::deserialize(d)?;
    Pubkey::from_str(&s).map_err(serde::de::Error::custom)
}

/// `chain/bridge.json` beside the genesis, or the file `ACRUX_BRIDGE` names; none without one.
fn bridge(chain: &Path) -> Result<Option<BridgeConfig>> {
    let path = std::env::var("ACRUX_BRIDGE").map(PathBuf::from).unwrap_or_else(|_| chain.join("bridge.json"));
    if !path.exists() {
        return Ok(None);
    }
    let config: BridgeConfig = serde_json::from_slice(&std::fs::read(&path).with_context(|| format!("Read {}", path.display()))?)?;
    ensure!(config.epoch_slots > 0 && config.epoch_slots % 4 == 0, "bridge epochSlots must be positive and divisible by four");
    ensure!(config.from_slot % 4 == 0, "bridge fromSlot must start at a four-slot cblock boundary");
    Ok(Some(config))
}

/// The placements the chain applies: `chain/placements/placements.json` beside the genesis, or the
/// manifest `ACRUX_PLACEMENTS` names; none when there is no manifest.
fn placements(chain: &Path) -> Result<Vec<crate::execution::Placement>> {
    let manifest = std::env::var("ACRUX_PLACEMENTS").map(PathBuf::from).unwrap_or_else(|_| chain.join("placements/placements.json"));
    if !manifest.exists() {
        return Ok(vec![]);
    }
    let dir = manifest.parent().context("placement manifest folder")?;
    let entries: PlacementManifest = serde_json::from_slice(&std::fs::read(&manifest).with_context(|| format!("Read {}", manifest.display()))?)?;
    entries
        .placements
        .into_iter()
        .map(|entry| {
            let file: PlacementFile = serde_json::from_slice(&std::fs::read(dir.join(&entry.file)).with_context(|| format!("Read placement {}", entry.file))?)?;
            let accounts = file
                .accounts
                .into_iter()
                .map(|a| Ok((Pubkey::from_str(&a.address)?, Account { lamports: a.lamports, data: B64.decode(a.data)?, owner: Pubkey::from_str(&a.owner)?, executable: a.executable, rent_epoch: 0 })))
                .collect::<Result<Vec<_>>>()?;
            ensure!(accounts.last().is_some_and(|(_, a)| a.executable), "placement {} must end with its program account", file.name);
            Ok(crate::execution::Placement { name: file.name, slot: entry.slot, accounts })
        })
        .collect()
}

impl Genesis {
    /// Program paths in the file are relative to the chain workspace, the file's grandparent.
    pub fn load(path: &Path) -> Result<Self> {
        let file: File = serde_json::from_slice(&std::fs::read(path).with_context(|| format!("Read genesis {}", path.display()))?)?;
        let chain = path.parent().and_then(Path::parent).context("Genesis file has no chain workspace")?;
        let faucet = keypair_from_seed(&Sha256::digest(format!("{}/faucet", file.seed))).map_err(|e| anyhow::anyhow!("{e}"))?;
        ensure!(faucet.pubkey().to_string() == file.faucet.address, "Genesis faucet key does not derive from the seed");
        Ok(Self {
            programs: file.programs.into_iter().map(|p| (p.address, chain.join(p.path))).collect(),
            accounts: file
                .accounts
                .into_iter()
                .map(|a| {
                    Ok((a.address, Account { lamports: a.lamports, data: B64.decode(a.data)?, owner: Pubkey::from_str(&a.owner)?, executable: a.executable, rent_epoch: 0 }))
                })
                .collect::<Result<_>>()?,
            tokens: file.tokens,
            pool: file.pool,
            faucet,
            faucet_token_accounts: file.faucet.token_accounts,
            placements: placements(chain)?,
            bridge: bridge(chain)?,
        })
    }

    /// An ACRUX transfer from `funder` plus, per demo mint, the receiver's associated token account
    /// (its rent paid by `funder`) and a grant signed by the faucet, which holds the demo tokens.
    pub fn faucet_instructions(&self, funder: &Pubkey, receiver: &Pubkey, lamports: u64) -> Result<Vec<Instruction>> {
        let token = Pubkey::from_str(TOKEN_PROGRAM)?;
        let ata = Pubkey::from_str(ATA_PROGRAM)?;
        let faucet = self.faucet.pubkey();
        let mut instructions = vec![solana_sdk::system_instruction::transfer(funder, receiver, lamports)];
        for (mint, source) in self.tokens.iter().zip(&self.faucet_token_accounts) {
            let mint_key = Pubkey::from_str(&mint.mint)?;
            let destination = associated_token_account(receiver, &mint_key);
            instructions.push(Instruction {
                program_id: ata,
                accounts: vec![
                    AccountMeta::new(*funder, true),
                    AccountMeta::new(destination, false),
                    AccountMeta::new_readonly(*receiver, false),
                    AccountMeta::new_readonly(mint_key, false),
                    AccountMeta::new_readonly(solana_sdk::system_program::id(), false),
                    AccountMeta::new_readonly(token, false),
                ],
                data: vec![1], // CreateIdempotent
            });
            let mut data = vec![12]; // TransferChecked
            data.extend_from_slice(&(GRANT_TOKENS * 10u64.pow(mint.decimals as u32)).to_le_bytes());
            data.push(mint.decimals);
            instructions.push(Instruction {
                program_id: token,
                accounts: vec![
                    AccountMeta::new(Pubkey::from_str(source)?, false),
                    AccountMeta::new_readonly(mint_key, false),
                    AccountMeta::new(destination, false),
                    AccountMeta::new_readonly(faucet, true),
                ],
                data,
            });
        }
        Ok(instructions)
    }

    pub fn demo_pool(&self, reserves: [u64; 2], slot: u64) -> Value {
        json!({
            "address": self.pool.address,
            "mints": self.tokens.iter().map(|t| json!({"address": t.mint, "symbol": t.symbol, "name": t.name, "decimals": t.decimals})).collect::<Vec<_>>(),
            "reserves": [reserves[0].to_string(), reserves[1].to_string()],
            "feeBps": self.pool.fee_bps,
            "slot": slot,
        })
    }

    /// The pool's quote, as the program computes it (backend/chain/README.md, Quote).
    pub fn quote(&self, reserves: [u64; 2], input_mint: &str, amount_in: u64, slippage_bps: u64, slot: u64) -> Result<Value> {
        let a_to_b = if input_mint == self.pool.mint_a {
            true
        } else {
            ensure!(input_mint == self.pool.mint_b, "inputMint is not one of the pool's mints");
            false
        };
        ensure!(amount_in > 0 && slippage_bps <= 10_000, "amountIn must be positive and slippageBps at most 10000");
        let (reserve_in, reserve_out) = if a_to_b { (reserves[0], reserves[1]) } else { (reserves[1], reserves[0]) };
        let fee = (amount_in as u128 * self.pool.fee_bps as u128).div_ceil(10_000);
        let net = amount_in as u128 - fee;
        let out = reserve_out as u128 * net / (reserve_in as u128 + net);
        let minimum = out * (10_000 - slippage_bps as u128) / 10_000;
        Ok(json!({
            "inputMint": input_mint,
            "outputMint": if a_to_b { &self.pool.mint_b } else { &self.pool.mint_a },
            "amountIn": amount_in.to_string(),
            "amountOut": out.to_string(),
            "minimumOut": minimum.to_string(),
            "feeAmount": fee.to_string(),
            "slot": slot,
        }))
    }
}

/// A hub of the Hub Registry in genesis, with the identities of its effective members.
pub struct RegistryHub {
    pub code: String,
    pub latitude: f64,
    pub longitude: f64,
    pub members: Vec<String>,
}

const HUB_DISCRIMINATOR: [u8; 8] = [143, 189, 4, 120, 169, 234, 14, 125];
const MEMBERSHIP_DISCRIMINATOR: [u8; 8] = [231, 141, 180, 98, 109, 168, 175, 166];

impl Genesis {
    /// The Hub Registry's hubs and members as genesis writes them (hub_registry's Hub and
    /// Membership accounts), in hub id order; empty when the genesis carries no registry.
    pub fn registry_hubs(&self) -> Result<Vec<RegistryHub>> {
        let mut hubs = vec![];
        let mut members = vec![];
        for (address, account) in &self.accounts {
            let d = &account.data;
            if d.len() >= 8 && d[..8] == HUB_DISCRIMINATOR {
                let id = u64::from_le_bytes(d[8..16].try_into()?);
                let code = String::from_utf8(d[16..24].iter().copied().take_while(|b| *b != 0).collect())?;
                let xyz = [24, 28, 32].map(|o| i32::from_le_bytes(d[o..o + 4].try_into().unwrap()) as f64);
                let (latitude, longitude) = geodetic(xyz);
                hubs.push((id, address.clone(), RegistryHub { code, latitude, longitude, members: vec![] }));
            } else if d.len() >= 8 && d[..8] == MEMBERSHIP_DISCRIMINATOR {
                let identity = bs58::encode(&d[8..40]).into_string();
                let hub = bs58::encode(&d[40..72]).into_string();
                // Effective at genesis, with nothing pending (pending is the byte after three u64s).
                if u64::from_le_bytes(d[88..96].try_into()?) == 0 && d[96] == 0 {
                    members.push((hub, identity));
                }
            }
        }
        hubs.sort_by_key(|(id, _, _)| *id);
        for (hub, identity) in members {
            hubs.iter_mut().find(|(_, address, _)| *address == hub).context("Membership of an unknown hub")?.2.members.push(identity);
        }
        Ok(hubs.into_iter().map(|(_, _, hub)| hub).collect())
    }
}

/// WGS84 ECEF (metres) to geodetic latitude and longitude in degrees, Bowring's method.
fn geodetic([x, y, z]: [f64; 3]) -> (f64, f64) {
    let (a, f) = (6_378_137.0f64, 1.0 / 298.257223563);
    let b = a * (1.0 - f);
    let (e2, ep2) = (f * (2.0 - f), (a * a - b * b) / (b * b));
    let p = x.hypot(y);
    let theta = (z * a).atan2(p * b);
    let latitude = (z + ep2 * b * theta.sin().powi(3)).atan2(p - e2 * a * theta.cos().powi(3));
    (latitude.to_degrees(), y.atan2(x).to_degrees())
}

/// The regions a run of windows stays inside (SIMD-0675), by location.
pub fn region_of(latitude: f64, longitude: f64) -> &'static str {
    if longitude < -30.0 {
        if latitude >= 12.0 { "North America" } else { "South America" }
    } else if longitude < 60.0 {
        if latitude >= 35.0 { "Europe" } else { "Africa and Middle East" }
    } else if latitude >= -10.0 {
        "Asia Pacific"
    } else {
        "Oceania"
    }
}

pub fn associated_token_account(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let ata = Pubkey::from_str(ATA_PROGRAM).unwrap();
    Pubkey::find_program_address(&[owner.as_ref(), token.as_ref(), mint.as_ref()], &ata).0
}

/// A classic SPL Token account's balance, bytes 64..72 of its data.
pub fn token_amount(account: &Account) -> u64 {
    account.data.get(64..72).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}
