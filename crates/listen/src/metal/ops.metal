// Heimdall FP16 graph primitives. Float accumulation; FP16 storage at MIL boundaries.
#include <metal_stdlib>
using namespace metal;
inline uint idx(uint t, constant int* p, int base) {
  int fast=p[26+(base-8)/6];if(fast>0)return t%uint(fast);
  uint o=0;
  for(int d=5;d>=0;--d){if(p[2+d]==1)continue;uint c=t%uint(p[2+d]);t/=uint(p[2+d]);o+=c*uint(p[base+d]);}
  return o;
}
kernel void element(device const half* a [[buffer(0)]], device const half* b [[buffer(1)]],
                    device const half* c [[buffer(2)]], device half* y [[buffer(3)]],
                    constant int* p [[buffer(4)]], uint t [[thread_position_in_grid]]) {
  if(t>=uint(p[0]))return;
  float x=float(a[idx(t,p,8)]),z=float(b[idx(t,p,14)]),r=0;
  switch(p[1]){
    case 0:r=x;break;case 1:r=x+z;break;case 2:r=x*z;break;case 3:r=floor(x);break;
    case 4:r=max(x,0.0f);break;case 5:r=x/(1.0f+exp(-x));break;case 6:r=1.0f/(1.0f+exp(-x));break;
    case 7:r=x<z;break;case 8:r=(x!=0)&&(z!=0);break;case 9:r=x==0;break;
    case 10:r=float(c[idx(t,p,20)])!=0?x:z;break;case 11:r=tanh(x);break;
    case 12:{float q=1.0f/(1.0f+0.3275911f*abs(x));float e=1.0f-(((((1.061405429f*q-1.453152027f)*q)+1.421413741f)*q-0.284496736f)*q+0.254829592f)*q*exp(-x*x);r=copysign(e,x);break;}
  }
  y[t]=half(r);
}
kernel void layout(device const half* x [[buffer(0)]], device half* y [[buffer(3)]],
                   constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
  if(t>=uint(p[0]))return;int off=p[32];uint v=t;bool valid=true;
  for(int d=5;d>=0;--d){if(p[2+d]==1&&p[20+d]==0)continue;int c=int(v%uint(p[2+d]))+p[20+d];v/=uint(p[2+d]);
    if(p[1]==2)c=c%p[8+d];
    if(c<0||c>=p[8+d])valid=false;
    off+=c*p[14+d];}
  y[t]=valid?x[off]:half(0);
}
kernel void norm(device const half* x [[buffer(0)]],device const half* g [[buffer(1)]],
                 device const half* b [[buffer(2)]],device half* y [[buffer(3)]],
                 constant int* p [[buffer(4)]],uint row [[threadgroup_position_in_grid]],
                 uint tid [[thread_index_in_threadgroup]],uint lane [[thread_index_in_simdgroup]],
                 uint sg [[simdgroup_index_in_threadgroup]]){
  const int C=p[1];threadgroup float red[8];float s=0;
  for(int i=tid;i<C;i+=256)s+=float(x[row*C+i]);s=simd_sum(s);if(lane==0)red[sg]=s;
  threadgroup_barrier(mem_flags::mem_threadgroup);s=tid<8?red[tid]:0;s=simd_sum(s);if(tid==0)red[0]=s/float(C);
  threadgroup_barrier(mem_flags::mem_threadgroup);float mean=red[0],v=0;
  // Every SIMD group must read the mean before reduction storage is reused.
  threadgroup_barrier(mem_flags::mem_threadgroup);
  for(int i=tid;i<C;i+=256){float d=float(x[row*C+i])-mean;v+=d*d;}
  v=simd_sum(v);if(lane==0)red[sg]=v;threadgroup_barrier(mem_flags::mem_threadgroup);
  v=tid<8?red[tid]:0;v=simd_sum(v);if(tid==0)red[0]=rsqrt(v/float(C)+as_type<float>(p[2]));
  threadgroup_barrier(mem_flags::mem_threadgroup);
  for(int i=tid;i<C;i+=256)y[row*C+i]=half((float(x[row*C+i])-mean)*red[0]*float(g[i])+float(b[i]));
}
kernel void softmax_rows(device const half* x [[buffer(0)]],device half* y [[buffer(3)]],
                         constant int* p [[buffer(4)]],uint row [[threadgroup_position_in_grid]],
                         uint tid [[thread_index_in_threadgroup]],uint lane [[thread_index_in_simdgroup]],
                         uint sg [[simdgroup_index_in_threadgroup]]){
 const int C=p[1];threadgroup float red[8];float m=-INFINITY;
 for(int i=tid;i<C;i+=256)m=max(m,float(x[row*C+i]));m=simd_max(m);if(lane==0)red[sg]=m;
 threadgroup_barrier(mem_flags::mem_threadgroup);m=tid<8?red[tid]:-INFINITY;m=simd_max(m);if(tid==0)red[0]=m;
 threadgroup_barrier(mem_flags::mem_threadgroup);m=red[0];float s=0;
 threadgroup_barrier(mem_flags::mem_threadgroup);
 for(int i=tid;i<C;i+=256)s+=exp(float(x[row*C+i])-m);s=simd_sum(s);if(lane==0)red[sg]=s;
 threadgroup_barrier(mem_flags::mem_threadgroup);s=tid<8?red[tid]:0;s=simd_sum(s);if(tid==0)red[0]=s;
 threadgroup_barrier(mem_flags::mem_threadgroup);
 for(int i=tid;i<C;i+=256)y[row*C+i]=half(exp(float(x[row*C+i])-m)/red[0]);
}
kernel void convolution(device const half* x [[buffer(0)]],device const half* w [[buffer(1)]],
                        device const half* b [[buffer(2)]],device half* y [[buffer(3)]],
                        constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t>=uint(p[0]))return;
 const int CI=p[1],HI=p[2],WI=p[3],CO=p[4],HO=p[5],WO=p[6],KH=p[7],KW=p[8],SH=p[9],SW=p[10],PH=p[11],PW=p[12],G=p[13];
 int xx=t%WO,yy=(t/WO)%HO,oc=(t/(WO*HO))%CO,bb=t/(WO*HO*CO);float s=float(b[oc]);int cg=CI/G;
 for(int ic=0;ic<cg;++ic)for(int h=0;h<KH;++h)for(int k=0;k<KW;++k){int ih=yy*SH-PH+h,iw=xx*SW-PW+k;
  if(ih>=0&&ih<HI&&iw>=0&&iw<WI){int cc=(oc/(CO/G))*cg+ic;s+=float(x[((bb*CI+cc)*HI+ih)*WI+iw])*float(w[((oc*cg+ic)*KH+h)*KW+k]);}}
 half v=half(s);if(p[14]==1)v=max(v,half(0));if(p[14]==2)v=half(float(v)/(1.0f+exp(-float(v))));y[t]=v;
}
kernel void matvec(device const half* x [[buffer(0)]],device const half* w [[buffer(1)]],
                    device const half* b [[buffer(2)]],device half* y [[buffer(3)]],constant int* p [[buffer(4)]],
                    uint row [[threadgroup_position_in_grid]],uint lane [[thread_index_in_simdgroup]]){
 float s=0;for(int k=lane;k<p[1];k+=32)s+=float(x[k])*float(w[row*p[1]+k]);s=simd_sum(s);
 if(lane==0)y[row]=half(s+ (p[3]?float(b[row]):0));
}

