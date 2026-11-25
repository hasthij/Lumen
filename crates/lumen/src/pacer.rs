//! Vsync pacer shared by the island and MacNotif overlays.
//!
//! While `fast` is set, a helper thread wakes the UI thread once per display refresh.
//! While it is clear the thread blocks on a condvar, so an idle app costs no wakeups.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// A flush that returns faster than this did not wait for vsync (DWM off, remote session).
const MIN_FLUSH: Duration = Duration::from_millis(2);
/// Sleep applied after such a flush so the thread cannot spin.
const FALLBACK_FRAME: Duration = Duration::from_millis(8);
/// How long an idle pacer sleeps before it re-checks that its window still exists.
const ALIVE_CHECK: Duration = Duration::from_secs(5);

pub struct Pacer {
    fast: AtomicBool,
    pending: AtomicBool,
    lock: Mutex<()>,
    wake: Condvar,
}

impl Pacer {
    pub const fn new() -> Self {
        Self { fast: AtomicBool::new(false), pending: AtomicBool::new(false), lock: Mutex::new(()), wake: Condvar::new() }
    }

    pub fn is_fast(&self) -> bool {
        self.fast.load(Ordering::Relaxed)
    }

    /// Turns per-refresh wakeups on or off; turning them on wakes a parked pacer thread.
    pub fn set_fast(&self, on: bool) {
        if self.fast.swap(on, Ordering::AcqRel) != on && on {
            // Taking the lock orders this against the waiter's check, so the wakeup cannot be lost.
            let _guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
            self.wake.notify_one();
        }
    }

    /// Call from the window procedure when the frame message arrives, to allow the next post.
    pub fn frame_done(&self) {
        self.pending.store(false, Ordering::Release);
    }

    /// Runs the pacer loop on the current thread until `alive` reports false.
    /// `flush` blocks until the next vsync; `post` sends the frame message and reports success.
    pub fn run(&self, flush: impl Fn(), alive: impl Fn() -> bool, post: impl Fn() -> bool) {
        loop {
            if !alive() {
                return;
            }
            if !self.is_fast() {
                let guard = self.lock.lock().unwrap_or_else(|e| e.into_inner());
                let _ = self.wake.wait_timeout_while(guard, ALIVE_CHECK, |_| !self.is_fast());
                continue;
            }
            let started = Instant::now();
            flush();
            if started.elapsed() < MIN_FLUSH {
                std::thread::sleep(FALLBACK_FRAME);
            }
            if !self.pending.swap(true, Ordering::AcqRel) && !post() {
                self.pending.store(false, Ordering::Release);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;

    fn spawn(p: &'static Pacer, posts: Arc<AtomicUsize>, alive: Arc<AtomicBool>) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            p.run(
                || std::thread::sleep(Duration::from_millis(4)),
                || alive.load(Ordering::Relaxed),
                || {
                    posts.fetch_add(1, Ordering::Relaxed);
                    p.frame_done();
                    true
                },
            )
        })
    }

    #[test]
    fn idle_pacer_posts_nothing() {
        static P: Pacer = Pacer::new();
        let (posts, alive) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(true)));
        let t = spawn(&P, posts.clone(), alive.clone());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(posts.load(Ordering::Relaxed), 0);
        alive.store(false, Ordering::Relaxed);
        P.set_fast(true);
        t.join().unwrap();
    }

    #[test]
    fn fast_wakes_parked_thread_and_off_parks_it_again() {
        static P: Pacer = Pacer::new();
        let (posts, alive) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(true)));
        let t = spawn(&P, posts.clone(), alive.clone());
        std::thread::sleep(Duration::from_millis(50));
        P.set_fast(true);
        std::thread::sleep(Duration::from_millis(100));
        assert!(posts.load(Ordering::Relaxed) >= 3, "pacer did not wake");
        P.set_fast(false);
        std::thread::sleep(Duration::from_millis(30));
        let parked = posts.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(posts.load(Ordering::Relaxed), parked, "pacer kept posting while idle");
        alive.store(false, Ordering::Relaxed);
        P.set_fast(true);
        t.join().unwrap();
    }

    #[test]
    fn pending_frame_blocks_further_posts_until_done() {
        static P: Pacer = Pacer::new();
        let posts = Arc::new(AtomicUsize::new(0));
        let alive = Arc::new(AtomicBool::new(true));
        let (p2, a2) = (posts.clone(), alive.clone());
        let t = std::thread::spawn(move || {
            P.run(|| std::thread::sleep(Duration::from_millis(4)), || a2.load(Ordering::Relaxed), || {
                p2.fetch_add(1, Ordering::Relaxed);
                true
            })
        });
        P.set_fast(true);
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(posts.load(Ordering::Relaxed), 1, "posted again before frame_done");
        P.frame_done();
        std::thread::sleep(Duration::from_millis(40));
        assert_eq!(posts.load(Ordering::Relaxed), 2);
        alive.store(false, Ordering::Relaxed);
        t.join().unwrap();
    }
}
