/*
 * Force-included before every decomp C file in the native build (-include dw1_native.h).
 *
 * The decomp C is PS1 code: ILP32, with Psy-Q's headers and libc. This header keeps that
 * meaning on arm64 macOS (LP64) and points the PS1 runtime calls at crates/psyq.
 *
 * The directory holding this file comes first on the include path, so the headers next to it
 * (inline_n.h, setjmp.h, __rts_info_t__.h) replace the PS1-only ones from the decomp and
 * Psy-Q.
 */
#ifndef DW1_NATIVE_H
#define DW1_NATIVE_H

#define DW1_NATIVE 1

/*
 * DW1_PLUS: additions the native app makes to the game that the PS1 never had (the widescreen's
 * draw-only culling and the extra map tile column). The PS1 build never defines it, so the
 * oracle replays run the vanilla game; every addition is also off at run time unless the HD
 * widescreen picture is on (`dw1_plus_margin`).
 */
#define DW1_PLUS 1

/*
 * The host's fixed-width types, read before `long` is redefined below so that intptr_t,
 * uintptr_t, int64_t and friends keep their native widths. dw/types.h includes <stdint.h>
 * again later; its include guard makes that a no-op.
 */
#include <stdint.h>

/*
 * `long` is 32 bits on the PS1 and 64 on arm64 macOS. Every `long` in the game C and in
 * Psy-Q's headers (u_long, VECTOR, MATRIX.t, GPU packet words, SDK prototypes) means 32 bits,
 * so it's made 32 bits here (docs/phase1-report.md, "long"). `long long` isn't expressible
 * after this; the one game use is guarded by DW1_NATIVE in the decomp. Integer literals with
 * an L suffix stay 64-bit, which only widens comparisons (all sign-correct; checked in S0.5).
 */
#define long int

/*
 * Psy-Q and the decomp define NULL as a plain 0 (each behind #ifndef NULL). That's a 32-bit
 * int, which arm64 doesn't widen when it's passed through `...` or to an unprototyped
 * function expecting a pointer; a pointer constant is always the right width.
 */
#define NULL ((void *)0)

/*
 * Fixed PS1 RAM addresses (the decomp's DW1_PS1_ADDR, dw/types.h): buffers like
 * GENERAL_BUFFER at 0x80010000, assets inside overlay images, the overlays' load addresses.
 * They point into a 2 MB stand-in for PS1 RAM (crates/psyq/src/memory.rs), at the same
 * offset; still a constant expression, so static initializers keep working.
 */
extern uint8_t dw1_ps1_ram[];
#define DW1_PS1_ADDR(type, addr) ((type)(dw1_ps1_ram + ((uint32_t)(addr) & 0x1FFFFFu)))

/*
 * A mapped TMD record's address field (DW1_TMD_ADDR in the decomp, dw/types.h; the field
 * itself on the PS1): natively a 32-bit encoded pointer (crates/psyq/src/memory.rs, native
 * libgs.h), decoded to a native one.
 */
void *dw1_ptr32_decode(uint32_t value);
#define DW1_TMD_ADDR(field) dw1_ptr32_decode(field)

/* Stops the program with a message: a native-only limit the PS1 code doesn't have was hit
 * (crates/psyq/src/trap.rs). */
_Noreturn void dw1_native_fatal(const char *what);

/* The game is loading overlay `lib` (its Overlay number): its initialised data goes back to
 * the file's values, as the PS1's load of the file would do (crates/decomp-c/src/overlays.rs). */
void dw1_overlay_reset(int32_t lib);

/* Nonzero when the game's save screens use the Mac app's save slots instead of memory cards
 * (crates/psyq/src/native.rs; the decomp's main_menu.c, Phase 7). */
extern int32_t dw1_pc_saves;

/*
 * The HD picture's widescreen margin in VRAM pixels each side: 0 when it is off (no HD surfaces,
 * or F10 off), `backend::hd::WIDE_MARGIN` (53) when on (crates/psyq/src/native.rs, kept current
 * by `psyq::gpu::with`). Draw-only code widens its culling by it; the game's own visibility
 * flags, its simulation and the RNG are never touched.
 */
int32_t dw1_plus_margin(void);

/*
 * Nonzero when `renderDigimon` drew that entity (`Entity *`) in the last frame, the widescreen
 * margin included, and the margins are on. The animation script's sounds ask it, so a digimon
 * walking in a margin is heard like one at the 4:3 picture's edge (decomp main/anim.c).
 */
int32_t dw1_plus_wide_drawn(void *entity);

/*
 * The map tile pixels captured for the extra widescreen column: 128*128 bytes a tile, row-major,
 * in map order (decomp map.c's setupMap; crates/psyq/src/gpu.rs draws the tiles). The buffer must
 * stay valid for the process (it is a static array).
 */
void dw1_plus_set_tile_pixels(const uint8_t *pixels);

/* Nonzero: the GTE projections that follow place a 2D picture (HUD text, menu glyphs), so the
 * no-wobble option keeps their integer corners; zero: back to 3D (crates/psyq/src/gte.rs). */
void dw1_pgxp_2d(int32_t on);

/*
 * PS1 0x801A1398-0x801B1B97 as one object: TAMER_MODEL_BUFFER (0x7800), MAIN_D_801A8B98
 * (0x6800) and MAP_COLLISION_DATA (0x2800). The tamer's MMD fills the first two (it is larger
 * than its buffer), and getTileTrigger can index before the collision grid into its end, so the
 * PS1 relied on their adjacency; the files that use them refer to this span with their own
 * element types (decomp main.c, map_collision.c, map.c, model.c, std_setup.c, vs_scene.c).
 */
extern uint8_t dw1_tamer_collision_span[0x10800];
#define DW1_TAMER_MODEL_BUFFER (dw1_tamer_collision_span)
#define DW1_MAIN_D_801A8B98 (dw1_tamer_collision_span + 0x7800)
#define DW1_MAP_COLLISION_DATA (dw1_tamer_collision_span + 0xE000)
/* A collision-grid index that stays inside the span (beyond it the PS1 read STDVS_CAMERA). */
static inline int32_t dw1_collision_index(int32_t i)
{
	if (i < -0xE000 || i >= 0x2800) {
		dw1_native_fatal("a MAP_COLLISION_DATA index outside the PS1 span it shares with the tamer model");
	}
	return i;
}

/* How wide the US game's drawString draws ASCII `str` (the decomp's font.c). */
int32_t dw1_text_width(char *str);

/* BIOS A(2Fh)/A(30h), reached through the SDK's libc stubs on the PS1. */
#define rand dw1_bios_rand
#define srand dw1_bios_srand

/*
 * The rest of the PS1 libc the game calls (crates/psyq/src/libc.rs), renamed so it can't bind
 * to macOS's: Psy-Q's prototypes take 32-bit sizes, and the BIOS versions refuse NULLs.
 */
#define abs dw1_abs
#define bzero dw1_bzero
#define exit dw1_exit
#define memcpy dw1_memcpy
#define memset dw1_memset
#define printf dw1_printf
#define sprintf dw1_sprintf
#define strcat dw1_strcat
#define strcmp dw1_strcmp
#define strcpy dw1_strcpy
#define strlen dw1_strlen
#define strncpy dw1_strncpy
#define strrchr dw1_strrchr

/* The game's main() (main.c), which the app calls; Rust's binary owns the C `main`. */
#define main dw1_game_main

#endif
