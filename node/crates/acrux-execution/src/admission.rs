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
    let hub_tag = Sha256::digest(b"account:Hub");
    let member_tag = Sha256::digest(b"account:Membership");
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
        hubs.insert(*address, (id, code.to_string()));
    }
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
        let Some((current_id, code)) = hubs.get(&hub) else { continue };
        if id != *current_id { continue }
        members.insert(identity, EffectiveMembership { identity, hub: code.clone(), effective_epoch });
    }
    members.into_values().collect()
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
    members.truncate(MAX_ADMITTED_MEMBERS);
    let order = catalog.iter().enumerate().map(|(index,identity)| (*identity,index)).collect::<BTreeMap<_,_>>();
    members.sort_by_key(|member| order[&member.identity]);
    Ok(AdmissionSnapshot { epoch, cutoff_slot, members })
}
