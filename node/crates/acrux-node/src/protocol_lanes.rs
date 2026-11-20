//! Lane content certificates feed the ordinary merge and execution finality path.
use super::*;

impl Network {
    /// An executed candidate keeps the exact inputs it used until reconciliation
    /// has removed it. A fresh candidate prefers finalized choices over NCs.
    pub(super) fn selected_lane_inputs(&self,id:usize,cblock:u64)->Option<crate::lanes::AcceptedLanes> {
        let node=&self.nodes[id];
        node.lane_selections.get(&cblock).cloned()
            .or_else(||node.lanes.as_ref().and_then(|lanes|lanes.decisions(cblock)))
            .or_else(||node.lanes.as_ref().and_then(|lanes|lanes.ready(cblock)))
    }

    /// A finalized lane may replace an NC-only candidate. Restore execution at
    /// the first changed cblock, retaining the authenticated content for replay.
    pub(super) async fn reconcile_lane_execution(&mut self,id:usize,window:u64)->Result<()> {
        if self.nodes[id].windows[&window].lane_parent_abandoned {return Ok(())}
        let changed={
            let node=&self.nodes[id];
            let lanes=node.lanes.as_ref().unwrap();
            node.lane_selections.range(window*4..window*4+4)
                .find_map(|(cblock,selected)|lanes.decisions(*cblock)
                    .filter(|finalized|finalized.hashes!=selected.hashes).map(|_|*cblock))
        };
        let Some(cblock)=changed else{return Ok(())};
        let from=cblock*4;
        let (checkpoint,threshold,last_executed,discarded)={
            let node=&self.nodes[id];
            let base=if cblock%4==0 {node.windows[&window].parent.map(|parent|parent.slot)}else{Some(cblock-1)};
            let checkpoint=match base {
                Some(base)=>node.checkpoints.get(&base).cloned().context("Missing lane repair checkpoint")?,
                None=>node.genesis.clone(),
            };
            let threshold=match base {
                Some(base)=>node.threshold_checkpoints.get(&base).cloned().context("Missing lane repair fragment checkpoint")?,
                None=>crate::threshold::ThresholdCache::new(self.config.fragment_expiry_slots)?,
            };
            let discarded=node.executed_blocks.range(cblock..).map(|(_,block)|*block).collect::<BTreeSet<_>>();
            ensure!(discarded.iter().all(|block|node.votor.initial_vote(block.slot).is_none_or(|vote|vote.hash!=Some(block.hash))),
                "A finalized lane repair conflicts with an execution vote already cast");
            (checkpoint,threshold,base.map(|base|base*4+3),discarded)
        };
        let abandoned={
            let node=&mut self.nodes[id];
            node.executor.as_mut().unwrap().restore(checkpoint)?;
            node.threshold=threshold;
            node.last_executed=last_executed;
            for (_,slot) in node.slots.range_mut(from..) {
                slot.executed=false;
                slot.state_hash=None;
                slot.agreed=None;
                slot.agreement=None;
                slot.burn=None;
                slot.payload.clear();
                slot.invalid_reason=None;
                slot.lane_execution_invalid=false;
            }
            node.executed_blocks.retain(|slot,_|*slot<cblock);
            node.checkpoints.retain(|slot,_|*slot<cblock);
            node.threshold_checkpoints.retain(|slot,_|*slot<cblock);
            node.lane_witnesses.retain(|slot,_|*slot<cblock);
            node.lane_registered.retain(|slot|*slot<cblock);
            node.lane_selections.retain(|slot,_|*slot<cblock);
            let votor=&node.votor;
            node.voted.retain(|slot|*slot<cblock||votor.initial_vote(*slot).is_some());
            let run=node.windows.get_mut(&window).unwrap();
            if run.skipped_from.is_some_and(|first|first>=cblock) {run.skipped_from=None;}
            let mut abandoned=Vec::new();
            for (number,run) in node.windows.range_mut(window+1..) {
                if run.parent.is_some_and(|parent|discarded.contains(&parent)) {
                    run.lane_parent_abandoned=true;
                    run.skipped_from.get_or_insert(number*4);
                    abandoned.push(*number);
                }
            }
            node.pending_starts.retain(|(_,parent)|!parent.is_some_and(|parent|discarded.contains(&parent)));
            abandoned
        };
        let mut step=Step::new();
        if id==0 {
            self.cblock_preimages.retain(|slot,_|*slot<cblock);
            self.pending_bridge_proofs.retain(|slot,_|*slot<cblock);
            // Discard processed transaction observations with the provisional
            // execution. They can be admitted and executed again on the repair.
            let removed=self.solana.rollback_from(from);
            let mut gateway=self.gateway.write().await;
            gateway.transactions_by_cblock.retain(|slot,_|*slot<cblock);
            for (signature,executed) in removed {
                self.signature_ids.remove(&signature);
                gateway.signatures.remove(&signature);
                if let Some(tx)=gateway.transactions.get_mut(&executed.id) {
                    tx["state"]=json!("sent");
                    for field in ["readableSlot","executedSlot","mergedPosition","result","failure","finalization"] {tx[field]=Value::Null;}
                    tx.as_object_mut().unwrap().remove("fees");
                    tx["logs"]=json!([]);
                    tx["includedCount"]=json!(0);
                    step.persist.push((format!("transaction/{}",executed.id),tx.clone()));
                }
            }
        }
        {
            let mut checks=self.state_checks.lock().unwrap();
            for (_,(_,voters)) in checks.lane_pending.range_mut((from,[0;32])..) {*voters&=!(1u16<<id);}
            checks.completed.retain(|slot,_|*slot<from);
        }
        // These children were sealed on discarded checkpoints. Lock their Skip
        // phase before any subsequent certificate can trigger an automatic vote.
        let mut actions=Vec::new();
        for number in abandoned {for slot in number*4..number*4+4 {
            actions.extend(self.nodes[id].votor.timeout(slot)?);
        }}
        self.consensus_actions(id,actions,&mut step)?;
        self.dispatch(id,step).await
    }
    /// Speculative SVM/checkpoints may lead lane finality. Execution votes may
    /// not: retain R23's final-content gate before registering the cblock.
    pub(super) fn lane_execution_actions(&mut self,id:usize,block:consensus::Block)->Result<Option<Vec<Action>>> {
        let cblock=block.id.slot;
        let lanes=self.nodes[id].lanes.as_ref().unwrap();
        let Some(finalized)=lanes.decisions(cblock) else{return Ok(None)};
        let selected=self.selected_lane_inputs(id,cblock).context("Finalized lane has no selected inputs")?;
        if finalized.hashes!=selected.hashes {return Ok(None)}
        self.nodes[id].lane_registered.insert(cblock);
        Ok(Some(self.nodes[id].votor.local_block(block)?))
    }
    /// Overlap the next window with execution consensus. Its parent is a real
    /// locally reproduced checkpoint backed by the three content certificates.
    /// The execution Votor still requires its NC before voting for any child.
    pub(super) async fn early_lane_handover(&mut self,id:usize,window:u64)->Result<()> {
        if self.nodes[id].windows.contains_key(&(window+1)) || self.nodes[id].pending_starts.iter().any(|(number,_)|*number==window+1) {return Ok(())}
        let node=&self.nodes[id];
        let run=node.windows[&window];
        if run.lane_parent_abandoned {return Ok(())}
        if let Some(parent)=run.parent {
            let certified=node.votor.finalized()==Some(parent) || node.seen_certificates.iter().any(|(slot,kind,hash)|*slot==parent.slot && *hash==Some(parent.hash) && !matches!(kind,consensus::CertificateKind::Skip));
            // At most one window may lead its execution-parent certificate.
            if !certified {return Ok(())}
        }
        let mut parent=run.parent;
        for cblock in window*4..window*4+4 {
            let Some(ready)=self.selected_lane_inputs(id,cblock) else{return Ok(())};
            if ready.proposers.is_empty() {continue}
            if let Some(block)=node.executed_blocks.get(&cblock) {
                if run.skipped_from.is_none_or(|first|cblock<first) {
                    if !node.checkpoints.contains_key(&cblock) {return Ok(())}
                    parent=Some(*block);
                    continue;
                }
            }
            if !node.seen_certificates.contains(&(cblock,consensus::CertificateKind::Skip,None)) {return Ok(())}
        }
        self.start_window(id,window+1,parent).await
    }
    /// Once a lane is certified, its selected hashes come from the validated
    /// content header, not from a later (possibly equivocating) attestation.
    pub(super) fn canonical_lane_pi(&self,id:usize,slot:u64)->Result<Vec<Option<[u8;32]>>> {
        let decision=self.selected_lane_inputs(id,slot/4).context("Lane certificates pending")?;
        (1..=3).map(|lane|if decision.proposers.contains(&lane) {
            Ok(Some(self.nodes[id].lane_commitments.get(&(lane,slot/4)).context("Certified lane commitments unavailable")?[(slot%4) as usize]))
        }else{Ok(None)}).collect()
    }

