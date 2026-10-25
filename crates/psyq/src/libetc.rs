//! libetc: VBlank timing, callbacks and the controllers.
//!
//! Time is virtual: the game's code takes no time, and each `VSync` wait advances a VBlank
//! counter by exactly the VBlanks it waits for, then hands the frame to the host (the app:
//! a window, or headless screenshots and replays). So a run is deterministic, and the pace
//! is the host's business (docs/phase2-plan.md, "Time").

use std::sync::Mutex;

/// What the app provides: frames out, controllers in.
pub trait Host: Send {
    /// A VBlank happened (`vblank` counts them since `ResetCallback`). The display is in
    /// `psyq::gpu` for the host to read.
    fn vblank(&mut self, vblank: u32);
    /// The controller state `PadRead` returns: pad 1 in bits 0-15, pad 2 in 16-31, a set bit
    /// for each button held, in libetc's bit order. `caller` is the return address into the
    /// game code that called `PadRead` (replays check it).
    fn pad(&mut self, caller: usize) -> u32;
    /// What `VSync(-1)` returns at native VBlank `native` (VBlanks since `ResetCallback`).
    /// A replay answers the PS1's count, which native loading (taking no time) leaves behind.
    fn vsync_count(&mut self, native: u32) -> u32 {
        native
    }
    /// Whether a loop polling the pad (`PadRead` again from the same caller within a VBlank)
    /// lets a VBlank pass before each poll, as the PS1's VBlank interrupt keeps updating the pad
    /// while the game spins (`handlePause` waits for Start that way, without `VSync`). A replay
    /// answers the recorded polls instead.
    fn polls_take_time(&self) -> bool {
        true
    }
}

struct Clock {
    /// VBlanks since `ResetCallback` (`VSync(-1)`).
    vblank: u32,
    /// The counter at the end of the last `VSync` wait.
    last_wait: u32,
    host: Option<Box<dyn Host>>,
}

static CLOCK: Mutex<Clock> = Mutex::new(Clock {
    vblank: 0,
    last_wait: 0,
    host: None,
});

/// Installs the host. Without one, VBlanks pass with nothing shown and the pads read 0.
pub fn set_host(host: Box<dyn Host>) {
    CLOCK.lock().unwrap().host = Some(host);
}

/// VBlanks since `ResetCallback`.
pub fn vblank_count() -> u32 {
    CLOCK.lock().unwrap().vblank
}

/// Horizontal blanks per NTSC field (263 lines, non-interlaced) [doc: psx-spx, GPU timings].
const HBLANKS_PER_FIELD: u32 = 263;

/// `ResetCallback()`: initialises the interrupt callbacks; the VBlank counter `VSync(-1)` reads
/// starts here [obs: VSync(-1) = 446 at Redux VBlank 1590, 446 VBlanks after main() called
/// ResetCallback]. The SDK calls it again internally (SsInit, CdInit); only the first call
/// restarts the count [guess: later calls during boot left the count running, per the same
/// observation]. Returns 0 [obs].
#[unsafe(no_mangle)]
pub extern "C" fn ResetCallback() -> i32 {
    static STARTED: Mutex<bool> = Mutex::new(false);
    let mut started = STARTED.lock().unwrap();
    if !*started {
        *started = true;
        let mut c = CLOCK.lock().unwrap();
        c.vblank = 0;
        c.last_wait = 0;
    }
    0
}

/// Runs VBlanks, host callback included, until the counter reaches `target`; returns how many
/// ran. The host runs without the lock held, so it may read the counter.
fn run_until(target: u32) -> u32 {
    let mut elapsed = 0;
    loop {
        let ending = {
            let c = CLOCK.lock().unwrap();
            if c.vblank >= target {
                return elapsed;
            }
            c.vblank
        };
        // The sound of the VBlank that's ending: the CD's XA sectors that have arrived, libsnd's
        // ticks, the SPU's samples.
        crate::libcd::vblank(ending);
        crate::libsnd::frame(ending);
        let (vblank, host) = {
            let mut c = CLOCK.lock().unwrap();
            c.vblank += 1;
            elapsed += 1;
            (c.vblank, c.host.take())
        };
        if let Some(mut host) = host {
            host.vblank(vblank);
            CLOCK.lock().unwrap().host = Some(host);
        }
    }
}

/// Lets virtual time pass until VBlank `target` (a count as in [`vblank_count`]), running each
/// VBlank's host callback, without a `VSync` wait: it doesn't move the point later
/// `VSync(n)` calls count from. Does nothing when `target` has passed. For things that happen
/// on the clock while the game polls instead of waiting, such as a streamed movie frame
/// arriving (libcd `StGetNext`, docs/phase2-plan.md "Time"). Returns the VBlanks that ran.
pub fn advance_to(target: u32) -> u32 {
    run_until(target)
}

/// `VSync(mode)`: `mode` < 0 returns the VBlank count; 0 waits for the next VBlank; `n` > 0
/// waits until `n` VBlanks have passed since the last wait [doc]. Here the game's code takes no
/// time, so a wait always lasts exactly 1 or `n` VBlanks. Returns the horizontal blanks
/// elapsed since the last wait [doc], counted as whole fields.
#[unsafe(no_mangle)]
pub extern "C" fn VSync(mode: i32) -> i32 {
    if mode < 0 {
        let native = vblank_count();
        let host = CLOCK.lock().unwrap().host.take();
        return match host {
            Some(mut host) => {
                let n = host.vsync_count(native);
                CLOCK.lock().unwrap().host = Some(host);
                n as i32
            }
            None => native as i32,
        };
    }
    let target = {
        let c = CLOCK.lock().unwrap();
        if mode == 0 {
            c.vblank + 1
        } else {
            c.last_wait.wrapping_add(mode as u32).max(c.vblank + 1)
        }
    };
    let elapsed = run_until(target);
    {
        let mut c = CLOCK.lock().unwrap();
        c.last_wait = c.vblank;
    }
    (elapsed * HBLANKS_PER_FIELD) as i32
}

