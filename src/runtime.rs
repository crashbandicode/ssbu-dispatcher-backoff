//! Switch-only glue: version guard, site verification, one inline hook, and
//! an install log. Decision logic is in [`crate::policy`].
//!
//! Hook site: `main+0x354ab48` in Super Smash Bros. Ultimate 13.0.5, the
//! `ldp x8, x9, [x24, #0xe0]` that follows the sixth empty queue probe in the
//! task dispatcher (`main+0x354a6d0`), after that probe released its lock.
//! x24 is the dispatcher object. The game continues with its own
//! pending-record rescan after the hook returns.

use crate::policy::{decide, Policy, Snapshot, Streak};
use skyline::hooks::{getRegionAddress, InlineCtx, Region};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

const SITE: usize = 0x354ab48;
/// `ldp x8, x9, [x24, #0xe0]`; `cmp x8, x9`; `b.ne` to the pending rescan;
/// `ldr x0, [sp]`; `bl nn::os::WaitLightEvent`. Audited from the 13.0.5
/// executable. All five must match; a changed first word with the rest
/// intact means another plugin already owns this site.
const SITE_WORDS: [u32; 5] = [
    0xa94e_2708,
    0xeb09_011f,
    0x5400_0d01,
    0xf940_03e0,
    0x9411_f072,
];
const PENDING_BEGIN: usize = 0xe0;
const PENDING_END: usize = 0xe8;
const PENDING_STRIDE: usize = 0x20;
const PENDING_MAX: usize = 4096;
/// Ready-to-resume fiber vector `(begin, end)` dispatcher offsets.
const RESUME_VECTORS: [(usize, usize); 1] = [(0xf8, 0x100)];
const LOCAL_SIZES: [usize; 3] = [0x38, 0x68, 0x98];
const SHARED_SIZES: [usize; 3] = [0x28, 0x58, 0x88];
const CONTEXT_ENABLED: usize = 0x58;
const DISPATCHER_SPAN: usize = 0x158;
const CONTEXT_SPAN: usize = 0x60;
const SHARED_SPAN: usize = 0xd0;
/// `nn::os::ThreadType + 0x1a0` holds the thread-name pointer.
const THREAD_NAME_POINTER: usize = 0x1a0;
const MAIN_THREAD_NAME: &[u8] = b"MainThread\0";
const SLOTS: usize = 8;
const DATA_DIR: &str = "sd:/ultimate/ssbu_dispatcher_backoff";

static INSTALLED: AtomicBool = AtomicBool::new(false);
static SPIN_PASSES: AtomicU32 = AtomicU32::new(Policy::DEFAULT.spin_passes);
static EVERY_PASSES: AtomicU32 = AtomicU32::new(Policy::DEFAULT.every_passes);
static BASE_NS: AtomicU32 = AtomicU32::new(Policy::DEFAULT.base_ns);
static MAX_NS: AtomicU32 = AtomicU32::new(Policy::DEFAULT.max_ns);
static COOLDOWN_TICKS: AtomicU64 = AtomicU64::new(0);
static MAIN_THREAD_ONLY: AtomicBool = AtomicBool::new(Policy::DEFAULT.main_thread_only);
static MAIN_THREAD: AtomicUsize = AtomicUsize::new(0);
static IDLE_PASSES: AtomicU64 = AtomicU64::new(0);
static SLEEPS: AtomicU64 = AtomicU64::new(0);
static COOLDOWN_SKIPS: AtomicU64 = AtomicU64::new(0);
static LAST_NS: AtomicU32 = AtomicU32::new(0);
static LAST_SLEEP_END: AtomicU64 = AtomicU64::new(0);
static KEYS: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];
static STREAKS: [AtomicU64; SLOTS] = [const { AtomicU64::new(0) }; SLOTS];

fn policy() -> Policy {
    Policy {
        spin_passes: SPIN_PASSES.load(Ordering::Relaxed),
        every_passes: EVERY_PASSES.load(Ordering::Relaxed),
        base_ns: BASE_NS.load(Ordering::Relaxed),
        max_ns: MAX_NS.load(Ordering::Relaxed),
        cooldown_us: 0,
        main_thread_only: MAIN_THREAD_ONLY.load(Ordering::Relaxed),
    }
}

fn store_policy(policy: Policy) {
    SPIN_PASSES.store(policy.spin_passes, Ordering::Relaxed);
    EVERY_PASSES.store(policy.every_passes, Ordering::Relaxed);
    BASE_NS.store(policy.base_ns, Ordering::Relaxed);
    MAX_NS.store(policy.max_ns, Ordering::Relaxed);
    COOLDOWN_TICKS.store(policy.cooldown_ticks(), Ordering::Relaxed);
    MAIN_THREAD_ONLY.store(policy.main_thread_only, Ordering::Relaxed);
}

