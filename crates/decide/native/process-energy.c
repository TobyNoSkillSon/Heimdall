// Agent-only, no privileges. SDK-sized structures avoid proc_pid_rusage overflow.
#include <libproc.h>
#include <mach/mach.h>
#include <sys/resource.h>
#include <stdint.h>
#include <unistd.h>
void heimdall_process_counters(uint64_t out[8]) {
    struct rusage_info_v6 r = {0};
    task_power_info_v2_data_t p = {0};
    mach_msg_type_number_t count = TASK_POWER_INFO_V2_COUNT;
    int rc = proc_pid_rusage(getpid(), RUSAGE_INFO_V6, (rusage_info_t *)&r);
    kern_return_t kr = task_info(mach_task_self(), TASK_POWER_INFO_V2, (task_info_t)&p, &count);
    out[0] = (uint64_t)rc;
    out[1] = r.ri_energy_nj;
    out[2] = r.ri_billed_energy;
    out[3] = r.ri_user_time + r.ri_system_time;
    out[4] = (uint64_t)kr;
    out[5] = p.task_energy;
    out[6] = p.gpu_energy.task_gpu_utilisation;
    out[7] = r.ri_phys_footprint;
}
