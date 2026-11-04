// A generation's responsibility root stays alive after Pi/preparation exits.
// Protected Apple executables hide their environment from KERN_PROCARGS2; their
// inherited responsibility still identifies them through setsid and double fork.
#include <spawn.h>
#include <string.h>
#include <unistd.h>
#include <stdlib.h>
#include <stdio.h>
#include <fcntl.h>
#include <signal.h>
#include <sys/wait.h>
#include <errno.h>
#include <dlfcn.h>
#include <poll.h>
#include <sys/event.h>
extern char **environ;
typedef int (*disclaim_fn)(posix_spawnattr_t *, int);
static void record(const char *root,const char *name,int value) {
    char p[4096];snprintf(p,sizeof(p),"%s/%s",root,name);
    char pending[4096];snprintf(pending,sizeof(pending),"%s/%s.pending",root,name);
    int fd=open(pending,O_WRONLY|O_CREAT|O_EXCL,0600);if(fd<0)_exit(125);
    char b[32];int n=snprintf(b,sizeof(b),"%d\n",value);
    if(write(fd,b,n)!=n||(!getenv("HEIMDALL_EPHEMERAL_OWNER")&&fsync(fd)))_exit(125);close(fd);
    if(rename(pending,p))_exit(125);
}
int heimdall_guardian_main(int argc,char **argv) {
    if(argc<4)return 125;
    signal(SIGPIPE,SIG_IGN);
    // Re-exec the same signed Heimdall executable, preserving PID and pipes.
    // Attribution is optional: admission still gates execution and durably owns
    // the token, parent tree and birth identities if the private API disappears.
    int limited=0;
    if(strcmp(argv[1],"--heimdall-guardian-root")) {
        disclaim_fn disclaim=(disclaim_fn)dlsym(RTLD_DEFAULT,"responsibility_spawnattrs_setdisclaim");
#ifdef HEIMDALL_GUARDIAN_TEST_FAULTS
        const char *fault=getenv("HEIMDALL_TEST_RESPONSIBILITY_FAILURE");
        if(fault && !strcmp(fault,"missing"))disclaim=NULL;
#else
        const char *fault=NULL;
#endif
        posix_spawnattr_t attr;
        if(!disclaim)limited=1;
        else if(posix_spawnattr_init(&attr))limited=2;
        else {
            if((fault && !strcmp(fault,"attribute")) || disclaim(&attr,1))limited=2;
            else if(posix_spawnattr_setflags(&attr,POSIX_SPAWN_SETEXEC))limited=2;
            else {
                char *mode=argv[1];argv[1]="--heimdall-guardian-root";pid_t ignored;
                if(!(fault && !strcmp(fault,"spawn")))posix_spawn(&ignored,argv[0],NULL,&attr,argv,environ);
                argv[1]=mode;limited=3; // A successful SETEXEC never returns.
            }
            posix_spawnattr_destroy(&attr);
        }
    }
#ifdef HEIMDALL_GUARDIAN_TEST_FAULTS
    const char *delay=getenv("HEIMDALL_TEST_GUARDIAN_DELAY_MS");
    if(delay)usleep((useconds_t)(atoi(delay)*1000));
#endif
    record(argv[2],"responsibility.status",limited);
    const char *lifetime=getenv("HEIMDALL_SUPERVISOR_FD");
    if(!lifetime)return 125;
    int supervisor=atoi(lifetime);
    // Only Heimdall holds the other end. Pi/tools never inherit this channel.
    struct pollfd life={supervisor,POLLIN|POLLHUP,0};
    int ready[2];if(pipe(ready))return 125;
    pid_t child=fork();if(child<0)return 125;
    if(!child) {
        close(supervisor);unsetenv("HEIMDALL_SUPERVISOR_FD");
        const char *exit_fd=getenv("HEIMDALL_PI_EXIT_FD");if(exit_fd){close(atoi(exit_fd));unsetenv("HEIMDALL_PI_EXIT_FD");}
        close(ready[1]);char b;
        if(read(ready[0],&b,1)!=1)_exit(125);close(ready[0]);
        execvp(argv[3],argv+3);_exit(127);
    }
    close(ready[0]);
    // A direct, unreaped child cannot have its PID reused. Register NOTE_EXIT
    // while it is still blocked and before Rust can admit or cancel it.
    int queue=kqueue();if(queue<0)return 125;
    struct kevent changes[2];
    EV_SET(&changes[0],supervisor,EVFILT_READ,EV_ADD|EV_ENABLE,0,0,NULL);
    EV_SET(&changes[1],child,EVFILT_PROC,EV_ADD|EV_ENABLE,NOTE_EXIT,0,NULL);
    if(kevent(queue,changes,2,NULL,0,NULL)<0)return 125;
    record(argv[2],"child.pid",child);
    char admitted[4096];snprintf(admitted,sizeof(admitted),"%s/admitted",argv[2]);
    while(access(admitted,F_OK)) {
        if(poll(&life,1,10)>0){close(ready[1]);return 124;}
    }
    int released=write(ready[1],"1",1);close(ready[1]);
    // Immediate Off may kill the blocked child before release; retain ownership
    // even when its ready pipe is already closed. Never die from SIGPIPE.
    (void)released;
    // The inherited lock stays with Pi; supervisor EOF drives immediate cleanup.
    const char *lock=getenv("HEIMDALL_LOCK_FD");if(lock)close(atoi(lock));
    close(0);close(1);close(2);
    int status;
    for(;;) {
        pid_t exited=waitpid(child,&status,WNOHANG);
        if(exited==child)break;
        if(exited<0&&errno!=EINTR){status=125<<8;break;}
        struct kevent event;
        int n=kevent(queue,NULL,0,&event,1,NULL);
        if(n<0){if(errno==EINTR)continue;return 125;}
        if(n>0&&event.filter==EVFILT_READ)return 124;
    }
    record(argv[2],"exit.status",status);
    const char *exit_fd=getenv("HEIMDALL_PI_EXIT_FD");
    if(exit_fd){int fd=atoi(exit_fd);write(fd,"1",1);close(fd);}
    // Keep attribution until normal Rust cleanup or supervisor EOF. kqueue sleeps
    // in the kernel; EOF cannot spin and never requires another Heimdall launch.
    for(;;) {
        struct kevent event;
        int n=kevent(queue,NULL,0,&event,1,NULL);
        if(n<0){if(errno==EINTR)continue;return 125;}
        if(n>0&&event.filter==EVFILT_READ)return 124;
    }
}