fn load_policy() -> (Policy, bool) {
    let path = std::path::Path::new(DATA_DIR).join("config.ini");
    match std::fs::read_to_string(path) {
        Ok(text) => (Policy::parse(&text, Policy::DEFAULT), true),
        Err(_) => (Policy::DEFAULT, false),
    }
}

/// Horizon `QueryMemory` (svc 0x6): readable mapping covering `[address, address+size)`.
unsafe fn readable(address: usize, size: usize) -> bool {
    #[repr(C)]
    #[derive(Default)]
    struct MemoryInfo {
        address: u64,
        size: u64,
        kind: u32,
        attributes: u32,
        permission: u32,
        ipc_references: u32,
        device_references: u32,
        padding: u32,
    }
    let Some(end) = address.checked_add(size) else {
        return false;
    };
    if address < 0x10000 || size == 0 {
        return false;
    }
    let mut info = MemoryInfo::default();
    let result: u64;
    core::arch::asm!(
        "svc 0x6",
        inout("x0") &mut info as *mut MemoryInfo as u64 => result,
        lateout("x1") _,
        in("x2") address as u64,
        clobber_abi("C"),
        options(nostack),
    );
    result as u32 == 0
        && info.permission & 1 == 1
        && info.address <= address as u64
        && info
            .address
            .checked_add(info.size)
            .is_some_and(|mapped_end| end as u64 <= mapped_end)
}

/// The calling thread is the SDK `MainThread`. Its `ThreadType` pointer is
/// resolved once by bounded name comparison and then compared directly.
unsafe fn on_main_thread() -> bool {
    let thread = skyline::nn::os::GetCurrentThread() as usize;
    if thread == 0 {
        return false;
    }
    let known = MAIN_THREAD.load(Ordering::Relaxed);
    if known != 0 {
        return known == thread;
    }
    if !readable(thread + THREAD_NAME_POINTER, 8) {
        return false;
    }
    let name = *((thread + THREAD_NAME_POINTER) as *const usize);
    if name == 0 || !readable(name, MAIN_THREAD_NAME.len()) {
        return false;
    }
    if core::slice::from_raw_parts(name as *const u8, MAIN_THREAD_NAME.len()) != MAIN_THREAD_NAME {
        return false;
    }
    MAIN_THREAD.store(thread, Ordering::Relaxed);
    true
}

unsafe fn validated(dispatcher: usize) -> bool {
    if !dispatcher.is_multiple_of(8) || !readable(dispatcher, DISPATCHER_SPAN) {
        return false;
    }
    let context = *(dispatcher as *const usize);
    if !context.is_multiple_of(8) || !readable(context, CONTEXT_SPAN) {
        return false;
    }
    let shared = *((context + 8) as *const usize);
    shared.is_multiple_of(8) && readable(shared, SHARED_SPAN)
}

unsafe fn slot(dispatcher: usize) -> Option<usize> {
    for (index, key) in KEYS.iter().enumerate() {
        let current = key.load(Ordering::Relaxed);
        if current == dispatcher {
            return Some(index);
        }
        if current != 0 {
            continue;
        }
        if !validated(dispatcher) {
            return None;
        }
        return match key.compare_exchange(0, dispatcher, Ordering::AcqRel, Ordering::Relaxed) {
            Ok(_) => Some(index),
            Err(existing) if existing == dispatcher => Some(index),
            Err(_) => None,
        };
    }
    None
}

unsafe fn snapshot(dispatcher: usize) -> Option<Snapshot> {
    let read = |address: usize| (address as *const u64).read_volatile();
    let context = read(dispatcher) as usize;
    let shared = read(context + 8) as usize;
    let enabled = ((context + CONTEXT_ENABLED) as *const u8).read_volatile() & 1 != 0;
    let mut queue_sizes = [0u64; 6];
    for (slot, offset) in queue_sizes.iter_mut().zip(LOCAL_SIZES) {
        *slot = read(dispatcher + offset);
    }
    for (slot, offset) in queue_sizes[3..].iter_mut().zip(SHARED_SIZES) {
        *slot = read(shared + offset);
    }
    let begin = read(dispatcher + PENDING_BEGIN) as usize;
    let end = read(dispatcher + PENDING_END) as usize;
    if begin == 0
        || end < begin
        || !begin.is_multiple_of(8)
        || !(end - begin).is_multiple_of(PENDING_STRIDE)
    {
        return None;
    }
    let pending_records = (end - begin) / PENDING_STRIDE;
    if pending_records > PENDING_MAX {
        return None;
    }
    let mut all_pending_blocked = true;
    let mut record_address = begin;
    while record_address < end {
        let counter = read(record_address + 8) as usize;
        if counter < 0x10000 || !counter.is_multiple_of(4) {
            return None;
        }
        if (counter as *const u32).read_volatile() == 0 {
            all_pending_blocked = false;
            break;
        }
        record_address += PENDING_STRIDE;
    }
    let resume_vectors_empty = RESUME_VECTORS
        .iter()
        .all(|&(first, last)| read(dispatcher + first) == read(dispatcher + last));
    Some(Snapshot {
        enabled,
        queue_sizes,
        pending_records,
        all_pending_blocked,
        resume_vectors_empty,
    })
}

