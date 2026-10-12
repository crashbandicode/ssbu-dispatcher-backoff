//! Host-testable backoff decision for the game's task dispatcher. No game
//! memory is touched here; `runtime::dispatcher_backoff` reads the dispatcher
//! and hands a [`Snapshot`] in.
//!
//! The dispatcher scans its pending-dependency records and then polls three
//! local and three shared priority queues. When every queue is empty and at
//! least one record is still pending, it repeats immediately instead of
//! waiting (its light event is only used when the pending vector is empty).
//! On the character select this costs the main thread roughly 5 ms of
//! polling per frame while one UI layout job runs on a worker. This module
//! decides, for one such idle pass, whether the calling dispatcher should
//! sleep briefly before the game's own rescan. It never decides to skip
//! work: any sign that work may be ready is a veto.
//!
//! The same file ships in the standalone `ssbu-dispatcher-backoff` NRO; keep
//! the two copies identical.

/// What the glue observed on one fall-through pass at the hook site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Snapshot {
    /// Context byte `+0x58` bit 0. A disabled dispatcher leaves the loop.
    pub enabled: bool,
    /// Three local and three shared queue sizes, in game order.
    pub queue_sizes: [u64; 6],
    /// Number of pending dependency records (stride 0x20 vector).
    pub pending_records: usize,
    /// True when every pending record's 32-bit counter is still nonzero.
    pub all_pending_blocked: bool,
    /// True when the ready-to-resume vector the game checks is empty.
    pub resume_vectors_empty: bool,
}

/// Tunables. Durations are nanoseconds for `nn::os::SleepThread`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Policy {
    /// Idle passes to spin through exactly as the game does before the first
    /// sleep. Keeps short waits at the game's native latency.
    pub spin_passes: u32,
    /// After the spin budget, sleep on every Nth idle pass (1 = every pass).
    /// The Eden diagnostic that measured the gain slept about once per 171
    /// passes with a 1 µs request.
    pub every_passes: u32,
    /// First sleep length once `spin_passes` is exhausted.
    pub base_ns: u32,
    /// Upper bound of the doubling ramp.
    pub max_ns: u32,
    /// Minimum gap between the end of one sleep and the start of the next,
    /// in microseconds. 0 means no limiter. A hardware trial can cap how
    /// many sleeps one frame may start.
    pub cooldown_us: u32,
    /// Only the dispatcher running on `MainThread` sleeps. The measured
    /// waste is there; worker dispatchers keep their native pickup latency.
    pub main_thread_only: bool,
}

impl Policy {
    /// Conservative hardware default: a few microseconds ramping to tens,
    /// bounded, every idle pass after a short spin, main thread only.
    pub const DEFAULT: Policy = Policy {
        spin_passes: 16,
        every_passes: 1,
        base_ns: 5_000,
        max_ns: 50_000,
        cooldown_us: 0,
        main_thread_only: true,
    };

    /// The cadence the Eden diagnostic measured: a 1 µs request about once
    /// per 171 idle passes, from the first pass, main thread only.
    pub const MEASURED: Policy = Policy {
        spin_passes: 0,
        every_passes: 171,
        base_ns: 1_000,
        max_ns: 1_000,
        cooldown_us: 0,
        main_thread_only: true,
    };

    /// Reject nonsense from an override file. Zero `max_ns` disables sleeping;
    /// any other `max_ns` is held to 1 µs..=2 ms and `base_ns` to 1 µs..=max.
    pub fn sanitized(self) -> Policy {
        let max_ns = if self.max_ns == 0 {
            0
        } else {
            self.max_ns.clamp(1_000, 2_000_000)
        };
        let base_ns = if max_ns == 0 {
            0
        } else {
            self.base_ns.clamp(1_000, max_ns)
        };
        Policy {
            spin_passes: self.spin_passes.min(1 << 20),
            every_passes: self.every_passes.clamp(1, 1 << 16),
            base_ns,
            max_ns,
            cooldown_us: self.cooldown_us.min(1_000_000),
            main_thread_only: self.main_thread_only,
        }
    }

