// The bridge's read model: deposits, credits, withdrawals and releases, from the acrux_bridge
// program on Solana (SOLANA_RPC_URL) and on Acrux (the gateway), read every BRIDGE_POLL_SECONDS.
// Deposit and withdrawal records are read once, by sequence; what changes after (credited,
// certified, released) is read for the transfers still on their way.
import { ACRUX_BRIDGE_IDL, paginate, type Bridge, type BridgeTransfer, type PageQuery } from '@acrux/contract'
import { BorshAccountsCoder, type Idl } from '@coral-xyz/anchor'
import { PublicKey } from '@solana/web3.js'

const coder = new BorshAccountsCoder(ACRUX_BRIDGE_IDL as unknown as Idl)
const u64 = (n: number) => { const b = Buffer.alloc(8); b.writeBigUInt64LE(BigInt(n)); return b }
type Account = { data: Buffer; owner: string } | null

type Deposit = { sequence: number; depositor: string; recipient: string; mint: string; amount: string; slot: number; seenAt: number }
type Withdrawal = { sequence: number; owner: string; recipient: string; solanaMint: string; amount: string; slot: number; seenAt: number; hash?: string; certified: boolean; released: boolean }

export class BridgeReader {
  readonly program: PublicKey | null
  private deposits = new Map<number, Deposit>()
  private withdrawals = new Map<number, Withdrawal>()
  private credited = 0
  private decimals = new Map<string, number>()
  private view: Omit<Bridge, 'status' | 'program' | 'fromSlot'> = { lightClient: { epoch: null, validators: 0, latestCertified: null }, vaults: [], totals: { deposits: 0, credited: 0, withdrawals: 0, released: 0 }, readAt: null }

  constructor(program: string | undefined, private readonly fromSlot: number | null, private readonly solanaUrl: string, private readonly acruxUrl: string, private readonly pollMs: number) {
    this.program = program ? new PublicKey(program) : null
  }

  start() {
    if (!this.program) return
    const loop = async () => {
      try { await this.read() } catch (error) { console.error(`bridge read: ${(error as Error).message}`) }
      setTimeout(loop, this.pollMs).unref()
    }
    void loop()
  }

  private pda(...seeds: Buffer[]) { return PublicKey.findProgramAddressSync(seeds, this.program!)[0] }

