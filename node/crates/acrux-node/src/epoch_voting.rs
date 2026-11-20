//! Hosted actors install finalized epoch sets before observing any of their votes.
use crate::{consensus::Votor,protocol::ProposerSchedules,transport::Received};
use anyhow::{Context,Result,ensure};
use std::collections::BTreeMap;

// Match the outside observer's existing envelope and material bounds.
const MAX_PENDING_ENVELOPES:usize=100_000;
const MAX_PENDING_BYTES:usize=64*1024*1024;

#[derive(Default)]
pub(crate) struct EpochVoting {
    installed_through:Option<u64>,
    pending:BTreeMap<(u64,[u8;32]),Received>,
    pending_bytes:usize,
}
impl EpochVoting {
    pub fn install(&mut self,votor:&mut Votor,schedules:&ProposerSchedules,identity:&str)->Result<()> {
        if schedules.config.admission_from_slot.is_none(){return Ok(())}
        let next=self.installed_through.map_or(0,|epoch|epoch.saturating_add(1));
        for (epoch,snapshot) in schedules.admissions.range(next..) {
            let from=epoch.checked_mul(schedules.config.epoch_slots/4).context("Voting epoch overflow")?;
            let own=snapshot.members.iter().position(|member|member.identity.to_string()==identity);
            let stakes=snapshot.members.iter().map(|member|member.stake).collect::<Vec<_>>();
            // The bootstrap marker inherits the already-known legacy set, even if
            // its first votes preceded the marker's finalization.
            if votor.stakes_at(from)!=stakes || votor.own_at(from)!=own {votor.install_stakes(from,own,stakes)?;}
            self.installed_through=Some(*epoch);
        }
        Ok(())
    }
    pub fn ready(&self,schedules:&ProposerSchedules,cblock:u64)->bool {
        let Some(from)=schedules.config.admission_from_slot else{return true};
        let epoch=cblock/(schedules.config.epoch_slots/4);
        epoch<=from/schedules.config.epoch_slots || (self.installed_through.is_some_and(|installed|epoch<=installed)&&schedules.admissions.contains_key(&epoch))
    }
    pub fn hold(&mut self,cblock:u64,packet:Received)->Result<()> {
        let key=(cblock,crate::crypto::hash(&packet.payload));
        if self.pending.contains_key(&key){return Ok(())}
        ensure!(self.pending.len()<MAX_PENDING_ENVELOPES&&self.pending_bytes.checked_add(packet.payload.len()).is_some_and(|bytes|bytes<=MAX_PENDING_BYTES),"Pending epoch votes exceeded the observer material bound");
        self.pending_bytes+=packet.payload.len();
        self.pending.insert(key,packet);
        Ok(())
    }
    pub fn take_ready(&mut self,schedules:&ProposerSchedules)->Vec<Received> {
        if self.pending.is_empty(){return vec![]}
        let epoch=self.installed_through.or_else(||schedules.config.admission_from_slot.map(|from|from/schedules.config.epoch_slots));
        let last=epoch.map_or(u64::MAX,|epoch|epoch.saturating_add(1).saturating_mul(schedules.config.epoch_slots/4).saturating_sub(1));
        let keys=self.pending.range(..=(last,[u8::MAX;32])).filter(|((slot,_),_)|self.ready(schedules,*slot)).map(|(key,_)|*key).collect::<Vec<_>>();
        keys.into_iter().map(|key|{
            let packet=self.pending.remove(&key).unwrap();
            self.pending_bytes-=packet.payload.len();packet
        }).collect()
    }
}

#[cfg(test)]
#[path="epoch_voting_tests.rs"]
mod tests;