    fn signed_lane_certificate(&self,id:usize,lane:Option<usize>,slot:u64,hash:Option<[u8;32]>)->Result<(consensus::Certificate,Vec<SignedVote>)> {
        use consensus::{CertificateKind as C,VoteKind as V};
        let candidates=if hash.is_none(){vec![(C::Skip,vec![V::Skip,V::SkipFallback])]}else{vec![(C::FastFinalization,vec![V::Notar]),(C::Finalization,vec![V::Finalize]),(C::Notarization,vec![V::Notar]),(C::NotarFallback,vec![V::Notar,V::NotarFallback])]};
        for (kind,vote_kinds) in candidates {
            if lane.is_none() && kind==C::Notarization {continue}
            if lane.is_none() && kind==C::NotarFallback {continue}
            let mut proofs=match lane {
                Some(lane)=>self.nodes[id].lane_votes.iter().filter(|((role,_),signed)|*role==lane && signed.vote.slot==slot && signed.vote.hash==hash && vote_kinds.contains(&signed.vote.kind)).map(|(_,signed)|signed.clone()).collect::<Vec<_>>(),
                None=>self.nodes[id].votes.values().filter(|signed|signed.vote.slot==slot && signed.vote.hash==hash && vote_kinds.contains(&signed.vote.kind)).cloned().collect(),
            };
            proofs.sort_by_key(|signed|(signed.vote.validator,vote_kinds.iter().position(|kind|*kind==signed.vote.kind).unwrap()));
            proofs.dedup_by_key(|signed|signed.vote.validator);
            let certificate=consensus::Certificate{slot,hash,kind,votes:proofs.iter().map(|signed|signed.vote.clone()).collect()};
            let valid=match lane {Some(lane)=>self.nodes[id].lanes.as_ref().unwrap().validate_certificate(lane,&certificate),None=>self.nodes[id].votor.validate_certificate(&certificate)};
            if valid.is_ok(){return Ok((certificate,proofs))}
        }
        anyhow::bail!("Signed lane/execution certificate unavailable")
    }

