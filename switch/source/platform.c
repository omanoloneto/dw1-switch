#include <pthread.h>
#include <stdalign.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <netinet/in.h>

#include <SDL2/SDL.h>
#include <switch.h>

int dw1_switch_main(void);
int __real_pthread_create(pthread_t *thread, const pthread_attr_t *attr, void *(*start)(void *),
			  void *arg);

enum {
	PS_L2 = 1 << 0,
	PS_R2 = 1 << 1,
	PS_L1 = 1 << 2,
	PS_R1 = 1 << 3,
	PS_TRIANGLE = 1 << 4,
	PS_CIRCLE = 1 << 5,
	PS_CROSS = 1 << 6,
	PS_SQUARE = 1 << 7,
	PS_SELECT = 1 << 8,
	PS_START = 1 << 11,
	PS_UP = 1 << 12,
	PS_RIGHT = 1 << 13,
	PS_DOWN = 1 << 14,
	PS_LEFT = 1 << 15,
};

enum { SCREEN_W = 1280, SCREEN_H = 720 };

static SDL_Window *window;
static SDL_GLContext context;
static SDL_AudioDeviceID audio;
static Uint32 audio_max_queued;
static PadState pad;
static u64 held;

/* The window with an OpenGL 3.3 core context (mesa) current on this thread, which draws and shows
 * the picture (crates/switch/src/video.rs). */
int dw1_platform_init(uint32_t audio_hz)
{
	padConfigureInput(1, HidNpadStyleSet_NpadStandard);
	padInitializeDefault(&pad);

	if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_AUDIO) != 0)
		return -1;
	atexit(SDL_Quit);
	SDL_GL_SetAttribute(SDL_GL_CONTEXT_PROFILE_MASK, SDL_GL_CONTEXT_PROFILE_CORE);
	SDL_GL_SetAttribute(SDL_GL_CONTEXT_MAJOR_VERSION, 3);
	SDL_GL_SetAttribute(SDL_GL_CONTEXT_MINOR_VERSION, 3);
	SDL_GL_SetAttribute(SDL_GL_DEPTH_SIZE, 0);
	SDL_GL_SetAttribute(SDL_GL_STENCIL_SIZE, 0);
	window = SDL_CreateWindow("Digimon World", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED,
				  SCREEN_W, SCREEN_H, SDL_WINDOW_FULLSCREEN | SDL_WINDOW_OPENGL);
	if (!window)
		return -1;
	context = SDL_GL_CreateContext(window);
	if (!context)
		return -1;
	SDL_GL_SetSwapInterval(1);
	if (R_SUCCEEDED(apmInitialize())) {
		u32 handheld = 0, docked = 0;
		apmGetPerformanceConfiguration(ApmPerformanceMode_Normal, &handheld);
		apmGetPerformanceConfiguration(ApmPerformanceMode_Boost, &docked);
		printf("dw1: performance configuration %#x handheld, %#x docked\n", handheld, docked);
		apmExit();
	}

	SDL_AudioSpec want = {
		.freq = (int)audio_hz,
		.format = AUDIO_S16SYS,
		.channels = 2,
		.samples = 1024,
	};
	audio = SDL_OpenAudioDevice(NULL, 0, &want, NULL, 0);
	if (audio) {
		/* at most a quarter second queued: after a stall the sound catches up with the picture */
		audio_max_queued = audio_hz;
		SDL_PauseAudioDevice(audio, 0);
	}
	return 0;
}

const char *dw1_platform_error(void)
{
	return SDL_GetError();
}

void *dw1_platform_gl_proc(const char *name)
{
	return SDL_GL_GetProcAddress(name);
}

/* Shows what was drawn, at the next vsync. */
void dw1_platform_swap(void)
{
	SDL_GL_SwapWindow(window);
}

/* In the dock (the GPU's clocks are higher there). */
bool dw1_platform_docked(void)
{
	return appletGetOperationMode() == AppletOperationMode_Console;
}

/* Events, and the controllers' state for this VBlank: the PS1 also read its pads once a field. */
bool dw1_platform_pump(void)
{
	SDL_Event event;
	while (SDL_PollEvent(&event)) {
		if (event.type == SDL_QUIT)
			return false;
	}
	padUpdate(&pad);
	held = padGetButtons(&pad);
	return true;
}

uint64_t dw1_platform_buttons(void)
{
	return held;
}

/*
 * The system's boost mode (CPU at 1785 MHz instead of 1020, GPU at its minimum): for the HD
 * picture drawn on the CPU, when OpenGL can't. Only applications have it, so it fails quietly when
 * the homebrew runs as an applet.
 */
void dw1_platform_boost(bool on)
{
	appletSetCpuBoostMode(on ? ApmCpuBoostMode_FastLoad : ApmCpuBoostMode_Normal);
}

/*
 * A line of text as 0xAARRGGBB pixels, light on dark, in libnx's 16x16 console font, into `out`
 * (room for `cap` pixels). 0 with its size in `w` and `h`, or -1 when it doesn't fit.
 */
