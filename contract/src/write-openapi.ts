import { writeFileSync } from 'node:fs'
import { z } from 'zod'
import * as s from './schemas.js'
import { ProtocolEvent, LabReplay, StreamCommand } from './events.js'
import { rpcMethods, RpcRequest, RpcFailure } from './rpc.js'
import { openApiDocument, type Operation } from './openapi.js'
const operations: Operation[] = [
  { method:'get', path:'/v1/bridge', operationId:'bridge',summary:'The Solana bridge: its program, vaults, totals and the light client\'s newest certified cblock',response:'Bridge' },
  { method:'get', path:'/v1/bridge/transfers', operationId:'bridgeTransfers',summary:'Deposits and withdrawals, newest first; address matches either end',response:'BridgeTransfers',query:s.BridgeTransfersQuery },
  { method:'get', path:'/v1/buyback', operationId:'buyback',summary:'Creator fee buyback totals and recent confirmed actions', response:'Buyback' },
  { method:'get', path:'/v1/replay', operationId:'replay',summary:'The replayed mainnet range: totals and its sandwiches, newest first; from and to narrow it to one slot range',response:'ReplayRange',query:z.object({from:s.Count.optional(),to:s.Count.optional()}) },
  { method:'get', path:'/v1/replay/{slot}', operationId:'replaySlot',summary:'One replayed mainnet slot: both orders, the Acrux slots and its sandwiches repriced; across an epoch only slots with sandwiches are kept whole',response:'ReplaySlotRecord',params:{slot:'Count'} },
  { method:'get', path:'/health', operationId:'health',summary:'Service and configuration status', response:'Health' },
  { method:'get', path:'/v1/hubs', operationId:'hubs',summary:'Registry hubs',response:'Hubs',query:s.PageQuery },
  { method:'get', path:'/v1/hubs/{code}', operationId:'hub',summary:'Hub and members',response:'Hub',params:{code:'HubCode'} },
  { method:'get', path:'/v1/registry/config', operationId:'registryConfig',summary:'Registry flow configuration',response:'RegistryConfig' },
  { method:'get', path:'/v1/registry/members/{identity}', operationId:'member',summary:'Validator membership and cooldown',response:'Member',params:{identity:'Address'} },
  { method:'get', path:'/v1/schedule', operationId:'schedule',summary:'Algorithm 3 epoch preview',response:'SchedulePreview',query:z.object({epoch:s.Count.optional()}) },
  { method:'get', path:'/v1/facts', operationId:'facts',summary:'Explicitly synthetic facts snapshot',response:'Facts' },
  { method:'get', path:'/v1/spec', operationId:'spec',summary:'Paper to implementation map',response:'SpecMap' },
  { method:'get', path:'/v1/transactions/{id}', operationId:'transaction',summary:'Transaction trail',response:'Transaction',params:{id:'TxId'} },
  { method:'get', path:'/v1/faucet/{address}',operationId:'faucetStatus',summary:'Faucet limits and eligibility',response:'FaucetStatus',params:{address:'Address'} },
  { method:'post',path:'/v1/faucet',operationId:'faucet',summary:'Local funding grant; node mode returns the real transaction ID and signature',request:'FaucetRequest',response:'FaucetGrant' },
  { method:'get',path:'/v1/lab/scenarios',operationId:'labScenarios',summary:'Six reproducible fault scenarios',response:'LabScenarios' },
  { method:'get',path:'/v1/lab/runs',operationId:'labRuns',summary:'Recorded lab runs',response:'LabRuns',query:s.PageQuery },
  { method:'post',path:'/v1/lab/runs',operationId:'startLabRun',summary:'Start a fixture lab run',response:'LabRun',request:'LabRunRequest',created:true },
  { method:'get',path:'/v1/lab/runs/{id}',operationId:'labRun',summary:'Lab run progress and evidence',response:'LabRun' },
  { method:'get',path:'/v1/lab/runs/{id}/events',operationId:'labReplay',summary:'Recorded protocol event replay',response:'LabReplay' },
]
const schemas = Object.fromEntries(Object.entries(s).filter(([,v]) => v instanceof z.ZodType)) as Record<string,z.ZodType>
Object.assign(schemas, { ProtocolEvent, LabReplay, StreamCommand, RpcRequest, RpcFailure, LabScenarios:z.array(s.LabScenario) })
const doc = openApiDocument({ title:'Acrux API',version:s.CONTRACT_VERSION,description:'Round 1 fixture contract. Amounts are decimal integer strings. Times are relative microseconds with UTC slot wallTime.',servers:[{url:'http://127.0.0.1:4920'}],schemas,operations,error:{schema:'ApiError',statuses:[...new Set(Object.values(s.errors.status))]},fixture:{header:'X-Fixture',values:[...s.SCENARIOS,...s.ErrorCode.options]} })
const ref = { $ref:'#/components/schemas/ProtocolEvent' }
Object.assign(doc.paths, { '/v1/lab/runs/{id}/stream': {get:{operationId:'labStream', summary:'SSE lab progress and replay; event equals type, id equals sequence',parameters:[{name:'id',in:'path',required:true,schema:{type:'string'}},{name:'Last-Event-ID',in:'header',schema:{type:'string'}},{name:'fixture',in:'query',schema:{type:'string'}}], responses:{'200':{description:'ProtocolEvent envelopes as text/event-stream',content:{'text/event-stream':{schema:ref}}}}}} })
const rpc = Object.fromEntries(Object.entries(rpcMethods).map(([name,method]) => [name, {params:z.toJSONSchema(method.params,{io:'input'}),result:z.toJSONSchema(method.result)}]))
writeFileSync(new URL('../openapi.json',import.meta.url), JSON.stringify({...doc,'x-acrux-json-rpc':{url:'http://127.0.0.1:4930',methods:rpc},'x-acrux-event-stream':{url:'ws://127.0.0.1:4931',command:z.toJSONSchema(StreamCommand),event:ref}},null,2)+'\n')