//! Lab content certificates feed the ordinary merge and execution finality path.
use super::*;

impl Network {
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
        for cblock in window*4..window*4+4 {
            for lane in 1..=3 {
                let state=self.nodes[id].lanes.as_ref().unwrap();
                if state.decision(lane,cblock).is_some() || self.nodes[id].lane_voted.contains(&(lane,cblock)) {continue}
                let mut skip=false;
                if cblock%4!=0 {
                    match state.decision(lane,cblock-1) {
                        None=>continue,
                        Some(None)=>skip=true,
                        Some(Some(_))=>{}
                    }
                }
                let expired=elapsed(self.nodes[id].windows[&window].start)>=750_000+(cblock%4+1)*200_000+50_000;
                let mut hashes=vec![];
                let mut pending=false;
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
                    let hash=crypto::hash(&encode(&(b"acrux-lab-lane-block-v1".as_slice(),lane as u8,cblock,parent,hashes))?);
                    let block=consensus::Block{id:BlockRef{slot:cblock,hash},parent};
                    if self.nodes[id].lane_proposed.contains_key(&(lane,cblock)) {continue}
                    self.nodes[id].lane_proposed.insert((lane,cblock),block.id);
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
                    let signature=crypto::sign_bytes(&self.keys[id],&lane_vote_message(lane,&vote)?);
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
                // execution Votor alone advances the shared Network window clock.
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