    pub(super) fn lane_material(&self,cblock:u64)->Result<Value> {
        let node=&self.nodes[0];
        let witness=node.lane_witnesses.get(&cblock).context("Lane cblock witness unavailable")?;
        let (certificate,proofs)=self.signed_lane_certificate(0,None,cblock,Some(witness.block.hash))?;
        let decisions=self.selected_lane_inputs(0,cblock).context("Lane certificates pending")?;
        let lanes=(1..=3).map(|lane|{
            let (certificate,proofs)=self.signed_lane_certificate(0,Some(lane),cblock,decisions.hashes[lane-1])?;
            let accepted=decisions.hashes[lane-1].is_some();
            Ok(json!({"lane":lane,"block":accepted.then(||node.lane_proposed.get(&(lane,cblock))).flatten(),"commitments":accepted.then(||node.lane_commitments.get(&(lane,cblock))).flatten().map(|hashes|hashes.iter().map(hex::encode).collect::<Vec<_>>()),"certificate":certificate,"proofs":proofs}))
        }).collect::<Result<Vec<_>>>()?;
        Ok(json!({"preimage":B64.encode(&witness.preimage),"hash":hex::encode(witness.block.hash),"outboxRoot":witness.outbox_root.map(hex::encode),"execution":{"certificate":certificate,"proofs":proofs},"lanes":lanes}))
    }
    /// Rotor verifies the shreds before they enter LocalSlot. Check the released key,
    /// availability and commitment here; the shared execution path decodes and validates
    /// the selected payloads before any transaction can change state.
    fn sealed_lane_hash(&self,id:usize,slot:u64,lane:usize)->Result<Option<[u8;32]>> {
        let Some(local)=self.nodes[id].slots.get(&slot) else{return Ok(None)};
        let finals=local.meta.iter().filter(|((role,_),meta)|*role==lane&&meta.final_slice).collect::<Vec<_>>();
        if finals.len()!=1 {return Ok(None)}
        let Some((attestation,_))=local.attest.get(&lane) else{return Ok(None)};
        let mut roots=vec![];
        for slice in 0..=finals[0].1.slice {
            let key=(lane,slice);
            let (Some(cipher),Some(meta),Some(shreds))=(local.cipher.get(&key),local.meta.get(&key),local.shreds.get(&key)) else{return Ok(None)};
            ensure!(crypto::hash(&attestation.key)==meta.key_hash,"Released lane key differs from its commitment");
            crypto::decrypt_slice(&attestation.key,slot,lane,slice,cipher)?;
            roots.push(shreds[0].root);
        }
        let parent=finals[0].1.parent.map(|(slot,hash)|BlockRef{slot,hash});
        let expected_parent=if slot%16==0 {self.nodes[id].windows[&(slot/16)].parent}else{None};
        ensure!(parent==expected_parent,"Lane block has a stale execution parent");
        Ok(Some(crypto::hash(&encode(&BlockCommitment{slot,proposer:lane,key_hash:finals[0].1.key_hash,parent,roots})?)))
    }