/// Integer-only; no allocation, file or log access; no lock is held here.
#[skyline::hook(offset = SITE, inline)]
unsafe fn dispatcher_idle_pass(ctx: &InlineCtx) {
    if MAIN_THREAD_ONLY.load(Ordering::Relaxed) && !on_main_thread() {
        return;
    }
    let dispatcher = ctx.registers[24].x() as usize;
    let Some(index) = slot(dispatcher) else {
        return;
    };
    let Some(snapshot) = snapshot(dispatcher) else {
        return;
    };
    let packed = STREAKS[index].load(Ordering::Relaxed);
    let mut streak = Streak {
        idle_passes: packed as u32,
        last_ns: (packed >> 32) as u32,
    };
    let decision = decide(policy(), &snapshot, &mut streak);
    STREAKS[index].store(
        u64::from(streak.idle_passes) | (u64::from(streak.last_ns) << 32),
        Ordering::Relaxed,
    );
    if streak.idle_passes == 1 {
        IDLE_PASSES.fetch_add(1, Ordering::Relaxed);
    }
    let Ok(nanoseconds) = decision else {
        return;
    };
    let cooldown = COOLDOWN_TICKS.load(Ordering::Relaxed);
    if cooldown != 0 {
        let now = skyline::nn::os::GetSystemTick() as u64;
        if now.wrapping_sub(LAST_SLEEP_END.load(Ordering::Relaxed)) < cooldown {
            COOLDOWN_SKIPS.fetch_add(1, Ordering::Relaxed);
            return;
        }
    }
    SLEEPS.fetch_add(1, Ordering::Relaxed);
    LAST_NS.store(nanoseconds, Ordering::Relaxed);
    skyline::nn::os::SleepThread(skyline::nn::TimeSpan {
        nanoseconds: u64::from(nanoseconds),
    });
    if cooldown != 0 {
        LAST_SLEEP_END.store(skyline::nn::os::GetSystemTick() as u64, Ordering::Relaxed);
    }
}

fn display_version() -> String {
    let mut version = skyline::nn::oe::DisplayVersion { name: [0; 16] };
    unsafe {
        skyline::nn::oe::GetDisplayVersion(&mut version);
    }
    let end = version
        .name
        .iter()
        .position(|v| *v == 0)
        .unwrap_or(version.name.len());
    String::from_utf8_lossy(&version.name[..end]).into_owned()
}

fn log(lines: &str) {
    skyline::println!("[dispatcher-backoff] {}", lines.trim_end());
    let _ = std::fs::create_dir_all(DATA_DIR);
    let path = std::path::Path::new(DATA_DIR).join("install.log");
    let _ = std::fs::write(path, lines);
}

#[skyline::main(name = "ssbu-dispatcher-backoff")]
pub fn main() {
    let mut report = String::new();
    let version = display_version();
    let _ = writeln!(report, "version={version}");
    if version != "13.0.5" {
        let _ = writeln!(report, "result=unsupported_version");
        log(&report);
        return;
    }
    let (policy, from_file) = load_policy();
    store_policy(policy);
    let _ = writeln!(
        report,
        "policy spin_passes={} every_passes={} base_ns={} max_ns={} cooldown_us={} main_thread_only={} source={}",
        policy.spin_passes,
        policy.every_passes,
        policy.base_ns,
        policy.max_ns,
        policy.cooldown_us,
        policy.main_thread_only,
        if from_file { "config.ini" } else { "default" }
    );
    unsafe {
        let base = getRegionAddress(Region::Text) as usize;
        let end = getRegionAddress(Region::Rodata) as usize;
        let address = base + SITE;
        if !address.is_multiple_of(4) || address + SITE_WORDS.len() * 4 > end {
            let _ = writeln!(report, "result=site_out_of_range");
            log(&report);
            return;
        }
        let mut words = [0u32; 5];
        for (index, word) in words.iter_mut().enumerate() {
            *word = ((address + index * 4) as *const u32).read_volatile();
        }
        let _ = writeln!(
            report,
            "site=main+{SITE:#x} words={:08x} {:08x} {:08x} {:08x} {:08x}",
            words[0], words[1], words[2], words[3], words[4]
        );
        if words != SITE_WORDS {
            let external = words[0] != SITE_WORDS[0] && words[1..] == SITE_WORDS[1..];
            let _ = writeln!(
                report,
                "result={}",
                if external {
                    "skipped_site_owned_by_another_plugin"
                } else {
                    "skipped_site_words_differ"
                }
            );
            log(&report);
            return;
        }
        skyline::install_hook!(dispatcher_idle_pass);
    }
    INSTALLED.store(true, Ordering::Release);
    let _ = writeln!(report, "result=installed");
    log(&report);
}