/// `PadInit(mode)`: starts reading the controllers [doc]. Nothing to set up here.
#[unsafe(no_mangle)]
pub extern "C" fn PadInit(_mode: i32) {}

/// `PadRead` calls so far (a replay's poll sequence number of the next poll, once the current
/// one has returned).
static POLLS: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The VBlank of the latest `PadRead` call.
static LAST_POLL_VBLANK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// The game code that made the latest `PadRead` call.
static LAST_POLL_CALLER: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// The VBlank (as in [`vblank_count`]) of the latest `PadRead` call.
pub fn last_poll_vblank() -> u32 {
    LAST_POLL_VBLANK.load(std::sync::atomic::Ordering::Relaxed)
}

/// How many times the game has called `PadRead`, counting the one in progress.
pub fn polls() -> u32 {
    POLLS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `PadRead(id)`: the buttons held on both pads [doc: pad 1 in the low half, pad 2 in the
/// high half, 1 = pressed]. DW1 passes 0 or 1; both read both pads [doc].
#[unsafe(no_mangle)]
pub extern "C" fn PadRead(_id: i32) -> u32 {
    use std::sync::atomic::Ordering::Relaxed;
    let caller = caller_address();
    let now = vblank_count();
    let again = LAST_POLL_CALLER.swap(caller, Relaxed) == caller
        && LAST_POLL_VBLANK.load(Relaxed) == now
        && POLLS.load(Relaxed) > 0;
    if again && CLOCK.lock().unwrap().host.as_ref().is_some_and(|h| h.polls_take_time()) {
        run_until(now + 1);
    }
    let host = CLOCK.lock().unwrap().host.take();
    POLLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    LAST_POLL_VBLANK.store(vblank_count(), std::sync::atomic::Ordering::Relaxed);
    match host {
        Some(mut host) => {
            let pad = host.pad(caller);
            CLOCK.lock().unwrap().host = Some(host);
            pad
        }
        None => 0,
    }
}

/// The return address into whatever called this function's caller (for `PadRead`: the game
/// code), from the frame-pointer chain, which arm64 macOS always keeps.
#[inline(never)]
fn caller_address() -> usize {
    unsafe extern "C" {
        fn backtrace(buffer: *mut *mut std::ffi::c_void, size: i32) -> i32;
    }
    // Not inlined: frame [0] returns into this function, [1] into PadRead and [2] into
    // PadRead's caller (tests/pad_caller.rs).
    let mut frames = [std::ptr::null_mut(); 3];
    let n = unsafe { backtrace(frames.as_mut_ptr(), 3) };
    if n >= 3 { frames[2] as usize } else { 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    struct Recorder(Arc<Mutex<Vec<u32>>>);

    impl Host for Recorder {
        fn vblank(&mut self, vblank: u32) {
            self.0.lock().unwrap().push(vblank);
        }
        fn pad(&mut self, _caller: usize) -> u32 {
            0x0000_0800
        }
    }

    /// One test: the clock is global.
    #[test]
    fn vsync_advances_exact_vblanks_and_reports_them() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        set_host(Box::new(Recorder(seen.clone())));
        ResetCallback();
        assert_eq!(VSync(-1), 0);
        assert_eq!(VSync(0), 263);
        assert_eq!(VSync(3), 3 * 263); // 3 since the last wait, which ended at 1
        assert_eq!(VSync(-1), 4);
        assert_eq!(VSync(1), 263);
        assert_eq!(*seen.lock().unwrap(), [1, 2, 3, 4, 5]);
        assert_eq!(PadRead(0), 0x800);
        // A second ResetCallback (the SDK's own) doesn't restart the count.
        ResetCallback();
        assert_eq!(VSync(-1), 5);
        // Advancing the clock without a wait runs the VBlanks (host callbacks included), does
        // nothing for a time already past, and leaves `VSync(n)` counting from its last wait.
        assert_eq!(advance_to(8), 3);
        assert_eq!(advance_to(8), 0);
        assert_eq!(advance_to(3), 0);
        assert_eq!(*seen.lock().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            VSync(3),
            263,
            "3 since the last wait (5) had passed, so it waits one"
        );
        assert_eq!(VSync(-1), 9);
        // A loop polling the pad from one place lets a VBlank pass before each poll after the
        // first one in a VBlank (`handlePause`); polls from different places don't.
        // (through a pointer, as the game's C calls it: not inlined, so its caller is a call site)
        let read = std::hint::black_box(PadRead as extern "C" fn(i32) -> u32);
        #[inline(never)]
        fn poll(read: extern "C" fn(i32) -> u32) -> u32 {
            // (not a tail call either)
            std::hint::black_box(read(0))
        }
        for _ in 0..3 {
            poll(read);
        }
        assert_eq!(VSync(-1), 11);
        read(0);
        read(0);
        assert_eq!(VSync(-1), 11);
        assert_eq!(*seen.lock().unwrap(), [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]);
    }
}
