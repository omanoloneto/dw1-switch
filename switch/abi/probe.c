/*
 * The C side of the ABI check (tools/check-switch-abi.sh): each value becomes the size of a
 * symbol (+1 so none is empty), read back with devkitA64's nm.
 */
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <stddef.h>
#include <sys/stat.h>
#include <sys/time.h>
#include <sys/types.h>
#include <time.h>
#include <unistd.h>

#define SIZE(name, type) char probe_size_##name[sizeof(type) + 1];
#define ALIGN(name, type) char probe_align_##name[_Alignof(type) + 1];
#define OFFSET(name, type, field) char probe_offset_##name##_##field[offsetof(type, field) + 1];
#define VALUE(name) char probe_value_##name[(name) + 1];

SIZE(dev_t, dev_t)
SIZE(ino_t, ino_t)
SIZE(off_t, off_t)
SIZE(mode_t, mode_t)
SIZE(nlink_t, nlink_t)
SIZE(uid_t, uid_t)
SIZE(gid_t, gid_t)
SIZE(blksize_t, blksize_t)
SIZE(blkcnt_t, blkcnt_t)
SIZE(time_t, time_t)
SIZE(clockid_t, clockid_t)
SIZE(suseconds_t, suseconds_t)
SIZE(pthread_t, pthread_t)
SIZE(pthread_key_t, pthread_key_t)

SIZE(timespec, struct timespec)
OFFSET(timespec, struct timespec, tv_nsec)
SIZE(timeval, struct timeval)

SIZE(stat, struct stat)
ALIGN(stat, struct stat)
OFFSET(stat, struct stat, st_ino)
OFFSET(stat, struct stat, st_mode)
OFFSET(stat, struct stat, st_nlink)
OFFSET(stat, struct stat, st_uid)
OFFSET(stat, struct stat, st_gid)
OFFSET(stat, struct stat, st_rdev)
OFFSET(stat, struct stat, st_size)
OFFSET(stat, struct stat, st_atim)
OFFSET(stat, struct stat, st_mtim)
OFFSET(stat, struct stat, st_ctim)
OFFSET(stat, struct stat, st_blksize)
OFFSET(stat, struct stat, st_blocks)

SIZE(dirent, struct dirent)
OFFSET(dirent, struct dirent, d_type)
OFFSET(dirent, struct dirent, d_name)

SIZE(pthread_attr_t, pthread_attr_t)
SIZE(pthread_mutex_t, pthread_mutex_t)
ALIGN(pthread_mutex_t, pthread_mutex_t)
SIZE(pthread_mutexattr_t, pthread_mutexattr_t)
SIZE(pthread_cond_t, pthread_cond_t)
ALIGN(pthread_cond_t, pthread_cond_t)
SIZE(pthread_condattr_t, pthread_condattr_t)
SIZE(pthread_rwlock_t, pthread_rwlock_t)
SIZE(pthread_rwlockattr_t, pthread_rwlockattr_t)

VALUE(O_RDONLY)
VALUE(O_WRONLY)
VALUE(O_RDWR)
VALUE(O_APPEND)
VALUE(O_CREAT)
VALUE(O_TRUNC)
VALUE(O_EXCL)
VALUE(O_CLOEXEC)
VALUE(O_NONBLOCK)
VALUE(S_IFMT)
VALUE(S_IFDIR)
VALUE(S_IFREG)
VALUE(S_IFLNK)
VALUE(F_GETFD)
VALUE(F_SETFD)
VALUE(F_GETFL)
VALUE(F_SETFL)
VALUE(FD_CLOEXEC)
VALUE(SEEK_SET)
VALUE(SEEK_CUR)
VALUE(SEEK_END)
VALUE(CLOCK_REALTIME)
VALUE(CLOCK_MONOTONIC)
VALUE(PTHREAD_MUTEX_NORMAL)
VALUE(PTHREAD_MUTEX_RECURSIVE)
VALUE(ENOENT)
VALUE(EINTR)
VALUE(EAGAIN)
VALUE(EEXIST)
VALUE(ENOTDIR)
VALUE(EISDIR)
VALUE(EINVAL)
VALUE(ENOSPC)
VALUE(ENOSYS)
VALUE(ENOTEMPTY)
VALUE(ETIMEDOUT)
VALUE(EBUSY)
VALUE(EACCES)
VALUE(EPERM)
VALUE(EXDEV)
VALUE(ERANGE)
VALUE(DT_DIR)
VALUE(DT_REG)
VALUE(DT_UNKNOWN)
VALUE(_SC_PAGESIZE)
VALUE(_SC_NPROCESSORS_ONLN)