kernel void im2col(device const half* x [[buffer(0)]],device half* y [[buffer(3)]],
                    constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t>=uint(p[0]))return;
 int kp=p[1],CI=p[2],HI=p[3],WI=p[4],HO=p[5],WO=p[6],KH=p[7],KW=p[8],SH=p[9],SW=p[10],PH=p[11],PW=p[12];
 int k=t%kp,m=t/kp,xx=m%WO,yy=(m/WO)%HO,bb=m/(HO*WO),ic=k/(KH*KW),h=(k/KW)%KH,w=k%KW;
 int ih=yy*SH-PH+h,iw=xx*SW-PW+w;
 y[t]=(ic<CI&&ih>=0&&ih<HI&&iw>=0&&iw<WI)?x[((bb*CI+ic)*HI+ih)*WI+iw]:half(0);
}
kernel void sum_time(device const half* x [[buffer(0)]],device half* y [[buffer(3)]],
                      constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t>=uint(p[0]))return;float s=0;for(int i=0;i<p[1];++i)s+=float(x[i*p[0]+t]);y[t]=half(s);
}

kernel void embedding(device const half* x [[buffer(0)]],device const uint* token [[buffer(1)]],
                      device half* y [[buffer(3)]],constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t<uint(p[0]))y[t]=x[token[0]*p[0]+t];
}
kernel void lstm_project(device const half* x [[buffer(0)]],device const half* h [[buffer(1)]],
                        device const half* wi [[buffer(2)]],device const half* wh [[buffer(3)]],
                        device const half* b [[buffer(4)]],device float* y [[buffer(5)]],constant int* p [[buffer(6)]],
                        uint row [[threadgroup_position_in_grid]],uint lane [[thread_index_in_simdgroup]]){
 float a=0,c=0;for(int k=lane;k<p[0];k+=32){a+=float(x[k])*float(wi[row*p[0]+k]);c+=float(h[k])*float(wh[row*p[0]+k]);}
 a=simd_sum(a);c=simd_sum(c);if(lane==0)y[row]=a+c+float(b[row]);
}
kernel void lstm_cell(device const float* gates [[buffer(0)]],device const half* prev [[buffer(1)]],
                     device half* h [[buffer(2)]],device half* c [[buffer(3)]],constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t>=uint(p[0]))return;int H=p[0];
 float i=1/(1+exp(-gates[t])),f=1/(1+exp(-gates[H+t])),o=1/(1+exp(-gates[2*H+t])),g=tanh(gates[3*H+t]);
 float cell=f*float(prev[t])+i*g;c[t]=half(cell);h[t]=half(o*tanh(cell));
}
kernel void stack_two(device const half* x [[buffer(0)]],device const half* z [[buffer(1)]],
                      device half* y [[buffer(3)]],constant int* p [[buffer(4)]],uint t [[thread_position_in_grid]]){
 if(t<uint(p[0]))y[t]=t<uint(p[1])?x[t]:z[t-p[1]];
}