int dw1_platform_text(const char *text, uint32_t *out, size_t cap, int *w, int *h)
{
	enum { GLYPH = 16, PAD = 8, MAX_CHARS = 72 };
	const ConsoleFont *font = &consoleGetDefault()->font;
	int len = (int)strnlen(text, MAX_CHARS);
	*w = len * GLYPH + 2 * PAD;
	*h = GLYPH + 2 * PAD;
	if ((size_t)*w * (size_t)*h > cap)
		return -1;
	for (int i = 0; i < *w * *h; i++)
		out[i] = 0xFF10121A;
	for (int k = 0; k < len; k++) {
		int c = (unsigned char)text[k] - font->asciiOffset;
		if (c < 0 || c >= font->numChars)
			continue;
		const u16 *glyph = (const u16 *)font->gfx + GLYPH * c;
		for (int y = 0; y < GLYPH; y++) {
			for (int x = 0; x < GLYPH; x++) {
				if (glyph[y] & (0x8000 >> x))
					out[(PAD + y) * *w + PAD + k * GLYPH + x] = 0xFFEBEEF5;
			}
		}
	}
	return 0;
}

/* Buttons by position, as on the PS1 pad: the bottom one is Cross, the right one is Circle. */
uint32_t dw1_platform_pad(void)
{
	static const struct {
		u64 nx;
		uint32_t ps;
	} map[] = {
		{ HidNpadButton_B, PS_CROSS },
		{ HidNpadButton_A, PS_CIRCLE },
		{ HidNpadButton_Y, PS_SQUARE },
		{ HidNpadButton_X, PS_TRIANGLE },
		{ HidNpadButton_L, PS_L1 },
		{ HidNpadButton_R, PS_R1 },
		{ HidNpadButton_ZL, PS_L2 },
		{ HidNpadButton_ZR, PS_R2 },
		{ HidNpadButton_Minus, PS_SELECT },
		{ HidNpadButton_Plus, PS_START },
		{ HidNpadButton_Up | HidNpadButton_StickLUp, PS_UP },
		{ HidNpadButton_Down | HidNpadButton_StickLDown, PS_DOWN },
		{ HidNpadButton_Left | HidNpadButton_StickLLeft, PS_LEFT },
		{ HidNpadButton_Right | HidNpadButton_StickLRight, PS_RIGHT },
	};
	uint32_t bits = 0;
	for (size_t i = 0; i < sizeof(map) / sizeof(map[0]); i++) {
		if (held & map[i].nx)
			bits |= map[i].ps;
	}
	return bits;
}

void dw1_platform_audio(const int16_t *samples, size_t count)
{
	if (!audio)
		return;
	if (SDL_GetQueuedAudioSize(audio) > audio_max_queued)
		SDL_ClearQueuedAudio(audio);
	SDL_QueueAudio(audio, samples, (Uint32)(count * sizeof(int16_t)));
}

_Noreturn void dw1_platform_fatal(const char *message)
{
	ErrorApplicationConfig config;
	errorApplicationCreate(&config, "Digimon World can't continue.", message);
	errorApplicationShow(&config);
	exit(1);
}

/* A crash prints where it happened (offsets into dw1.elf, for addr2line) and ends the program. */
alignas(16) u8 __nx_exception_stack[0x4000];
u64 __nx_exception_stack_size = sizeof(__nx_exception_stack);
extern char __start__[];

void __libnx_exception_handler(ThreadExceptionDump *ctx)
{
	uintptr_t base = (uintptr_t)__start__;
	printf("dw1: crash (exception %u): pc %#lx lr %#lx far %#lx sp %#lx\n", ctx->error_desc,
	       ctx->pc.x - base, ctx->lr.x - base, ctx->far.x, ctx->sp.x);
	uint64_t *frame = (uint64_t *)ctx->fp.x;
	for (int i = 0; i < 16 && frame && ((uintptr_t)frame & 0xF) == 0; i++) {
		printf("dw1:   called from %#lx\n", frame[1] - base);
		frame = (uint64_t *)frame[0];
	}
	fflush(stdout);
	svcSleepThread(500000000);
	svcExitProcess();
}

/* The Mac's main thread has 8 MB of stack; the game gets as much on a thread of its own. */
enum { GAME_STACK_SIZE = 8 << 20 };

#define LOG_PATH "sdmc:/switch/dw1/dw1.log"

static int game_status;

static void *game_thread(void *arg)
{
	(void)arg;
	game_status = dw1_switch_main();
	return NULL;
}

int main(int argc, char **argv)
{
	(void)argc;
	(void)argv;
	/* started by nxlink, text output goes to the computer's terminal; otherwise to a log on the SD card */
	if (__nxlink_host.s_addr != 0 && R_SUCCEEDED(socketInitializeDefault())) {
		atexit(socketExit);
		nxlinkStdio();
	} else if (freopen(LOG_PATH, "w", stdout)) {
		dup2(fileno(stdout), STDERR_FILENO);
		setvbuf(stdout, NULL, _IOLBF, 0);
	}
	pthread_attr_t attr;
	pthread_t thread;
	pthread_attr_init(&attr);
	pthread_attr_setstacksize(&attr, GAME_STACK_SIZE);
	/* unwrapped (threads.c): the game stays on the default core, core 0 */
	if (__real_pthread_create(&thread, &attr, game_thread, NULL) != 0)
		dw1_platform_fatal("Couldn't start the game's thread.");
	pthread_join(thread, NULL);
	return game_status;
}
