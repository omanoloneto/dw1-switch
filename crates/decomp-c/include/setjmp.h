/*
 * Native replacement for Psy-Q's <setjmp.h> (whose jmp_buf holds the 12 MIPS registers).
 *
 * The script interpreter (src/main/script_interp.c) uses setjmp/longjmp as a non-local exit;
 * the host's _setjmp/_longjmp do the same without touching the signal mask, which the PS1
 * doesn't have. The buffer is the host's (arm64 macOS: _JBLEN = 48 ints; checked against the
 * SDK's own <setjmp.h> by native/host_check.c; the Switch's newlib below).
 */
#ifndef DW1_NATIVE_SETJMP_H
#define DW1_NATIVE_SETJMP_H

#if defined(__APPLE__)

#define DW1_HOST_JBLEN 48
typedef int jmp_buf[DW1_HOST_JBLEN];

int _setjmp(jmp_buf) __attribute__((returns_twice));
void _longjmp(jmp_buf, int) __attribute__((noreturn));

#define setjmp(env) _setjmp(env)
#define longjmp(env, value) _longjmp(env, value)

#else

/* devkitA64's newlib (Switch): _JBLEN is 22 long longs on aarch64, and there's no signal mask. */
#define DW1_HOST_JBLEN 22
typedef int64_t jmp_buf[DW1_HOST_JBLEN];

int setjmp(jmp_buf) __attribute__((returns_twice));
void longjmp(jmp_buf, int) __attribute__((noreturn));

#endif

#endif
