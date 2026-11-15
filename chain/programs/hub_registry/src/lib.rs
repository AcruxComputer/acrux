//! Hub Registry: the hubs of New Constellation and the validators that joined them.
//!
//! A hub is registered by code and location with a deposit. A validator joins a hub by
//! its identity. Joins, leaves and moves take effect at the next epoch boundary, and a
//! leave or move starts a cooldown. A hub that stays without enough members past the
//! inactivity period can be released by anyone: the deposit returns to the registrant
//! and the code is free again.

use anchor_lang::prelude::*;
use anchor_lang::system_program;

declare_id!("ABjgHL8TcjxUjx6vfwsVQyvxiXR6pFgMkeESgwK6CbDm");

pub const REGISTRY_SEED: &[u8] = b"registry";
pub const FEE_VAULT_SEED: &[u8] = b"fee_vault";
pub const HUB_SEED: &[u8] = b"hub";
pub const MEMBER_SEED: &[u8] = b"member";

pub const MAX_CODE_LEN: usize = 8;
pub const BPS: u64 = 10_000;
/// A hub location is a point near the WGS84 ellipsoid: its distance from the Earth's
/// centre lies between these bounds, in metres.
pub const MIN_RADIUS_M: u64 = 6_300_000;
pub const MAX_RADIUS_M: u64 = 6_400_000;

#[program]
pub mod hub_registry {
    use super::*;

    /// Creates the registry and its fee vault. Only the program's upgrade authority can.
    pub fn initialize(ctx: Context<Initialize>, config: Config) -> Result<()> {
        require!(config.registration_fee_bps as u64 <= BPS, RegistryError::InvalidConfig);
        require!(
            config.code_min_len >= 1
                && config.code_min_len <= config.code_max_len
                && config.code_max_len as usize <= MAX_CODE_LEN,
            RegistryError::InvalidConfig
        );
        ctx.accounts.registry.set_inner(Registry {
            authority: ctx.accounts.authority.key(),
            fee_destination: config.fee_destination,
            fee_vault: ctx.accounts.fee_vault.key(),
            deposit_lamports: config.deposit_lamports,
            registration_fee_bps: config.registration_fee_bps,
            cooldown_epochs: config.cooldown_epochs,
            inactivity_epochs: config.inactivity_epochs,
            min_members: config.min_members,
            code_min_len: config.code_min_len,
            code_max_len: config.code_max_len,
            hubs_registered: 0,
            bump: ctx.bumps.registry,
        });
        ctx.accounts.fee_vault.bump = ctx.bumps.fee_vault;
        Ok(())
    }

    /// Registers a hub under `code`. The deposit goes into the hub account and the
    /// registration fee into the fee vault.
    pub fn register_hub(ctx: Context<RegisterHub>, code: String, location: Location) -> Result<()> {
        let registry = &mut ctx.accounts.registry;
        let bytes = code.as_bytes();
        require!(
            bytes.len() >= registry.code_min_len as usize
                && bytes.len() <= registry.code_max_len as usize
                && bytes.iter().all(u8::is_ascii_uppercase),
            RegistryError::InvalidCode
        );
        let radius_sq = [location.x, location.y, location.z]
            .iter()
            .map(|v| (v.unsigned_abs() as u64).pow(2))
            .sum::<u64>();
        require!(
            (MIN_RADIUS_M.pow(2)..=MAX_RADIUS_M.pow(2)).contains(&radius_sq),
            RegistryError::InvalidLocation
        );

        let deposit = registry.deposit_lamports;
        let fee = (deposit as u128 * registry.registration_fee_bps as u128 / BPS as u128) as u64;
        let epoch = Clock::get()?.epoch;
        registry.hubs_registered += 1;

        let mut stored = [0u8; MAX_CODE_LEN];
        stored[..bytes.len()].copy_from_slice(bytes);
        ctx.accounts.hub.set_inner(Hub {
            id: registry.hubs_registered,
            code: stored,
            location,
            registrant: ctx.accounts.registrant.key(),
            deposit_lamports: deposit,
            members: 0,
            joining: 0,
            leaving: 0,
            counted_epoch: epoch,
            created_epoch: epoch,
            last_active_epoch: epoch,
            bump: ctx.bumps.hub,
        });

        for (to, lamports) in [
            (ctx.accounts.hub.to_account_info(), deposit),
            (ctx.accounts.fee_vault.to_account_info(), fee),
        ] {
            system_program::transfer(
                CpiContext::new(
                    ctx.accounts.system_program.to_account_info(),
                    system_program::Transfer { from: ctx.accounts.registrant.to_account_info(), to },
                ),
                lamports,
            )?;
        }
        emit_cpi!(HubRegistered {
            hub: ctx.accounts.hub.key(),
            id: ctx.accounts.hub.id,
            code: stored,
            location,
            registrant: ctx.accounts.registrant.key(),
            deposit_lamports: deposit,
            fee_lamports: fee,
            epoch,
        });
        Ok(())
    }

