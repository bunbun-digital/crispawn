/* SPDX-License-Identifier: Apache-2.0 */
/* Isolated native-test process only. Never included in the LiveISO. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <fcntl.h>
#include <limits.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <unistd.h>
ssize_t write(int fd, const void *data, size_t count) {
    ssize_t (*real)(int,const void *,size_t) = dlsym(RTLD_NEXT,"write");
    const char *gate=getenv("CRISPAWN_TEST_LOG_GATE");
    if (gate) {
        char link[64],path[PATH_MAX],enabled[PATH_MAX],entered[PATH_MAX],release[PATH_MAX];
        snprintf(link,sizeof link,"/proc/self/fd/%d",fd);
        ssize_t n=readlink(link,path,sizeof path-1);
        if(n>0){
            path[n]=0;
            snprintf(enabled,sizeof enabled,"%s.enabled",gate);
            snprintf(entered,sizeof entered,"%s.entered",gate);
            snprintf(release,sizeof release,"%s.release",gate);
            if(strstr(path,"/continuous/events-") && access(enabled,F_OK)==0){
                int output=syscall(SYS_openat,AT_FDCWD,entered,O_CREAT|O_WRONLY,0600);
                if(output>=0)syscall(SYS_close,output);
                while(access(release,F_OK)!=0)usleep(10000);
            }
        }
    }
    return real(fd,data,count);
}
