"""Agent-only synthetic English smoke audio; Apple's installed Samantha, no network."""
import json,pathlib,subprocess
crate=pathlib.Path(__file__).resolve().parents[1];target=crate.parents[1]/'models'/'decide'/'reference'/'smoke';target.mkdir(parents=True,exist_ok=True)
for item in json.loads((crate/'tests'/'fixtures'/'smoke.json').read_text()):
 p=target/(item['id']+'.wav')
 if not p.exists():subprocess.run(['say','-v','Samantha','-o',str(p),'--file-format=WAVE','--data-format=LEF32@16000',item['text']],check=True,timeout=20,capture_output=True)
print('30 local Samantha clips prepared; synthetic smoke, not real-world quality evidence')
