#!/usr/bin/env python3
"""Agent-operated conversion of the private tuned ONNX to FP16 Metal data.
No private weight or derived plan is committed; ONNX and NumPy are conversion-only.
"""
import argparse,hashlib,json,pathlib
import numpy as np
import onnx
from onnx import numpy_helper,helper
p=argparse.ArgumentParser();p.add_argument('model',type=pathlib.Path);p.add_argument('--reference-dir',type=pathlib.Path);a=p.parse_args()
raw=a.model.read_bytes();model=onnx.shape_inference.infer_shapes(onnx.load(a.model),strict_mode=True,data_prop=True)
shapes={v.name:[d.dim_value for d in v.type.tensor_type.shape.dim] for v in list(model.graph.value_info)+list(model.graph.input)+list(model.graph.output)}
arrays={t.name:numpy_helper.to_array(t) for t in model.graph.initializer}
for n in model.graph.node:
 if n.op_type=='Constant':arrays[n.output[0]]=numpy_helper.to_array(n.attribute[0].t)
constants={};blob=bytearray();nodes=[]
def store(name,array):
 array=np.asarray(array);shape=list(array.shape);shapes[name]=shape
 if array.size<16:
  constants[name]={'dtype':'fp16','shape':shape,'values':array.reshape(-1).tolist()}
 else:
  offset=len(blob);blob.extend(array.astype('<f2').ownertes());constants[name]={'dtype':'fp16','shape':shape,'offset':offset,'raw':True}
for name,array in arrays.items():store(name,array)
def cfg(key,value):
 name=f'__cfg_{len(constants)}_{key}';store(name,np.asarray(value));return name
for n in model.graph.node:
 op=n.op_type;x=n.input;attrs={v.name:helper.get_attribute_value(v) for v in n.attribute};args={}
 if op=='Constant':continue
 if op in ['Add','Mul']:args={'x':x[0],'y':x[1]};op=op.lower()
 elif op=='Div':
  assert x[1] in arrays
  args={'x':x[0],'y':cfg('reciprocal',1/arrays[x[1]])};op='mul'
 elif op in ['Erf','Tanh','Sigmoid']:args={'x':x[0]};op=op.lower()
 elif op=='Transpose':args={'x':x[0],'perm':cfg('perm',attrs['perm'])};op='transpose'
 elif op=='Reshape':args={'x':x[0]};op='reshape'
 elif op=='LayerNormalization':
  assert attrs['axis']==-1
  args={'x':x[0],'gamma':x[1],'beta':x[2],'axes':cfg('axes',[-1]),'epsilon':cfg('epsilon',attrs['epsilon'])};op='layer_norm'
 elif op=='Softmax':args={'x':x[0],'axis':cfg('axis',attrs['axis'])};op='softmax'
 elif op=='MatMul' and x[1] in arrays:
  weight=x[1]+'__transposed';store(weight,arrays[x[1]].T);args={'x':x[0],'weight':weight};op='linear'
 elif op=='MatMul':args={'x':x[0],'y':x[1],'transpose_x':cfg('transpose_x',False),'transpose_y':cfg('transpose_y',False)};op='matmul'
 elif op=='Gemm':
  assert attrs.get('alpha',1)==1 and attrs.get('beta',1)==1 and attrs.get('transB')==1 and not attrs.get('transA',0)
  args={'x':x[0],'weight':x[1],'bias':x[2]};op='linear'
 elif op=='Conv':args={'x':x[0],'weight':x[1],'bias':x[2],'groups':cfg('groups',attrs.get('group',1)),'strides':cfg('strides',attrs['strides']),'pad':cfg('pad',attrs['pads']),'dilations':cfg('dilations',attrs['dilations'])};op='conv'
 elif op=='ReduceSum':args={'x':x[0],'axes':x[1]};op='reduce_sum'
 else:raise ValueError(op)
 nodes.append({'op':op,'args':args,'outputs':[{'name':v,'shape':shapes[v],'dtype':'fp16'} for v in n.output]})
# Discard unreferenced original MatMul operands; only the transposed representation is resident/stored.
used={v for n in nodes for v in n['args'].values()}
# Repack only live constants and config; avoid retaining duplicate weights in the derived file.
packed=bytearray()
for name in list(constants):
 if name not in used:del constants[name];continue
 c=constants[name]
 if c.get('raw'):
  start=c['offset'];count=int(np.prod(c['shape']));c['offset']=len(packed);packed.extend(blob[start:start+count*2])
plan={'mil_sha256':hashlib.sha256(raw).hexdigest(),'weights_sha256':hashlib.sha256(packed).hexdigest(),'shapes':shapes,'constants':constants,'nodes':nodes,'outputs':[v.name for v in model.graph.output]}
a.model.with_suffix('.metal.json').write_text(json.dumps(plan,separators=(',',':'))+'\n')
a.model.with_suffix('.metal.bin').write_bytes(packed)
print('Tuned turn',len(nodes),'nodes',len(packed),'weight bytes')

if a.reference_dir:
 a.reference_dir.mkdir(parents=True,exist_ok=True)
 names=[o['name'] for n in nodes for o in n['outputs']]
 values={v.name:v for v in [*model.graph.value_info,*model.graph.output]}
 if not (a.reference_dir/'features.bin').exists():
  del model.graph.output[:]
  for name in names:model.graph.output.append(values[name])
  onnx.save(model,a.reference_dir/'reference.onnx')
  (a.reference_dir/'outputs.json').write_text(json.dumps(names))
  (a.reference_dir/'origin.json').write_text(json.dumps({'onnx_sha256':hashlib.sha256(raw).hexdigest(),'onnx_version':onnx.__version__,'numpy_version':np.__version__}))