    /// The validator `identity` joins a hub. The membership counts from the next epoch.
    pub fn join_hub(ctx: Context<JoinHub>) -> Result<()> {
        let epoch = Clock::get()?.epoch;
        let registry = &ctx.accounts.registry;
        let member = &mut ctx.accounts.membership;
        member.identity = ctx.accounts.identity.key();
        member.bump = ctx.bumps.membership;
        member.settle(epoch);
        require!(member.pending == Pending::None, RegistryError::ChangePending);
        require!(member.hub == Pubkey::default(), RegistryError::AlreadyMember);
        require!(epoch >= member.cooldown_until_epoch, RegistryError::CooldownActive);

        let hub = &mut ctx.accounts.hub;
        hub.roll(epoch, registry.min_members);
        hub.joining += 1;
        member.hub = hub.key();
        member.hub_id = hub.id;
        member.joined_epoch = epoch;
        member.effective_epoch = epoch + 1;
        emit_cpi!(MemberJoined {
            identity: member.identity,
            hub: member.hub,
            hub_id: member.hub_id,
            effective_epoch: member.effective_epoch,
        });
        Ok(())
    }

    /// The validator leaves its hub at the next epoch boundary and starts its cooldown.
    pub fn leave_hub(ctx: Context<LeaveHub>) -> Result<()> {
        let epoch = Clock::get()?.epoch;
        let registry = &ctx.accounts.registry;
        let member = &mut ctx.accounts.membership;
        member.begin_change(epoch, registry.cooldown_epochs, &ctx.accounts.hub.key())?;
        member.pending = Pending::Leave;
        depart(&ctx.accounts.hub, member.hub_id, epoch, registry.min_members)?;
        emit_cpi!(MemberLeaving {
            identity: member.identity,
            hub: member.hub,
            hub_id: member.hub_id,
            effective_epoch: member.pending_epoch,
            cooldown_until_epoch: member.cooldown_until_epoch,
        });
        Ok(())
    }

    /// The validator moves to another hub at the next epoch boundary and starts its cooldown.
    pub fn move_hub(ctx: Context<MoveHub>) -> Result<()> {
        let epoch = Clock::get()?.epoch;
        let registry = &ctx.accounts.registry;
        let member = &mut ctx.accounts.membership;
        let to = &mut ctx.accounts.to_hub;
        require_keys_neq!(to.key(), ctx.accounts.from_hub.key(), RegistryError::SameHub);
        member.begin_change(epoch, registry.cooldown_epochs, &ctx.accounts.from_hub.key())?;
        member.pending = Pending::Move;
        member.pending_hub = to.key();
        member.pending_hub_id = to.id;
        depart(&ctx.accounts.from_hub, member.hub_id, epoch, registry.min_members)?;
        to.roll(epoch, registry.min_members);
        to.joining += 1;
        emit_cpi!(MemberMoving {
            identity: member.identity,
            from_hub: member.hub,
            from_hub_id: member.hub_id,
            to_hub: member.pending_hub,
            to_hub_id: member.pending_hub_id,
            effective_epoch: member.pending_epoch,
            cooldown_until_epoch: member.cooldown_until_epoch,
        });
        Ok(())
    }

    /// Closes a hub that has been inactive past the registry's period. Anyone can call it.
    /// The deposit and the account's rent return to the registrant.
    pub fn release_hub(ctx: Context<ReleaseHub>) -> Result<()> {
        let epoch = Clock::get()?.epoch;
        let registry = &ctx.accounts.registry;
        let hub = &mut ctx.accounts.hub;
        hub.roll(epoch, registry.min_members);
        require!(
            epoch > hub.last_active_epoch + registry.inactivity_epochs,
            RegistryError::HubNotInactive
        );
        emit_cpi!(HubReleased {
            hub: hub.key(),
            id: hub.id,
            code: hub.code,
            registrant: hub.registrant,
            returned_lamports: hub.get_lamports(),
            epoch,
        });
        Ok(())
    }

