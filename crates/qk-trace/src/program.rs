//! The kernel program, loaded and attached. Loading needs `CAP_BPF` and
//! `CAP_PERFMON`, which is why only the helper loads it.

use anyhow::{Context, Result};
use aya::maps::{Array, MapData, RingBuf};
use aya::programs::BtfTracePoint;
use aya::{Btf, Ebpf};

use crate::calls::{Arch, Arg, CALLS};

#[cfg(target_arch = "x86_64")]
static OBJECT: &[u8] = aya::include_bytes_aligned!("../bpf/trace.x86.o");
#[cfg(target_arch = "aarch64")]
static OBJECT: &[u8] = aya::include_bytes_aligned!("../bpf/trace.arm64.o");
/// Elsewhere [`Program::load`] fails before it would load one.
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
static OBJECT: &[u8] = &[];

const NO_PATH: u8 = 0xff;

/// `struct settings` in the kernel program.
#[repr(C)]
#[derive(Clone, Copy)]
struct Settings {
    prctl: u32,
    option: u32,
    secret: u64,
}

// SAFETY: plain integers, laid out as the kernel program declares them.
unsafe impl aya::Pod for Settings {}

/// `struct call` in the kernel program.
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct CallSpec {
    recorded: u8,
    paths: [u8; 2],
    flags_at: u8,
    dup_commands_only: u8,
}

// SAFETY: plain bytes, laid out as the kernel program declares them.
unsafe impl aya::Pod for CallSpec {}

pub struct Program {
    _bpf: Ebpf,
    pub events: RingBuf<MapData>,
    dropped: Array<MapData, u64>,
}

impl Program {
    /// Loads the program for this architecture, with `option` and `secret`
    /// as what a process names to join a recording, and attaches it.
    pub fn load(option: u32, secret: u64) -> Result<Self> {
        let arch = Arch::current().context("the recorder knows x86-64 and arm64 only")?;
        let mut bpf = Ebpf::load(OBJECT).context("cannot load the recorder's program")?;
        let mut settings: Array<_, Settings> =
            Array::try_from(bpf.map_mut("settings").context("no settings map")?)?;
        settings.set(
            0,
            Settings {
                prctl: arch.prctl(),
                option,
                secret,
            },
            0,
        )?;
        let mut calls: Array<_, CallSpec> =
            Array::try_from(bpf.map_mut("calls").context("no calls map")?)?;
        for call in CALLS {
            let Some(number) = arch.number(call) else {
                continue;
            };
            let mut paths = call
                .args
                .iter()
                .enumerate()
                .filter(|(_, arg)| **arg == Arg::Path)
                .map(|(index, _)| index as u8);
            calls.set(
                u32::from(number),
                CallSpec {
                    recorded: 1,
                    paths: [
                        paths.next().unwrap_or(NO_PATH),
                        paths.next().unwrap_or(NO_PATH),
                    ],
                    flags_at: call.flags_at.unwrap_or(NO_PATH),
                    dup_commands_only: u8::from(call.dup_commands_only),
                },
                0,
            )?;
        }
        let btf = Btf::from_sys_fs().context("the kernel has no BTF")?;
        for (name, tracepoint) in [
            ("on_enter", "sys_enter"),
            ("on_exit", "sys_exit"),
            ("on_fork", "sched_process_fork"),
            ("on_process_exit", "sched_process_exit"),
        ] {
            let program: &mut BtfTracePoint = bpf
                .program_mut(name)
                .with_context(|| format!("no program {name}"))?
                .try_into()?;
            program
                .load(tracepoint, &btf)
                .with_context(|| format!("cannot load {name}"))?;
            program
                .attach()
                .with_context(|| format!("cannot attach {name}"))?;
        }
        let events = RingBuf::try_from(bpf.take_map("events").context("no events map")?)?;
        let dropped = Array::try_from(bpf.take_map("dropped").context("no dropped map")?)?;
        Ok(Self {
            _bpf: bpf,
            events,
            dropped,
        })
    }

    /// How many events the ring buffer has had no room for.
    pub fn dropped(&self) -> Result<u64> {
        self.dropped
            .get(&0, 0)
            .context("cannot read how many events were dropped")
    }
}
