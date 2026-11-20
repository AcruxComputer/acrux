//! The same proposer packing, encryption and sealing on a local or an outside validator.
use crate::{
    consensus::BlockRef,
    crypto::{self, Shred, ShredMeta},
    execution::{SvmExecutor, ValidatedTx},
    threshold::VerifiedFragment,
    types::{BlockCommitment, Commitment},
};
use anyhow::{ensure, Result};
use ed25519_dalek::SigningKey;
use std::{collections::BTreeMap,sync::Arc,time::{Duration,Instant}};

#[derive(Clone)]
pub(crate) enum ProposalLink {
    Local(Arc<crate::transport::LocalTransport>),
    Member {control:Arc<crate::member_link::MemberLink>,seed:u64,relay_stakes:Vec<u64>},
}
impl ProposalLink {
    pub(crate) async fn send(&self,from:usize,to:usize,bytes:Vec<u8>)->Result<()> {
        match self {
            Self::Local(link)=>link.send(from,to,bytes).await,
            Self::Member {control,..}=>control.send(from,to,bytes).await,
        }
    }
    fn relay(&self,slot:u64,slice:u32,role:usize,shred:u8)->usize {
        match self {
            Self::Local(link)=>link.relay(slot,slice,role,shred),
            Self::Member {seed,relay_stakes,..}=>crate::transport::stake_relay(*seed,slot,slice,role,shred,relay_stakes),
        }
    }
}

/// Transaction/fragment budget, leaving space under the wire bound for handovers and proofs.
pub const BLOCK_BYTES: u64 = 64 * 1024;
const SLICE_BYTES: usize = 38_000;
const MAX_BLOCK_BYTES: usize = 1024 * 1024;

pub(crate) struct BuiltBlock {
    pub key: [u8; 32],
    pub commitment: Commitment,
    pub slices: Vec<Vec<Shred>>,
}

pub(crate) struct EarlyBlock {
    pub key: [u8; 32],
    pub roots: Vec<[u8; 32]>,
    pub next_slice: u32,
}

pub(crate) async fn send_slice(link:&ProposalLink,id:usize,signer:&SigningKey,shreds:Vec<Shred>)->Result<()> {
    let mut groups:BTreeMap<usize,Vec<Shred>>=BTreeMap::new();
    for shred in shreds {
        groups.entry(link.relay(shred.meta.slot,shred.meta.slice,shred.meta.proposer,shred.index)).or_default().push(shred);
    }
    for (to,group) in groups {
        let body=crate::types::Wire::Shreds(group);
        let signature=crypto::sign_bytes(signer,&bincode::serialize(&body)?);
        let bytes=bincode::serialize(&crate::types::Envelope {from:id,body,signature})?;
        let link=link.clone();
        tokio::spawn(async move {if let Err(error)=link.send(id,to,bytes).await {tracing::warn!(%error,"proposer slice send failed");}});
    }
    Ok(())
}

/// Both kinds of proposer stream their slices to the same Rotor relays at the
/// same instants. The exchange engine independently commits at start + 50 ms.
pub(crate) fn stream_block(link:ProposalLink,id:usize,signer:SigningKey,block:BuiltBlock,start:Instant,
    gateway:crate::gateway::SharedGateway,identity:String) {
    tokio::spawn(async move {
        let total=block.slices.len();
        for (index,shreds) in block.slices.into_iter().enumerate() {
            let when=start+Duration::from_micros(((index+1)as u64*50_000/total as u64).saturating_sub(1_000));
            tokio::time::sleep_until(when.into()).await;
            let first=&shreds[0];
            let slot=first.meta.slot;
            let at=start.elapsed().as_micros()as u64;
            let data=serde_json::json!({"proposer":first.meta.proposer,"slice":{"index":first.meta.slice,"shredCount":64,"rebuildShreds":32,"bytes":first.meta.encrypted_len,"sentUs":at,"arrivedUs":0,"merkleRoot":hex::encode(first.root)}});
            if let Err(error)=send_slice(&link,id,&signer,shreds).await {tracing::warn!(%error,"proposer stream failed");return}
            let mut g=gateway.write().await;
            if let Some(view)=g.slots.get_mut(&slot) {crate::projection::apply(view,"block.slice",&data);}
            g.emit(Some(identity.clone()),Some(slot),"slot",at,"block.slice",data);
        }
    });
}

fn slice(signer: &SigningKey, key: &[u8; 32], slot: u64, role: usize, index: u32, plain: &[u8], final_slice: bool, parent: Option<BlockRef>) -> Result<Vec<Shred>> {
    let cipher = crypto::encrypt_slice(key,slot,role,index,plain)?;
    let meta = ShredMeta {
        slot,proposer:role,slice:index,final_slice,key_hash:crypto::hash(key),
        parent:parent.map(|parent|(parent.slot,parent.hash)),encrypted_len:cipher.len() as u32,
    };
    crypto::encode_slice(signer,&meta,&cipher)
}

fn seal(slot: u64, role: usize, key: &[u8; 32], parent: Option<BlockRef>, roots: Vec<[u8; 32]>) -> Result<Commitment> {
    let key_hash = crypto::hash(key);
    Ok(Commitment { slot,proposer:role,key_hash,
        hash:crypto::hash(&bincode::serialize(&BlockCommitment {slot,proposer:role,key_hash,parent,roots})?),
    })
}