    /// Sends everything the fee vault holds above its rent to the configured destination.
    pub fn sweep_fees(ctx: Context<SweepFees>) -> Result<()> {
        let vault = ctx.accounts.fee_vault.to_account_info();
        let lamports = vault
            .lamports()
            .saturating_sub(Rent::get()?.minimum_balance(vault.data_len()));
        require!(lamports > 0, RegistryError::NothingToSweep);
        emit_cpi!(FeesSwept { destination: ctx.accounts.destination.key(), lamports });
        ctx.accounts.fee_vault.sub_lamports(lamports)?;
        ctx.accounts.destination.add_lamports(lamports)?;
        Ok(())
    }
}

/// Records a departure on the hub the member is leaving. A hub that was released since
/// the member joined (the account is gone, or the code was registered again under a new
/// id) has no count to change.
fn depart(hub_info: &UncheckedAccount, hub_id: u64, epoch: u64, min_members: u32) -> Result<()> {
    if hub_info.owner != &ID || hub_info.data_is_empty() {
        return Ok(());
    }
    let mut data = hub_info.try_borrow_mut_data()?;
    let mut hub = Hub::try_deserialize(&mut &data[..])?;
    if hub.id != hub_id {
        return Ok(());
    }
    hub.roll(epoch, min_members);
    hub.leaving += 1;
    hub.try_serialize(&mut &mut data[..])
}

// Accounts

#[account]
#[derive(InitSpace)]
pub struct Registry {
    /// The upgrade authority that initialized the registry.
    pub authority: Pubkey,
    /// Where `sweep_fees` sends the fee vault's balance.
    pub fee_destination: Pubkey,
    pub fee_vault: Pubkey,
    pub deposit_lamports: u64,
    /// Registration fee, in basis points of the deposit.
    pub registration_fee_bps: u16,
    /// Epochs after a leave or move takes effect before the validator can change again.
    pub cooldown_epochs: u64,
    /// A hub is releasable once the epoch is past its last active epoch by more than this.
    pub inactivity_epochs: u64,
    /// Members a hub needs in an epoch to count as active (n_min).
    pub min_members: u32,
    /// A code is `code_min_len..=code_max_len` characters, each `A-Z`.
    pub code_min_len: u8,
    pub code_max_len: u8,
    /// Hubs ever registered. The next hub's id is this plus one.
    pub hubs_registered: u64,
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct FeeVault {
    pub bump: u8,
}

#[account]
#[derive(InitSpace)]
pub struct Hub {
    /// Unique per registration: a code registered again after a release gets a new id.
    pub id: u64,
    /// The code, padded with zero bytes.
    pub code: [u8; MAX_CODE_LEN],
    pub location: Location,
    pub registrant: Pubkey,
    /// Held in this account above its rent.
    pub deposit_lamports: u64,
    /// Members in effect during `counted_epoch`.
    pub members: u32,
    /// Joins that take effect at `counted_epoch + 1`.
    pub joining: u32,
    /// Departures that take effect at `counted_epoch + 1`.
    pub leaving: u32,
    /// The epoch the three counts above were last brought up to.
    pub counted_epoch: u64,
    pub created_epoch: u64,
    /// The last epoch with at least `min_members` members in effect, as of `counted_epoch`.
    /// Equal to `created_epoch` for a hub that has never been active.
    pub last_active_epoch: u64,
    pub bump: u8,
}

impl Hub {
    /// Brings the counts up to `epoch` and records activity. No instruction touched the hub
    /// between `counted_epoch` and `epoch`, so every epoch in between had the count that
    /// took effect at `counted_epoch + 1`.
    pub fn roll(&mut self, epoch: u64, min_members: u32) {
        if epoch > self.counted_epoch {
            if self.members >= min_members {
                self.last_active_epoch = self.counted_epoch;
            }
            self.members = self.members + self.joining - self.leaving;
            self.joining = 0;
            self.leaving = 0;
            self.counted_epoch = epoch;
        }
        if self.members >= min_members {
            self.last_active_epoch = epoch;
        }
    }
}

#[account]
#[derive(InitSpace)]
pub struct Membership {
    pub identity: Pubkey,
    /// The member's hub, or the default key when it has none.
    pub hub: Pubkey,
    pub hub_id: u64,
    /// The epoch of the join or move that led to `hub`.
    pub joined_epoch: u64,
    /// The first epoch the membership in `hub` counts.
    pub effective_epoch: u64,
    /// A leave or move waiting for `pending_epoch`. From that epoch on it is in effect,
    /// whether or not an instruction has written it into `hub` yet.
    pub pending: Pending,
    pub pending_hub: Pubkey,
    pub pending_hub_id: u64,
    pub pending_epoch: u64,
    /// No join, leave or move before this epoch.
    pub cooldown_until_epoch: u64,
    pub bump: u8,
}

impl Membership {
    /// Writes a pending leave or move into `hub` once its epoch has come.
    pub fn settle(&mut self, epoch: u64) {
        if self.pending == Pending::None || epoch < self.pending_epoch {
            return;
        }
        if self.pending == Pending::Move {
            self.joined_epoch = self.pending_epoch - 1;
        }
        self.hub = self.pending_hub;
        self.hub_id = self.pending_hub_id;
        self.effective_epoch = self.pending_epoch;
        self.pending = Pending::None;
        self.pending_hub = Pubkey::default();
        self.pending_hub_id = 0;
        self.pending_epoch = 0;
    }

