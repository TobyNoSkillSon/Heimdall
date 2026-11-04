import {test} from 'node:test';
import assert from 'node:assert/strict';
import {providerFetch,retryDelay,MAX_WAIT_MS,headersDeadline,HEADERS_MS} from './provider-retry.mjs';
const GO='https://opencode.ai/zen/go/v1/messages';
test('reset headers: seconds, HTTP dates, milliseconds, epoch; none is null',()=>{
 assert.equal(retryDelay(new Headers({'retry-after':'2.25'})),2250);
 assert.equal(retryDelay(new Headers({'retry-after':'Thu, 01 Jan 1970 00:00:05 GMT'}),1000),4000);
 assert.equal(retryDelay(new Headers({'retry-after-ms':'1500'})),1500);
 assert.equal(retryDelay(new Headers({'x-ratelimit-reset':'2000000000'}),1999999990000),10000);
 assert.equal(retryDelay(new Headers()),null);
});
test('throttles with a reset wait and resend the same request, then report resumption',async()=>{
 let calls=0; const waits=[],events=[]; const init={method:'POST',body:'original tool request'};
 const fetch=providerFetch(async(i,o)=>{assert.equal(i,GO);assert.equal(o.body,init.body);calls++;return calls<=3?new Response('',{status:429,headers:{'retry-after':'0.125'}}):new Response('stream');},e=>events.push(e),async ms=>waits.push(ms));
 assert.equal(await (await fetch(GO,init)).text(),'stream'); assert.equal(calls,4);assert.deepEqual(waits,[125,125,125]);
 assert.equal(events.filter(e=>e.type==='heimdall_provider_wait'&&e.delay_ms===125).length,3);assert.equal(events.at(-1).delay_ms,0);
});
test('an exhausted allowance or a 429 without a reset is reported once, never looped',async()=>{
 for (const headers of [{}, {'retry-after':String(MAX_WAIT_MS/1000+60)}]) {
  let calls=0;const events=[];
  const fetch=providerFetch(async()=>{calls++;return new Response('limit',{status:429,headers});},e=>events.push(e),async()=>assert.fail('must not wait'),()=>0);
  assert.equal((await fetch(GO,{method:'POST'})).status,429);assert.equal(calls,1);
  assert.equal(events.length,1);assert.equal(events[0].type,'heimdall_provider_problem');
  assert.match(events[0].message,headers['retry-after']?/^OpenCode Go: usage limit reached · resets \d\d:\d\d$/:/^OpenCode Go: usage limit reached$/);
 }
});
test('rejected keys and oversized requests pass through with a named reason',async()=>{
 for (const [status,text] of [[401,'access rejected / no subscription'],[403,'access rejected / no subscription'],[413,'request too large for this model'],[404,'endpoint not found for this model']]) {
  const events=[];let calls=0;
  const fetch=providerFetch(async()=>{calls++;return new Response('x',{status});},e=>events.push(e));
  assert.equal((await fetch('https://opencode.ai/inference/go/anthropic/v1/messages',{method:'POST'})).status,status);
  assert.equal(calls,1);assert.deepEqual(events,[{type:'heimdall_provider_problem',message:`OpenCode Go: ${text}`}]);
 }
});
test('cancellation stops the wait and never submits another request',async()=>{
 const controller=new AbortController();let calls=0;
 const fetch=providerFetch(async()=>{calls++;return new Response('',{status:429,headers:{'retry-after':'60'}});},()=>controller.abort());
 await assert.rejects(fetch(GO,{method:'POST',signal:controller.signal})); assert.equal(calls,1);
});
test('the start deadline aborts a silent provider but never a stream that has started',async()=>{
 let seen;const started=providerFetch(async(_,o)=>{seen=o.signal;return new Response('ok');},()=>{},undefined,undefined,10);
 await started(GO,{method:'POST'}); await new Promise(r=>setTimeout(r,40)); assert.equal(seen.aborted,false);
 const silent=providerFetch((_,o)=>new Promise((_,reject)=>o.signal.addEventListener('abort',()=>reject(o.signal.reason))),()=>{},undefined,undefined,10);
 await assert.rejects(silent(GO,{method:'POST'}),/did not answer within 0.01 seconds/);
});
test('other origins, model lists and local engine traffic pass through untouched',async()=>{
 for(const [url,method] of [['http://127.0.0.1:8100/v1/chat/completions','POST'],['https://opencode.ai/inference/go/v1/models','GET'],['https://api.example.com/v1/messages','POST']]){
  let n=0;const fetch=providerFetch(async(i,o)=>{n++;assert.equal(o?.signal,undefined);return new Response('',{status:429});},()=>assert.fail('no events'));
  assert.equal((await fetch(url,{method})).status,429);assert.equal(n,1);
 }
});
test('thinking requests ask the provider to drop blocks invalidated by a rewritten history (compaction)',async()=>{
 const body={model:'claude-haiku-5-5',thinking:{type:'adaptive',display:'summarized'},messages:[{role:'user',content:'hi'}]};
 let seen;const fetch=providerFetch(async(_,o)=>{seen=o;return new Response('stream');},()=>{});
 await fetch(GO,{method:'POST',body:JSON.stringify(body),headers:{'anthropic-beta':'interleaved-thinking-2025-05-14','x-org-id':'o'}});
 const sent=JSON.parse(seen.body);
 assert.deepEqual(sent.thinking,{type:'adaptive',display:'summarized',block_binding:{prefix_mismatch_behavior:'drop_block'}});
 assert.deepEqual(sent.messages,body.messages);
 const headers=new Headers(seen.headers);
 assert.equal(headers.get('anthropic-beta'),'interleaved-thinking-2025-05-14,thinking-binding-controls-2026-08-01');
 assert.equal(headers.get('x-org-id'),'o');
 for (const plain of [{model:'m',messages:[]},{model:'m',thinking:{type:'disabled'},messages:[]}]) {
  let got;await providerFetch(async(_,o)=>{got=o;return new Response('');},()=>{})(GO,{method:'POST',body:JSON.stringify(plain)});
  assert.equal(got.body,JSON.stringify(plain));assert.equal(new Headers(got.headers).get('anthropic-beta'),null);
 }
});
test('a rejected thinking signature is retried once without thinking blocks, never looped',async()=>{
 const signed={role:'assistant',content:[{type:'thinking',thinking:'t',signature:'s'},{type:'redacted_thinking',data:'r'},{type:'tool_use',id:'tu',name:'echo',input:{}}]};
 const body={model:'claude-haiku-5-5',thinking:{type:'adaptive'},messages:[{role:'user',content:'summary'},signed,{role:'user',content:[{type:'tool_result',tool_use_id:'tu',content:'1'}]}]};
 const rejected=()=>new Response(JSON.stringify({type:'error',error:{type:'invalid_request_error',message:'Upstream request failed: [invalid_request_error] messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.'}}),{status:400});
 for (const [second,status,calls] of [[()=>new Response('stream'),200,2],[rejected,400,2]]) {
  const bodies=[];const events=[];
  const fetch=providerFetch(async(_,o)=>{bodies.push(JSON.parse(o.body));return bodies.length===1?rejected():second();},e=>events.push(e));
  assert.equal((await fetch(GO,{method:'POST',body:JSON.stringify(body)})).status,status);
  assert.equal(bodies.length,calls);
  assert.deepEqual(bodies[1].messages[1].content,[{type:'tool_use',id:'tu',name:'echo',input:{}}]);
  assert.deepEqual(bodies[1].messages[2],body.messages[2]);
  assert.equal(events.filter(e=>e.type==='heimdall_thinking_dropped').length,1);
 }
 // Any other 400 passes through on the first answer.
 let n=0;await providerFetch(async()=>{n++;return new Response('{"error":{"message":"bad"}}',{status:400});},()=>{})(GO,{method:'POST',body:JSON.stringify(body)});
 assert.equal(n,1);
});
const ANT='https://api.anthropic.com/v1/messages';
test('Anthropic: 529 is resent with backoff, a stated delay wins, and it gives up after three',async()=>{
 for (const [plan,expectStatus,expectWaits,headers] of [[[529,529,200],200,[1000,2000],{}],[[529,200],200,[1500],{'retry-after':'1.5'}],[[529,529,529,529,529],529,[1000,2000,4000],{}]]) {
  let n=0;const waits=[],events=[];const init={method:'POST',body:'same request'};
  const fetch=providerFetch(async(i,o)=>{assert.equal(o.body,init.body);const s=plan[n++];return s===200?new Response('stream'):new Response('{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}',{status:s,headers});},e=>events.push(e),async ms=>waits.push(ms),()=>0);
  assert.equal((await fetch(ANT,init)).status,expectStatus);
  assert.deepEqual(waits,expectWaits);assert.equal(n,expectWaits.length+1);
  assert.equal(events.filter(e=>e.type==='heimdall_provider_wait'&&e.delay_ms>0).length,expectWaits.length);
  if (expectStatus===529) assert.deepEqual(events.filter(e=>e.type==='heimdall_provider_problem'),[{type:'heimdall_provider_problem',message:'Anthropic overloaded · try again shortly'}]);
  assert.equal(events.at(-1).delay_ms,0,'the wait is reported over');
 }
});
test('Anthropic: spent credits are named and never waited on; keys and rate limits say what they are',async()=>{
 for (const [status,body,message] of [
  [400,'{"type":"error","error":{"type":"invalid_request_error","message":"Your credit balance is too low to access the Anthropic API. Please go to Plans & Billing to upgrade or purchase credits."}}','Anthropic credits used up'],
  [402,'{"type":"error","error":{"type":"billing_error","message":"Payment required"}}','Anthropic credits used up'],
  [401,'{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}','Anthropic: key rejected'],
  [403,'{"type":"error","error":{"type":"permission_error","message":"no"}}','Anthropic: key not allowed to use this model'],
  [429,'{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}','Anthropic: rate limit reached'],
 ]) {
  let n=0;const events=[];
  const fetch=providerFetch(async()=>{n++;return new Response(body,{status});},e=>events.push(e),async()=>assert.fail('must not wait'),()=>0);
  const response=await fetch(ANT,{method:'POST'});
  assert.equal(response.status,status);assert.equal(n,1);
  assert.equal(await response.text(),body,'Pi still reads the body');
  assert.deepEqual(events,[{type:'heimdall_provider_problem',message}]);
 }
 // A throttle with a stated reset waits, as for OpenCode.
 let n=0;const waits=[];
 const fetch=providerFetch(async()=>{n++;return n===1?new Response('{"type":"error","error":{"type":"rate_limit_error","message":"slow"}}',{status:429,headers:{'retry-after':'2'}}):new Response('ok');},()=>{},async ms=>waits.push(ms),()=>0);
 assert.equal((await fetch(ANT,{method:'POST'})).status,200);assert.deepEqual(waits,[2000]);
});
test('Anthropic shares the thinking-binding control and the one signature resend; Pi\'s own drop_block is not doubled',async()=>{
 const body={model:'claude-opus-5-5',thinking:{type:'adaptive',display:'summarized',block_binding:{prefix_mismatch_behavior:'drop_block'}},messages:[{role:'user',content:'hi'},{role:'assistant',content:[{type:'thinking',thinking:'t',signature:'s'},{type:'text',text:'x'}]}]};
 const rejected=()=>new Response('{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}',{status:400});
 const seen=[];const events=[];
 const fetch=providerFetch(async(_,o)=>{seen.push(o);return seen.length===1?rejected():new Response('stream');},e=>events.push(e));
 assert.equal((await fetch(ANT,{method:'POST',body:JSON.stringify(body),headers:{'anthropic-beta':'thinking-binding-controls-2026-08-01'}})).status,200);
 assert.equal(seen.length,2);
 assert.equal(new Headers(seen[0].headers).get('anthropic-beta'),'thinking-binding-controls-2026-08-01');
 assert.deepEqual(JSON.parse(seen[0].body).thinking.block_binding,{prefix_mismatch_behavior:'drop_block'});
 assert.deepEqual(JSON.parse(seen[1].body).messages[1].content,[{type:'text',text:'x'}]);
 assert.deepEqual(events.map(e=>e.type),['heimdall_thinking_dropped']);
});
test('a prompt near the context window gets longer to start answering (26 Oct: 183k tokens took 12-13 s)',()=>{
 assert.equal(headersDeadline(undefined),HEADERS_MS);
 assert.equal(headersDeadline('x'.repeat(183000*4)),HEADERS_MS,'up to 200k tokens: two minutes');
 assert.equal(headersDeadline('x'.repeat(1000000*4)),HEADERS_MS+200000,'1M tokens: 320 s');
 assert.equal(headersDeadline('x'.repeat(400000*4),10),10+50000);
});
// Pi 1.4.0 sends thinking Off as `disabled`, which Sonnet 5.5 rejects. The rewrite happens on the wire, so it covers
// turns and compaction summaries (which skip `before_provider_request`) alike.
const MESSAGES='https://api.anthropic.com/v1/messages?beta=true';
const offBody=model=>({model,max_tokens:16000,stream:true,thinking:{type:'disabled'},messages:[{role:'user',content:[{type:'text',text:'hi'}]}]});
async function sent(url,body,mode){
 let seen;const fetch=providerFetch(async(_,o)=>{seen=o;return new Response('stream');},()=>{},undefined,undefined,undefined,mode);
 await fetch(url,{method:'POST',body:JSON.stringify(body),headers:{'anthropic-beta':'x'}});
 return {body:JSON.parse(seen.body),headers:new Headers(seen.headers)};
}
test('thinking Off on the wire: Sonnet becomes between_tools at effort Low, without block binding',async()=>{
 const {body,headers}=await sent(MESSAGES,offBody('claude-sonnet-5-5'),'between_tools');
 assert.deepEqual(body.thinking,{type:'between_tools'});
 assert.deepEqual(body.output_config,{effort:'low'});
 assert.deepEqual(body.messages,offBody('claude-sonnet-5-5').messages);
 assert.equal(headers.get('anthropic-beta'),'x','no thinking-binding beta for between_tools');
});
test('thinking Off on the wire: Haiku keeps disabled and gains effort Low',async()=>{
 const {body}=await sent(MESSAGES,offBody('claude-haiku-5-5'),'disabled');
 assert.deepEqual(body.thinking,{type:'disabled'});
 assert.deepEqual(body.output_config,{effort:'low'});
});
test('thinking Off on the wire: no Off mode, another level, or another endpoint is left alone',async()=>{
 assert.deepEqual((await sent(MESSAGES,offBody('claude-opus-5-5'),undefined)).body,offBody('claude-opus-5-5'));
 assert.deepEqual((await sent('https://api.anthropic.com/v1/messages/count_tokens',offBody('claude-sonnet-5-5'),'between_tools')).body,offBody('claude-sonnet-5-5'));
 assert.deepEqual((await sent(GO,offBody('claude-sonnet-5-5'),'between_tools')).body,offBody('claude-sonnet-5-5'));
 const adaptive={...offBody('claude-sonnet-5-5'),thinking:{type:'adaptive',display:'summarized'},output_config:{effort:'high'}};
 const out=(await sent(MESSAGES,adaptive,'between_tools')).body;
 assert.equal(out.thinking.type,'adaptive');assert.deepEqual(out.output_config,{effort:'high'});
});
// Pi 1.4.0 retries any error text matching "upstream request failed", OpenCode Go's wording for every rejected key.
test('a rejected key stays final: OpenCode Go\'s "Upstream request failed" becomes Heimdall\'s words, nothing else changes',async()=>{
 const body='{"type":"error","error":{"type":"api_error","message":"Upstream request failed: Invalid credential"}}';
 const events=[];let calls=0;
 const fetch=providerFetch(async()=>{calls++;return new Response(body,{status:401,headers:{'content-type':'application/json','content-length':String(body.length),'x-request-id':'r1'}});},e=>events.push(e));
 for (let i=0;i<2;i++) {
  const r=await fetch(GO,{method:'POST',body:'{}'});
  assert.equal(r.status,401);assert.equal(r.headers.get('x-request-id'),'r1');assert.equal(r.headers.get('content-length'),null);
  const text=await r.text();
  assert.doesNotMatch(text,/upstream.?request.?failed/i);
  assert.deepEqual(JSON.parse(text),{type:'error',error:{type:'api_error',message:'access rejected / no subscription: Invalid credential'}});
 }
 assert.equal(calls,2,'one request per call, no resend');
 assert.deepEqual(events.map(e=>e.message),['OpenCode Go: access rejected / no subscription','OpenCode Go: access rejected / no subscription']);
 const plain='{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}';
 const anthropic=providerFetch(async()=>new Response(plain,{status:401}),()=>{});
 assert.equal(await (await anthropic('https://api.anthropic.com/v1/messages',{method:'POST',body:'{}'})).text(),plain,'other rejections pass through unchanged');
});