/// Encrypts each normal block slice and commits its roots in their original order.
pub(crate) fn build_block(signer: &SigningKey, slot: u64, role: usize, parent: Option<BlockRef>, payload: Vec<u8>) -> Result<BuiltBlock> {
    ensure!(payload.len() <= MAX_BLOCK_BYTES,"Proposer block exceeds local size limit");
    let key = crypto::fresh_key();
    let chunks = payload.chunks(SLICE_BYTES);
    let count = chunks.len();
    let mut roots = vec![]; let mut slices = vec![];
    for (index,plain) in chunks.enumerate() {
        let final_slice = index + 1 == count;
        let shreds = slice(signer,&key,slot,role,index as u32,plain,final_slice,if final_slice {parent} else {None})?;
        roots.push(shreds[0].root); slices.push(shreds);
    }
    Ok(BuiltBlock {commitment:seal(slot,role,&key,parent,roots)?,key,slices})
}

/// Streams the first slot's payload before its parent is known; none of these slices is final.
pub(crate) fn prepare_early_block(signer: &SigningKey, slot: u64, role: usize, payload: Vec<u8>) -> Result<(EarlyBlock,Vec<Vec<Shred>>)> {
    ensure!(payload.len() <= MAX_BLOCK_BYTES,"Proposer block exceeds local size limit");
    let key = crypto::fresh_key(); let mut roots = vec![]; let mut slices = vec![];
    for (index,plain) in payload.chunks(SLICE_BYTES).enumerate() {
        let shreds = slice(signer,&key,slot,role,index as u32,plain,false,None)?;
        roots.push(shreds[0].root); slices.push(shreds);
    }
    Ok((EarlyBlock {key,next_slice:slices.len() as u32,roots},slices))
}

/// Seals an early block with exactly the existing empty final slice carrying the chosen parent.
pub(crate) fn complete_early_block(signer: &SigningKey, slot: u64, role: usize, parent: Option<BlockRef>, mut early: EarlyBlock) -> Result<BuiltBlock> {
    let shreds = slice(signer,&early.key,slot,role,early.next_slice,&[],true,parent)?;
    early.roots.push(shreds[0].root);
    Ok(BuiltBlock {commitment:seal(slot,role,&early.key,parent,early.roots)?,key:early.key,slices:vec![shreds]})
}

fn within_budget<K: Ord + Clone>(mut due: Vec<(K, u128, u64)>, budget: u64) -> Vec<K> {
    due.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    let mut used = 0;
    due.into_iter()
        .filter(|(_, _, size)| {
            let fits = used + size <= budget;
            if fits {
                used += size;
            }
            fits
        })
        .map(|(key, _, _)| key)
        .collect()
}

pub(crate) fn pack(
    executor: &SvmExecutor,
    mempool: &mut BTreeMap<String, ValidatedTx>,
    fragment_mempool: &mut BTreeMap<(String, u8), VerifiedFragment>,
    in_flight: &mut BTreeMap<String, (u64, usize, ValidatedTx)>,
    s: u64,
    q: usize,
    censored: Option<&String>,
) -> (Vec<ValidatedTx>, Vec<crate::threshold::VerifiedFragment>) {
    let priority = |h: &crate::execution::TxHeader| h.priority_fee_per_cu.parse::<u128>().unwrap_or(0);
    let due = mempool
        .iter()
        .filter(|(id, tx)| tx.header.target_slot <= s && Some(*id) != censored)
        .map(|(id, tx)| (id.clone(), priority(&tx.header), bincode::serialized_size(tx).unwrap_or(u64::MAX)))
        .collect::<Vec<_>>();
    let due_fragments = fragment_mempool
        .iter()
        .filter(|(_, f)| f.header.target_slot <= s)
        .map(|(key, f)| (key.clone(), priority(&f.header), bincode::serialized_size(f).unwrap_or(u64::MAX)))
        .collect::<Vec<_>>();
    // Fragments first: a threshold transaction needs t of them together, so they take the budget's
    // first half and plain transactions the rest of it.
    let fragment_keys = within_budget(due_fragments, BLOCK_BYTES / 2);
    let used = fragment_keys
        .iter()
        .map(|key| bincode::serialized_size(&fragment_mempool[key]).unwrap_or(0))
        .sum::<u64>();
    let txs = within_budget(due, BLOCK_BYTES - used)
        .into_iter()
        .filter_map(|id| mempool.remove(&id))
        .filter(|tx| !executor.is_executed(&tx.header.id) && executor.blockhash_valid(&tx.bytes))
        .collect::<Vec<_>>();
    let fragments = fragment_keys
        .into_iter()
        .filter_map(|key| fragment_mempool.remove(&key))
        .collect::<Vec<_>>();
    let items = txs
        .iter()
        .map(|tx| (tx.header.fee_payer.clone(), q as u8, 1u64))
        .chain(fragments.iter().map(|f| (f.header.fee_payer.clone(), q as u8, 1u64)))
        .collect::<Vec<_>>();
    let mut admitted = executor
        .admit_inclusions(&items, s / 16)
        .unwrap_or_else(|_| vec![false; items.len()])
        .into_iter();
    let txs = txs
        .into_iter()
        .filter(|_| admitted.next().unwrap())
        .collect::<Vec<_>>();
    let fragments = fragments
        .into_iter()
        .filter(|_| admitted.next().unwrap())
        .collect();
    for tx in &txs {
        in_flight.insert(tx.header.id.clone(), (s, q, tx.clone()));
    }
    (txs, fragments)
}
