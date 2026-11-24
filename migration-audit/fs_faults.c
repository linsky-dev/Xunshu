/* Task-owned synthetic fault injection. Both actions require an exact path
 * under the explicitly approved tmpfs prefix; no real directories are used. */
#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

static int approved(const char *s) {
    const char *prefix="/tmp/xunshu-rust-validation-036c199d3807/rust-review-faults-";
    return s && strncmp(s,prefix,strlen(prefix))==0;
}
int fsync(int fd) {
    static int (*real_fsync)(int);
    if (!real_fsync) real_fsync=dlsym(RTLD_NEXT,"fsync");
    const char *fail=getenv("XS_REVIEW_FAIL_DIR_FSYNC");
    if (approved(fail)) {
        char proc[64],path[4096]; struct stat st;
        snprintf(proc,sizeof proc,"/proc/self/fd/%d",fd);
        ssize_t n=readlink(proc,path,sizeof(path)-1);
        if (n>=0) {path[n]=0;
            if (strcmp(path,fail)==0 && fstat(fd,&st)==0 && S_ISDIR(st.st_mode)) {
                errno=EIO;return -1;
            }
        }
    }
    return real_fsync(fd);
}
static void maybe_swap(const char *path,int flags) {
    static int done;
    const char *parent=getenv("XS_REVIEW_SWAP_PARENT");
    const char *saved=getenv("XS_REVIEW_SAVED_PARENT");
    const char *redirect=getenv("XS_REVIEW_REDIRECT_PARENT");
    if (!done && (flags&O_CREAT) && approved(parent) && approved(saved) && approved(redirect)
        && strncmp(path,parent,strlen(parent))==0 && strncmp(path+strlen(parent),"/generation-",12)==0) {
        done=1;
        if(rename(parent,saved)!=0 || symlink(redirect,parent)!=0) {
            fprintf(stderr,"review swap failed: %s\n",strerror(errno));
        } else {fprintf(stderr,"XS_REVIEW_SWAPPED_PARENT\n");}
    }
}
static void maybe_swap_at(int dirfd,const char *path,int flags) {
    static int done;
    const char *parent=getenv("XS_REVIEW_SWAP_PARENT");
    const char *saved=getenv("XS_REVIEW_SAVED_PARENT");
    const char *redirect=getenv("XS_REVIEW_REDIRECT_PARENT");
    if (done || !(flags&O_CREAT) || !approved(parent) || !approved(saved) || !approved(redirect)
        || (strncmp(path,".xs-write-",10)!=0 && strncmp(path,"generation-",11)!=0)) return;
    char proc[64],actual[4096];
    snprintf(proc,sizeof proc,"/proc/self/fd/%d",dirfd);
    ssize_t n=readlink(proc,actual,sizeof(actual)-1);
    if(n<0) return;
    actual[n]=0;
    if(strcmp(actual,parent)!=0) return;
    done=1;
    if(rename(parent,saved)!=0 || symlink(redirect,parent)!=0) {
        fprintf(stderr,"review openat swap failed: %s\n",strerror(errno));
    } else {fprintf(stderr,"XS_REVIEW_SWAPPED_PARENT_OPENAT\n");}
}
int open64(const char *path,int flags,...) {
    static int (*real_open64)(const char *,int,...);
    if(!real_open64) real_open64=dlsym(RTLD_NEXT,"open64");
    mode_t mode=0;if(flags&O_CREAT){va_list ap;va_start(ap,flags);mode=va_arg(ap,int);va_end(ap);}
    maybe_swap(path,flags);
    return real_open64(path,flags,mode);
}
int open(const char *path,int flags,...) {
    static int (*real_open)(const char *,int,...);
    if(!real_open) real_open=dlsym(RTLD_NEXT,"open");
    mode_t mode=0;if(flags&O_CREAT){va_list ap;va_start(ap,flags);mode=va_arg(ap,int);va_end(ap);}
    maybe_swap(path,flags);
    return real_open(path,flags,mode);
}
int openat(int dirfd,const char *path,int flags,...) {
    static int (*real_openat)(int,const char *,int,...);
    if(!real_openat) real_openat=dlsym(RTLD_NEXT,"openat");
    mode_t mode=0;if(flags&O_CREAT){va_list ap;va_start(ap,flags);mode=va_arg(ap,int);va_end(ap);}
    maybe_swap_at(dirfd,path,flags);
    return real_openat(dirfd,path,flags,mode);
}
int openat64(int dirfd,const char *path,int flags,...) {
    static int (*real_openat64)(int,const char *,int,...);
    if(!real_openat64) real_openat64=dlsym(RTLD_NEXT,"openat64");
    mode_t mode=0;if(flags&O_CREAT){va_list ap;va_start(ap,flags);mode=va_arg(ap,int);va_end(ap);}
    maybe_swap_at(dirfd,path,flags);
    return real_openat64(dirfd,path,flags,mode);
}
