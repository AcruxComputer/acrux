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
        })
    }

    /// A SOL transfer plus, per demo mint, the receiver's associated token account and a grant.
    pub fn faucet_instructions(&self, receiver: &Pubkey, lamports: u64) -> Result<Vec<Instruction>> {
        let token = Pubkey::from_str(TOKEN_PROGRAM)?;
        let ata = Pubkey::from_str(ATA_PROGRAM)?;
        let payer = self.faucet.pubkey();
        let mut instructions = vec![solana_sdk::system_instruction::transfer(&payer, receiver, lamports)];
        for (mint, source) in self.tokens.iter().zip(&self.faucet_token_accounts) {
            let mint_key = Pubkey::from_str(&mint.mint)?;
            let destination = associated_token_account(receiver, &mint_key);
            instructions.push(Instruction {
                program_id: ata,
                accounts: vec![
                    AccountMeta::new(payer, true),
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
                    AccountMeta::new_readonly(payer, true),
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

pub fn associated_token_account(owner: &Pubkey, mint: &Pubkey) -> Pubkey {
    let token = Pubkey::from_str(TOKEN_PROGRAM).unwrap();
    let ata = Pubkey::from_str(ATA_PROGRAM).unwrap();
    Pubkey::find_program_address(&[owner.as_ref(), token.as_ref(), mint.as_ref()], &ata).0
}

/// A classic SPL Token account's balance, bytes 64..72 of its data.
pub fn token_amount(account: &Account) -> u64 {
    account.data.get(64..72).map_or(0, |b| u64::from_le_bytes(b.try_into().unwrap()))
}
