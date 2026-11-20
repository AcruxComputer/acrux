//! The legacy commitment stays byte-for-byte stable; Lanes append their ordered decisions.
use crate::{consensus::{BlockRef, Hash}, crypto};
use anyhow::{ensure, Context, Result};
use bincode::Options;

pub type Accepted = (u64, Option<Vec<usize>>, Vec<Option<Hash>>);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Parsed {
    pub cblock: u64,
    pub parent: Option<BlockRef>,
    pub accepted: Vec<Accepted>,
    pub state: String,
    pub lane_hashes: Option<Vec<Option<Hash>>>,
}

pub fn encode_legacy(cblock: u64, parent: Option<BlockRef>, accepted: &[Accepted], state: &str) -> Result<Vec<u8>> {
    // Field order, representations and bincode's fixed-width encoding are the
    // original tuple. References serialize identically to their owned values.
    Ok(bincode::serialize(&(cblock, parent, accepted, state))?)
}

pub fn encode_lanes(cblock: u64, parent: Option<BlockRef>, accepted: &[Accepted], state: &str,
    lane_hashes: &[Option<Hash>]) -> Result<Vec<u8>> {
    validate(cblock, parent, accepted, state, Some(lane_hashes))?;
    // The bridge reads only the first u64 and hashes the remaining opaque bytes.
    Ok(bincode::serialize(&(cblock, parent, accepted, state, lane_hashes))?)
}

pub fn decode(bytes: &[u8], lanes: bool) -> Result<Parsed> {
    let options = bincode::DefaultOptions::new().with_fixint_encoding()
        .with_limit(1024 * 1024).reject_trailing_bytes();
    let parsed = if lanes {
        let (cblock, parent, accepted, state, lane_hashes) = options.deserialize::<
            (u64, Option<BlockRef>, Vec<Accepted>, String, Vec<Option<Hash>>)
        >(bytes).context("Invalid Lanes cblock preimage")?;
        Parsed { cblock, parent, accepted, state, lane_hashes: Some(lane_hashes) }
    } else {
        let (cblock, parent, accepted, state) = options.deserialize::<
            (u64, Option<BlockRef>, Vec<Accepted>, String)
        >(bytes).context("Invalid legacy cblock preimage")?;
        Parsed { cblock, parent, accepted, state, lane_hashes: None }
    };
    validate(parsed.cblock, parsed.parent, &parsed.accepted, &parsed.state, parsed.lane_hashes.as_deref())?;
    Ok(parsed)
}

pub fn lane_hash(lane: usize, cblock: u64, parent: Option<BlockRef>, hashes: &[Hash; 4]) -> Result<Hash> {
    ensure!((1..=3).contains(&lane), "Invalid proposer lane");
    Ok(crypto::hash(&bincode::serialize(&(
        b"acrux-lab-lane-block-v1".as_slice(), lane as u8, cblock, parent, hashes.as_slice(),
    ))?))
}