  private async rpc(url: string, method: string, params: unknown[]) {
    const response = await fetch(url, { method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ jsonrpc: '2.0', id: 1, method, params }), signal: AbortSignal.timeout(10_000) })
    const body = await response.json() as { result?: any; error?: { message: string } }
    if (body.error) throw new Error(`${method}: ${body.error.message}`)
    return body.result
  }

  private async accounts(url: string, keys: PublicKey[]): Promise<Account[]> {
    const out: Account[] = []
    for (let i = 0; i < keys.length; i += 100) {
      const result = await this.rpc(url, 'getMultipleAccounts', [keys.slice(i, i + 100).map(k => k.toBase58()), { encoding: 'base64', commitment: 'finalized' }])
      out.push(...result.value.map((a: any) => a ? { data: Buffer.from(a.data[0], 'base64'), owner: a.owner } : null))
    }
    return out
  }

  private async programAccounts(name: string) {
    const discriminator = coder.accountDiscriminator(name)
    const result = await this.rpc(this.solanaUrl, 'getProgramAccounts', [this.program!.toBase58(), { encoding: 'base64', commitment: 'finalized', filters: [{ memcmp: { offset: 0, bytes: discriminator.toString('base64'), encoding: 'base64' } }] }])
    return (result as any[]).map(a => ({ address: a.pubkey as string, value: coder.decode(name, Buffer.from(a.account.data[0], 'base64')) }))
  }

  private async read() {
    const now = Date.now()
    const [bridgeAccount, outbound, creditedAccount] = await Promise.all([
      this.accounts(this.solanaUrl, [this.pda(Buffer.from('bridge'))]).then(a => a[0]),
      this.accounts(this.acruxUrl, [this.pda(Buffer.from('outbound'))]).then(a => a[0]),
      this.accounts(this.acruxUrl, [this.pda(Buffer.from('credited'))]).then(a => a[0]),
    ])
    // New deposits on Solana and withdrawals on Acrux, by sequence.
    const made = bridgeAccount ? Number(coder.decode('Bridge', bridgeAccount.data).deposits) : 0
    const newDeposits = Array.from({ length: Math.max(0, made - this.deposits.size) }, (_, i) => this.deposits.size + i)
    for (const [i, record] of (await this.accounts(this.solanaUrl, newDeposits.map(s => this.pda(Buffer.from('deposit'), u64(s))))).entries()) {
      if (!record) break
      const d = coder.decode('DepositRecord', record.data)
      this.deposits.set(newDeposits[i], { sequence: newDeposits[i], depositor: d.depositor.toBase58(), recipient: d.recipient.toBase58(), mint: d.mint.toBase58(), amount: d.amount.toString(), slot: Number(d.slot), seenAt: now })
    }
    const burned = outbound ? Number(coder.decode('Outbound', outbound.data).withdrawals) : 0
    const newWithdrawals = Array.from({ length: Math.max(0, burned - this.withdrawals.size) }, (_, i) => this.withdrawals.size + i)
    for (const [i, record] of (await this.accounts(this.acruxUrl, newWithdrawals.map(s => this.pda(Buffer.from('withdrawal'), u64(s))))).entries()) {
      if (!record) break
      const w = coder.decode('Withdrawal', record.data)
      this.withdrawals.set(newWithdrawals[i], { sequence: newWithdrawals[i], owner: w.owner.toBase58(), recipient: w.recipient.toBase58(), solanaMint: w.solana_mint.toBase58(), amount: w.amount.toString(), slot: Number(w.slot), seenAt: now, certified: false, released: false })
    }
    // The credit counter on Acrux: the next deposit sequence to credit.
    this.credited = creditedAccount ? Number(creditedAccount.data.readBigUInt64LE(0)) : 0

    // Solana's light client: its validator sets and the cblocks posted to it.
    const [sets, cblocks] = await Promise.all([this.programAccounts('ValidatorSet'), this.programAccounts('Cblock')])
    const certified = new Map(cblocks.filter(c => c.value.certified).map(c => [Buffer.from(c.value.hash).toString('hex'), c]))
    const latest = [...certified.values()].sort((a, b) => Number(b.value.slot) - Number(a.value.slot))[0]
    // Withdrawals on their way: their cblock's hash from the gateway, then certified and released.
    const pending = [...this.withdrawals.values()].filter(w => !w.released)
    for (const w of pending.filter(w => !w.hash)) {
      const proof = await this.rpc(this.acruxUrl, 'getCblockProof', { consensusSlot: Math.floor(w.slot / 4) } as never).catch(() => null)
      if (proof?.hash) w.hash = proof.hash
    }
    for (const w of pending) w.certified = !!w.hash && certified.has(w.hash)
    const releases = await this.accounts(this.solanaUrl, pending.map(w => this.pda(Buffer.from('released'), u64(w.sequence))))
    pending.forEach((w, i) => { w.released = !!releases[i] })

    // Vaults on Solana and the bridged mints on Acrux, for every mint that came across.
    const mints = [...new Set([...this.deposits.values()].map(d => d.mint))]
    const unknown = mints.filter(m => !this.decimals.has(m))
    ;(await this.accounts(this.solanaUrl, unknown.map(m => new PublicKey(m)))).forEach((a, i) => { if (a) this.decimals.set(unknown[i], a.data[44]) })
    const vaults = await this.accounts(this.solanaUrl, mints.map(m => this.pda(Buffer.from('vault'), new PublicKey(m).toBuffer())))
    const bridgedMints = mints.map(m => this.pda(Buffer.from('mint'), new PublicKey(m).toBuffer()))
    const supplies = await this.accounts(this.acruxUrl, bridgedMints)
    this.view = {
      lightClient: {
        epoch: sets.length ? Math.max(...sets.map(s => Number(s.value.epoch))) : null,
        validators: sets.length ? sets.sort((a, b) => Number(b.value.epoch) - Number(a.value.epoch))[0].value.validators.length : 0,
        latestCertified: latest ? { consensusSlot: Number(latest.value.slot), epoch: Number(latest.value.epoch), hash: Buffer.from(latest.value.hash).toString('hex'), outboxRoot: Buffer.from(latest.value.outbox_root).toString('hex'), account: latest.address } : null,
      },
      vaults: mints.map((m, i) => ({
        solanaMint: m, acruxMint: bridgedMints[i].toBase58(), decimals: this.decimals.get(m) ?? 0,
        lockedBaseUnits: vaults[i] ? vaults[i]!.data.readBigUInt64LE(64).toString() : '0',
        bridgedBaseUnits: supplies[i] ? supplies[i]!.data.readBigUInt64LE(36).toString() : '0',
      })),
      totals: { deposits: this.deposits.size, credited: this.credited, withdrawals: this.withdrawals.size, released: [...this.withdrawals.values()].filter(w => w.released).length },
      readAt: new Date(now).toISOString(),
    }
  }

  overview(): Bridge {
    return { status: this.program && this.view.readAt ? 'live' : 'waiting', program: this.program?.toBase58() ?? null, fromSlot: this.fromSlot, ...this.view }
  }

  transfers(query: PageQuery & { address?: string; kind?: 'deposit' | 'withdrawal' }) {
    const acruxMint = (mint: string) => this.pda(Buffer.from('mint'), new PublicKey(mint).toBuffer()).toBase58()
    const deposits = [...this.deposits.values()].map((d): BridgeTransfer & { seenAt: number } => ({
      kind: 'deposit', sequence: d.sequence, status: d.sequence < this.credited ? 'credited' : 'locked', solanaMint: d.mint, acruxMint: acruxMint(d.mint),
      amount: d.amount, from: d.depositor, to: d.recipient, solanaSlot: d.slot, acruxSlot: null, cblock: null, releaseAccount: null, seenAt: d.seenAt,
    }))
    const withdrawals = [...this.withdrawals.values()].map((w): BridgeTransfer & { seenAt: number } => ({
      kind: 'withdrawal', sequence: w.sequence, status: w.released ? 'released' : w.certified ? 'certified' : 'burned', solanaMint: w.solanaMint, acruxMint: acruxMint(w.solanaMint),
      amount: w.amount, from: w.owner, to: w.recipient, solanaSlot: null, acruxSlot: w.slot, cblock: Math.floor(w.slot / 4),
      releaseAccount: w.released ? this.pda(Buffer.from('released'), u64(w.sequence)).toBase58() : null, seenAt: w.seenAt,
    }))
    const rows = [...deposits, ...withdrawals]
      .filter(t => (!query.kind || t.kind === query.kind) && (!query.address || t.from === query.address || t.to === query.address))
      .sort((a, b) => b.seenAt - a.seenAt || b.sequence - a.sequence)
      .map(({ seenAt, ...t }) => t)
    return paginate(rows, query)
  }
}
