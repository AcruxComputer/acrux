//! Deployed to Acrux with `solana program deploy` and called with any instruction data: logs a
//! greeting, the data it was given and the slot it ran in.
use solana_program::{account_info::AccountInfo, clock::Clock, entrypoint, entrypoint::ProgramResult, msg, pubkey::Pubkey, sysvar::Sysvar};

entrypoint!(process);

fn process(program_id: &Pubkey, _accounts: &[AccountInfo], data: &[u8]) -> ProgramResult {
    msg!("Hello from Acrux: program {}", program_id);
    msg!("called with {:?}", std::str::from_utf8(data).unwrap_or("bytes"));
    msg!("slot {}", Clock::get()?.slot);
    Ok(())
}
