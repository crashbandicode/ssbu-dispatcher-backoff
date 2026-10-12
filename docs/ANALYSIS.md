# Why the character select drops frames, and what the backoff changes

Super Smash Bros. Ultimate 13.0.5. All addresses are offsets from the main
module's text base. The investigation ran on the Eden emulator (Linux, Vulkan,
1x internal resolution, isolated profile, headless private display) with host
`perf`, a JIT symbol map, per-thread scheduler statistics and a diagnostic
build of the Deluxe plugin that recorded dispatcher state. Nothing in that
diagnostic build ships; the production plugin is a separate, much smaller hook.

## 1. The symptom

With a two-swapchain-image rendering profile (the Deluxe mod's LessLag), the
character select screen (CSS) runs at 48–50 fps on a Switch while every match
holds 60. Three-image profiles hold 60 on the CSS. On Eden at 1x nothing drops,
but limiting the emulator's own systemd unit to 175% CPU reproduces the
pattern: Vanilla 60, LLDoubles 60, LessLag 53–57.

## 2. The mod's own code is not the cause

Span instrumentation of every CSS-related path in the Deluxe plugin: the
native CSS handler costs about 0.005 ms per frame, the banner paint 0.085 ms,
and the scene-manager wrapper 0.136 ms. That cannot account for a frame budget
that overruns by 3–8 ms.

## 3. What the main thread actually does

The main loop at `0x374b898` enters `0x354ccd0` and waits on dependency
counter `0x5333160` (aggregate record `0x5333150`) before the fighter-record
step. Live registration records attach **600 of 600** CSS registrations on
that counter to callback `0x35487f0` on worker 1, priority 1. The callback
calls vtable `0x523d418` slot `+0x10`, which resolves to **`0x375e260`**; its
owner at `+0x458` is an `nn::ui2d::Layout` derivative (constructor and RTTI
verified), and its slot `+0x58` runs `0x4b810`, the traversal of bound
`AnimTransformBasic` objects through `0x3f070` and `0x3f250`: floating-point,
vertex-colour and visibility animation tracks. That is the CSS portrait grid
animating.

| Scene (direct probe, no SDK wrapper) | FPS | Main-thread wait on that job, mean / p95 | Eden CPU ms per frame |
| --- | ---: | ---: | ---: |
| CSS, LessLag, unlimited | 59.99 | 4.827 / 5.740 ms | 24.72 |
| CSS, LessLag, 175% | 53.78 | 8.246 / 12.976 ms | 26.90 |
| Live match, LessLag, unlimited | 57.43 | 0.913 / 1.579 ms | 21.22 |
| Live match, LessLag, 175% | 55.58 | 1.329 / 4.008 ms | 22.44 |

While it waits, the main thread runs the task dispatcher `0x354a6d0`. The
dispatcher keeps its state in `x24`; `[x24]` is the context, `[context+8]` the
shared queue state. It scans its pending-dependency records, then checks three
local and three shared priority queues. Each queue pop is protected by
`std::mutex::lock` / `unlock` (PLT `0x39c19c0` / `0x39c19d0`), which reach
`nn::os::LockMutex` and the SDK critical-section Enter/Leave. The six lock
call sites are `0x354a750`, `0x354a818`, `0x354a904`, `0x354a994`,
`0x354aa3c` and `0x354aacc`.

At `0x354ab48` all six probes have fallen through without a job:

| Offset | Word | Instruction |
| --- | --- | --- |
| `0x354ab48` | `a94e2708` | `ldp x8, x9, [x24, #0xe0]` (pending vector begin/end) |
| `0x354ab4c` | `eb09011f` | `cmp x8, x9` |
| `0x354ab50` | `54000d01` | `b.ne 0x354acf0` (vector non-empty: rescan pending records) |
| `0x354ab54` | `f94003e0` | `ldr x0, [sp]` (light event pointer) |
| `0x354ab58` | `9411f072` | `bl nn::os::WaitLightEvent` |

Only an **empty** pending vector reaches the `WaitLightEvent`. With a pending
record the dispatcher goes back to `0x354ab60`, rescans the records (stride
`0x20`; `record+8` points at the 32-bit counter; a zero counter selects the
record and resumes its fiber) and polls all six queues again. Completion of a
task decrements counters (`0x354a7e8`, `0x354a8a4`) and propagates to the
parent, but it never signals that light event. So while worker 1 runs the
layout job, the main thread spins.

Detailed state samples: **1,313 of 1,350** observations on the CSS show all
six queues empty, exactly one pending record, counter 1. The lock word was
unowned in 98% of the samples immediately before Enter: this is lock churn,
not contention. The Enter/Leave fast paths alone are 22.0% of all Eden
on-CPU samples unlimited and 27.8% under the quota. Within the quota capture,
slow frames show the aggregate wait growing from 7.5 to 9.9 ms while worker-1
execution grows only 5.5 → 5.9 ms and its run-queue delay 2.8 → 4.8 ms: the
tails are scheduling delay amplified by polling, not a heavier UI algorithm.

Why only two-image profiles show it: the wait sits before the frame's render
work, so the frame's variance lands on the presentation deadline. Three images
absorb a long frame with the following short ones; two cannot. Rendering
options add about 0.45 ms of serialized main-thread work per CSS frame and
amplify the effect, but the two-image ring alone reproduces it.

## 4. The intervention

A/B/A on one boot of one binary: control, bounded backoff, control. The
diagnostic backoff called the SDK `SleepThread(TimeSpan{nanoseconds: 1000})`
about once per 171 idle passes, only when the queues were empty and a
dependency was still pending, before re-acquiring the original lock. All jobs,
counters and locks stayed as they were. A plain `YieldThread` (SVC sleep −1)
did **not** help (57.27 → 56.34 fps); a positive timed sleep enters a real
timed wait while a yield may return immediately.

| Scene / profile / quota | FPS A / B / A | CPU ms per frame A / B / A |
| --- | --- | --- |
| CSS LessLag 175% | 52.23 / 59.31 / 51.89 | 27.84 / 22.90 / 28.08 |
| CSS Vanilla 175% | 59.32 / 60.10 / 59.85 | 26.90 / 23.04 / 26.27 |
| CSS LLDoubles 175% | 59.55 / 60.03 / 58.33 | 26.44 / 23.13 / 27.84 |
| CSS LessLag unlimited | 59.08 / 60.00 / 59.80 | 24.82 / 23.79 / 24.88 |
| Live match LessLag 175% | 57.43 / 59.35 / 59.90 | 20.62 / 18.74 / 19.32 |

Main-core CPU: 11.70 → 6.67 → 11.63 ms per frame; worker-1 CPU 5.7–5.8 ms in
all three. Sampled polling per frame fell about 60%. The gain is removed
polling, not removed UI work; in a match the lower idle CPU is the supported
benefit and frame rate is unchanged.

Screenshots: `screenshots/before-css-lesslag-eden-cpu175.png` (54.1 fps,
27.2 CPU ms/frame) and `screenshots/after-css-lesslag-eden-cpu175.png`
(59.4 fps, 23.3 CPU ms/frame), same CSS, same quota, Performance view open.

## 5. The production hook

One inline hook at **`0x354ab48`**, installed only after the five words above
match exactly. A different first word with the other four intact means
another plugin has already patched the site; the plugin then skips and logs
it. The hook runs on the dispatcher's thread with the game's registers saved
by Skyline's trampoline and the displaced `ldp` replayed afterwards, so the
game's own rescan follows unchanged.

Eligibility before any sleep (any failure returns to the game immediately):

1. `GetCurrentThread()` is the thread whose SDK name is `MainThread`
   (resolved once through the name pointer at `ThreadType+0x1a0`, then
   compared by pointer).
2. Dispatcher, context and shared state were validated once (aligned,
   readable `0x158`, `0x60` and `0xd0` byte spans).
3. Context byte `+0x58` bit 0 (enabled) is set.
4. Local queue sizes at `d+0x38`, `d+0x68`, `d+0x98` and shared sizes at
   `s+0x28`, `s+0x58`, `s+0x88` are all zero.
5. The pending vector `[d+0xe0, d+0xe8)` is non-empty, 8-byte aligned, a
   multiple of `0x20` long and at most 4,096 records; every record's `+8`
   pointer is a plausible aligned address and its 32-bit counter is above
   zero.
6. The ready-to-resume vector `[d+0xf8, d+0x100)` is empty.

Policy (host-tested, `src/policy.rs`): per dispatcher, count consecutive
eligible passes; spin through the first `spin_passes` (16); then on every
`every_passes`-th pass (1) sleep `base_ns` (5 µs) doubling to `max_ns`
(50 µs). Any veto resets the streak. An optional `cooldown_us` enforces a
minimum gap between sleeps. `preset=measured` in the config file reproduces
the diagnostic cadence exactly (1 µs once per 171 passes from the first pass).

Latency bound: a job finishing just after a sleep starts waits for that sleep
plus kernel wake latency. With the defaults that is at most 50 µs plus wake
latency per occurrence, in a frame of 16.667 ms. The measured Eden cadence
used 1 µs requests; the hardware defaults are longer and fewer, and are what
the Switch test validates.

## 5b. Production build on Eden

The same hook as shipped here, built into ssbu-online-deluxe-rewrite 0.3.0
and toggled only through its `DISABLE_BACKOFF` flag and INI, on one isolated
Eden profile under the 175% quota (17–20 s windows each):

| Run | Hook | FPS | CPU ms per frame | Sleep rate |
| --- | --- | ---: | ---: | --- |
| CSS, two-image LessLag (A) | off | 55.59 | 26.55 | — |
| CSS, two-image LessLag (B) | default policy | 60.00 | 19.50 | 3068/s, 50 µs |
| CSS, two-image LessLag (A) | off | 54.15 | 26.83 | — |
| CSS, two-image LessLag | `preset=measured` | 59.03 | 22.92 | 2744/s, 1 µs |
| CSS, Vanilla | default policy | 60.01 | 20.08 | 2590/s, 50 µs |
| Live match (Lucario vs Kirby, Battlefield) | default policy | 59.84 | 16.34 | 1069/s, 50 µs |
| CSS, two-image LessLag, unlimited CPU | default policy | 60.0 | — | — |

The default policy beats the measured diagnostic cadence on the CSS and
costs nothing in Vanilla or in a match, where it only lowers idle CPU.

## 6. What the Switch test must show

Install, boot, and confirm `install.log` ends with `result=installed` and the
five words. Then:

1. Character select with the two-image LessLag profile: frame rate versus the
   48–50 fps baseline, and the Deluxe Performance view's **Backoff** row
   showing a non-zero sleep rate while on the CSS. Pass: no lower than before;
   expected: visibly higher.
2. Character select with Vanilla: still 60, no stutter.
3. One offline match and one linked online match: same frame rate as before,
   no added input delay, no desync.
4. Stage loading and results screens: no hang, no visibly longer load.
5. Repeat the CSS visit with `DISABLE_BACKOFF` (Deluxe) or `max_ns=0`
   (standalone) to confirm the difference is this hook.
6. If anything regresses, try `preset=measured`, then `cooldown_us=16667`,
   and report the Backoff row and `install.log` with the result.

## 7. Provenance

Attribution and A/B/A measurements by the GPT-6 Astra agent working in the
ssbu-online-deluxe-rewrite project on 2026-10-12; production hook, policy and
this plugin by the project's primary agent the same day. Raw captures, the
source audit and the diagnostic probe patch live in the project's private
evidence store; this repository carries only the summary, the screenshots and
the production code.
