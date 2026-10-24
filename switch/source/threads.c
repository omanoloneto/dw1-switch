#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdlib.h>

#include <switch.h>

/*
 * Horizon starts every thread on the process's default core, so the HD renderer's drawing threads
 * (Rust's std::thread) would all share core 0 with the game. The linker sends pthread_create here
 * (--wrap=pthread_create in the Makefile): each new thread moves to core 1 or 2 in turn before it
 * runs, and main() starts the game itself with __real_pthread_create, on core 0.
 */
int __real_pthread_create(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *),
			  void *arg);

struct pinned_start {
	void *(*start)(void *);
	void *arg;
	int core;
};

static void *start_pinned(void *p)
{
	struct pinned_start s = *(struct pinned_start *)p;
	free(p);
	svcSetThreadCoreMask(CUR_THREAD_HANDLE, s.core, BIT(s.core));
	return s.start(s.arg);
}

int __wrap_pthread_create(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *),
			  void *arg)
{
	static atomic_uint next;
	struct pinned_start *s = malloc(sizeof(*s));
	if (!s)
		return EAGAIN;
	s->start = start;
	s->arg = arg;
	s->core = 1 + (int)(atomic_fetch_add(&next, 1) % 2);
	int result = __real_pthread_create(thread, attr, start_pinned, s);
	if (result != 0)
		free(s);
	return result;
}