    /// Parse `key=value` lines. Unknown keys are ignored; bad values keep the
    /// previous field. Keys: `spin_passes`, `every_passes`, `base_ns`,
    /// `max_ns`, `cooldown_us`, `all_threads` (1 lets worker dispatchers
    /// sleep too), and `preset=measured` to start from [`Policy::MEASURED`].
    pub fn parse(text: &str, defaults: Policy) -> Policy {
        let mut policy = defaults;
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            if key == "preset" {
                match value {
                    "measured" => policy = Policy::MEASURED,
                    "default" => policy = Policy::DEFAULT,
                    _ => {}
                }
                continue;
            }
            let Ok(value) = value.parse::<u32>() else {
                continue;
            };
            match key {
                "spin_passes" => policy.spin_passes = value,
                "every_passes" => policy.every_passes = value,
                "base_ns" => policy.base_ns = value,
                "max_ns" => policy.max_ns = value,
                "cooldown_us" => policy.cooldown_us = value,
                "all_threads" => policy.main_thread_only = value == 0,
                _ => {}
            }
        }
        policy.sanitized()
    }

    /// Packed form for one diagnostics record: `spin_passes` in bits 0..15,
    /// `base_ns / 1000` in 16..31, `max_ns / 1000` in 32..47, bit 48 set
    /// when worker dispatchers may sleep too, `every_passes` in bits 49..63.
    pub fn packed(self) -> u64 {
        u64::from(self.spin_passes.min(0xffff))
            | (u64::from((self.base_ns / 1000).min(0xffff)) << 16)
            | (u64::from((self.max_ns / 1000).min(0xffff)) << 32)
            | (u64::from(!self.main_thread_only) << 48)
            | (u64::from(self.every_passes.min(0x7fff)) << 49)
    }

    /// Cooldown in 19.2 MHz system ticks.
    pub fn cooldown_ticks(self) -> u64 {
        u64::from(self.cooldown_us) * 192 / 10
    }
}

/// Per-dispatcher idle streak. One lives per dispatcher the hook has seen.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Streak {
    /// Consecutive idle passes since work was last observed.
    pub idle_passes: u32,
    /// Last sleep length, or 0 before the first sleep of this streak.
    pub last_ns: u32,
}

/// Why a pass did not sleep. Reported as counters only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Veto {
    Disabled,
    QueueNotEmpty,
    NoPending,
    PendingReady,
    ResumePending,
    Spinning,
}

/// Decide one pass and advance the streak. `Ok(ns)` means sleep that long,
/// then fall through to the game's rescan. `Err` means fall through now.
pub fn decide(policy: Policy, snapshot: &Snapshot, streak: &mut Streak) -> Result<u32, Veto> {
    let veto = if !snapshot.enabled {
        Some(Veto::Disabled)
    } else if snapshot.queue_sizes.iter().any(|&size| size != 0) {
        Some(Veto::QueueNotEmpty)
    } else if snapshot.pending_records == 0 {
        Some(Veto::NoPending)
    } else if !snapshot.all_pending_blocked {
        Some(Veto::PendingReady)
    } else if !snapshot.resume_vectors_empty {
        Some(Veto::ResumePending)
    } else {
        None
    };
    if let Some(veto) = veto {
        *streak = Streak::default();
        return Err(veto);
    }
    streak.idle_passes = streak.idle_passes.saturating_add(1);
    if policy.max_ns == 0 || streak.idle_passes <= policy.spin_passes {
        return Err(Veto::Spinning);
    }
    let since_budget = streak.idle_passes - policy.spin_passes - 1;
    if !since_budget.is_multiple_of(policy.every_passes.max(1)) {
        return Err(Veto::Spinning);
    }
    let next = if streak.last_ns == 0 {
        policy.base_ns
    } else {
        streak.last_ns.saturating_mul(2).min(policy.max_ns)
    };
    streak.last_ns = next;
    Ok(next)
}