    /// Checks that a leave or move may start now and schedules it for the next epoch.
    fn begin_change(&mut self, epoch: u64, cooldown_epochs: u64, hub: &Pubkey) -> Result<()> {
        self.settle(epoch);
        require!(self.pending == Pending::None, RegistryError::ChangePending);
        require!(self.hub != Pubkey::default(), RegistryError::NotAMember);
        require_keys_eq!(*hub, self.hub, RegistryError::WrongHub);
        require!(epoch >= self.effective_epoch, RegistryError::MembershipPending);
        require!(epoch >= self.cooldown_until_epoch, RegistryError::CooldownActive);
        self.pending_epoch = epoch + 1;
        self.cooldown_until_epoch = self.pending_epoch + cooldown_epochs;
        Ok(())
    }
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, PartialEq, Eq, InitSpace)]
pub enum Pending {
    None,
    Leave,
    Move,
}

/// Earth-centred, Earth-fixed coordinates in the WGS84 frame, in whole metres.
#[derive(AnchorSerialize, AnchorDeserialize, Clone, Copy, InitSpace)]
pub struct Location {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(AnchorSerialize, AnchorDeserialize, Clone)]
pub struct Config {
    pub fee_destination: Pubkey,
    pub deposit_lamports: u64,
    pub registration_fee_bps: u16,
    pub cooldown_epochs: u64,
    pub inactivity_epochs: u64,
    pub min_members: u32,
    pub code_min_len: u8,
    pub code_max_len: u8,
}

// Instruction accounts

#[derive(Accounts)]
pub struct Initialize<'info> {
    #[account(mut)]
    pub authority: Signer<'info>,
    #[account(init, payer = authority, space = 8 + Registry::INIT_SPACE, seeds = [REGISTRY_SEED], bump)]
    pub registry: Account<'info, Registry>,
    #[account(init, payer = authority, space = 8 + FeeVault::INIT_SPACE, seeds = [FEE_VAULT_SEED], bump)]
    pub fee_vault: Account<'info, FeeVault>,
    #[account(constraint = program.programdata_address()? == Some(program_data.key()) @ RegistryError::NotUpgradeAuthority)]
    pub program: Program<'info, crate::program::HubRegistry>,
    #[account(constraint = program_data.upgrade_authority_address == Some(authority.key()) @ RegistryError::NotUpgradeAuthority)]
    pub program_data: Account<'info, ProgramData>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
