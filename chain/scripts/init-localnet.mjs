// Waits for the local validator, then initializes the registry with the local values.
import { LOCAL_CONFIG, RPC_URL, feeVaultAddress, initializeLocal, programId, registryAddress, registryProgram } from './registry.mjs'

const program = registryProgram()
const deadline = Date.now() + 90_000
for (;;) {
  try {
    // The program is in genesis; a confirmed slot past the first means the validator serves.
    if ((await program.provider.connection.getSlot('confirmed')) > 1) break
  } catch {}
  if (Date.now() > deadline) throw new Error(`No validator at ${RPC_URL}`)
  await new Promise((r) => setTimeout(r, 500))
}
const registry = await initializeLocal(program)
console.log(`RPC URL        ${RPC_URL}`)
console.log(`Program id     ${programId.toBase58()}`)
console.log(`Registry       ${registryAddress.toBase58()}`)
console.log(`Fee vault      ${feeVaultAddress.toBase58()}`)
console.log(`Authority      ${registry.authority.toBase58()}`)
console.log(`Deposit        ${registry.depositLamports} lamports, fee ${registry.registrationFeeBps} bps`)
console.log(`Cooldown       ${registry.cooldownEpochs} epoch(s), inactivity ${registry.inactivityEpochs} epochs, n_min ${registry.minMembers}`)
console.log(`Hub code       A-Z, ${LOCAL_CONFIG.codeMinLen} to ${LOCAL_CONFIG.codeMaxLen} characters`)
