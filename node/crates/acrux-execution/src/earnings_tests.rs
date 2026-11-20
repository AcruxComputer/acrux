use super::*;
use solana_sdk::{compute_budget::ComputeBudgetInstruction, signature::{Keypair, Signer},
    system_instruction, transaction::Transaction};

#[test]
fn validator_earnings_count_merged_fee_credits_across_epochs_and_rollback() {
    let payer = Keypair::new();
    let proposers = [Pubkey::new_unique(), Pubkey::new_unique(), Pubkey::new_unique()];
    let balances = std::iter::once((payer.pubkey().to_string(), 100_000_000))
        .chain(proposers.iter().map(|key| (key.to_string(), 2_000_000))).collect::<Vec<_>>();
    let mut tracked = SvmExecutor::new(balances.clone(), &[]).unwrap();
    let mut plain = SvmExecutor::new(balances, &[]).unwrap();
    let recipients = proposers.iter().enumerate().map(|(index, key)| ((index + 1) as u8, key.to_string())).collect::<Vec<_>>();
    tracked.set_earnings(32, vec![]).unwrap();
    assert_eq!(tracked.state_hash(), plain.state_hash(), "enabling reporting cannot change the chain state");
    for executor in [&mut tracked, &mut plain] {
        executor.set_proposers(&recipients).unwrap();
        executor.execute_slot(29, vec![]).unwrap();
    }
    let before = tracked.checkpoint();
    assert_eq!(before.validator_earnings()[&0].counted_from_slot, 29, "empty slots establish exact partial-epoch coverage");
    assert!(before.validator_earnings()[&0].rows.is_empty());
    let transaction = Transaction::new_signed_with_payer(&[
        ComputeBudgetInstruction::set_compute_unit_limit(20_000),
        ComputeBudgetInstruction::set_compute_unit_price(1_000_050),
        // An ordinary transfer to another proposer is not a fee earning.
        system_instruction::transfer(&payer.pubkey(), &proposers[2], 1_000_000),
    ], Some(&payer.pubkey()), &[&payer], Hash::from_str(&tracked.latest_blockhash()).unwrap());
    let bytes = bincode::serialize(&transaction).unwrap();
    let validated = tracked.validate_transaction(&bytes, 31).unwrap();
    let execute = |executor: &mut SvmExecutor| {
        executor.observe_earnings_slot(31);
        for role in [1, 2] {
            executor.charge_inclusion(&payer.pubkey().to_string(), role, 1, 1).unwrap();
        }
        let result = executor.execute_slot(31, vec![
            ExecutionItem { transaction: validated.clone(), source_proposers: vec![2] },
            ExecutionItem { transaction: validated.clone(), source_proposers: vec![1] },
        ]).unwrap();
        assert_eq!(result.results.len(), 1, "two carried copies execute only once");
        assert!(result.results[0].success, "{:?}", result.results[0]);
        assert_eq!(result.results[0].base_fee, 5_000);
        assert_eq!(result.results[0].priority_fee, 20_001);
    };
    execute(&mut tracked);
    execute(&mut plain);
    let paid = tracked.checkpoint();
    let epoch = &paid.validator_earnings()[&0];
    assert_eq!(epoch.counted_from_slot, 29);
    assert_eq!(epoch.rows.len(), 2);
    for (index, priority) in [(0, 10_001), (1, 10_000)] {
        let identity = proposers[index].to_string();
        let row = epoch.rows.iter().find(|row| row.identity == identity).unwrap();
        assert_eq!((row.base, row.priority, row.inclusion), (1_250, priority, 1_000));
        assert_eq!(tracked.balance(&identity).unwrap(), 2_000_000 + row.base as u64 + row.priority as u64 + row.inclusion as u64);
    }
    assert_eq!(tracked.balance(&proposers[2].to_string()).unwrap(), 3_000_000);
    assert_eq!(tracked.state_hash(), plain.state_hash());
    assert_eq!(serde_json::to_vec(&tracked.snapshot(&paid)).unwrap(),
        serde_json::to_vec(&plain.snapshot(&plain.checkpoint())).unwrap(), "reporting is absent from the public execution snapshot");
    tracked.execute_slot(32, vec![]).unwrap();
    let crossed = tracked.checkpoint();
    assert_eq!(crossed.validator_earnings()[&1].counted_from_slot, 32);
    assert!(crossed.validator_earnings()[&1].rows.is_empty());
    assert_eq!(crossed.validator_earnings()[&0], paid.validator_earnings()[&0]);
    let next = Transaction::new_signed_with_payer(&[
        system_instruction::transfer(&payer.pubkey(), &proposers[2], 1_000_001),
    ], Some(&payer.pubkey()), &[&payer], Hash::from_str(&tracked.latest_blockhash()).unwrap());
    let next = tracked.validate_transaction(&bincode::serialize(&next).unwrap(), 33).unwrap();
    tracked.observe_earnings_slot(33);
    tracked.charge_inclusion(&payer.pubkey().to_string(), 1, 2, 1).unwrap();
    assert!(tracked.execute_slot(33, vec![ExecutionItem { transaction: next, source_proposers: vec![1] }]).unwrap().results[0].success);
    let next_epoch = tracked.checkpoint();
    let row = &next_epoch.validator_earnings()[&1].rows[0];
    assert_eq!(row.identity, proposers[0].to_string());
    assert_eq!((row.base, row.priority, row.inclusion), (2_500, 0, 1_000));
    assert_eq!(next_epoch.validator_earnings()[&0], paid.validator_earnings()[&0]);

    tracked.restore(before.clone()).unwrap();
    assert_eq!(tracked.checkpoint().validator_earnings(), before.validator_earnings(), "a rollback removes speculative credits and the next epoch");
    execute(&mut tracked);
    assert_eq!(tracked.checkpoint().validator_earnings(), paid.validator_earnings(), "re-execution counts each successful credit once");
    assert_eq!(tracked.state_hash(), plain.state_hash());
    let checkpoint = tracked.checkpoint();
    let mut snapshot = tracked.snapshot_meta(&checkpoint);
    snapshot.accounts = checkpoint.account_changes(None).0.into_iter().map(|(key, account)| (key.to_string(), account.as_ref().clone())).collect();
    snapshot.executed = checkpoint.executed_changes(None).0;
    let saved = paid.validator_earnings().values().map(|epoch| epoch.as_ref().clone()).collect();
    let mut restarted = SvmExecutor::from_state(&[], snapshot).unwrap();
    assert!(restarted.checkpoint().validator_earnings().is_empty());
    restarted.set_earnings(32, saved).unwrap();
    assert_eq!(restarted.checkpoint().validator_earnings(), paid.validator_earnings(), "restart attaches only separately persisted reporting rows");
    assert_eq!(restarted.state_hash(), tracked.state_hash());
}
