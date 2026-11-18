// Regression for fresh hand-off dependencies: exercise the installed web3 send serializer.
// A processed bank without the confirmed parent drops the old skipPreflight send; the matched
// confirmed bank accepts it within the same journal deadline. No server or network is used.
import assert from 'node:assert/strict'
import { Connection } from '@solana/web3.js'
import { createJournal } from '../dist/engines/keeper/journal.js'
let accepted = false, rejectPreflight = false, sentConfig
const row = { key:'bridge:program:handoff:1', state:'signed', signature:'handoff-signature', signedBase64:Buffer.from('journaled-hand-off').toString('base64'), blockhash:'blockhash', contextSlot:100, lastValidBlockHeight:200, error:null }
const connection = new Connection('http://127.0.0.1:5041', { commitment:'confirmed', fetch:async (_url, init) => {
  const request = JSON.parse(init.body)
  let result
  if(request.method==='getSignatureStatuses') result={context:{slot:101},value:[accepted ? {slot:100,confirmations:1,err:null,confirmationStatus:'confirmed'} : null]}
  else if(request.method==='getBlockHeight') result=100
  else if(request.method==='sendTransaction') {
    sentConfig=request.params[1]
    if(rejectPreflight) return new Response(JSON.stringify({jsonrpc:'2.0',id:request.id,error:{code:-32002,message:'Transaction simulation failed: Blockhash not found',data:{err:'BlockhashNotFound',logs:[]}}}),{headers:{'content-type':'application/json'}})
    accepted=sentConfig.preflightCommitment==='confirmed' && sentConfig.skipPreflight!==true && sentConfig.minContextSlot===100
    result=row.signature
  } else throw new Error(`Unexpected RPC ${request.method}`)
  return new Response(JSON.stringify({jsonrpc:'2.0',id:request.id,result}),{headers:{'content-type':'application/json'}})
} })
const store={get:async()=>row,put:async()=>{throw new Error('The signed hand-off must not be rebuilt')},update:async(_key,patch)=>Object.assign(row,patch),pending:async()=>[row]}
const started=Date.now()
const journal=createJournal({connection,store,signer:{},minContextSlot:()=>100,followMs:3000})
assert.equal(await journal.follow(row.key),'confirmed','hand-off must confirm before its journal deadline')
assert.ok(Date.now()-started<3000)
assert.equal(sentConfig.preflightCommitment,'confirmed')
assert.equal(sentConfig.skipPreflight,undefined) // false is omitted by web3's serializer.
assert.equal(sentConfig.maxRetries,0)
// A refusal with no program logs still exposes the RPC reason and never replaces signed bytes.
accepted=false; rejectPreflight=true; row.state='signed'
const originalBytes=row.signedBase64
await assert.rejects(journal.follow(row.key),/Blockhash not found/)
assert.equal(row.state,'signed')
assert.equal(row.signedBase64,originalBytes)
console.log('PASS hand-off deadline: fresh confirmed dependency uses confirmed preflight and lands without rebuilding')
