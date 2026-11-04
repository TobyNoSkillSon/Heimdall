#!/usr/bin/python3
"""Strict local Pi RPC fake. No provider or browser is ever contacted."""
import sys, json, os, time
from pathlib import Path
if sys.argv[1:] == ['--version']:
    print('1.4.0'); sys.exit(0)
session = Path(sys.argv[sys.argv.index('--session')+1])
(session.parent/'mock-argv.json').write_text(json.dumps(sys.argv[1:]))
if (session.parent/'die-on-start').exists(): sys.exit(3)
entries = [json.loads(line) for line in session.read_text().splitlines() if line.strip()] if session.exists() else []
loop_pending = False
held_steers = []
held_consumed = []
held_tool = False
def emit(value): print(json.dumps(value), flush=True)
def response(v, data=None, success=True, error=None):
    emit({'type':'response','id':v.get('id'),'command':v['type'],'success':success,'data':data,'error':error})
def context_messages():
    active = session.parent/'active-messages.json'
    return json.loads(active.read_text()) if active.exists() else [e['message'] for e in entries if e['type']=='message']
def consume(v):
    # Production Pi appends coordinate notes when it resizes an input image.
    user={'role':'user','content':([{'type':'text','text':v['message']+'\n\n[Image: original 3456x2234, displayed at 2000x1293. Multiply coordinates by 1.73 to map to original image.]'}]+v['images']) if v.get('images') else v['message'],'timestamp':1}
    entry={'type':'message','id':v['id'],'parentId':None,'timestamp':'2026-10-14T10:00:00Z','message':user}
    entries.append(entry)
    if (session.parent/'active-messages.json').exists():
        (session.parent/'active-messages.json').write_text(json.dumps(context_messages()+[user]))
    with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush()
    emit({'type':'message_end','message':user})
