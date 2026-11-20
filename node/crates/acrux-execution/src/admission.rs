//! Registry membership and native delegations frozen at the previous epoch's cutoff.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::{account::Account, pubkey::Pubkey, stake::state::StakeStateV2};
use std::{collections::{BTreeMap, BTreeSet}, sync::Arc};

/// The live bridge certifies at most 32 voters with its u32 signature bitmap.
pub const MAX_ADMITTED_MEMBERS: usize = 32;
pub(crate) const RETAINED_SNAPSHOTS: usize = 4;
pub(crate) const LEGACY_GENESIS_STAKE: u64 = 100_000_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionConfig {
    pub from_slot: u64,
    pub epoch_slots: u64,
    pub registry_program: Pubkey,
    pub genesis: Vec<(Pubkey, String)>,
    pub genesis_stake_lamports: u64,
}
impl AdmissionConfig {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(self.epoch_slots > 0 && self.epoch_slots % 32 == 0 && self.from_slot % self.epoch_slots == 0,
            "Admission starts at an epoch boundary with a midpoint proposer-window boundary");
        ensure!(self.genesis.len() == 12 && self.genesis.iter().map(|(identity,_)| identity).collect::<BTreeSet<_>>().len() == 12
            && self.genesis.iter().all(|(_,hub)| valid_code(hub)) && self.genesis_stake_lamports > 0, "Invalid admission genesis rule");
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionMember {
    pub identity: Pubkey,
    pub hub: String,
    pub stake: u64,
    pub admitted_at_epoch: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionSnapshot {
    pub epoch: u64,
    pub cutoff_slot: u64,
    pub members: Vec<AdmissionMember>,
}
/// Registry ECEF metres frozen beside an epoch's voter snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AdmissionHub { pub code:String, pub x:i32, pub y:i32, pub z:i32, pub activated_at_epoch:Option<u64> }
impl AdmissionHub {
    pub fn latitude_longitude(&self)->(f64,f64) {
        let (x,y,z)=(self.x as f64,self.y as f64,self.z as f64);
        let (a,f)=(6_378_137.0f64,1.0/298.257223563); let b=a*(1.0-f);
        let (e2,ep2)=(f*(2.0-f),(a*a-b*b)/(b*b));
        let p=x.hypot(y); let theta=(z*a).atan2(p*b);
        ((z+ep2*b*theta.sin().powi(3)).atan2(p-e2*a*theta.cos().powi(3)).to_degrees(),y.atan2(x).to_degrees())
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct HubSnapshot { pub epoch:u64, pub hubs:Vec<AdmissionHub> }
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerAnnouncement { pub identity:Pubkey, pub address:String, pub certificate:Vec<u8> }
pub fn peer_address(registry:&Pubkey,identity:&Pubkey)->Pubkey { Pubkey::find_program_address(&[b"peer",identity.as_ref()],registry).0 }
pub(crate) fn hubs_address(bridge:&Pubkey)->Pubkey { Pubkey::find_program_address(&[b"admission_hubs"],bridge).0 }
pub(crate) fn hub_activation_address(bridge:&Pubkey,code:&str,id:u64)->Pubkey {
    Pubkey::find_program_address(&[b"hub_activation",code.as_bytes(),&id.to_le_bytes()],bridge).0
}
pub(crate) fn eligible_hubs(members:&[AdmissionMember])->Result<BTreeSet<String>> {
    let total=members.iter().try_fold(0u64,|sum,member|sum.checked_add(member.stake)).context("Admission stake overflow")?;
    let mut hubs=BTreeMap::<String,acrux_schedule::Hub>::new();
    for member in members {
        hubs.entry(member.hub.clone()).or_insert_with(||acrux_schedule::Hub {code:member.hub.clone(),region:String::new(),members:vec![]})
            .members.push(acrux_schedule::Member {identity:member.identity.to_string(),stake:member.stake});
    }
    Ok(hubs.into_values().filter(|hub|hub.members.len()>=acrux_schedule::MINIMUM_MEMBERS)
        .map(|hub|acrux_schedule::cap_stakes(&hub)).collect::<Result<Vec<_>>>()?.into_iter()
        .filter(|hub|hub.cap>0&&u128::from(hub.stake)*100>=u128::from(total)).map(|hub|hub.code).collect())
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectiveMembership {
    pub identity: Pubkey,
    pub hub: String,
    pub effective_epoch: u64,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct AdmissionState {
    pub rule: AdmissionConfig,
    pub prepared_epoch: u64,
    pub next_cutoff_epoch: u64,
    pub catalog_len: u64,
    pub snapshots: Vec<AdmissionSnapshot>,
}
pub(crate) fn catalog_address(bridge: &Pubkey, identity: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"validator_identity", identity.as_ref()], bridge).0
}
pub(crate) fn catalog(accounts: &BTreeMap<Pubkey, Arc<Account>>, bridge: &Pubkey, state: &AdmissionState) -> Result<Vec<Pubkey>> {
    let tag = Sha256::digest(b"account:ValidatorIdentity");
    let mut entries = BTreeMap::new();
    for (address, account) in accounts {
        let data = &account.data;
        if account.owner != *bridge || account.executable || data.len() != 48 || data[..8] != tag[..8] { continue }
        let index = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let identity = Pubkey::new_from_array(data[16..48].try_into().unwrap());
        if *address != catalog_address(bridge, &identity) { continue }
        ensure!(index < state.catalog_len && entries.insert(index, identity).is_none(), "Invalid committed validator catalog");
    }
    ensure!(entries.len() as u64 == state.catalog_len && entries.keys().enumerate().all(|(expected,index)| expected as u64 == *index), "Committed validator catalog is incomplete");
    let identities = entries.into_values().collect::<Vec<_>>();
    ensure!(identities.len() >= state.rule.genesis.len() && state.rule.genesis.iter().map(|(identity,_)| identity).eq(identities[..state.rule.genesis.len()].iter()), "Validator catalog changed its genesis order");
    Ok(identities)
}
pub(crate) fn marker_address(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"admission"], bridge).0
}
fn valid_code(code: &str) -> bool {
    !code.is_empty() && code.len() <= 8 && code.bytes().all(|byte| byte.is_ascii_uppercase())
}

/// Only authentic, canonical Hub Registry PDAs participate. Old memberships of a released hub
/// are harmless: its id no longer matches, so they do not become members of a re-registered hub.
pub(crate) fn memberships(accounts: &BTreeMap<Pubkey, Arc<Account>>, registry: &Pubkey, epoch: u64) -> Vec<EffectiveMembership> {
    let hubs=registry_hubs(accounts,registry);
    let member_tag = Sha256::digest(b"account:Membership");
    let mut members = BTreeMap::new();
    for (address, account) in accounts {
        let data = &account.data;
        if account.owner != *registry || account.executable || data.len() != 154 || data[..8] != member_tag[..8] { continue }
        let identity = Pubkey::new_from_array(data[8..40].try_into().unwrap());
        let (canonical, bump) = Pubkey::find_program_address(&[b"member", identity.as_ref()], registry);
        if *address != canonical || data[153] != bump { continue }
        let pending = data[96];
        if pending > 2 { continue }
        let effective = u64::from_le_bytes(data[88..96].try_into().unwrap());
        let pending_epoch = u64::from_le_bytes(data[137..145].try_into().unwrap());
        let (hub, id, effective_epoch) = if pending != 0 && pending_epoch <= epoch {
            if pending == 1 { continue }
            (Pubkey::new_from_array(data[97..129].try_into().unwrap()), u64::from_le_bytes(data[129..137].try_into().unwrap()), pending_epoch)
        } else {
            if effective > epoch { continue }
            (Pubkey::new_from_array(data[40..72].try_into().unwrap()), u64::from_le_bytes(data[72..80].try_into().unwrap()), effective)
        };
        let Some((current_id, place)) = hubs.get(&hub) else { continue };
        if id != *current_id { continue }
        members.insert(identity, EffectiveMembership { identity, hub: place.code.clone(), effective_epoch });
    }
    members.into_values().collect()
}

pub(crate) fn registry_hubs(accounts:&BTreeMap<Pubkey,Arc<Account>>,registry:&Pubkey)->BTreeMap<Pubkey,(u64,AdmissionHub)> {
    let hub_tag = Sha256::digest(b"account:Hub");
    let mut hubs = BTreeMap::new();
    for (address, account) in accounts {
        let data = &account.data;
        if account.owner != *registry || account.executable || data.len() != 113 || data[..8] != hub_tag[..8] { continue }
        let code_bytes = &data[16..24];
        let length = code_bytes.iter().position(|byte| *byte == 0).unwrap_or(8);
        if code_bytes[length..].iter().any(|byte| *byte != 0) { continue }
        let Ok(code) = std::str::from_utf8(&code_bytes[..length]) else { continue };
        if !valid_code(code) { continue }
        let (canonical, bump) = Pubkey::find_program_address(&[b"hub", code.as_bytes()], registry);
        if *address != canonical || data[112] != bump { continue }
        let id = u64::from_le_bytes(data[8..16].try_into().unwrap());
        if id == 0 { continue }
        let [x,y,z]=[24,28,32].map(|offset|i32::from_le_bytes(data[offset..offset+4].try_into().unwrap()));
        let radius=[x,y,z].into_iter().map(|value|u128::from(value.unsigned_abs()).pow(2)).sum::<u128>();
        if !(6_300_000u128.pow(2)..=6_400_000u128.pow(2)).contains(&radius) {continue}
        hubs.insert(*address, (id, AdmissionHub {code:code.to_string(),x,y,z,activated_at_epoch:None}));
    }
    hubs
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerMemo { address:String, certificate:String }
fn valid_peer_address(address:&str)->bool {
    if address.is_empty()||address.len()>255||!address.is_ascii()||address.bytes().any(|byte|byte.is_ascii_whitespace()) {return false}
    let Some((host,port))=address.rsplit_once(':') else{return false};
    if port.is_empty()||!port.bytes().all(|byte|byte.is_ascii_digit())||port.parse::<u16>().ok().is_none_or(|port|port==0) {return false}
    if let Some(host)=host.strip_prefix('[').and_then(|host|host.strip_suffix(']')) {return host.parse::<std::net::Ipv6Addr>().is_ok()}
    !host.is_empty()&&host.len()<=253&&host.bytes().all(|byte|byte.is_ascii_alphanumeric()||matches!(byte,b'.'|b'-'))
}
fn valid_certificate(bytes:&[u8])->bool {
    if bytes.len()<2||bytes.len()>2048||bytes[0]!=0x30 {return false}
    let (header,length)=match bytes[1] {
        length@0..=127=>(2,usize::from(length)),
        0x81 if bytes.len()>=3&&bytes[2]>=128=>(3,usize::from(bytes[2])),
        0x82 if bytes.len()>=4&&bytes[2]>0=>(4,usize::from(u16::from_be_bytes([bytes[2],bytes[3]]))),
        _=>return false,
    };
    header+length==bytes.len()
}
pub(crate) fn valid_peer(peer:&PeerAnnouncement)->bool {valid_peer_address(&peer.address)&&valid_certificate(&peer.certificate)}
/// Only a successful top-level Memo with exactly its identity signer can announce a link.
pub(crate) fn peer_memos(tx:&solana_sdk::transaction::VersionedTransaction)->Vec<PeerAnnouncement> {
    use base64::Engine;
    let keys=tx.message.static_account_keys();
    let Some(identity)=keys.first() else{return vec![]};
    let signers=usize::from(tx.message.header().num_required_signatures);
    tx.message.instructions().iter().filter_map(|instruction| {
        let program=keys.get(instruction.program_id_index as usize)?.to_string();
        if program!="MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr"&&program!="Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo" {return None}
        if instruction.accounts.as_slice()!=[0]||signers==0 {return None}
        let raw=std::str::from_utf8(&instruction.data).ok()?.strip_prefix("acrux-peer-v1:")?;
        let memo:PeerMemo=serde_json::from_str(raw).ok()?;
        if memo.certificate.len()>2732 {return None}
        let peer=PeerAnnouncement {identity:*identity,address:memo.address,certificate:base64::engine::general_purpose::STANDARD.decode(memo.certificate).ok()?};
        valid_peer(&peer).then_some(peer)
    }).collect()
}

/// A validator's own native delegation is counted only while funded and active in the target
/// epoch. Stake-owned accounts outside the actual StakeStateV2 ABI are not delegations.
pub(crate) fn delegated_weights(accounts: &BTreeMap<Pubkey, Arc<Account>>, identities: &[Pubkey], epoch: u64) -> Result<BTreeMap<Pubkey, u64>> {
    let votes = identities.iter().map(|identity| (crate::execution::validator_vote_address(identity), *identity)).collect::<BTreeMap<_,_>>();
    let mut weights = BTreeMap::<Pubkey,u64>::new();
    for account in accounts.values() {
        if account.owner != solana_sdk::stake::program::id() || account.executable || account.data.len() != StakeStateV2::size_of() { continue }
        let Ok(StakeStateV2::Stake(meta, stake, _)) = bincode::deserialize::<StakeStateV2>(&account.data) else { continue };
        let delegation = stake.delegation;
        if delegation.activation_epoch >= epoch || delegation.deactivation_epoch < epoch
            || delegation.stake > account.lamports.saturating_sub(meta.rent_exempt_reserve) { continue }
        let Some(identity) = votes.get(&delegation.voter_pubkey) else { continue };
        let total = weights.entry(*identity).or_default();
        *total = total.checked_add(delegation.stake).context("Admission stake overflow")?;
    }
    Ok(weights)
}

pub(crate) fn snapshot(rule: &AdmissionConfig, accounts: &BTreeMap<Pubkey, Arc<Account>>, catalog: &[Pubkey], epoch: u64, cutoff_slot: u64, previous: Option<&AdmissionSnapshot>) -> Result<AdmissionSnapshot> {
    let effective = memberships(accounts, &rule.registry_program, epoch).into_iter().map(|member| (member.identity, member)).collect::<BTreeMap<_,_>>();
    let delegated = delegated_weights(accounts, catalog, epoch)?;
    let genesis = rule.genesis.iter().map(|(identity,_)| *identity).collect::<BTreeSet<_>>();
    let mut members = vec![];
    for identity in catalog {
        let Some(membership) = effective.get(identity) else { continue };
        let stake = delegated.get(identity).copied().unwrap_or(0).checked_add(if genesis.contains(identity) { rule.genesis_stake_lamports } else { 0 }).context("Admission stake overflow")?;
        if stake == 0 { continue }
        let admitted_at_epoch = previous.and_then(|snapshot| snapshot.members.iter().find(|member| member.identity == *identity)).map_or(epoch, |member| member.admitted_at_epoch);
        members.push(AdmissionMember { identity:*identity,hub:membership.hub.clone(),stake,admitted_at_epoch });
    }
    // Excess stake remains waiting instead of halting permissionless membership processing.
    members.sort_by(|a,b| b.stake.cmp(&a.stake).then(a.identity.cmp(&b.identity)));
    let genesis_count=members.iter().filter(|member|genesis.contains(&member.identity)).count();
    let mut outside=0;
    members.retain(|member| {
        if genesis.contains(&member.identity) {true} else {outside+=1;outside<=MAX_ADMITTED_MEMBERS-genesis_count}
    });
    let eligible=eligible_hubs(&members)?;
    members.retain(|member|genesis.contains(&member.identity)||eligible.contains(&member.hub));
    let order = catalog.iter().enumerate().map(|(index,identity)| (*identity,index)).collect::<BTreeMap<_,_>>();
    members.sort_by_key(|member| order[&member.identity]);
    Ok(AdmissionSnapshot { epoch, cutoff_slot, members })
}