// Rust's TDT policy, replayed in bounded GPU blocks. State: frame, length,
// symbols-at-frame, emit, token, duration, record-index, nonfinite.
struct TdtRecord { uint token, frame, duration; float probability; };
kernel void tdt_gather(device const half* encoded [[buffer(0)]], device const uint* s [[buffer(1)]],
                       device half* step [[buffer(3)]], uint i [[thread_position_in_grid]]) {
 if(i<1024)step[i]=encoded[i*188+min(s[0],187u)];
}
kernel void tdt_decision(device const half* logits [[buffer(0)]],device uint* s [[buffer(1)]],
 device TdtRecord* records [[buffer(2)]],device uint* target [[buffer(3)]],
 device const uint* next [[buffer(4)]],device const float* bonus [[buffer(5)]],device uint* bias [[buffer(6)]],
 uint tid [[thread_index_in_threadgroup]],uint lane [[thread_index_in_simdgroup]],uint sg [[simdgroup_index_in_threadgroup]]) {
 threadgroup float maxima[8], sums[8], rawmax[8];threadgroup uint ids[8], bad[8];
 float best=-INFINITY,raw=-INFINITY;uint index=8193,invalid=0,row=(s[0]<=bias[1]+8?bias[0]:0u)*8193;
 for(uint i=tid;i<8198;i+=256){float v=float(logits[i]);invalid|=!isfinite(v);if(i<8193){raw=max(raw,v);v+=bonus[row+i];if(v>best || (v==best && i<index)){best=v;index=i;}}}
 float m=simd_max(best);uint ix=simd_min(best==m?index:8193u);invalid=simd_or(invalid);
 raw=simd_max(raw);if(lane==0){maxima[sg]=m;ids[sg]=ix;bad[sg]=invalid;rawmax[sg]=raw;}
 threadgroup_barrier(mem_flags::mem_threadgroup);
 m=simd_max(tid<8?maxima[tid]:-INFINITY);ix=simd_min(tid<8 && maxima[tid]==m?ids[tid]:8193u);
 raw=simd_max(tid<8?rawmax[tid]:-INFINITY);
 ix=min(ix,8192u);
 if(tid==0){rawmax[0]=raw;maxima[0]=m;ids[0]=float(logits[8192])>=raw && float(logits[ix])<raw && !(next[row+ix]&0x80000000u)?8192u:ix;}
 threadgroup_barrier(mem_flags::mem_threadgroup);m=rawmax[0];ix=ids[0];
 float sum=0;for(uint i=tid;i<8193;i+=256)sum+=exp(float(logits[i])-m);
 sum=simd_sum(sum);if(lane==0)sums[sg]=sum;
 threadgroup_barrier(mem_flags::mem_threadgroup);
 sum=simd_sum(tid<8?sums[tid]:0.f);
 if(tid==0){
  uint token=min(ix,8192u),duration=0;for(uint i=1;i<5;i++)if(logits[8193+i]>logits[8193+duration])duration=i;
  bool active=s[0]<s[1];uint frame=s[0];s[3]=active && token!=8192;
  for(uint i=0;i<8;i++)s[7]|=bad[i];
  float probability=float(half(exp(float(logits[token])-m)/sum));
  records[s[6]++]={active?token:8192u,frame,duration,probability};
  target[0]=token;s[4]=token;s[5]=duration;
  if(active){if(s[3]){s[2]++;bias[0]=next[row+token]&0x7fffffffu;bias[1]=frame;}uint advance=(token==8192 || s[2]>=10)?max(duration,1u):duration;if(advance){s[0]+=advance;s[2]=0;}}
 }
}
kernel void tdt_commit(device const uint* s [[buffer(0)]],device const half* h [[buffer(1)]],
 device const half* c [[buffer(2)]],device const half* pred [[buffer(3)]],device half* hi [[buffer(4)]],
 device half* ci [[buffer(5)]],device half* pi [[buffer(6)]],uint i [[thread_position_in_grid]]){
 if(!s[3])return;if(i<1280){hi[i]=h[i];ci[i]=c[i];}if(i<640)pi[i]=pred[i];
}