#[instruction(code: String)]
pub struct RegisterHub<'info> {
    #[account(mut)]
    pub registrant: Signer<'info>,
    #[account(mut, seeds = [REGISTRY_SEED], bump = registry.bump, has_one = fee_vault)]
    pub registry: Account<'info, Registry>,
    #[account(init, payer = registrant, space = 8 + Hub::INIT_SPACE, seeds = [HUB_SEED, code.as_bytes()], bump)]
    pub hub: Account<'info, Hub>,
    #[account(mut)]
    pub fee_vault: Account<'info, FeeVault>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct JoinHub<'info> {
    #[account(mut)]
    pub payer: Signer<'info>,
    /// The validator identity.
    pub identity: Signer<'info>,
    #[account(seeds = [REGISTRY_SEED], bump = registry.bump)]
    pub registry: Account<'info, Registry>,
    #[account(mut)]
    pub hub: Account<'info, Hub>,
    #[account(init_if_needed, payer = payer, space = 8 + Membership::INIT_SPACE, seeds = [MEMBER_SEED, identity.key().as_ref()], bump)]
    pub membership: Account<'info, Membership>,
    pub system_program: Program<'info, System>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct LeaveHub<'info> {
    pub identity: Signer<'info>,
    #[account(seeds = [REGISTRY_SEED], bump = registry.bump)]
    pub registry: Account<'info, Registry>,
    /// CHECK: the member's hub address, checked against the membership. The account may
    /// have been released since the member joined.
    #[account(mut)]
    pub hub: UncheckedAccount<'info>,
    #[account(mut, seeds = [MEMBER_SEED, identity.key().as_ref()], bump = membership.bump)]
    pub membership: Account<'info, Membership>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct MoveHub<'info> {
    pub identity: Signer<'info>,
    #[account(seeds = [REGISTRY_SEED], bump = registry.bump)]
    pub registry: Account<'info, Registry>,
    /// CHECK: the member's hub address, checked against the membership. The account may
    /// have been released since the member joined.
    #[account(mut)]
    pub from_hub: UncheckedAccount<'info>,
    #[account(mut)]
    pub to_hub: Account<'info, Hub>,
    #[account(mut, seeds = [MEMBER_SEED, identity.key().as_ref()], bump = membership.bump)]
    pub membership: Account<'info, Membership>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct ReleaseHub<'info> {
    #[account(seeds = [REGISTRY_SEED], bump = registry.bump)]
    pub registry: Account<'info, Registry>,
    #[account(mut, close = registrant, has_one = registrant)]
    pub hub: Account<'info, Hub>,
    /// CHECK: the hub's registrant, checked by `has_one`.
    #[account(mut)]
    pub registrant: UncheckedAccount<'info>,
}

#[event_cpi]
#[derive(Accounts)]
pub struct SweepFees<'info> {
    #[account(seeds = [REGISTRY_SEED], bump = registry.bump, has_one = fee_vault)]
    pub registry: Account<'info, Registry>,
    #[account(mut)]
    pub fee_vault: Account<'info, FeeVault>,
    /// CHECK: the registry's fee destination.
    #[account(mut, address = registry.fee_destination)]
    pub destination: UncheckedAccount<'info>,
}

// Events

#[event]
pub struct HubRegistered {
    pub hub: Pubkey,
    pub id: u64,
    pub code: [u8; MAX_CODE_LEN],
    pub location: Location,
    pub registrant: Pubkey,
    pub deposit_lamports: u64,
    pub fee_lamports: u64,
    pub epoch: u64,
}

#[event]
pub struct MemberJoined {
    pub identity: Pubkey,
    pub hub: Pubkey,
    pub hub_id: u64,
    pub effective_epoch: u64,
}

#[event]
pub struct MemberLeaving {
    pub identity: Pubkey,
    pub hub: Pubkey,
    pub hub_id: u64,
    pub effective_epoch: u64,
    pub cooldown_until_epoch: u64,
}

#[event]
pub struct MemberMoving {
    pub identity: Pubkey,
    pub from_hub: Pubkey,
    pub from_hub_id: u64,
    pub to_hub: Pubkey,
    pub to_hub_id: u64,
    pub effective_epoch: u64,
    pub cooldown_until_epoch: u64,
}

#[event]
pub struct HubReleased {
    pub hub: Pubkey,
    pub id: u64,
    pub code: [u8; MAX_CODE_LEN],
    pub registrant: Pubkey,
    pub returned_lamports: u64,
    pub epoch: u64,
}

#[event]
pub struct FeesSwept {
    pub destination: Pubkey,
    pub lamports: u64,
}

#[error_code]
pub enum RegistryError {
    #[msg("Only the program's upgrade authority can initialize the registry")]
    NotUpgradeAuthority,
    #[msg("The configuration is out of range")]
    InvalidConfig,
    #[msg("A hub code is capital letters A-Z within the registry's length range")]
    InvalidCode,
    #[msg("The location is not an ECEF point near the Earth's surface, in metres")]
    InvalidLocation,
    #[msg("The validator is already a member of a hub")]
    AlreadyMember,
    #[msg("The validator is not a member of a hub")]
    NotAMember,
    #[msg("The hub account is not the member's hub")]
    WrongHub,
    #[msg("The validator is already in that hub")]
    SameHub,
    #[msg("The membership takes effect at the next epoch boundary")]
    MembershipPending,
    #[msg("A leave or move is already waiting for the next epoch boundary")]
    ChangePending,
    #[msg("The validator's cooldown has not ended")]
    CooldownActive,
    #[msg("The hub has not been inactive for the registry's period")]
    HubNotInactive,
    #[msg("The fee vault holds nothing above its rent")]
    NothingToSweep,
}
