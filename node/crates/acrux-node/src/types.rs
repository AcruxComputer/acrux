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
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BlockPayload {
    pub transactions: Vec<ValidatedTx>,
    pub fragments: Vec<crate::threshold::VerifiedFragment>,
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
