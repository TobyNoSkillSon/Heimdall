import os,subprocess,time
# Both detach paths record their final pid before the supervisor stops Pi.
p=subprocess.Popen(['/bin/sh','-c','echo $$ > setsid.pid; exec sleep 60'],start_new_session=True,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
if os.fork()==0:
 os.setsid()
 if os.fork()==0:
  with open('double.pid','w') as f: f.write(str(os.getpid()))
  os.execl('/bin/sleep','sleep','60')
 os._exit(0)
time.sleep(60)
