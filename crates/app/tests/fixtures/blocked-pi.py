#!/usr/bin/python3
import sys,time,json,os
from pathlib import Path
p=Path.cwd()
while not (p/'release.marker').exists(): time.sleep(0.01)
line=globals().get('first',b'').decode()+sys.stdin.readline()
# Pi executes only complete JSON RPC frames. Pipe EOF after a cancelled
# partial write is evidence of cancellation, not receipt of a full prompt.
try:
    json.loads(line)
    complete=line.endswith('\n')
except json.JSONDecodeError:
    complete=False
if complete:
    (p/'received.json').write_text(json.dumps({'received_after_agent_off':(p/'off.marker').exists(),'prompt_bytes':len(line),'cloud':False}))
else:
    (p/'partial.json').write_text(json.dumps({'prompt_bytes':len(line),'cloud':False}))
for line in sys.stdin: pass