for line in sys.stdin:
    v = json.loads(line)
    typ = v['type']
    if typ == 'get_state': response(v, {'model':{'id':'qwen3.8-27b'},'sessionFile':str(session),'sessionId':'fake-session','messageCount':len(context_messages()),'isStreaming':False,'isCompacting':False})
    elif typ == 'get_messages':
        (session.parent/'context-queried').touch()
        if (session.parent/'stall-context').exists(): time.sleep(12)
        if (session.parent/'compact-between-controls').exists():
            response(v, {'messages':context_messages()})
            (session.parent/'active-messages.json').write_text(json.dumps([{'role':'user','content':'Summary: earlier conversation.'} for _ in context_messages()]))
            emit({'type':'compaction_start','reason':'threshold'})
            emit({'type':'compaction_end','reason':'threshold','aborted':False,'willRetry':False})
            continue
        response(v, {'messages':context_messages()})
    elif typ == 'get_session_stats':
        # Knob: 'context-tokens' holds Pi's estimate; after a compaction the estimate is unknown, as in Pi.
        knob = session.parent/'context-tokens'
        tokens = None if (session.parent/'compacted').exists() else (int(knob.read_text()) if knob.exists() else 1000)
        response(v, {'contextUsage':{'tokens':tokens,'contextWindow':102400}})
    elif typ == 'get_commands':
        # Knob: 'no-context-command' stands for a runtime whose extension cannot add context.
        names = ['heimdall_bridge_status','heimdall_reload_note','heimdall_compaction_source','heimdall_compaction_supply'] + ([] if (session.parent/'no-context-command').exists() else ['heimdall_context'])
        response(v, {'commands':[{'name':n,'source':'extension'} for n in names]})
    elif typ == 'prompt' and v.get('message','').startswith('/heimdall_reload_note '):
        note = v['message'][len('/heimdall_reload_note '):]
        entry={'type':'custom_message','id':'reload-'+str(len(entries)),'parentId':entries[-1]['id'] if entries else None,'timestamp':'2026-10-18T10:00:00Z','customType':'heimdall_reload','content':note,'display':True}
        entries.append(entry)
        with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush()
        emit({'type':'mock_reload_note','note':note})
        response(v, {'disposition':'handled'})
    elif typ == 'prompt' and v.get('message','').startswith('/heimdall_compaction_source '):
        # The extension's source (runtime/compaction-fallback.mjs): what the failed compaction would summarise. Knobs:
        # 'compaction-source-tokens' (its estimate), 'context-tokens' (the whole history).
        knob = lambda n, d: int((session.parent/n).read_text()) if (session.parent/n).exists() else d
        src = {'firstKeptEntryId':entries[-1]['id'] if entries else 'none','previous':None,'tokensBefore':knob('context-tokens',1000),
               'tokens':knob('compaction-source-tokens',1000),'previousSummary':None,'focus':'keep decisions','conversation':'[User]: earlier words'}
        Path(v['message'][len('/heimdall_compaction_source '):]).write_text(json.dumps(src))
        response(v, {'disposition':'handled'})
    elif typ == 'prompt' and v.get('message','').startswith('/heimdall_compaction_supply '):
        supply = Path(v['message'][len('/heimdall_compaction_supply '):])
        (session.parent/'supplied.json').write_text(supply.read_text()); supply.unlink()
        response(v, {'disposition':'handled'})
    elif typ == 'prompt' and v.get('message','').startswith('/heimdall_context ') and (session.parent/'crash-on-context').exists():
        # Knob: Pi dies at each start's restart message, after it opened (night audit H3: restarts that keep failing).
        with (session.parent/'context-crashes').open('a') as f: f.write('x')
        os._exit(17)
    elif typ == 'prompt' and v.get('message','').startswith('/heimdall_context '):
        # The production extension's command: a context-only custom message in the same session, no turn.
        message={'role':'custom','customType':'heimdall_context','content':v['message'][len('/heimdall_context '):],'display':True,'timestamp':1}
        entry={'type':'custom_message','id':'ctx-'+str(len(entries)),'parentId':entries[-1]['id'] if entries else None,'timestamp':'2026-10-18T10:00:00Z','customType':'heimdall_context','content':message['content'],'display':True}
        entries.append(entry)
        with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush()
        emit({'type':'message_start','message':message}); emit({'type':'message_end','message':message})
        response(v, {'disposition':'handled'})
    elif typ == 'get_entries':
        since=v.get('since'); start=next((i+1 for i,e in enumerate(entries) if e['id']==since),0) if since else 0
        response(v, {'entries':entries[start:],'leafId':entries[-1]['id'] if entries else None})
    elif typ == 'get_fork_messages': response(v, {'messages':[{'entryId':e['id'],'text':e['message']['content']} for e in entries if e['type']=='message' and e['message']['role']=='user']})
    elif typ == 'fork':
        session = session.parent / 'forked.jsonl'
        session.write_text(''.join(json.dumps(e)+'\n' for e in entries)); response(v, {'cancelled':False})
    elif typ == 'compact' and (session.parent/'compaction-flood').exists():
        while True:
            emit({'type':'compaction_start','reason':'manual'}); time.sleep(.002)
    elif typ in ('abort','clear_queue','compact'):
        emit({'type':'mock_control','command':typ})
        supplied = session.parent/'supplied.json'
        if typ == 'compact' and supplied.exists():
            # A compaction Heimdall made (the extension's hook): Pi appends it as its own entry, no model call.
            s = json.loads(supplied.read_text()); supplied.unlink()
            if (session.parent/'supply-cancels').exists():
                # The extension's hook found the history changed since the source: Pi cancels, no model call.
                response(v, success=False, error='Compaction cancelled')
                continue
            summary = s.get('summary') or '[Heimdall: the older part of this conversation was cut without a summary]'
            entry = {'type':'compaction','id':'compaction-'+str(len(entries)),'parentId':entries[-1]['id'] if entries else None,'timestamp':'2026-10-28T17:28:00Z',
                     'summary':summary,'firstKeptEntryId':s.get('firstKeptEntryId') or (entries[-1]['id'] if entries else None),'tokensBefore':0,'fromHook':True,'details':{'heimdall':s.get('model','cut'),'keepTokens':s.get('keepTokens')}}
            entries.append(entry)
            with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush()
            (session.parent/'compacted').touch()
            emit({'type':'compaction_start','reason':'manual'})
            emit({'type':'compaction_end','reason':'manual','result':{'summary':summary,'firstKeptEntryId':entry['firstKeptEntryId'],'tokensBefore':0},'aborted':False,'willRetry':False})
            response(v, {'summary':summary})
            continue
        if typ == 'compact' and (session.parent/'compaction-fails').exists():
            # The provider cannot summarise (key rejected, offline): Pi answers the compact request with a failure. The
            # knob's text, when it has one, is the provider's error (28 Oct 17:27:55: a spent Anthropic credit).
            error = (session.parent/'compaction-fails').read_text().strip() or 'Compaction failed: Anthropic: key rejected'
            response(v, success=False, error=error)
            continue
        if typ == 'compact' and (session.parent/'compaction-crashes').exists():
            # Knob: Pi dies while it summarises (the harness's silence limit or a crash), the history not compacted.
            with (session.parent/'compaction-crashes-count').open('a') as f: f.write('x')
            emit({'type':'compaction_start','reason':'manual'})
            time.sleep(.05)
            os._exit(17)
        if typ == 'compact':
            (session.parent/'compacted').touch()
            hold = session.parent/'hold-compaction'
            if hold.exists():
                emit({'type':'compaction_start','reason':'manual'})
                while hold.exists(): time.sleep(.01)
        if typ == 'clear_queue':
            hold = session.parent/'hold-clear-queue'
            if hold.exists():
                (session.parent/'clearing-queue').touch()
                while hold.exists(): time.sleep(.01)
        if typ == 'abort' and loop_pending:
            emit({'type':'message_end','message':{'role':'assistant','content':[], 'stopReason':'aborted'}})
            emit({'type':'agent_settled','aborted':True})
            loop_pending = False
        if typ == 'abort' and held_consumed and not held_tool:
            # Like Pi: an abort while the model still thinks ends the run before the abort's response; the next
            # prompt starts a fresh run (Heimdall's rejoin, night pass H2).
            emit({'type':'message_end','message':{'role':'assistant','content':[], 'stopReason':'aborted'}})
            emit({'type':'agent_settled','aborted':True})
            held_steers = []; held_consumed = []
        response(v, {'steering':[],'followUp':[]})
    elif typ == 'prompt':
        hold = session.parent/'hold-turn-compaction'
        if hold.exists():
            # Pi's own compaction inside a turn, held running until the test removes the knob (Agent off waits for it).
            emit({'type':'compaction_start','reason':'threshold'})
            while hold.exists(): time.sleep(.01)
            emit({'type':'compaction_end','reason':'threshold','result':{'summary':'continuity','firstKeptEntryId':v['id']},'aborted':False,'willRetry':False})
        if (session.parent/'compact-on-prompt').exists():
            (session.parent/'compact-on-prompt').unlink()
            emit({'type':'compaction_start','reason':'threshold'})
            (session.parent/'active-messages.json').write_text(json.dumps([{'role':'user','content':'Summary: earlier conversation.'}]))
            emit({'type':'compaction_end','reason':'threshold','aborted':False,'willRetry':False})
        emit({'type':'mock_pi','request':v,'pid':os.getpid(),'args':sys.argv,'cwd':os.getcwd(),'agent_dir':os.environ.get('PI_CODING_AGENT_DIR')})
        scenario=v.get('message','').split('\n')[-1]
        if scenario == '[fixture:compaction-preflight]' and not (session.parent/'compaction-rejected').exists():
            (session.parent/'compaction-rejected').touch()
            response(v, success=False, error='Cannot submit a prompt while compaction is in progress. Wait for compaction to finish and retry.')
            emit({'type':'compaction_end','aborted':False,'willRetry':False})
            continue
        if scenario == '[fixture:crash]': os._exit(17)
        if scenario == '[fixture:garbage]': print('{invalid-json',flush=True);continue
        if scenario == '[fixture:oversized]': print('x'*(64*1024*1024+1),flush=True);continue
        if scenario == '[fixture:json-flood]':
            while True:
                emit({}); emit({'type':'unknown'}); emit({'type':'response','command':'prompt','id':'other','success':True});emit({'type':'tool_execution_start'}); time.sleep(0.002)
        if scenario == '[fixture:hang]': time.sleep(600);continue
        if held_consumed:
            # Pi acknowledges a steer now, but appends it only after the current
            # model/tool turn ends. Its default queue drains one message per turn.
            response(v, {'disposition':'steering'})
            held_steers.append(v)
            if scenario != '[fixture:steering-finish]': continue
            if held_tool:
                emit({'type':'tool_execution_end','toolCallId':'held-tool','toolName':'read','result':{'content':[{'type':'text','text':'done'}]},'isError':False})
            else:
                held_answer = {'role':'assistant','content':[{'type':'text','text':'initial answer'}],'stopReason':'stop'}
                emit({'type':'message_end','message':held_answer})
            emit({'type':'turn_end','message':held_answer,'toolResults':[]})
            for queued in held_steers:
                consume(queued)
                held_consumed.append(queued['id'])
                # Real Pi stamps every message, so two replies with the same words stay two replies.
                answer = {'role':'assistant','content':[{'type':'text','text':'one steered answer'}],'model':'qwen3.8-27b','stopReason':'stop','timestamp':100+len(held_consumed)}
                emit({'type':'message_end','message':answer})
                emit({'type':'turn_end','message':answer,'toolResults':[]})
            # The extension's before-settle hook receipts only messages actually
            # consumed above, after the last queued steer has had its model turn.
            for delivery in held_consumed:
                entry = {'type':'custom','customType':'heimdall_receipt','data':{'id':delivery,'completed':True,'effects':held_tool}}
                with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush(); os.fsync(f.fileno())
                emit({'type':'entry_appended','entry':entry})
            held_steers = []; held_consumed = []; held_tool = False
            emit({'type':'agent_settled','aborted':False})
            continue
        response(v, {'disposition':'started'})
        consume(v)
        # Knob: the turn is accepted, its reply waits while 'hold-reply' exists (a test's ack plays first).
        while (session.parent/'hold-reply').exists(): time.sleep(.01)
        if scenario == '[fixture:thinking]':
            # Still thinking when the user presses Stop: the turn ends only through abort (aborted settlement).
            emit({'type':'message_update','assistantMessageEvent':{'type':'thinking_delta','contentIndex':0,'delta':'hmm'}})
            loop_pending = True
            continue
        if scenario in ('[fixture:steering-thinking]', '[fixture:steering-tool]', '[fixture:steering-reply-streaming]', '[fixture:steering-toolcall-streaming]'):
            held_consumed.append(v['id'])
            if scenario == '[fixture:steering-tool]':
                held_tool = True
                held_answer = {'role':'assistant','content':[{'type':'toolCall','id':'held-tool','name':'read','arguments':{}}],'stopReason':'toolUse'}
                emit({'type':'message_end','message':held_answer})
                emit({'type':'tool_execution_start','toolCallId':'held-tool','toolName':'read','args':{}})
            else:
                emit({'type':'message_update','assistantMessageEvent':{'type':'thinking_delta','delta':'thinking'}})
            # The model starts writing right after it thought (night review 4): a reply, or a tool call's arguments.
            # Both events are written at once, so the bridge may still be reading the first when the user speaks.
            if scenario == '[fixture:steering-reply-streaming]':
                emit({'type':'message_update','assistantMessageEvent':{'type':'text_start','contentIndex':1}})
                emit({'type':'message_update','assistantMessageEvent':{'type':'text_delta','contentIndex':1,'delta':'The build'}})
            if scenario == '[fixture:steering-toolcall-streaming]':
                emit({'type':'message_update','assistantMessageEvent':{'type':'toolcall_start','contentIndex':1}})
            continue
        if scenario.startswith('[fixture:loop-'):
            import subprocess
            driver = subprocess.Popen(['node', str(Path(__file__).with_name('loop-guard-driver.mjs'))],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
            def hook(event):
                driver.stdin.write(json.dumps(event)+'\n'); driver.stdin.flush()
                reply = json.loads(driver.stdout.readline())
                for entry in reply['entries']:
                    entry['id'] = v['id'] + '-' + entry['id']
                    entries.append(entry)
                    emit({'type':'entry_appended','entry':entry})
                    with session.open('a') as f: f.write(json.dumps(entry)+'\n')
                return reply.get('decision')
            try:
                count = 12 if scenario == '[fixture:loop-poll]' else 8 if scenario == '[fixture:loop-abort]' else 6
                for i in range(count):
                    tool = 'mcp__heimdall__transcript_search' if i % 2 else 'read'
                    # One scenario interrupts the streak with another tool.
                    if scenario != '[fixture:loop-interrupted]': tool = 'read'
                    args = {'path':'same', 'nested': {'a':1,'b':2}} if i % 2 else {'nested':{'b':2,'a':1},'path':'same'}
                    call = {'type':'toolCall','id':f"{v['id']}-tool-{i}",'name':tool,'arguments':args}
                    emit({'type':'message_end','message':{'role':'assistant','content':[call],'stopReason':'toolUse'}})
                    emit({'type':'tool_execution_start','toolCallId':call['id'],'toolName':tool,'args':args})
                    decision = hook({'type':'tool_call','toolCallId':call['id'],'toolName':tool,'input':args})
                    result = {'content':[{'type':'text','text':decision['reason'] if decision else str(i) if scenario == '[fixture:loop-poll]' else 'unchanged'}], 'details':{'actionId':str(i)}}
                    emit({'type':'mock_tool_attempt','ran':not bool(decision),'decision':decision,'index':i})
                    event = {'type':'tool_execution_end','toolCallId':call['id'],'toolName':tool,'result':result,'isError':bool(decision)}
                    hook(event); emit(event)
                if scenario in ('[fixture:loop-abort]', '[fixture:loop-hold]'):
                    loop_pending = True
                    continue
            finally:
                driver.stdin.close(); driver.wait(timeout=3)
        if scenario == '[fixture:hold]':
            while not (session.parent/'release-turn').exists(): time.sleep(.01)
        if scenario == '[fixture:slow]':
            for _ in range(3): time.sleep(.05);emit({'type':'message_update','assistantMessageEvent':{'type':'text_delta','contentIndex':0,'delta':'x'}})
        if scenario == '[fixture:receipt-tool]':
            call={'type':'toolCall','id':'call_screen_frames','name':'mcp__heimdall__screen_frames','arguments':{}}
            emit({'type':'message_end','message':{'role':'assistant','content':[call],'stopReason':'toolUse'}})
            emit({'type':'tool_execution_start','toolCallId':call['id'],'toolName':call['name']})
            emit({'type':'tool_execution_end','toolCallId':call['id'],'toolName':call['name'],'result':{'content':[{'type':'image','data':'AQID','mimeType':'image/png'}]},'isError':False})
        if scenario == '[fixture:tool-error]':
            emit({'type':'tool_execution_start','toolCallId':'tool1','toolName':'write','args':{'path':'a'}})
            emit({'type':'tool_execution_end','toolCallId':'tool1','toolName':'write','result':{'content':[{'type':'text','text':'done'}]},'isError':False})
        if scenario == '[fixture:compaction]':
            emit({'type':'compaction_start','reason':'threshold'})
            emit({'type':'compaction_end','result':{'summary':'continuity','firstKeptEntryId':v['id']},'aborted':False,'willRetry':False})
        if scenario == '[fixture:reload-me]':
            import urllib.request
            url, token = os.environ['HEIMDALL_MCP_URL'], os.read(int(os.environ['HEIMDALL_MCP_TOKEN_FD']), 4096).decode()
            headers = {'Authorization':'Bearer '+token,'Content-Type':'application/json','Accept':'application/json, text/event-stream','MCP-Protocol-Version':'2025-06-18'}
            call_id = 'stable-fake-reload-call'
            emit({'type':'tool_execution_start','toolCallId':call_id,'toolName':'mcp__heimdall__reload_me','args':{'reason':'fake Pi integration'}})
            payload = {'jsonrpc':'2.0','id':1,'method':'tools/call','params':{'name':'reload_me','arguments':{'reason':'fake Pi integration'},'_meta':{'heimdallActionId':call_id}}}
            with urllib.request.urlopen(urllib.request.Request(url,json.dumps(payload).encode(),headers),timeout=3) as r:
                result = json.load(r)['result']
            emit({'type':'tool_execution_end','toolCallId':call_id,'toolName':'mcp__heimdall__reload_me','result':result,'isError':result.get('isError',False)})
            session.parent.mkdir(parents=True,exist_ok=True)
            with session.open('a') as f:
                f.write(json.dumps({'type':'message','id':call_id,'message':{'role':'toolResult','toolCallId':call_id,'content':result['content']}})+'\n'); f.flush(); os.fsync(f.fileno())
        if not scenario.startswith('[fixture:receipt-'): emit({'type':'agent_end','willRetry':True})
        if scenario != 'missing':
            message={'role':'assistant','content':[{'type':'text','text':'fixture reply'}], 'model':'qwen3.8-27b','stopReason':'error' if scenario in ('error','[fixture:503]','[fixture:tool-error]','[fixture:drop]') else 'aborted' if scenario=='aborted' else 'stop', 'errorMessage':'terminated' if scenario=='[fixture:drop]' else '503 queue full' if scenario in ('[fixture:503]','[fixture:tool-error]','[fixture:drop]') else 'fixture failure' if scenario=='error' else ''}
            # A spent plan quota on every request while the marker exists (emergency mode, 26 Oct 23:33).
            if (session.parent/'provider-quota').exists():
                message.update({'stopReason':'error','errorMessage':'The ChatGPT user has reached their Subscription Sharing usage limit. Ask the user to try again after their usage limit resets or use an API key instead.'})
            emit({'type':'message_end','message':message})
        if scenario.startswith('[fixture:receipt-'):
            entry={'type':'custom','customType':'heimdall_receipt','data':{'id':'unrelated' if scenario=='[fixture:receipt-wrong]' else v['id'],'completed':scenario!='[fixture:receipt-failed]','effects':True}}
            with session.open('a') as f: f.write(json.dumps(entry)+'\n'); f.flush(); os.fsync(f.fileno())
            emit({'type':'agent_end','willRetry':False})
            if scenario!='[fixture:receipt-durable]': emit({'type':'entry_appended','entry':entry})
        else:
            emit({'type':'agent_settled','aborted':scenario=='aborted'})
