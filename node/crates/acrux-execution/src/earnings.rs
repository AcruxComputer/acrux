//! Validator fee credits for the read model, outside the account-state commitment.
use anyhow::{ensure, Result};
use serde::{Deserialize, Serialize};
use solana_sdk::pubkey::Pubkey;
use std::{collections::BTreeMap, sync::Arc};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorEarnings {
    pub identity: String,
    #[serde(with = "decimal")]
    pub base: u128,
    #[serde(with = "decimal")]
    pub priority: u128,
    #[serde(with = "decimal")]
    pub inclusion: u128,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ValidatorEarningsEpoch {
    pub epoch: u64,
    pub counted_from_slot: u64,
    pub rows: Vec<ValidatorEarnings>,
}

#[derive(Clone, Default)]
pub(crate) struct EarningsState {
    pub epochs: Arc<BTreeMap<u64, Arc<ValidatorEarningsEpoch>>>,
    slot: Option<u64>,
}

#[derive(Clone, Copy)]
pub(crate) enum FeeKind { Base, Priority, Inclusion }

#[derive(Default)]
pub(crate) struct Earnings {
    epoch_slots: Option<u64>,
    pub state: EarningsState,
}
impl Earnings {
    pub fn configure(&mut self, epoch_slots: u64, saved: Vec<ValidatorEarningsEpoch>) -> Result<()> {
        ensure!(epoch_slots > 0 && epoch_slots % 16 == 0, "Earnings require bridge epochs aligned to proposer windows");
        let mut epochs = BTreeMap::new();
        for mut epoch in saved {
            ensure!(epoch.counted_from_slot / epoch_slots == epoch.epoch, "Saved earnings start outside their epoch");
            epoch.rows.sort_by(|a, b| a.identity.cmp(&b.identity));
            ensure!(epoch.rows.windows(2).all(|pair| pair[0].identity != pair[1].identity), "Duplicate saved earnings identity");
            ensure!(epochs.insert(epoch.epoch, Arc::new(epoch)).is_none(), "Duplicate saved earnings epoch");
        }
        self.epoch_slots = Some(epoch_slots);
        self.state = EarningsState { epochs: Arc::new(epochs), slot: None };
        Ok(())
    }

    pub fn observe_slot(&mut self, slot: u64) {
        let Some(epoch_slots) = self.epoch_slots else { return };
        self.state.slot = Some(slot);
        let epoch = slot / epoch_slots;
        if !self.state.epochs.contains_key(&epoch) {
            Arc::make_mut(&mut self.state.epochs).insert(epoch, Arc::new(ValidatorEarningsEpoch {
                epoch, counted_from_slot: slot, rows: vec![],
            }));
        }
    }

    /// Only called after the corresponding account credit succeeds. Wider counters cannot
    /// overflow at the chain's transaction rate and never alter the credited amount.
    pub fn credit(&mut self, identity: Pubkey, amount: u64, kind: FeeKind) {
        let Some(epoch_slots) = self.epoch_slots else { return };
        if amount == 0 { return }
        let slot = self.state.slot.expect("Observe the earnings slot before charging fees");
        let epochs = Arc::make_mut(&mut self.state.epochs);
        let epoch = Arc::make_mut(epochs.get_mut(&(slot / epoch_slots)).expect("Observed earnings epoch"));
        let identity = identity.to_string();
        let index = match epoch.rows.binary_search_by(|row| row.identity.cmp(&identity)) {
            Ok(index) => index,
            Err(index) => {
                epoch.rows.insert(index, ValidatorEarnings { identity, ..Default::default() });
                index
            },
        };
        let row = &mut epoch.rows[index];
        let total = match kind { FeeKind::Base => &mut row.base, FeeKind::Priority => &mut row.priority, FeeKind::Inclusion => &mut row.inclusion };
        *total += u128::from(amount);
    }
}

mod decimal {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(value: &u128, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_string())
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u128, D::Error> {
        String::deserialize(deserializer)?.parse().map_err(serde::de::Error::custom)
    }
}
