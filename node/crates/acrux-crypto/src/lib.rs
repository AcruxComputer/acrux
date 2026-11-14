//! Block encryption and Rotor's authenticated 32-of-64 slice encoding.
use anyhow::{bail, ensure, Context, Result};
use chacha20poly1305::{
    aead::{Aead, KeyInit, Payload},
    ChaCha20Poly1305, Nonce,
};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use rand::{rngs::OsRng, RngCore};
use reed_solomon_erasure::galois_8::ReedSolomon;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const DATA_SHREDS: usize = 32;
pub const TOTAL_SHREDS: usize = 64;
pub const MAX_SLICE_BYTES: usize = 64 * 1024;

pub fn hash(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}
pub fn fresh_key() -> [u8; 32] {
    let mut key = [0; 32];
    OsRng.fill_bytes(&mut key);
    key
}
pub fn sign_bytes(key: &SigningKey, bytes: &[u8]) -> Vec<u8> {
    key.sign(bytes).to_bytes().to_vec()
}
pub fn verify_bytes(key: &VerifyingKey, bytes: &[u8], signature: &[u8]) -> Result<()> {
    let signature = Signature::from_slice(signature).context("invalid Ed25519 signature length")?;
    key.verify_strict(bytes, &signature)
        .context("invalid Ed25519 signature")
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShredMeta {
    pub slot: u64,
    pub proposer: usize,
    pub slice: u32,
    pub final_slice: bool,
    pub key_hash: [u8; 32],
    pub parent: Option<(u64, [u8; 32])>,
    pub encrypted_len: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Shred {
    pub meta: ShredMeta,
    pub index: u8,
    pub data: Vec<u8>,
    pub root: [u8; 32],
    pub proof: Vec<[u8; 32]>,
    pub signature: Vec<u8>,
}

fn nonce(slot: u64, proposer: usize, slice: u32) -> [u8; 12] {
    // Every block has a fresh key; this domain also keeps the slices within it distinct.
    let mut input = b"acrux-slice-nonce-v1".to_vec();
    input.extend(slot.to_le_bytes());
    input.extend((proposer as u64).to_le_bytes());
    input.extend(slice.to_le_bytes());
    hash(&input)[..12].try_into().unwrap()
}
fn associated_data(slot: u64, proposer: usize, slice: u32) -> Vec<u8> {
    let mut data = b"acrux-slice-v1".to_vec();
    data.extend(slot.to_le_bytes());
    data.extend((proposer as u64).to_le_bytes());
    data.extend(slice.to_le_bytes());
    data
}
pub fn encrypt_slice(
    key: &[u8; 32],
    slot: u64,
    proposer: usize,
    slice: u32,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        plaintext.len() <= MAX_SLICE_BYTES - 16,
        "slice plaintext exceeds size limit"
    );
    ChaCha20Poly1305::new(key.into())
        .encrypt(
            Nonce::from_slice(&nonce(slot, proposer, slice)),
            Payload {
                msg: plaintext,
                aad: &associated_data(slot, proposer, slice),
            },
        )
        .map_err(|_| anyhow::anyhow!("slice encryption failed"))
}
pub fn decrypt_slice(
    key: &[u8; 32],
    slot: u64,
    proposer: usize,
    slice: u32,
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    ensure!(
        ciphertext.len() <= MAX_SLICE_BYTES,
        "encrypted slice exceeds size limit"
    );
    ChaCha20Poly1305::new(key.into())
        .decrypt(
            Nonce::from_slice(&nonce(slot, proposer, slice)),
            Payload {
                msg: ciphertext,
                aad: &associated_data(slot, proposer, slice),
            },
        )
        .map_err(|_| anyhow::anyhow!("slice authentication failed"))
}

fn leaf(index: usize, data: &[u8]) -> [u8; 32] {
    let mut bytes = Vec::with_capacity(data.len() + 3);
    bytes.extend([0, index as u8]);
    bytes.extend(data);
    hash(&bytes)
}
fn parent(left: &[u8; 32], right: &[u8; 32]) -> [u8; 32] {
    let mut bytes = [0; 65];
    bytes[0] = 1;
    bytes[1..33].copy_from_slice(left);
    bytes[33..].copy_from_slice(right);
    hash(&bytes)
}
fn levels(shards: &[Vec<u8>]) -> Vec<Vec<[u8; 32]>> {
    let mut tree = vec![shards
        .iter()
        .enumerate()
        .map(|(i, shard)| leaf(i, shard))
        .collect::<Vec<_>>()];
    while tree.last().unwrap().len() > 1 {
        let next = tree
            .last()
            .unwrap()
            .chunks_exact(2)
            .map(|p| parent(&p[0], &p[1]))
            .collect();
        tree.push(next);
    }
    tree
}
fn signed_shred(meta: &ShredMeta, index: u8, root: [u8; 32], data: &[u8]) -> Result<Vec<u8>> {
    // Signature binds finality, parent and length metadata as well as the indexed Merkle root.
    Ok(bincode::serialize(&(
        b"acrux-shred-v1",
        meta,
        index,
        root,
        hash(data),
    ))?)
}

pub fn encode_slice(key: &SigningKey, meta: &ShredMeta, ciphertext: &[u8]) -> Result<Vec<Shred>> {
    ensure!(
        !ciphertext.is_empty() && ciphertext.len() <= MAX_SLICE_BYTES,
        "invalid encrypted slice length"
    );
    ensure!(
        meta.encrypted_len as usize == ciphertext.len(),
        "encrypted length disagrees with header"
    );
    let shard_size = ciphertext.len().div_ceil(DATA_SHREDS);
    let mut shards = vec![vec![0; shard_size]; TOTAL_SHREDS];
    for (i, chunk) in ciphertext.chunks(shard_size).enumerate() {
        shards[i][..chunk.len()].copy_from_slice(chunk);
    }
    ReedSolomon::new(DATA_SHREDS, TOTAL_SHREDS - DATA_SHREDS)?.encode(&mut shards)?;
    let tree = levels(&shards);
    let root = tree.last().unwrap()[0];
    shards
        .into_iter()
        .enumerate()
        .map(|(i, data)| {
            let mut index = i;
            let proof = tree[..tree.len() - 1]
                .iter()
                .map(|level| {
                    let sibling = level[index ^ 1];
                    index /= 2;
                    sibling
                })
                .collect();
            let signature = sign_bytes(key, &signed_shred(meta, i as u8, root, &data)?);
            Ok(Shred {
                meta: meta.clone(),
                index: i as u8,
                data,
                root,
                proof,
                signature,
            })
        })
        .collect()
}

pub fn verify_shred(shred: &Shred, key: &VerifyingKey) -> Result<()> {
    ensure!(
        shred.index < TOTAL_SHREDS as u8,
        "shred index outside codeword"
    );
    ensure!(
        shred.meta.encrypted_len > 0 && shred.meta.encrypted_len as usize <= MAX_SLICE_BYTES,
        "invalid encrypted length"
    );
    ensure!(
        shred.data.len() == (shred.meta.encrypted_len as usize).div_ceil(DATA_SHREDS),
        "invalid shard length"
    );
    ensure!(shred.proof.len() == 6, "invalid Merkle proof depth");
    verify_bytes(
        key,
        &signed_shred(&shred.meta, shred.index, shred.root, &shred.data)?,
        &shred.signature,
    )?;
    let mut value = leaf(shred.index as usize, &shred.data);
    let mut index = shred.index as usize;
    for sibling in &shred.proof {
        value = if index % 2 == 0 {
            parent(&value, sibling)
        } else {
            parent(sibling, &value)
        };
        index /= 2;
    }
    ensure!(
        value == shred.root,
        "shred Merkle proof does not match root"
    );
    Ok(())
}

pub fn reconstruct_slice(shreds: &[Shred], key: &VerifyingKey) -> Result<Vec<u8>> {
    for shred in shreds {
        verify_shred(shred, key)?;
    }
    reconstruct_verified_slice(shreds)
}

/// Rebuild a pile whose signatures and Merkle proofs were checked at reception.
/// The caller must never place an unverified network shred into this pile.
pub fn reconstruct_verified_slice(shreds: &[Shred]) -> Result<Vec<u8>> {
    ensure!(
        shreds.len() >= DATA_SHREDS && shreds.len() <= TOTAL_SHREDS,
        "reconstruction requires 32 through 64 shreds"
    );
    let first = &shreds[0];
    ensure!(
        first.meta.encrypted_len > 0 && first.meta.encrypted_len as usize <= MAX_SLICE_BYTES,
        "invalid encrypted length"
    );
    let mut shards: Vec<Option<Vec<u8>>> = vec![None; TOTAL_SHREDS];
    for shred in shreds {
        ensure!(shred.index < TOTAL_SHREDS as u8, "invalid shred index");
        ensure!(
            shred.data.len() == (first.meta.encrypted_len as usize).div_ceil(DATA_SHREDS),
            "invalid shred length"
        );
        ensure!(
            shred.meta == first.meta && shred.root == first.root,
            "mixed slices or conflicting Merkle roots"
        );
        if let Some(existing) = &shards[shred.index as usize] {
            ensure!(existing == &shred.data, "conflicting duplicate shred");
        }
        shards[shred.index as usize] = Some(shred.data.clone());
    }
    if shards.iter().flatten().count() < DATA_SHREDS {
        bail!("fewer than 32 distinct shreds");
    }
    ReedSolomon::new(DATA_SHREDS, TOTAL_SHREDS - DATA_SHREDS)?.reconstruct(&mut shards)?;
    let shards: Vec<Vec<u8>> = shards.into_iter().map(Option::unwrap).collect();
    ensure!(
        levels(&shards).last().unwrap()[0] == first.root,
        "reconstructed codeword does not match committed root"
    );
    let mut ciphertext = shards[..DATA_SHREDS].concat();
    ciphertext.truncate(first.meta.encrypted_len as usize);
    Ok(ciphertext)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn encrypted_slice_rebuilds_from_parity_and_rejects_tampering() {
        let signing = SigningKey::generate(&mut OsRng);
        let key = fresh_key();
        let message = b"sealed until unanimous commitments";
        let ciphertext = encrypt_slice(&key, 42, 2, 0, message).unwrap();
        let meta = ShredMeta {
            slot: 42,
            proposer: 2,
            slice: 0,
            final_slice: true,
            key_hash: hash(&key),
            parent: Some((9, [1; 32])),
            encrypted_len: ciphertext.len() as u32,
        };
        let shreds = encode_slice(&signing, &meta, &ciphertext).unwrap();
        let rebuilt = reconstruct_slice(&shreds[32..], &signing.verifying_key()).unwrap();
        assert_eq!(decrypt_slice(&key, 42, 2, 0, &rebuilt).unwrap(), message);
        let mut corrupt = shreds[0].clone();
        corrupt.meta.final_slice = false;
        assert!(verify_shred(&corrupt, &signing.verifying_key()).is_err());
        let mut corrupt = shreds[0].clone();
        corrupt.data[0] ^= 1;
        assert!(verify_shred(&corrupt, &signing.verifying_key()).is_err());
        assert!(decrypt_slice(&fresh_key(), 42, 2, 0, &rebuilt).is_err());
    }
}