    pub(super) async fn try_lanes(&mut self,id:usize,window:u64)->Result<()> {
        if !self.sync_voting(id,window*4)? {return Ok(())}
        let parent=self.nodes[id].windows[&window].parent;
        self.nodes[id].lanes.as_mut().unwrap().ensure_window(window*4,parent)?;
        for cblock in window*4..window*4+4 {
            for lane in 1..=3 {
                let state=self.nodes[id].lanes.as_ref().unwrap();
                if state.decision(lane,cblock).is_some() || self.nodes[id].lane_voted.contains(&(lane,cblock)) {continue}
                let mut skip=false;
                if cblock%4!=0 {
                    match state.readiness(lane,cblock-1) {
                        None=>continue,
                        Some(None)=>skip=true,
                        Some(Some(_))=>{}
                    }
                }
                let expired=elapsed(self.nodes[id].windows[&window].start)>=750_000+(cblock%4+1)*200_000+50_000;
                let mut hashes=vec![];
                let mut pending=false;
                // These are authenticated co-proposer rows, not a local absence or
                // a fault flag. Two signed nulls make the existing Skip vote safe
                // before the fallback timer or the complete attestation view.
                if !skip && (cblock*4..cblock*4+4).any(|slot|self.exchange.lane_missing(id,slot,lane)) {skip=true;}
                if !skip {for slot in cblock*4..cblock*4+4 {
                    if self.nodes[id].slots.get(&slot).is_some_and(|local|local.lane_equivocation.contains(&lane)) {skip=true;break}
                    let Some(inputs)=self.exchange.lane_snapshot(id,slot) else {pending=true;break};
                    let Some(expected)=inputs.hashes[lane-1] else {skip=true;break};
                    match self.sealed_lane_hash(id,slot,lane) {
                        Ok(Some(actual)) if actual==expected=>hashes.push(actual),
                        Ok(None)=>{pending=true;break},
                        Ok(Some(_))=>{skip=true;break},
                        Err(error)=>{tracing::warn!(event="lab_lane_invalid",lane,slot,%error);skip=true;break}
                    }
                }}
                let actions=if skip || (pending&&expired) {
                    self.nodes[id].lanes.as_mut().unwrap().timeout(lane,cblock)?
                } else if pending {
                    continue;
                } else {
                    let parent=self.nodes[id].lanes.as_ref().unwrap().parent(lane,cblock);
                    let hash=crate::cblock_format::lane_hash(lane,cblock,parent,&hashes.clone().try_into().expect("four lane commitments"))?;
                    let block=consensus::Block{id:BlockRef{slot:cblock,hash},parent};
                    if self.nodes[id].lane_proposed.contains_key(&(lane,cblock)) {continue}
                    self.nodes[id].lane_proposed.insert((lane,cblock),block.clone());
                    self.nodes[id].lane_commitments.insert((lane,cblock),hashes);
                    self.nodes[id].lanes.as_mut().unwrap().local_block(lane,block)?
                };
                let mut step=Step::new();
                self.lane_actions(id,actions,&mut step)?;
                self.dispatch(id,step).await?;
            }
        }
        Ok(())
    }