fn validate(cblock: u64, parent: Option<BlockRef>, accepted: &[Accepted], state: &str,
    lane_hashes: Option<&[Option<Hash>]>) -> Result<()> {
    ensure!(cblock <= (u64::MAX - 3) / 4 && accepted.len() == 4, "Invalid cblock slot range");
    ensure!(state.len() == 64 && hex::decode(state).is_ok_and(|bytes| bytes.len() == 32),
        "Invalid cblock end state hash");
    if let Some(parent) = parent {
        ensure!(parent.slot < cblock, "Cblock parent does not precede its child");
        ensure!(cblock % 4 == 0 || parent.slot + 1 == cblock,
            "Cblock inside a window must follow its parent");
    } else {
        ensure!(cblock % 4 == 0, "Cblock inside a window must have a parent");
    }
    let minimum = if lane_hashes.is_some() { 1 } else { 2 };
    for (offset, (slot, agreed, pi)) in accepted.iter().enumerate() {
        ensure!(*slot == cblock * 4 + offset as u64 && pi.len() == 3, "Invalid accepted-slot preimage");
        let roles = agreed.as_ref().context("Cblock contains a slot without agreement")?;
        ensure!((minimum..=3).contains(&roles.len()) && roles.windows(2).all(|pair| pair[0] < pair[1])
            && roles.iter().all(|role| (1..=3).contains(role) && pi[*role - 1].is_some()),
            "Invalid agreed proposer set");
        ensure!(agreed == &accepted[0].1, "Cblock changes its accepted proposer set");
        if lane_hashes.is_some() {
            ensure!(pi.iter().enumerate().all(|(index, hash)| roles.contains(&(index + 1)) == hash.is_some()),
                "Skipped lane has an accepted-slot hash");
        }
    }
    if let Some(hashes) = lane_hashes {
        ensure!(hashes.len() == 3, "Invalid ordered lane decisions");
        let roles = accepted[0].1.as_ref().unwrap();
        ensure!(hashes.iter().enumerate().all(|(index, hash)| roles.contains(&(index + 1)) == hash.is_some()),
            "Lane decisions differ from the accepted proposer set");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_commitment_bytes_stay_exact_and_lanes_keep_the_bridge_slot_prefix() {
        let cblock = 12;
        let parent = Some(BlockRef { slot: 11, hash: [7; 32] });
        let state = hex::encode([8; 32]);
        let accepted = (0..4).map(|offset| (cblock * 4 + offset, Some(vec![1, 2]),
            vec![Some([1; 32]), Some([2; 32]), None])).collect::<Vec<_>>();
        let old = bincode::serialize(&(cblock, parent, accepted.clone(), state.clone())).unwrap();
        let unchanged = encode_legacy(cblock, parent, &accepted, &state).unwrap();
        assert_eq!(unchanged, old, "every pre-activation hash retains the original tuple bytes");
        let parsed = decode(&unchanged, false).unwrap();
        assert_eq!(parsed.accepted, accepted);
        assert_eq!(parsed.lane_hashes, None);

        // Reference the exact pre-Lanes tuple, chain-tagged Vote and Wire variant
        // layouts, then obtain the same certificate through the ordinary Votor.
        use crate::{consensus::{Action, Block, Certificate, CertificateKind, Message, Vote, VoteKind, Votor},
            types::{vote_message, SignedVote, Wire}};
        let outbox_root = [10; 32];
        let old_hash = crypto::hash(&[old.as_slice(), &outbox_root].concat());
        let current_hash = crypto::hash(&[unchanged.as_slice(), &outbox_root].concat());
        assert_eq!(current_hash, old_hash, "pre-Lanes bridge cblock hashes are unchanged");
        let rule: crate::genesis::BridgeConfig = serde_json::from_value(serde_json::json!({
            "program":solana_sdk::pubkey::Pubkey::new_from_array([3;32]).to_string(),
            "fromSlot":16,"epochSlots":432_000,"chainId":"legacy-compatibility-check","lanesFromSlot":64,
        })).unwrap();
        assert!(cblock * 4 < rule.lanes_from_slot.unwrap());
        let old_votes = (0..10).map(|validator| Vote {
            validator, slot:cblock, hash:Some(old_hash), kind:VoteKind::Notar,
        }).collect::<Vec<_>>();
        let old_certificate = Certificate {
            kind:CertificateKind::FastFinalization, slot:cblock, hash:Some(old_hash), votes:old_votes.clone(),
        };
        let mut votor = Votor::new(0, vec![1;12]).unwrap();
        votor.resume(parent.unwrap());
        let mut actions = votor.local_block(Block {
            id:BlockRef {slot:cblock,hash:current_hash}, parent,
        }).unwrap();
        for vote in old_votes.iter().skip(1) {
            actions.extend(votor.receive(Message::Vote(vote.clone())).unwrap());
        }
        let certificate = actions.into_iter().find_map(|action| match action {
            Action::SendCertificate(certificate) if certificate.kind == CertificateKind::FastFinalization => Some(certificate),
            _ => None,
        }).unwrap();
        assert_eq!(bincode::serialize(&certificate).unwrap(), bincode::serialize(&old_certificate).unwrap(),
            "ordinary Votor emits the same pre-Lanes certificate bytes");
        assert_eq!(votor.finalized(), Some(BlockRef {slot:cblock,hash:old_hash}));
        let old_proofs = old_votes.iter().map(|vote| {
            let signer = ed25519_dalek::SigningKey::from_bytes(&[(vote.validator + 1) as u8;32]);
            let bytes = bincode::serialize(vote).unwrap();
            assert_eq!(vote_message(vote,None).unwrap(), bytes, "unbridged legacy votes keep their old bytes");
            let old_message = [rule.chain_tag().as_slice(), bytes.as_slice()].concat();
            let message = vote_message(vote,Some(&rule)).unwrap();
            assert_eq!(message, old_message, "the pre-Lanes bridge chain tag and Vote bytes are unchanged");
            let signature = crypto::sign_bytes(&signer,&old_message);
            assert_eq!(crypto::sign_bytes(&signer,&message), signature);
            crypto::verify_bytes(&signer.verifying_key(),&message,&signature).unwrap();
            SignedVote {vote:vote.clone(),signature}
        }).collect::<Vec<_>>();
        let proofs = certificate.votes.iter().map(|vote| {
            let signer = ed25519_dalek::SigningKey::from_bytes(&[(vote.validator + 1) as u8;32]);
            SignedVote {vote:vote.clone(),signature:crypto::sign_bytes(&signer,&vote_message(vote,Some(&rule)).unwrap())}
        }).collect::<Vec<_>>();
        assert_eq!(bincode::serialize(&proofs).unwrap(), bincode::serialize(&old_proofs).unwrap());
        assert_eq!(bincode::serialize(&Wire::Certificate {certificate,proofs}).unwrap(),
            bincode::serialize(&(5u32,old_certificate,old_proofs)).unwrap(),
            "the original Wire Certificate discriminant and signed proof layout stay exact");

        let one_lane = (0..4).map(|offset| (cblock * 4 + offset, Some(vec![2]),
            vec![None, Some([2; 32]), None])).collect::<Vec<_>>();
        let lane_parent = Some(BlockRef { slot: 11, hash: [9; 32] });
        let hashes = [Some([1; 32]), None, Some([3; 32])];
        let sealed = [[4; 32], [5; 32], [6; 32], [7; 32]];
        let commitment = lane_hash(2, cblock, lane_parent, &sealed).unwrap();
        assert_eq!(commitment, crypto::hash(&bincode::serialize(&(
            b"acrux-lab-lane-block-v1".as_slice(), 2u8, cblock, lane_parent, sealed.to_vec(),
        )).unwrap()));
        let ordered = [None, Some(commitment), None];
        let lanes = encode_lanes(cblock, parent, &one_lane, &state, &ordered).unwrap();
        assert_eq!(&lanes[..8], &cblock.to_le_bytes(), "the unchanged Solana program reads this cblock slot");
        let parsed = decode(&lanes, true).unwrap();
        assert_eq!(parsed.cblock, cblock);
        assert_eq!(parsed.parent, parent);
        assert_eq!(parsed.accepted, one_lane);
        assert_eq!(parsed.state, state);
        assert_eq!(parsed.lane_hashes, Some(ordered.to_vec()));
        assert!(decode(&lanes, false).is_err(), "the configured legacy decoder accepts no Lanes suffix");
        assert!(decode(&old, true).is_err(), "Lanes decoding does not guess a legacy fallback");
        assert!(encode_lanes(cblock, parent, &parsed.accepted, &parsed.state, &hashes).is_err());
        let root = [10; 32];
        let certified = crypto::hash(&[lanes.as_slice(), &root].concat());
        assert_ne!(certified, crypto::hash(&lanes));
        assert_ne!(certified, crypto::hash(&[lanes.as_slice(), &[11; 32]].concat()),
            "the opaque preimage still binds the outbox root exactly as post_cblock does");
    }
}