/// Overlay label: sleeps per second and the last sleep length.
pub fn status_label(installed: bool, sleeps_per_second: Option<u32>, last_ns: u32) -> String {
    if !installed {
        return "Off".to_string();
    }
    match sleeps_per_second {
        Some(rate) if rate > 0 => format!("{rate}/s {} us", last_ns / 1000),
        _ => "Idle".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idle() -> Snapshot {
        Snapshot {
            enabled: true,
            queue_sizes: [0; 6],
            pending_records: 1,
            all_pending_blocked: true,
            resume_vectors_empty: true,
        }
    }

    #[test]
    fn spins_through_budget_then_ramps_and_caps() {
        let policy = Policy {
            spin_passes: 2,
            base_ns: 20_000,
            max_ns: 50_000,
            ..Policy::DEFAULT
        };
        let mut streak = Streak::default();
        assert_eq!(decide(policy, &idle(), &mut streak), Err(Veto::Spinning));
        assert_eq!(decide(policy, &idle(), &mut streak), Err(Veto::Spinning));
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(20_000));
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(40_000));
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(50_000));
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(50_000));
        assert_eq!(streak.idle_passes, 6);
    }

    #[test]
    fn measured_preset_sleeps_once_per_171_passes_from_the_first() {
        let policy = Policy::MEASURED.sanitized();
        let mut streak = Streak::default();
        let mut sleeps = Vec::new();
        for pass in 1..=600u32 {
            if let Ok(ns) = decide(policy, &idle(), &mut streak) {
                sleeps.push((pass, ns));
            }
        }
        assert_eq!(
            sleeps,
            vec![(1, 1_000), (172, 1_000), (343, 1_000), (514, 1_000)]
        );
    }

    #[test]
    fn any_sign_of_work_vetoes_and_resets_the_streak() {
        let policy = Policy {
            spin_passes: 0,
            ..Policy::DEFAULT
        };
        let mut streak = Streak::default();
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(5_000));

        let mut queue = idle();
        queue.queue_sizes[4] = 1;
        assert_eq!(
            decide(policy, &queue, &mut streak),
            Err(Veto::QueueNotEmpty)
        );
        assert_eq!(streak, Streak::default());

        let ready = Snapshot {
            all_pending_blocked: false,
            ..idle()
        };
        assert_eq!(decide(policy, &ready, &mut streak), Err(Veto::PendingReady));

        let resume = Snapshot {
            resume_vectors_empty: false,
            ..idle()
        };
        assert_eq!(
            decide(policy, &resume, &mut streak),
            Err(Veto::ResumePending)
        );

        let none = Snapshot {
            pending_records: 0,
            ..idle()
        };
        assert_eq!(decide(policy, &none, &mut streak), Err(Veto::NoPending));

        let disabled = Snapshot {
            enabled: false,
            ..idle()
        };
        assert_eq!(decide(policy, &disabled, &mut streak), Err(Veto::Disabled));

        // The streak restarts from the base after a reset.
        assert_eq!(decide(policy, &idle(), &mut streak), Ok(5_000));
    }

    #[test]
    fn zero_max_disables_sleeping_but_keeps_counting() {
        let policy = Policy {
            spin_passes: 0,
            base_ns: 20_000,
            max_ns: 0,
            ..Policy::DEFAULT
        }
        .sanitized();
        let mut streak = Streak::default();
        assert_eq!(decide(policy, &idle(), &mut streak), Err(Veto::Spinning));
        assert_eq!(streak.idle_passes, 1);
    }

    #[test]
    fn parse_overrides_and_sanitizes() {
        let text = "# tuning\nspin_passes = 4\nbase_ns=5\nmax_ns = 40000\nall_threads=1\n\
                    every_passes=0\ncooldown_us=250\nnonsense\nbogus=x\n";
        let policy = Policy::parse(text, Policy::DEFAULT);
        assert_eq!(
            policy,
            Policy {
                spin_passes: 4,
                every_passes: 1,
                base_ns: 1_000,
                max_ns: 40_000,
                cooldown_us: 250,
                main_thread_only: false,
            }
        );
        let huge = Policy::parse("max_ns=999999999\nbase_ns=999999999", Policy::DEFAULT);
        assert_eq!(huge.max_ns, 2_000_000);
        assert_eq!(huge.base_ns, 2_000_000);
        assert!(huge.main_thread_only);
        assert_eq!(Policy::parse("", Policy::DEFAULT), Policy::DEFAULT);
        assert!(Policy::parse("all_threads=0", Policy::DEFAULT).main_thread_only);
        let preset = Policy::parse("preset=measured\ncooldown_us=16667", Policy::DEFAULT);
        assert_eq!(
            preset,
            Policy {
                cooldown_us: 16_667,
                ..Policy::MEASURED
            }
        );
        assert_eq!(preset.cooldown_ticks(), 320_006);
    }

    #[test]
    fn sub_microsecond_max_is_raised_not_panicked() {
        for max in [1u32, 500, 999] {
            let policy = Policy::parse(&format!("max_ns={max}\nbase_ns=1"), Policy::DEFAULT);
            assert_eq!(policy.max_ns, 1_000);
            assert_eq!(policy.base_ns, 1_000);
        }
        let policy = Policy::parse("max_ns=3000\nbase_ns=90000", Policy::DEFAULT);
        assert_eq!((policy.base_ns, policy.max_ns), (3_000, 3_000));
    }

    #[test]
    fn packed_and_label() {
        assert_eq!(
            Policy::DEFAULT.packed(),
            16 | (5 << 16) | (50 << 32) | (1 << 49)
        );
        let workers = Policy {
            main_thread_only: false,
            ..Policy::DEFAULT
        };
        assert_eq!((workers.packed() >> 48) & 1, 1);
        assert_eq!(Policy::MEASURED.packed() >> 49, 171);
        assert_eq!(status_label(false, Some(5), 20_000), "Off");
        assert_eq!(status_label(true, None, 0), "Idle");
        assert_eq!(status_label(true, Some(0), 0), "Idle");
        assert_eq!(status_label(true, Some(120), 40_000), "120/s 40 us");
    }
}