    pub(super) fn lane_actions(&mut self,id:usize,actions:Vec<(usize,Action)>,step:&mut Step)->Result<()> {
        for (lane,action) in actions {
            match action {
                Action::SendVote(vote)=>{
                    let signature=crypto::sign_bytes(&self.keys[id],&lane_vote_message(lane,&vote,self.genesis.as_ref().and_then(|g|g.bridge.as_ref()))?);
                    let signed=SignedVote{vote:vote.clone(),signature};
                    self.nodes[id].lane_votes.insert((lane,encode(&vote)?),signed.clone());
                    if matches!(vote.kind,consensus::VoteKind::Notar|consensus::VoteKind::Skip) {self.nodes[id].lane_voted.insert((lane,vote.slot));}
                    step.out.push(Outbox{from:id,to:(0..12).collect(),body:Wire::LaneVote{lane,signed}});
                }
                Action::SendCertificate(certificate)=>{
                    let proofs=certificate.votes.iter().map(|vote|self.nodes[id].lane_votes.get(&(lane,encode(vote).unwrap())).cloned().context("Missing signed lane certificate vote")).collect::<Result<Vec<_>>>()?;
                    self.nodes[id].lane_certificates.insert((lane,certificate.slot,certificate.kind,certificate.hash),certificate.clone());
                    step.out.push(Outbox{from:id,to:(0..12).collect(),body:Wire::LaneCertificate{lane,certificate,proofs}});
                }
                // Lane finality and parent notifications are observed by Lanes. The
                // The shared clock uses early_lane_handover or the execution
                // Votor; an individual content lane cannot start a window.
                Action::Finalized(_)|Action::ParentReady{..}=>{}
            }
        }
        Ok(())
    }

    pub(super) fn lane_yardstick(&self,cblock:u64)->Value {
        let node=&self.nodes[0];
        json!((1..=3).map(|lane| {
            let status=match node.lanes.as_ref().unwrap().decision(lane,cblock) {Some(Some(_))=>"finalized",Some(None)=>"skipped",None=>"pending"};
            let mut votes=BTreeMap::<&str,u64>::new();
            for ((role,_),signed) in &node.lane_votes {if *role==lane&&signed.vote.slot==cblock {
                let kind=match signed.vote.kind {consensus::VoteKind::Notar=>"notar",consensus::VoteKind::NotarFallback=>"notar_fallback",consensus::VoteKind::Skip=>"skip",consensus::VoteKind::SkipFallback=>"skip_fallback",consensus::VoteKind::Finalize=>"finalize"};
                *votes.entry(kind).or_default()+=1;
            }}
            let certificates=node.lane_certificates.keys().filter(|(role,slot,_,_)|*role==lane&&*slot==cblock).map(|(_,_,kind,_)|match kind {
                consensus::CertificateKind::Notarization=>"notarization",consensus::CertificateKind::NotarFallback=>"notar_fallback",consensus::CertificateKind::FastFinalization=>"fast_finalization",consensus::CertificateKind::Finalization=>"slow_finalization",consensus::CertificateKind::Skip=>"skip"
            }).collect::<BTreeSet<_>>();
            json!({"proposer":lane,"status":status,"votes":votes,"certificates":certificates})
        }).collect::<Vec<_>>())
    }
}
