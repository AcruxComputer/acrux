//! Prepare real signed plain and 2-of-3 transfer requests for a manual gateway smoke check.
//! Usage: cargo run --example prepare_transfer -- <blockhash> <target-slot> [n] [t]
//! Fund `feePayer` through the local API before submitting either returned request.
use acrux_node::{execution::SvmExecutor,threshold::seal_for_receivers};
use anyhow::{Context,Result};
use base64::{Engine,engine::general_purpose::STANDARD as B64};
use ed25519_dalek::SigningKey;
use serde_json::json;
use solana_sdk::{compute_budget::ComputeBudgetInstruction,hash::Hash,signature::{Keypair,Signer},system_instruction,transaction::Transaction};

fn main()->Result<()> {
    let mut arguments=std::env::args().skip(1);
    let blockhash=arguments.next().context("Expected blockhash")?.parse::<Hash>()?;
    let slot=arguments.next().context("Expected target slot")?.parse::<u64>()?;
    let n=arguments.next().map(|v|v.parse::<u8>()).transpose()?.unwrap_or(3);
    let t=arguments.next().map(|v|v.parse::<u8>()).transpose()?.unwrap_or(2);
    let payer=Keypair::new();let recipient=Keypair::new();
    let transaction=Transaction::new_signed_with_payer(&[
        ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        ComputeBudgetInstruction::set_compute_unit_price(100),
        system_instruction::transfer(&payer.pubkey(),&recipient.pubkey(),10_000),
    ],Some(&payer.pubkey()),&[&payer],blockhash);
    let bytes=bincode::serialize(&transaction)?;
    let executor=SvmExecutor::new(Vec::<(String,u64)>::new(),&[])?;
    let checked=executor.validate_transaction(&bytes,slot)?;
    let signer=SigningKey::from_bytes(&payer.to_bytes()[..32].try_into().unwrap());
    let receivers=(1..=n).collect::<Vec<_>>();
    let fragments=seal_for_receivers(&bytes,checked.header.clone(),&signer,n,t,&receivers)?;
    println!("{}",serde_json::to_string_pretty(&json!({
        "feePayer":payer.pubkey().to_string(),
        "recipient":recipient.pubkey().to_string(),
        "plain":{"header":checked.header,"transaction":B64.encode(bytes),"receivers":receivers},
        "fragments":fragments,
    }))?);
    Ok(())
}
