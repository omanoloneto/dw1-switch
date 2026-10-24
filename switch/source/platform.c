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
static SDL_Renderer *renderer;
static SDL_Texture *texture;
static int texture_w, texture_h;
static SDL_AudioDeviceID audio;
static Uint32 audio_max_queued;
static PadState pad;
static u64 held;
static SDL_Texture *note;
static int note_w, note_h;
static Uint32 note_until;

int dw1_platform_init(uint32_t audio_hz)
{
	padConfigureInput(1, HidNpadStyleSet_NpadStandard);
	padInitializeDefault(&pad);

	if (SDL_Init(SDL_INIT_VIDEO | SDL_INIT_AUDIO) != 0)
		return -1;
	atexit(SDL_Quit);
	window = SDL_CreateWindow("Digimon World", SDL_WINDOWPOS_CENTERED, SDL_WINDOWPOS_CENTERED,
				  SCREEN_W, SCREEN_H, SDL_WINDOW_FULLSCREEN);
	if (!window)
		return -1;
	renderer = SDL_CreateRenderer(window, -1, SDL_RENDERER_ACCELERATED | SDL_RENDERER_PRESENTVSYNC);
	if (!renderer)
		return -1;

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

/* A line of text in the picture's corner for two seconds, in libnx's 16x16 console font. */
void dw1_platform_note(const char *text)
{
	enum { GLYPH = 16, PAD = 8, MAX_CHARS = 72 };
	const ConsoleFont *font = &consoleGetDefault()->font;
	int len = (int)strnlen(text, MAX_CHARS);
	int w = len * GLYPH + 2 * PAD;
	int h = GLYPH + 2 * PAD;
	Uint32 *px = malloc((size_t)w * h * sizeof(Uint32));
	if (!px)
		return;
	for (int i = 0; i < w * h; i++)
		px[i] = 0xFF10121A;
	for (int k = 0; k < len; k++) {
		int c = (unsigned char)text[k] - font->asciiOffset;
		if (c < 0 || c >= font->numChars)
			continue;
		const u16 *glyph = (const u16 *)font->gfx + GLYPH * c;
		for (int y = 0; y < GLYPH; y++) {
			for (int x = 0; x < GLYPH; x++) {
				if (glyph[y] & (0x8000 >> x))
					px[(PAD + y) * w + PAD + k * GLYPH + x] = 0xFFEBEEF5;
			}
		}
	}
	if (note)
		SDL_DestroyTexture(note);
	note = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_ARGB8888, SDL_TEXTUREACCESS_STATIC, w, h);
	if (note)
		SDL_UpdateTexture(note, NULL, px, w * (int)sizeof(Uint32));
	free(px);
	note_w = w;
	note_h = h;
	note_until = SDL_GetTicks() + 2000;
}

/* The PS1's picture is always shown at 4:3, whatever the video mode's resolution. */
static SDL_Rect picture_rect(void)
{
	int h = SCREEN_H;
	int w = h * 4 / 3;
	return (SDL_Rect){ (SCREEN_W - w) / 2, 0, w, h };
}

static void ensure_texture(int w, int h)
{
	if (texture && texture_w == w && texture_h == h)
		return;
	if (texture)
		SDL_DestroyTexture(texture);
	texture = SDL_CreateTexture(renderer, SDL_PIXELFORMAT_RGB888, SDL_TEXTUREACCESS_STREAMING, w, h);
	texture_w = w;
	texture_h = h;
	SDL_Rect dst = picture_rect();
	bool whole = dst.w % w == 0 && dst.h % h == 0;
	SDL_SetTextureScaleMode(texture, whole ? SDL_ScaleModeNearest : SDL_ScaleModeLinear);
}

void dw1_platform_present(const uint32_t *pixels, int w, int h)
{
	SDL_SetRenderDrawColor(renderer, 0, 0, 0, 255);
	SDL_RenderClear(renderer);
	if (pixels) {
		ensure_texture(w, h);
		if (texture) {
			SDL_UpdateTexture(texture, NULL, pixels, w * (int)sizeof(uint32_t));
			SDL_Rect dst = picture_rect();
			SDL_RenderCopy(renderer, texture, NULL, &dst);
		}
	}
	if (note && !SDL_TICKS_PASSED(SDL_GetTicks(), note_until)) {
		SDL_Rect corner = { 24, 24, note_w, note_h };
		SDL_RenderCopy(renderer, note, NULL, &corner);
	}
	SDL_RenderPresent(renderer);
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
