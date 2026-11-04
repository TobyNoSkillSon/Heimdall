#!/usr/bin/env python3
"""Reproduce the two independent port fixtures (agent use, macOS Swift + CPU Torch).
Only source code is downloaded. Pinned FluidAudio supplies cache logic; Torch supplies STFT.
"""
from pathlib import Path
import subprocess,urllib.request
ROOT=Path(__file__).resolve().parents[3];OUT=ROOT/'models/speakers/bakeoff'
REV='e560c6445ffdc42e60c7b282d162aae271f18e2a'
# Retain the original independent generators with the fixture provenance.
# The Swift source below embeds the public config/state definitions required by
# FluidAudio's updater, then uses eighty synthetic steps including cache overflow.
SWIFT_PRELUDE='import Foundation\nstruct AppLogger {init(category:String){};func error(_ text:String){}}\npublic struct SortformerConfig {\n let preEncoderDims=512,numSpeakers=4,spkcacheLen=188,fifoLen=40,spkcacheUpdatePeriod=31,spkcacheSilFramesPerSpk=3,maxIndex=99999\n let silenceThreshold:Float=0.2,predScoreThreshold:Float=0.25,scoresBoostLatest:Float=0.05,strongBoostRate:Float=0.75,weakBoostRate:Float=1.5,minPosScoresRate:Float=0.5\n}\npublic struct SortformerStreamingState {\n var spkcache:[Float]=[],fifo:[Float]=[],spkcachePreds:[Float]?=nil,fifoPreds:[Float]?=nil\n var spkcacheLength=0,fifoLength=0,silenceFrameCount=0\n var meanSilenceEmbedding=[Float](repeating:0,count:512)\n}\npublic enum SortformerError:Error {case insufficientPredsLength(String),insufficientChunkLength(String)}\npublic struct StreamingUpdateResult {\n var confirmed:[Float],tentative:[Float];var numSpeakers:Int\n}\n'
SWIFT_DRIVER='var state=SortformerStreamingState()\nlet updater=SortformerStateUpdater(config:SortformerConfig())\nfor step in 0..<80 {\n let lc=step==0 ? 0 : 1;let count=lc+13\n var emb:[Float]=[]\n for i in 0..<count {emb.append(contentsOf:[Float](repeating:sinf(Float(step*6+i)*0.07),count:512))}\n var preds:[Float]=[]\n for i in 0..<(state.spkcacheLength+state.fifoLength+count) {\n  for s in 0..<4 {let p:Float = i%11==0 ? 0.01 : (s == (i/15+step/20)%4 ? 0.7+0.299*Float((i*17+step*19)%997)/997 : 0.003+0.03*Float((i*13+step*23+s*11)%991)/991);preds.append(p)}\n }\n _ = try updater.streamingUpdate(state:&state,chunk:emb,preds:preds,leftContext:lc,rightContext:7)\n}\nlet result:[String:Any]=["cache":stride(from:0,to:state.spkcache.count,by:512).map{state.spkcache[$0]},"fifo":stride(from:0,to:state.fifo.count,by:512).map{state.fifo[$0]},"cache_preds":state.spkcachePreds!,"silence":state.meanSilenceEmbedding[0],"silence_count":state.silenceFrameCount]\nprint(String(data:try JSONSerialization.data(withJSONObject:result,options:[.sortedKeys]),encoding:.utf8)!)\n'
TORCH_DRIVER="import torch,numpy as np,json\n# Independent PyTorch centered STFT and Slaney filterbank, matching pinned FluidAudio frontend.\nx=torch.sin(torch.arange(16000,dtype=torch.float32)*.02)\nx=torch.cat([x[:1],x[1:]-.97*x[:-1]])\nw=torch.hann_window(400,periodic=False)\npower=torch.stft(x,n_fft=512,hop_length=160,win_length=400,window=w,center=True,pad_mode='constant',return_complex=True).abs().square()[:,:100]\ndef hzmel(h):return np.where(h<1000,h/(200/3),15+np.log(np.maximum(h,1)/1000)/(np.log(6.4)/27))\ndef melhz(m):return np.where(m<15,m*(200/3),1000*np.exp((m-15)*(np.log(6.4)/27)))\np=melhz(np.linspace(0,hzmel(np.array(8000)),130));freq=np.arange(257)*16000/512\nf=np.array([np.maximum(0,np.minimum((freq-a)/(b-a),(c-freq)/(c-b)))*2/(c-a) for a,b,c in zip(p,p[1:],p[2:])],dtype=np.float32)\ny=torch.log(torch.from_numpy(f)@power+2**-24).T\nrows=[dict(frame=t,values=y[t].tolist()) for t in [0,1,7,33,99]]\nopen('crates/speakers/tests/fixtures/sortformer-mel.json','w').write(json.dumps(rows))\n"
if __name__=='__main__':
 OUT.mkdir(parents=True,exist_ok=True)
 url=f'https://raw.githubusercontent.com/FluidInference/FluidAudio/{REV}/Sources/FluidAudio/Diarizer/Sortformer/SortformerStateUpdater.swift'
 updater=urllib.request.urlopen(url,timeout=45).read().decode()
 p=OUT/'state-fixture.swift';p.write_text(SWIFT_PRELUDE+updater+SWIFT_DRIVER)
 subprocess.run(['swiftc',str(p),'-o',str(OUT/'state-fixture')],check=True,timeout=120)
 fixture=ROOT/'crates/speakers/tests/fixtures/sortformer-state.json'
 with fixture.open('w') as f:subprocess.run([str(OUT/'state-fixture')],check=True,stdout=f,timeout=30)
 import os
 os.chdir(ROOT)
 exec(TORCH_DRIVER)
 print('reproduced Swift cache and PyTorch mel fixtures')
