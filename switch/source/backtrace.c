#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/*
 * newlib has no execinfo; psyq uses backtrace() to find PadRead's caller and to report stops.
 * Walking the frame pointers is enough: the build keeps them in all of the code.
 */
int backtrace(void **buffer, int size)
{
	void **frame = __builtin_frame_address(0);
	int n = 0;
	while (frame && n < size) {
		void **next = frame[0];
		buffer[n++] = frame[1];
		if (next <= frame || ((uintptr_t)next & 0xF) || (uintptr_t)next - (uintptr_t)frame > 0x100000)
			break;
		frame = next;
	}
	return n;
}

void backtrace_symbols_fd(void *const *buffer, int size, int fd)
{
	char line[32];
	for (int i = 0; i < size; i++) {
		int len = snprintf(line, sizeof(line), "  %p\n", buffer[i]);
		if (len > 0)
			write(fd, line, (size_t)len);
	}
}
