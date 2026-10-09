//! Performance benchmarks: timings are collected per operation and
//! reported after the correctness suite passes.

use std::time::Instant;

use crate::step_log::say;
use crate::target::Target;

struct Stats {
    label: String,
    times_us: Vec<u64>,
    errors: usize,
}

impl Stats {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            times_us: Vec::new(),
            errors: 0,
        }
    }

    fn report(&self) {
        if self.times_us.is_empty() {
            say(&format!(
                "  {:<24} no samples ({} error(s))",
                self.label, self.errors
            ));
            return;
        }
        let mut sorted = self.times_us.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let sum: u64 = sorted.iter().sum();
        let avg = sum / n as u64;
        let p50 = sorted[n / 2];
        let p99 = sorted[((n as f64 * 0.99) as usize).min(n - 1)];
        let min = sorted[0];
        let max = sorted[n - 1];
        let ops_per_sec = if avg > 0 {
            1_000_000.0 / avg as f64
        } else {
            0.0
        };
        let errors = if self.errors > 0 {
            format!("  [{} errors]", self.errors)
        } else {
            String::new()
        };
        say(&format!(
            "  {:<24} n={:<5} avg={:>7} us  p50={:>7} us  p99={:>7} us  min={:>7} us  max={:>7} us  {:.0} ops/s{}",
            self.label, n, avg, p50, p99, min, max, ops_per_sec, errors
        ));
    }
}

/// Times `iters` executions of `op`; op errors are counted, printed once
/// and never abort the benchmark (correctness is the suite's job).
fn bench(label: &str, iters: usize, mut op: impl FnMut() -> Result<(), String>) -> Stats {
    say(&format!("RUN  bench {label}"));
    let mut stats = Stats::new(label);
    for _ in 0..iters {
        let t0 = Instant::now();
        match op() {
            Ok(()) => stats.times_us.push(t0.elapsed().as_micros() as u64),
            Err(error) => {
                stats.errors += 1;
                if stats.errors == 1 {
                    say(&format!("    [{label}] {error}"));
                }
            }
        }
    }
    stats
}

pub(crate) fn run_benchmarks(t: &Target, iters: usize) {
    bench_single_reads(t, iters);
    bench_alternate_paths(t, iters);
    bench_batch(t, iters);
    bench_pointer_walk(t, iters);
    bench_sequential(t, iters);
}

/// The plain read path at every payload size.
fn bench_single_reads(t: &Target, iters: usize) {
    say("--- Single Read Benchmarks ---");
    bench("ReadMemory(4)", iters, || {
        let data =
            ks_sdk::read_bytes(t.pid, t.slot(0x0), 4, false, false).map_err(|e| e.to_string())?;
        if data.len() == 4 {
            Ok(())
        } else {
            Err("short read".into())
        }
    })
    .report();

    bench("ReadMemory(64)", iters, || {
        let data =
            ks_sdk::read_bytes(t.pid, t.slot(0x40), 64, false, false).map_err(|e| e.to_string())?;
        if data == t.buffer[0x40..0x80] {
            Ok(())
        } else {
            Err("content mismatch".into())
        }
    })
    .report();

    bench("ReadMemory(4096)", iters, || {
        let data = ks_sdk::read_bytes(t.pid, t.slot(0x0), 4096, false, false)
            .map_err(|e| e.to_string())?;
        if data == t.buffer {
            Ok(())
        } else {
            Err("content mismatch".into())
        }
    })
    .report();
    say("");
}

/// The alternate transports: MDL-remap reads/writes and RVA addressing.
fn bench_alternate_paths(t: &Target, iters: usize) {
    say("--- Alternate Path Benchmarks ---");
    bench("ReadMdl(4)", iters, || {
        ks_sdk::read_bytes(t.pid, t.slot(0x0), 4, false, true)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("ReadMdl(4096)", iters, || {
        ks_sdk::read_bytes(t.pid, t.slot(0x0), 4096, false, true)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("ReadRva(4)", iters, || {
        ks_sdk::read_bytes(t.pid, t.rva_target_rva, 4, true, false)
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMemory(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0x400), &0u32.to_le_bytes(), false, false)
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMdl(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0x400), &0u32.to_le_bytes(), false, true)
            .map_err(|e| e.to_string())
    })
    .report();

    bench("WriteRva(4)", iters, || {
        ks_sdk::write_bytes(t.pid, t.rva_target_rva, &0u32.to_le_bytes(), true, false)
            .map_err(|e| e.to_string())
    })
    .report();

    // Full-size writes: identity pushes of the whole target buffer, so the
    // pointer-chain fields at 0x100/0x200 are restored rather than
    // disturbed and the later benchmarks still see a walkable chain.
    bench("WriteMemory(4096)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0), &t.buffer, false, false).map_err(|e| e.to_string())
    })
    .report();

    bench("WriteMdl(4096)", iters, || {
        ks_sdk::write_bytes(t.pid, t.slot(0), &t.buffer, false, true).map_err(|e| e.to_string())
    })
    .report();
    say("");
}

/// Batch read/write at both directions of the entry-count limits.
fn bench_batch(t: &Target, iters: usize) {
    say("--- Batch Benchmarks ---");
    let batch_50: Vec<u64> = (0..50).map(|i| t.slot(0x900 + i * 4)).collect();
    bench("BatchRead(50x4)", iters, || {
        let data = ks_sdk::batch_read(t.pid, 4, &batch_50).map_err(|e| e.to_string())?;
        if data.len() == batch_50.len() * 4 {
            Ok(())
        } else {
            Err("short batch".into())
        }
    })
    .report();

    let batch_200: Vec<u64> = (0..200).map(|i| t.slot(i * 2)).collect();
    bench("BatchRead(200x12)", iters, || {
        let data = ks_sdk::batch_read(t.pid, 12, &batch_200).map_err(|e| e.to_string())?;
        if data.len() == batch_200.len() * 12 {
            Ok(())
        } else {
            Err("short batch".into())
        }
    })
    .report();

    let write_entries: Vec<(u64, Vec<u8>)> = (0..64u64)
        .map(|i| (t.slot(0xA00 + i * 4), vec![0xAA; 4]))
        .collect();
    bench("BatchWrite(64x4)", iters, || {
        let statuses = ks_sdk::batch_write(t.pid, &write_entries).map_err(|e| e.to_string())?;
        if statuses.iter().all(|&status| status == 0) {
            Ok(())
        } else {
            Err("non-zero entry status".into())
        }
    })
    .report();
    say("");
}

/// One-hop pointer walks.
fn bench_pointer_walk(t: &Target, iters: usize) {
    say("--- Pointer Walk Benchmarks ---");
    bench("PtrWalk(1 offs)", iters, || {
        ks_sdk::traverse_pointer_chain(t.pid, t.slot(0x0), &[0x100])
            .map(|_| ())
            .map_err(|e| e.to_string())
    })
    .report();
    say("");
}

/// Three dependent reads in sequence (round trips cannot be batched).
fn bench_sequential(t: &Target, iters: usize) {
    say("--- Sequential Chain (3 x ReadI32) ---");
    bench("Seq3_ReadI32", iters, || {
        for offset in [0x0u64, 0x100, 0x200] {
            ks_sdk::read_bytes(t.pid, t.slot(offset), 4, false, false)
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    })
    .report();
}
