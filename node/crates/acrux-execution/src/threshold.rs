//! NewCon 3.2 threshold transactions. This is a wire format, not a transport fallback.
//!
//! Header signatures cover UTF-8 `serde_json::to_vec(TxHeader)` with fields in this exact order:
//! id, feePayer, priorityFeePerCu, computeLimit, writableAccounts, targetSlot; no whitespace.
//! Amount strings and the writable-account array order are preserved. AEAD uses those same bytes
//! as associated data. `fragment` decodes to version(1), nonce(12), ciphertext length(u32 LE), then
//! one Reed-Solomon shard. `keyShare` decodes to version(1), x=index+1, then 32 GF(256) bytes.
//! All fragments of an ID must have identical headers, n, t, nonce and ciphertext length.
//! The first threshold-decoded slot owns the candidate. A candidate rejected by Algorithm 4
//! is not retried under the same ID; a later SDK may expose explicit resubmission with a new message.
use crate::{
    crypto,
    execution::{SvmExecutor, TxHeader, ValidatedTx},
};
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use bincode::Options;
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use ed25519_dalek::{SigningKey, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use reed_solomon_erasure::galois_8::ReedSolomon;
use serde::{Deserialize, Serialize};
use solana_sdk::transaction::VersionedTransaction;
use std::collections::{BTreeMap, BTreeSet};

const PREFIX_LEN: usize = 17;
const MAX_CIPHERTEXT: usize = 1232 + 16;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FragmentSubmission {
    pub header: TxHeader,
    pub n: u8,
    pub t: u8,
    pub index: u8,
    pub receiver: u8,
    pub fragment: String,
    pub key_share: String,
    pub header_signature: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct VerifiedFragment {
    pub header: TxHeader,
    pub n: u8,
    pub t: u8,
    pub index: u8,
    pub receiver: u8,
    pub nonce: [u8; 12],
    pub ciphertext_len: u32,
    pub shard: Vec<u8>,
    pub share: [u8; 32],
    pub header_signature: Vec<u8>,
}

impl VerifiedFragment {
    /// Verify again after deserialization from a proposer block or another network boundary.
    pub fn revalidate(&self) -> Result<Self> {
        let mut fragment = vec![1];
        fragment.extend(self.nonce);
        fragment.extend(self.ciphertext_len.to_le_bytes());
        fragment.extend(&self.shard);
        let mut share = vec![
            1,
            self.index
                .checked_add(1)
                .context("Invalid fragment index")?,
        ];
        share.extend(self.share);
        let submission = FragmentSubmission {
            header: self.header.clone(),
            n: self.n,
            t: self.t,
            index: self.index,
            receiver: self.receiver,
            fragment: B64.encode(fragment),
            key_share: B64.encode(share),
            header_signature: bs58::encode(&self.header_signature).into_string(),
        };
        submission.verify()
    }
}

pub fn header_bytes(header: &TxHeader) -> Result<Vec<u8>> {
    Ok(serde_json::to_vec(header)?)
}

impl FragmentSubmission {
    pub fn parse(value: serde_json::Value) -> Result<VerifiedFragment> {
        let submission: Self =
            serde_json::from_value(value).context("Invalid fragment submission")?;
        submission.verify()
    }

    pub fn verify(&self) -> Result<VerifiedFragment> {
        ensure!(
            (2..=3).contains(&self.n) && (2..=self.n).contains(&self.t),
            "Require 2 <= t <= n <= 3"
        );
        ensure!(
            self.index < self.n && (1..=3).contains(&self.receiver),
            "Invalid fragment index or receiver"
        );
        ensure!(
            self.header.id.len() == 64 && hex::decode(&self.header.id)?.len() == 32,
            "Invalid transaction ID"
        );
        ensure!(
            self.header.compute_limit > 0 && self.header.compute_limit <= 1_400_000,
            "Invalid compute limit"
        );
        self.header
            .priority_fee_per_cu
            .parse::<u64>()
            .context("Invalid priority fee")?;
        for address in
            std::iter::once(&self.header.fee_payer).chain(self.header.writable_accounts.iter())
        {
            ensure!(
                address.len() <= 44 && bs58::decode(address).into_vec()?.len() == 32,
                "Invalid public key"
            );
        }
        let payer: [u8; 32] = bs58::decode(&self.header.fee_payer)
            .into_vec()?
            .try_into()
            .map_err(|_| anyhow::anyhow!("Invalid fee payer"))?;
        ensure!(self.header_signature.len() <= 88, "Invalid header signature");
        let signature = bs58::decode(&self.header_signature).into_vec()?;
        crypto::verify_bytes(
            &VerifyingKey::from_bytes(&payer)?,
            &header_bytes(&self.header)?,
            &signature,
        )?;
        ensure!(
            self.fragment.len() <= 2048 && self.key_share.len() <= 48,
            "Encoded fragment exceeds size limit"
        );
        let fragment = B64
            .decode(&self.fragment)
            .context("Invalid fragment base64")?;
        let share = B64
            .decode(&self.key_share)
            .context("Invalid key share base64")?;
        ensure!(
            fragment.len() >= PREFIX_LEN && fragment[0] == 1,
            "Unsupported fragment envelope"
        );
        ensure!(
            share.len() == 34 && share[0] == 1 && share[1] == self.index + 1,
            "Invalid key share index or length"
        );
        let ciphertext_len = u32::from_le_bytes(fragment[13..17].try_into().unwrap());
        ensure!(
            (16..=MAX_CIPHERTEXT).contains(&(ciphertext_len as usize)),
            "Invalid ciphertext length"
        );
        ensure!(
            fragment.len() == PREFIX_LEN + (ciphertext_len as usize).div_ceil(self.t as usize),
            "Invalid fragment length"
        );
        Ok(VerifiedFragment {
            header: self.header.clone(),
            n: self.n,
            t: self.t,
            index: self.index,
            receiver: self.receiver,
            nonce: fragment[1..13].try_into().unwrap(),
            ciphertext_len,
            shard: fragment[PREFIX_LEN..].to_vec(),
            share: share[2..].try_into().unwrap(),
            header_signature: signature,
        })
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inclusion {
    pub index: u8,
    pub receiver: u8,
    pub slot: u64,
    pub proposer: u8,
    pub at_us: u64,
}
#[derive(Debug)]
pub enum AcceptOutcome {
    Pending {
        accepted: usize,
        threshold: u8,
        inclusions: Vec<Inclusion>,
    },
    Decoded {
        header: TxHeader,
        bytes: Vec<u8>,
        inclusions: Vec<Inclusion>,
    },
    AlreadyDecoded,
}
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    first: VerifiedFragment,
    first_slot: u64,
    fragments: BTreeMap<u8, VerifiedFragment>,
    inclusions: Vec<Inclusion>,
}
/// Serializable, so a follower can start from a finalized cblock's cache.
#[derive(Clone, Serialize, Deserialize)]
pub struct ThresholdCache {
    expiry_slots: u64,
    entries: BTreeMap<String, Cached>,
    decoded: BTreeSet<String>,
    expired: BTreeSet<String>,
}

impl ThresholdCache {
    pub fn new(expiry_slots: u64) -> Result<Self> {
        ensure!(expiry_slots > 0, "Fragment expiry must be positive");
        Ok(Self {
            expiry_slots,
            entries: BTreeMap::new(),
            decoded: BTreeSet::new(),
            expired: BTreeSet::new(),
        })
    }

    /// Discard a malformed threshold candidate after the caller records its failure.
    /// A later copy cannot revive the partially reconstructed candidate under the same ID.
    pub fn reject(&mut self,id:&str) {
        self.entries.remove(id);
        self.expired.insert(id.to_owned());
    }

    /// Accept only after the fragment's proposer block belongs to the agreed set P.
    /// A decoded candidate is still passed through `validate_decoded` and normal Algorithm 4.
    pub fn accept(
        &mut self,
        slot: u64,
        proposer: u8,
        fragment: VerifiedFragment,
    ) -> Result<AcceptOutcome> {
        self.accept_at(slot, 0, proposer, fragment)
    }

    pub fn accept_at(
        &mut self,
        slot: u64,
        at_us: u64,
        proposer: u8,
        fragment: VerifiedFragment,
    ) -> Result<AcceptOutcome> {
        ensure!(
            slot >= fragment.header.target_slot,
            "Fragment predates target slot"
        );
        ensure!(
            proposer == fragment.receiver,
            "Fragment included by a different receiver"
        );
        let id = fragment.header.id.clone();
        ensure!(!self.expired.contains(&id), "Fragment cache expired");
        if self.decoded.contains(&id) {
            return Ok(AcceptOutcome::AlreadyDecoded);
        }
        let entry = self.entries.entry(id.clone()).or_insert_with(|| Cached {
            first: fragment.clone(),
            first_slot: slot,
            fragments: BTreeMap::new(),
            inclusions: vec![],
        });
        ensure!(
            slot <= entry.first_slot.saturating_add(self.expiry_slots),
            "Fragment cache expired"
        );
        ensure!(
            entry.first.header == fragment.header
                && entry.first.n == fragment.n
                && entry.first.t == fragment.t
                && entry.first.nonce == fragment.nonce
                && entry.first.ciphertext_len == fragment.ciphertext_len,
            "Conflicting fragment header or encoding"
        );
        if let Some(old) = entry.fragments.get(&fragment.index) {
            ensure!(
                old.shard == fragment.shard
                    && old.share == fragment.share
                    && old.receiver == fragment.receiver,
                "Conflicting duplicate fragment"
            );
        } else {
            ensure!(
                !entry
                    .fragments
                    .values()
                    .any(|old| old.receiver == fragment.receiver),
                "A receiver may carry one fragment per transaction"
            );
            entry.inclusions.push(Inclusion {
                index: fragment.index,
                receiver: fragment.receiver,
                slot,
                proposer,
                at_us,
            });
            entry.fragments.insert(fragment.index, fragment);
        }
        if entry.fragments.len() < entry.first.t as usize {
            return Ok(AcceptOutcome::Pending {
                accepted: entry.fragments.len(),
                threshold: entry.first.t,
                inclusions: entry.inclusions.clone(),
            });
        }
        let bytes = reconstruct(
            &entry.first,
            &entry.fragments.values().cloned().collect::<Vec<_>>(),
        )?;
        validate_message_commitment(&entry.first.header, &bytes)?;
        let outcome = AcceptOutcome::Decoded {
            header: entry.first.header.clone(),
            bytes,
            inclusions: entry.inclusions.clone(),
        };
        self.entries.remove(&id);
        self.decoded.insert(id);
        Ok(outcome)
    }

    /// Returns IDs whose incomplete accepted fragments are past expiry since first inclusion.
    pub fn expire(&mut self, slot: u64) -> Vec<String> {
        let ids = self
            .entries
            .iter()
            .filter(|(_, entry)| slot > entry.first_slot.saturating_add(self.expiry_slots))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in &ids {
            self.entries.remove(id);
            self.expired.insert(id.clone());
        }
        ids
    }
}

fn reconstruct(first: &VerifiedFragment, fragments: &[VerifiedFragment]) -> Result<Vec<u8>> {
    ensure!(
        fragments.len() >= first.t as usize,
        "Insufficient fragments"
    );
    let mut shards = vec![None; first.n as usize];
    for fragment in fragments {
        shards[fragment.index as usize] = Some(fragment.shard.clone());
    }
    if first.n > first.t {
        ReedSolomon::new(first.t as usize, (first.n - first.t) as usize)?
            .reconstruct(&mut shards)?;
    }
    let mut ciphertext = shards
        .into_iter()
        .take(first.t as usize)
        .map(|shard| shard.ok_or_else(|| anyhow::anyhow!("Missing data shard")))
        .collect::<Result<Vec<_>>>()?
        .concat();
    ciphertext.truncate(first.ciphertext_len as usize);
    let selected = &fragments[..first.t as usize];
    let mut key = [0u8; 32];
    for (i, share) in selected.iter().enumerate() {
        let xi = share.index + 1;
        let mut basis = 1;
        for (j, other) in selected.iter().enumerate() {
            if i != j {
                let xj = other.index + 1;
                basis = gf_mul(basis, gf_div(xj, xi ^ xj)?);
            }
        }
        for (out, value) in key.iter_mut().zip(share.share) {
            *out ^= gf_mul(value, basis);
        }
    }
    ChaCha20Poly1305::new((&key).into())
        .decrypt(
            Nonce::from_slice(&first.nonce),
            Payload {
                msg: &ciphertext,
                aad: &header_bytes(&first.header)?,
            },
        )
        .map_err(|_| anyhow::anyhow!("Threshold ciphertext authentication failed"))
}

fn validate_message_commitment(header: &TxHeader, bytes: &[u8]) -> Result<()> {
    ensure!(
        bytes.len() <= 1232,
        "Decoded transaction exceeds packet size"
    );
    let transaction: VersionedTransaction = bincode::DefaultOptions::new()
        .with_fixint_encoding()
        .with_limit(1232)
        .reject_trailing_bytes()
        .deserialize(bytes)
        .context("Invalid reconstructed transaction")?;
    transaction.sanitize()?;
    transaction
        .verify_and_hash_message()
        .context("Invalid reconstructed transaction signature")?;
    ensure!(
        hex::encode(crypto::hash(&transaction.message.serialize())) == header.id,
        "Decoded message does not match transaction ID"
    );
    Ok(())
}

pub fn validate_decoded(
    executor: &SvmExecutor,
    header: &TxHeader,
    bytes: &[u8],
) -> Result<ValidatedTx> {
    let tx = executor.validate_transaction(bytes, header.target_slot)?;
    ensure!(
        &tx.header == header,
        "Decoded transaction differs from signed public header"
    );
    Ok(tx)
}

/// Browser sealer reference used by the roundtrip check; returns one payload per receiver.
pub fn seal_for_receivers(
    bytes: &[u8],
    header: TxHeader,
    signer: &SigningKey,
    n: u8,
    t: u8,
    receivers: &[u8],
) -> Result<Vec<FragmentSubmission>> {
    ensure!(
        (2..=3).contains(&n) && (2..=n).contains(&t) && receivers.len() == n as usize,
        "Invalid threshold configuration"
    );
    ensure!(
        receivers.iter().all(|r| (1..=3).contains(r))
            && receivers.iter().collect::<BTreeSet<_>>().len() == receivers.len(),
        "Receivers must be distinct proposer roles"
    );
    ensure!(
        bs58::encode(signer.verifying_key().as_bytes()).into_string() == header.fee_payer,
        "Sealer key is not fee payer"
    );
    validate_message_commitment(&header, bytes)?;
    let key = crypto::fresh_key();
    let mut nonce = [0; 12];
    OsRng.fill_bytes(&mut nonce);
    let canonical = header_bytes(&header)?;
    let ciphertext = ChaCha20Poly1305::new((&key).into())
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: bytes,
                aad: &canonical,
            },
        )
        .map_err(|_| anyhow::anyhow!("Transaction encryption failed"))?;
    let size = ciphertext.len().div_ceil(t as usize);
    let mut shards = vec![vec![0; size]; n as usize];
    for (i, chunk) in ciphertext.chunks(size).enumerate() {
        shards[i][..chunk.len()].copy_from_slice(chunk);
    }
    if n > t {
        ReedSolomon::new(t as usize, (n - t) as usize)?.encode(&mut shards)?;
    }
    let signature = bs58::encode(crypto::sign_bytes(signer, &canonical)).into_string();
    let coefficients = (0..32)
        .map(|byte| {
            let mut values = vec![0; t as usize];
            values[0] = key[byte];
            OsRng.fill_bytes(&mut values[1..]);
            values
        })
        .collect::<Vec<_>>();
    let mut submissions = vec![];
    for index in 0..n {
        let x = index + 1;
        let mut share = vec![1, x];
        for coefficients in &coefficients {
            let mut value = 0;
            for coefficient in coefficients.iter().rev() {
                value = gf_mul(value, x) ^ coefficient;
            }
            share.push(value);
        }
        let mut fragment = vec![1];
        fragment.extend(nonce);
        fragment.extend((ciphertext.len() as u32).to_le_bytes());
        fragment.extend(&shards[index as usize]);
        submissions.push(FragmentSubmission {
            header: header.clone(),
            n,
            t,
            index,
            receiver: receivers[index as usize],
            fragment: B64.encode(fragment),
            key_share: B64.encode(share),
            header_signature: signature.clone(),
        });
    }
    Ok(submissions)
}

fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0;
    for _ in 0..8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let high = a & 0x80;
        a <<= 1;
        if high != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    product
}
fn gf_div(a: u8, b: u8) -> Result<u8> {
    ensure!(b != 0, "Duplicate Shamir share index");
    let mut inverse = 1;
    let mut base = b;
    let mut exponent = 254;
    while exponent > 0 {
        if exponent & 1 != 0 {
            inverse = gf_mul(inverse, base);
        }
        base = gf_mul(base, base);
        exponent >>= 1;
    }
    Ok(gf_mul(a, inverse))
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_sdk::{
        compute_budget::ComputeBudgetInstruction,
        hash::Hash,
        signature::{Keypair, Signer},
        system_instruction,
        transaction::Transaction,
    };

    #[test]
    fn any_threshold_reconstructs_and_one_share_is_insufficient() {
        let payer = Keypair::new();
        let receiver = Keypair::new();
        let tx = Transaction::new_signed_with_payer(
            &[
                ComputeBudgetInstruction::set_compute_unit_limit(20000),
                ComputeBudgetInstruction::set_compute_unit_price(123),
                system_instruction::transfer(&payer.pubkey(), &receiver.pubkey(), 1000),
            ],
            Some(&payer.pubkey()),
            &[&payer],
            Hash::new_unique(),
        );
        let bytes = bincode::serialize(&tx).unwrap();
        let header = TxHeader {
            id: hex::encode(crypto::hash(&tx.message.serialize())),
            fee_payer: payer.pubkey().to_string(),
            priority_fee_per_cu: "123".into(),
            compute_limit: 20000,
            writable_accounts: vec![payer.pubkey().to_string(), receiver.pubkey().to_string()],
            target_slot: 8,
        };
        let signer = SigningKey::from_bytes(&payer.to_bytes()[..32].try_into().unwrap());
        for (n, t) in [(2, 2), (3, 2), (3, 3)] {
            let sealed = seal_for_receivers(
                &bytes,
                header.clone(),
                &signer,
                n,
                t,
                &[1, 2, 3][..n as usize],
            )
            .unwrap();
            let mut cache = ThresholdCache::new(8).unwrap();
            for index in (0..n).rev().take(t as usize) {
                let result = cache
                    .accept(
                        8,
                        sealed[index as usize].receiver,
                        sealed[index as usize].verify().unwrap(),
                    )
                    .unwrap();
                if index == n - t {
                    match result {
                        AcceptOutcome::Decoded { bytes: decoded, .. } => assert_eq!(decoded, bytes),
                        _ => panic!("threshold did not decode"),
                    }
                } else {
                    assert!(matches!(result, AcceptOutcome::Pending { .. }));
                }
            }
            let mut cache = ThresholdCache::new(8).unwrap();
            assert!(matches!(
                cache
                    .accept_at(11, 74123, 1, sealed[0].verify().unwrap())
                    .unwrap(),
                AcceptOutcome::Pending { accepted: 1, .. }
            ));
            assert!(cache.expire(19).is_empty());
            assert_eq!(cache.expire(20), vec![header.id.clone()]);
            assert!(cache.accept(20, 2, sealed[1].verify().unwrap()).is_err());
        }
    }
}
