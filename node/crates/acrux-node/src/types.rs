use crate::{consensus, crypto::Shred, execution::ValidatedTx};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Commitment {
    pub slot: u64,
    pub proposer: usize,
    pub hash: [u8; 32],
    pub key_hash: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Attestation {
    pub slot: u64,
    pub proposer: usize,
    pub pi: Vec<Option<[u8; 32]>>,
    pub key: [u8; 32],
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SignedVote {
    pub vote: consensus::Vote,
    pub signature: Vec<u8>,
}
/// The existing bincode Vote, prefixed by the configured chain's SHA-256 only from the bridge's
/// first cblock onward. Earlier signatures and their on-disk encodings remain unchanged.
pub fn vote_message(vote: &consensus::Vote, bridge: Option<&crate::genesis::BridgeConfig>) -> anyhow::Result<Vec<u8>> {
    let mut message = vec![];
    if let Some(bridge) = bridge.filter(|rule| vote.slot >= rule.from_slot.div_ceil(4)) {
        message.extend_from_slice(&bridge.chain_tag());
    }
    message.extend_from_slice(&bincode::serialize(vote)?);
    Ok(message)
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Wire {
    Shreds(Vec<Shred>),
    Relay(Vec<Shred>),
    Commit(Commitment),
    Attest(Attestation),
    Vote(SignedVote),
    Certificate {
        certificate: consensus::Certificate,
        proofs: Vec<SignedVote>,
    },
    Submit(ValidatedTx),
    SubmitFragment(crate::threshold::VerifiedFragment),
    Handover {
        slot: u64,
        parent: consensus::BlockRef,
        shreds: Vec<Shred>,
        attestations: Vec<(Attestation, Vec<u8>)>,
    },
    /// Two signed commitments of one proposer for one slot: a slashing proof (Lemma 2.4).
    Equivocation {
        first: (Commitment, Vec<u8>),
        second: (Commitment, Vec<u8>),
    },
}
/// A Fault Lab scenario: the faulty behaviour of one proposer role in one window.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fault {
    pub scenario: String,
    pub window: u64,
    pub attacker: usize,
    /// Where the runner writes `outcome.json`.
    pub out: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Envelope {
    pub from: usize,
    pub body: Wire,
    pub signature: Vec<u8>,
}
/// What a proposer's block carries. From the bridge's first slot it also carries bridge credits;
/// blocks before it were encoded without them (`LegacyBlockPayload`), and `BlockPayload::decode`
/// reads both.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockPayload {
    pub transactions: Vec<ValidatedTx>,
    pub fragments: Vec<crate::threshold::VerifiedFragment>,
    pub credits: Vec<crate::execution::BridgeCredit>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LegacyBlockPayload {
    pub transactions: Vec<ValidatedTx>,
    pub fragments: Vec<crate::threshold::VerifiedFragment>,
}

impl BlockPayload {
    fn options() -> impl bincode::Options {
        use bincode::Options;
        bincode::DefaultOptions::new().with_fixint_encoding().with_limit(1024 * 1024).reject_trailing_bytes()
    }
    /// Encodes the payload, without the credits field before the bridge's first slot.
    pub fn encode(&self, with_credits: bool) -> anyhow::Result<Vec<u8>> {
        use bincode::Options;
        Ok(if with_credits {
            Self::options().serialize(self)?
        } else {
            Self::options().serialize(&LegacyBlockPayload { transactions: self.transactions.clone(), fragments: self.fragments.clone() })?
        })
    }
    pub fn decode(bytes: &[u8]) -> anyhow::Result<Self> {
        use bincode::Options;
        match Self::options().deserialize::<Self>(bytes) {
            Ok(payload) => Ok(payload),
            Err(_) => {
                let legacy: LegacyBlockPayload = Self::options().deserialize(bytes)?;
                Ok(Self { transactions: legacy.transactions, fragments: legacy.fragments, credits: vec![] })
            }
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockCommitment {
    pub slot: u64,
    pub proposer: usize,
    pub key_hash: [u8; 32],
    pub parent: Option<consensus::BlockRef>,
    pub roots: Vec<[u8; 32]>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Configuration {
    pub seed: u64,
    pub track_capacity_cu: u64,
    pub inclusion_fee: u64,
    pub base_fee: u64,
    pub reserve: u64,
    pub fragment_expiry_slots: u64,
    /// Slots of history the run store keeps; older slot and transaction records are deleted.
    pub retain_slots: u64,
    pub traffic_per_second: u64,
    pub windows_per_epoch: usize,
    pub crash_window: Option<u64>,
    pub crash_proposer: usize,
    pub gateway_port: u16,
    pub websocket_port: u16,
    pub metrics_port: u16,
    pub base_port: u16,
    pub store_path: String,
    pub genesis_programs: Vec<String>,
    /// Acrux Net's genesis accounts and programs (`backend/chain/genesis/acrux-net.json`).
    pub genesis_path: Option<String>,
    pub fault: Option<Fault>,
    pub run_seconds: Option<u64>,
    /// Keep the chain's finalized state in the store and continue from it on the next start.
    #[serde(default)]
    pub persist: bool,
    /// How long a persisting net keeps the transactions people send, in days and at most in count.
    #[serde(default)]
    pub keep_days: u64,
    #[serde(default)]
    pub keep_max: u64,
}
impl Default for Configuration {
    fn default() -> Self {
        Self {
            seed: 20261116,
            track_capacity_cu: 2_000_000,
            inclusion_fee: 1000,
            base_fee: 5000,
            reserve: 1_000_000,
            fragment_expiry_slots: 8,
            retain_slots: 24_000,
            traffic_per_second: 50,
            windows_per_epoch: 96,
            crash_window: None,
            crash_proposer: 3,
            gateway_port: 4930,
            websocket_port: 4931,
            metrics_port: 4989,
            base_port: 4950,
            store_path: "target/runs/net".into(),
            genesis_programs: vec![],
            genesis_path: None,
            fault: None,
            run_seconds: None,
            persist: false,
            keep_days: 7,
            keep_max: 200_000,
        }
    }
}

#[cfg(test)]
mod vote_tests {
    use super::*;
    use crate::{crypto, genesis::BridgeConfig};

    fn rule(chain_id: &str) -> BridgeConfig {
        BridgeConfig { program: solana_sdk::pubkey::Pubkey::new_unique(), from_slot: 400, epoch_slots: 432_000, chain_id: chain_id.into(), min_withdrawal: Default::default(), native_from_slot: None, native_mint: None, native_scale: None, stake_from_slot: None, genesis_stake_lamports: None, fee_burn_from_slot: None }
    }

    #[test]
    fn bridge_vote_is_bound_to_its_chain() {
        let vote = consensus::Vote { validator: 2, slot: 100, hash: Some([7; 32]), kind: consensus::VoteKind::Finalize };
        let signer = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let original = vote_message(&vote, Some(&rule("chain-a"))).unwrap();
        let other = vote_message(&vote, Some(&rule("chain-b"))).unwrap();
        let signature = crypto::sign_bytes(&signer, &original);
        assert_eq!(&original[..32], &crypto::hash(b"chain-a"));
        assert_eq!(&original[32..], bincode::serialize(&vote).unwrap());
        assert!(crypto::verify_bytes(&signer.verifying_key(), &original, &signature).is_ok());
        assert!(crypto::verify_bytes(&signer.verifying_key(), &other, &signature).is_err());
        assert!(crypto::verify_bytes(&signer.verifying_key(), &bincode::serialize(&vote).unwrap(), &signature).is_err());
    }

    #[test]
    fn votes_before_bridge_activation_keep_their_original_signatures() {
        let signer = ed25519_dalek::SigningKey::from_bytes(&[6; 32]);
        for kind in [consensus::VoteKind::Notar, consensus::VoteKind::NotarFallback, consensus::VoteKind::Skip, consensus::VoteKind::SkipFallback, consensus::VoteKind::Finalize] {
            let vote = consensus::Vote { validator: 3, slot: 99, hash: Some([9; 32]), kind };
            let legacy = bincode::serialize(&vote).unwrap();
            let signature = crypto::sign_bytes(&signer, &legacy);
            assert_eq!(vote_message(&vote, None).unwrap(), legacy);
            let message = vote_message(&vote, Some(&rule("chain-a"))).unwrap();
            assert_eq!(message, legacy);
            assert!(crypto::verify_bytes(&signer.verifying_key(), &message, &signature).is_ok());
            // Stored certificate votes retain their original bincode layout as well.
            let stored = bincode::serialize(&SignedVote { vote, signature }).unwrap();
            let restored: SignedVote = bincode::deserialize(&stored).unwrap();
            assert!(crypto::verify_bytes(&signer.verifying_key(), &vote_message(&restored.vote, Some(&rule("chain-a"))).unwrap(), &restored.signature).is_ok());
        }
    }
}
