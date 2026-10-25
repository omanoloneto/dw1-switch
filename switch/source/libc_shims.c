/*
 * The libc functions Rust's std calls that devkitA64's newlib doesn't provide.
 */
#include <errno.h>
#include <sys/types.h>
#include <unistd.h>

#include <switch.h>

ssize_t getrandom(void *buffer, size_t length, unsigned int flags)
{
	(void)flags;
	randomGet(buffer, length);
	return (ssize_t)length;
}

/* Not atomic, unlike POSIX's: the game reads the disc from one thread only. */
ssize_t pread(int fd, void *buffer, size_t count, off_t offset)
{
	off_t was = lseek(fd, 0, SEEK_CUR);
	if (was < 0 || lseek(fd, offset, SEEK_SET) < 0)
		return -1;
	ssize_t n = read(fd, buffer, count);
	int saved = errno;
	lseek(fd, was, SEEK_SET);
	errno = saved;
	return n;
}

long sysconf(int name)
{
	switch (name) {
	case _SC_PAGESIZE:
		return 0x1000;
	case _SC_NPROCESSORS_ONLN:
		/* applications get cores 0 to 2; the system keeps core 3 */
		return 3;
	default:
		errno = EINVAL;
		return -1;
	}
}
