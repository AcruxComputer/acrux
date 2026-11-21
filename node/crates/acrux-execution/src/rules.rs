//! Rule changes signed by the validator set. A validator identity signs a Memo naming a change to
//! the chain's rule; each such Memo is its vote. Once the identities that signed hold 60% of the
//! epoch's admitted stake, the change is in force from the next bridge-epoch boundary.
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use solana_sdk::pubkey::Pubkey;
use std::collections::BTreeMap;

pub const MEMO_PREFIX: &str = "acrux-rule-v1:";
/// The share of the epoch's admitted stake that puts a change in force.
pub const PASS_PERCENT: u128 = 60;
/// The keys this node applies from a rule in force: activation slots it gates in execution.
pub const KNOWN_KEYS: &[&str] = &["genesisStepFromSlot", "nativeMintFromSlot"];
const MAX_KEYS: usize = 8;
const MAX_OPEN: usize = 16;

/// A change to the rule: activation slots by key, and the oldest node version that carries them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RuleChange {
    pub keys: BTreeMap<String, u64>,
    pub min_version: String,
}
impl RuleChange {
    pub fn hash(&self) -> [u8; 32] {
        Sha256::digest(bincode::serialize(self).expect("rule change encodes")).into()
    }
    /// Structural only, so every node version accepts and rejects the same votes.
    fn valid(&self, epoch_slots: u64) -> bool {
        !self.keys.is_empty() && self.keys.len() <= MAX_KEYS
            && self.keys.iter().all(|(key, slot)| !key.is_empty() && key.len() <= 40 && key.bytes().all(|byte| byte.is_ascii_alphanumeric()) && slot % epoch_slots == 0)
            && version(&self.min_version).is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Rule {
    pub hash: [u8; 32],
    pub change: RuleChange,
    /// The epoch whose admitted stake the votes are counted against.
    pub epoch: u64,
    pub voters: Vec<Pubkey>,
    pub signed_stake: u64,
    pub total_stake: u64,
    pub in_force_from_slot: Option<u64>,
}

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct RulesState {
    pub rules: Vec<Rule>,
}

pub(crate) fn rules_address(bridge: &Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[b"rules"], bridge).0
}

pub fn version(text: &str) -> Option<(u64, u64, u64)> {
    let mut parts = text.split('.').map(|part| (!part.is_empty() && part.len() <= 6 && part.bytes().all(|byte| byte.is_ascii_digit())).then(|| part.parse().ok()).flatten());
    let parsed = (parts.next()??, parts.next()??, parts.next()??);
    parts.next().is_none().then_some(parsed)
}

/// Rule votes in a successful transaction: top-level Memos whose only account is the fee payer.
pub(crate) fn rule_memos(tx: &solana_sdk::transaction::VersionedTransaction) -> Vec<RuleChange> {
    let keys = tx.message.static_account_keys();
    if keys.is_empty() || tx.message.header().num_required_signatures == 0 { return vec![] }
    tx.message.instructions().iter().filter_map(|instruction| {
        let program = keys.get(instruction.program_id_index as usize)?.to_string();
        if program != "MemoSq4gqABAXKb96qnH8TysNcWxMyWCqXgDLGmfcHr" && program != "Memo1UhkJRfHyvLMcVucJwxXeuD728EqVDDwQDxFMNo" { return None }
        if instruction.accounts.as_slice() != [0] { return None }
        serde_json::from_str(std::str::from_utf8(&instruction.data).ok()?.strip_prefix(MEMO_PREFIX)?).ok()
    }).collect()
}

/// Counts one identity's vote for `change` in `epoch`, against that epoch's admitted stakes.
/// Returns whether the state changed.
pub(crate) fn vote(state: &mut RulesState, change: RuleChange, voter: Pubkey, epoch: u64, epoch_slots: u64, stakes: &BTreeMap<Pubkey, u64>) -> bool {
    let Some(&own) = stakes.get(&voter) else { return false };
    if own == 0 || !change.valid(epoch_slots) { return false }
    let boundary = (epoch + 1) * epoch_slots;
    // A change only names slots from the boundary it would take force at.
    if change.keys.values().any(|slot| *slot < boundary) { return false }
    let hash = change.hash();
    state.rules.retain(|rule| rule.in_force_from_slot.is_some() || rule.epoch >= epoch || rule.hash == hash);
    let at = match state.rules.iter().position(|rule| rule.hash == hash) {
        Some(at) => at,
        None => {
            let open = state.rules.iter().filter(|rule| rule.in_force_from_slot.is_none()).count();
            if open >= MAX_OPEN { return false }
            state.rules.push(Rule { hash, change, epoch, voters: vec![], signed_stake: 0, total_stake: 0, in_force_from_slot: None });
            state.rules.len() - 1
        }
    };
    let rule = &mut state.rules[at];
    if rule.in_force_from_slot.is_some() { return false }
    if rule.epoch != epoch { rule.epoch = epoch; rule.voters.clear(); }
    if rule.voters.contains(&voter) { return false }
    rule.voters.push(voter);
    rule.voters.sort();
    rule.signed_stake = rule.voters.iter().map(|voter| stakes.get(voter).copied().unwrap_or(0)).sum();
    rule.total_stake = stakes.values().sum();
    if u128::from(rule.signed_stake) * 100 >= u128::from(rule.total_stake) * PASS_PERCENT {
        rule.in_force_from_slot = Some(boundary);
    }
    true
}

/// A key's activation slot at `slot`: the genesis rule's, replaced by each rule in force that
/// names it, until the slot has been reached. A key that has activated is not moved again.
pub(crate) fn effective_slot(rules: &[Rule], key: &str, genesis: Option<u64>, slot: u64) -> Option<u64> {
    let mut passed = rules.iter().filter_map(|rule| Some((rule.in_force_from_slot.filter(|from| *from <= slot)?, rule.change.keys.get(key)?))).collect::<Vec<_>>();
    passed.sort_by_key(|(from, _)| *from);
    passed.into_iter().fold(genesis, |current, (from, value)| if current.is_some_and(|active| active <= from) { current } else { Some(*value) })
}

/// Why this node can't carry a rule in force at `slot`, if it can't.
pub(crate) fn unsupported(rules: &[Rule], slot: u64, own_version: &str) -> Option<String> {
    let own = version(own_version).expect("crate version is x.y.z");
    rules.iter().filter(|rule| rule.in_force_from_slot.is_some_and(|from| from <= slot)).find_map(|rule| {
        let unknown = rule.change.keys.keys().filter(|key| !KNOWN_KEYS.contains(&key.as_str())).cloned().collect::<Vec<_>>();
        (version(&rule.change.min_version) > Some(own) || !unknown.is_empty()).then(|| format!(
            "The rule {} in force from slot {} needs node {} or later{}; this node is {own_version}. Update the node to carry on.",
            hex::encode(rule.hash), rule.in_force_from_slot.unwrap(), rule.change.min_version,
            if unknown.is_empty() { String::new() } else { format!(" for {}", unknown.join(", ")) }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn change(keys: &[(&str, u64)], min: &str) -> RuleChange {
        RuleChange { keys: keys.iter().map(|(key, slot)| (key.to_string(), *slot)).collect(), min_version: min.into() }
    }

    #[test]
    fn a_change_takes_force_at_the_next_boundary_once_sixty_percent_sign() {
        let ids = (0..10).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
        let stakes = ids.iter().map(|id| (*id, 10)).collect::<BTreeMap<_, _>>();
        let mut state = RulesState::default();
        let wanted = change(&[("genesisStepFromSlot", 64)], "0.4.37");
        for id in &ids[..5] { assert!(vote(&mut state, wanted.clone(), *id, 2, 16, &stakes)); }
        assert!(!vote(&mut state, wanted.clone(), ids[0], 2, 16, &stakes), "a second vote from one identity counts once");
        assert_eq!(state.rules[0].in_force_from_slot, None);
        assert!(vote(&mut state, wanted.clone(), ids[5], 2, 16, &stakes));
        assert_eq!((state.rules[0].signed_stake, state.rules[0].total_stake, state.rules[0].in_force_from_slot), (60, 100, Some(48)));
        assert!(!vote(&mut state, wanted, ids[6], 2, 16, &stakes), "a rule in force takes no more votes");
        assert!(!vote(&mut state, change(&[("genesisStepFromSlot", 32)], "0.4.37"), ids[0], 2, 16, &stakes), "a change can't name a slot before its boundary");
        assert!(!vote(&mut state, change(&[("genesisStepFromSlot", 72)], "0.4.37"), ids[0], 2, 16, &stakes), "slots are epoch boundaries");
        assert!(!vote(&mut state, change(&[("genesisStepFromSlot", 64)], "next"), ids[0], 2, 16, &stakes));
        assert!(!vote(&mut state, change(&[("genesisStepFromSlot", 80)], "0.4.37"), Pubkey::new_unique(), 2, 16, &stakes), "only admitted identities vote");
    }

    #[test]
    fn votes_are_counted_within_one_epoch() {
        let ids = (0..10).map(|_| Pubkey::new_unique()).collect::<Vec<_>>();
        let stakes = ids.iter().map(|id| (*id, 10)).collect::<BTreeMap<_, _>>();
        let mut state = RulesState::default();
        let wanted = change(&[("genesisStepFromSlot", 128)], "0.4.37");
        for id in &ids[..5] { vote(&mut state, wanted.clone(), *id, 2, 16, &stakes); }
        vote(&mut state, wanted, ids[5], 3, 16, &stakes);
        assert_eq!((state.rules[0].epoch, state.rules[0].voters.len(), state.rules[0].in_force_from_slot), (3, 1, None));
        vote(&mut state, change(&[("nativeMintFromSlot", 128)], "0.4.37"), ids[0], 5, 16, &stakes);
        assert_eq!(state.rules.len(), 1, "an open change from an earlier epoch is dropped");
    }

    #[test]
    fn a_key_moves_only_until_it_activates_and_a_newer_rule_stops_an_older_node() {
        let rule = |from: u64, slot: u64, min: &str| Rule { hash: [from as u8; 32], change: change(&[("genesisStepFromSlot", slot)], min), epoch: 0, voters: vec![], signed_stake: 0, total_stake: 0, in_force_from_slot: Some(from) };
        let rules = vec![rule(32, 96, "0.4.37"), rule(128, 160, "0.4.37")];
        assert_eq!(effective_slot(&rules, "genesisStepFromSlot", None, 31), None);
        assert_eq!(effective_slot(&rules, "genesisStepFromSlot", None, 32), Some(96));
        assert_eq!(effective_slot(&rules, "genesisStepFromSlot", None, 128), Some(96), "an activated key is not moved");
        assert_eq!(effective_slot(&rules, "genesisStepFromSlot", Some(256), 32), Some(96));
        assert_eq!(effective_slot(&rules, "nativeMintFromSlot", Some(8), 200), Some(8));
        assert_eq!(unsupported(&rules, 200, "0.4.37"), None);
        let newer = vec![rule(32, 96, "0.5.0")];
        assert_eq!(unsupported(&newer, 31, "0.4.37"), None);
        assert!(unsupported(&newer, 32, "0.4.37").unwrap().contains("needs node 0.5.0"));
        let unknown = vec![Rule { change: change(&[("laterFromSlot", 64)], "0.4.37"), ..rule(32, 64, "0.4.37") }];
        assert!(unsupported(&unknown, 32, "0.4.37").unwrap().contains("laterFromSlot"));
    }
}
