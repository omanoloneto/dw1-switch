//! The Rust side of the ABI check: the symbols of `probe.c`, with the values the patched `libc`
//! crate gives.
#![no_std]
#![allow(non_upper_case_globals)]

use core::mem::{align_of, offset_of, size_of};

#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

macro_rules! size {
    ($name:ident, $t:ty) => {
        #[unsafe(no_mangle)]
        #[used]
        static $name: [u8; size_of::<$t>() + 1] = [0; size_of::<$t>() + 1];
    };
}

macro_rules! align {
    ($name:ident, $t:ty) => {
        #[unsafe(no_mangle)]
        #[used]
        static $name: [u8; align_of::<$t>() + 1] = [0; align_of::<$t>() + 1];
    };
}

macro_rules! offset {
    ($name:ident, $t:ty, $field:ident) => {
        #[unsafe(no_mangle)]
        #[used]
        static $name: [u8; offset_of!($t, $field) + 1] = [0; offset_of!($t, $field) + 1];
    };
}

macro_rules! value {
    ($name:ident, $v:expr) => {
        #[unsafe(no_mangle)]
        #[used]
        static $name: [u8; $v as usize + 1] = [0; $v as usize + 1];
    };
}

size!(probe_size_dev_t, libc::dev_t);
size!(probe_size_ino_t, libc::ino_t);
size!(probe_size_off_t, libc::off_t);
size!(probe_size_mode_t, libc::mode_t);
size!(probe_size_nlink_t, libc::nlink_t);
size!(probe_size_uid_t, libc::uid_t);
size!(probe_size_gid_t, libc::gid_t);
size!(probe_size_blksize_t, libc::blksize_t);
size!(probe_size_blkcnt_t, libc::blkcnt_t);
size!(probe_size_time_t, libc::time_t);
size!(probe_size_clockid_t, libc::clockid_t);
size!(probe_size_suseconds_t, libc::suseconds_t);
size!(probe_size_pthread_t, libc::pthread_t);
size!(probe_size_pthread_key_t, libc::pthread_key_t);

size!(probe_size_timespec, libc::timespec);
offset!(probe_offset_timespec_tv_nsec, libc::timespec, tv_nsec);
size!(probe_size_timeval, libc::timeval);

size!(probe_size_stat, libc::stat);
align!(probe_align_stat, libc::stat);
offset!(probe_offset_stat_st_ino, libc::stat, st_ino);
offset!(probe_offset_stat_st_mode, libc::stat, st_mode);
offset!(probe_offset_stat_st_nlink, libc::stat, st_nlink);
offset!(probe_offset_stat_st_uid, libc::stat, st_uid);
offset!(probe_offset_stat_st_gid, libc::stat, st_gid);
offset!(probe_offset_stat_st_rdev, libc::stat, st_rdev);
offset!(probe_offset_stat_st_size, libc::stat, st_size);
offset!(probe_offset_stat_st_atim, libc::stat, st_atim);
offset!(probe_offset_stat_st_mtim, libc::stat, st_mtim);
offset!(probe_offset_stat_st_ctim, libc::stat, st_ctim);
offset!(probe_offset_stat_st_blksize, libc::stat, st_blksize);
offset!(probe_offset_stat_st_blocks, libc::stat, st_blocks);

size!(probe_size_dirent, libc::dirent);
offset!(probe_offset_dirent_d_type, libc::dirent, d_type);
offset!(probe_offset_dirent_d_name, libc::dirent, d_name);

size!(probe_size_pthread_attr_t, libc::pthread_attr_t);
size!(probe_size_pthread_mutex_t, libc::pthread_mutex_t);
align!(probe_align_pthread_mutex_t, libc::pthread_mutex_t);
size!(probe_size_pthread_mutexattr_t, libc::pthread_mutexattr_t);
size!(probe_size_pthread_cond_t, libc::pthread_cond_t);
align!(probe_align_pthread_cond_t, libc::pthread_cond_t);
size!(probe_size_pthread_condattr_t, libc::pthread_condattr_t);
size!(probe_size_pthread_rwlock_t, libc::pthread_rwlock_t);
size!(probe_size_pthread_rwlockattr_t, libc::pthread_rwlockattr_t);

value!(probe_value_O_RDONLY, libc::O_RDONLY);
value!(probe_value_O_WRONLY, libc::O_WRONLY);
value!(probe_value_O_RDWR, libc::O_RDWR);
value!(probe_value_O_APPEND, libc::O_APPEND);
value!(probe_value_O_CREAT, libc::O_CREAT);
value!(probe_value_O_TRUNC, libc::O_TRUNC);
value!(probe_value_O_EXCL, libc::O_EXCL);
value!(probe_value_O_CLOEXEC, libc::O_CLOEXEC);
value!(probe_value_O_NONBLOCK, libc::O_NONBLOCK);
value!(probe_value_S_IFMT, libc::S_IFMT);
value!(probe_value_S_IFDIR, libc::S_IFDIR);
value!(probe_value_S_IFREG, libc::S_IFREG);
value!(probe_value_S_IFLNK, libc::S_IFLNK);
value!(probe_value_F_GETFD, libc::F_GETFD);
value!(probe_value_F_SETFD, libc::F_SETFD);
value!(probe_value_F_GETFL, libc::F_GETFL);
value!(probe_value_F_SETFL, libc::F_SETFL);
value!(probe_value_FD_CLOEXEC, libc::FD_CLOEXEC);
value!(probe_value_SEEK_SET, libc::SEEK_SET);
value!(probe_value_SEEK_CUR, libc::SEEK_CUR);
value!(probe_value_SEEK_END, libc::SEEK_END);
value!(probe_value_CLOCK_REALTIME, libc::CLOCK_REALTIME);
value!(probe_value_CLOCK_MONOTONIC, libc::CLOCK_MONOTONIC);
value!(probe_value_PTHREAD_MUTEX_NORMAL, libc::PTHREAD_MUTEX_NORMAL);
value!(probe_value_PTHREAD_MUTEX_RECURSIVE, libc::PTHREAD_MUTEX_RECURSIVE);
value!(probe_value_ENOENT, libc::ENOENT);
value!(probe_value_EINTR, libc::EINTR);
value!(probe_value_EAGAIN, libc::EAGAIN);
value!(probe_value_EEXIST, libc::EEXIST);
value!(probe_value_ENOTDIR, libc::ENOTDIR);
value!(probe_value_EISDIR, libc::EISDIR);
value!(probe_value_EINVAL, libc::EINVAL);
value!(probe_value_ENOSPC, libc::ENOSPC);
value!(probe_value_ENOSYS, libc::ENOSYS);
value!(probe_value_ENOTEMPTY, libc::ENOTEMPTY);
value!(probe_value_ETIMEDOUT, libc::ETIMEDOUT);
value!(probe_value_EBUSY, libc::EBUSY);
value!(probe_value_EACCES, libc::EACCES);
value!(probe_value_EPERM, libc::EPERM);
value!(probe_value_EXDEV, libc::EXDEV);
value!(probe_value_ERANGE, libc::ERANGE);
value!(probe_value_DT_DIR, libc::DT_DIR);
value!(probe_value_DT_REG, libc::DT_REG);
value!(probe_value_DT_UNKNOWN, libc::DT_UNKNOWN);
value!(probe_value__SC_PAGESIZE, libc::_SC_PAGESIZE);
value!(probe_value__SC_NPROCESSORS_ONLN, libc::_SC_NPROCESSORS_ONLN);
