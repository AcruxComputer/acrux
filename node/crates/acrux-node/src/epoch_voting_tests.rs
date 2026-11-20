use super::*;
use crate::{consensus::{Action,Block,BlockRef,CertificateKind,Message,Vote,VoteKind},crypto,
    execution::{AdmissionHub,AdmissionMember,AdmissionSnapshot},protocol::{ProposerConfig,ProposerSchedules},
    schedule::{Hub,Member},types::{Envelope,SignedVote,Wire,vote_message}};
use ed25519_dalek::SigningKey;
use solana_sdk::pubkey::Pubkey;
use std::time::Instant;

#[test]
fn hosted_epoch_votes_wait_for_the_finalized_cutoff_set() {
    let keys=(1..=16).map(|seed|SigningKey::from_bytes(&[seed;32])).collect::<Vec<_>>();
    let identities=keys.iter().map(|key|bs58::encode(key.verifying_key().as_bytes()).into_string()).collect::<Vec<_>>();
    let catalog=keys.iter().map(|key|Pubkey::new_from_array(key.verifying_key().to_bytes())).collect::<Vec<_>>();
    let codes=["FRA","NYC","TYO"];
    let hubs=codes.into_iter().enumerate().map(|(index,code)|Hub{code:code.into(),region:code.into(),
        members:identities[index*4..index*4+4].iter().map(|identity|Member{identity:identity.clone(),stake:100_000_000}).collect()}).collect();
    let mut schedules=ProposerSchedules::new(ProposerConfig{seed:"9".into(),windows_per_epoch:96,
        identities:identities[..12].to_vec(),hubs,epoch_slots:256,stake_from_slot:Some(256),
        genesis_stake_lamports:Some(1_000_000_000),admission_from_slot:Some(512)}).unwrap();
    let bootstrap=AdmissionSnapshot{epoch:2,cutoff_slot:512,members:catalog[..12].iter().enumerate().map(|(id,identity)|AdmissionMember{
        identity:*identity,hub:codes[id/4].into(),stake:100_000_000,admitted_at_epoch:0}).collect()};
    schedules.learn_admission(bootstrap,catalog.clone()).unwrap();
    let parent=BlockRef{slot:191,hash:[1;32]};
    let block=Block{id:BlockRef{slot:192,hash:[9;32]},parent:Some(parent)};
    let mut votor=Votor::new(0,vec![100_000_000;12]).unwrap();
    votor.resume(parent);
    let mut gate=EpochVoting::default();
    gate.install(&mut votor,&schedules,&identities[0]).unwrap();

    // Membership made their catalog IDs known, but epoch 3 is not fixed yet.
    // The real hosted intake holds these outer-authenticated envelopes before
    // looking up their compact voter indices or passing any vote to Votor.
    for id in 12..16 {
        let vote=Vote{validator:id,slot:192,hash:Some(block.id.hash),kind:VoteKind::Notar};
        let signed=SignedVote{signature:crypto::sign_bytes(&keys[id],&vote_message(&vote,None).unwrap()),vote};
        let body=Wire::Vote(signed);
        let signature=crypto::sign_bytes(&keys[id],&bincode::serialize(&body).unwrap());
        crypto::verify_bytes(&keys[id].verifying_key(),&bincode::serialize(&body).unwrap(),&signature).unwrap();
        let payload=bincode::serialize(&Envelope{from:id,body,signature}).unwrap();
        assert!(!gate.ready(&schedules,192));
        gate.hold(192,Received{from:id,to:0,payload,received_at:Instant::now()}).unwrap();
    }
    assert_eq!(gate.pending.len(),4);
    assert!(gate.take_ready(&schedules).is_empty());
    // Before this fix try_vote's timeout branch inserted this Skip immediately
    // under the inherited twelve-member set, permanently poisoning the install.
    if gate.ready(&schedules,192) {votor.timeout(192).unwrap();}
    assert_eq!(votor.sizes()[1],0);

    // Epoch 2's finalized midpoint fixes epoch 3, ahead of execution slot 768.
    let snapshot=AdmissionSnapshot{epoch:3,cutoff_slot:640,members:catalog.iter().enumerate().map(|(id,identity)|AdmissionMember{
        identity:*identity,hub:if id<12 {codes[id/4].into()}else{"SIN".into()},
        stake:if id<12 {1_000_000_000}else{2_000_000_000},admitted_at_epoch:if id<12 {0}else{3}}).collect()};
    schedules.learn_hubs(3,vec![AdmissionHub{code:"SIN".into(),x:-1_482_000,y:6_203_000,z:150_000,activated_at_epoch:Some(3)}]).unwrap();
    schedules.learn_admission(snapshot.clone(),catalog).unwrap();
    gate.install(&mut votor,&schedules,&identities[0]).unwrap();
    assert!(gate.ready(&schedules,192));
    assert_eq!(votor.own_at(192),Some(0));
    assert_eq!(votor.stakes_at(192),snapshot.members.iter().map(|member|member.stake).collect::<Vec<_>>());
    let pending=gate.take_ready(&schedules);
    assert_eq!(pending.len(),4);
    assert_eq!(gate.pending_bytes,0);
    let mut actions=vec![];
    for packet in pending {
        let envelope:Envelope=bincode::deserialize(&packet.payload).unwrap();
        let Wire::Vote(signed)=envelope.body else{panic!("Expected the held vote")};
        crypto::verify_bytes(&keys[packet.from].verifying_key(),&vote_message(&signed.vote,None).unwrap(),&signed.signature).unwrap();
        actions.extend(votor.receive(Message::Vote(signed.vote)).unwrap());
    }
    assert_eq!(votor.sizes()[1],4);
    actions.extend(votor.local_block(block.clone()).unwrap());
    for validator in 1..=7 {
        actions.extend(votor.receive(Message::Vote(Vote{validator,slot:192,hash:Some(block.id.hash),kind:VoteKind::Notar})).unwrap());
    }
    assert_eq!(votor.finalized(),Some(block.id));
    assert!(actions.iter().any(|action|matches!(action,Action::SendCertificate(certificate)
        if certificate.kind==CertificateKind::FastFinalization && (12..16).all(|id|certificate.votes.iter().any(|vote|vote.validator==id)))));
    assert!(gate.take_ready(&schedules).is_empty());
    assert!(gate.ready(&schedules,193));
    assert!(votor.timeout(193).unwrap().iter().any(|action|matches!(action,Action::SendVote(vote) if vote.slot==193&&vote.validator==0&&vote.kind==VoteKind::Skip)));
    gate.install(&mut votor,&schedules,&identities[0]).unwrap();
}
