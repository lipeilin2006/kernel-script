use std::time::{Duration, Instant};

use ks_core::protocol::{
    Frame, ReadProcessMemory, Request, Response, WireDecode, WireEncode, HEADER_SIZE,
    MAX_FRAME_SIZE,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::windows::named_pipe::{ClientOptions, NamedPipeClient};

const PIPE_NAME: &str = r"\\.\pipe\KernelScript";

async fn connect() -> Result<NamedPipeClient, String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match ClientOptions::new().open(PIPE_NAME) {
            Ok(stream) => return Ok(stream),
            Err(_e) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            Err(e) => return Err(format!("connect failed: {e}")),
        }
    }
}

async fn send_request(
    stream: &mut NamedPipeClient,
    request: &Request<'_>,
) -> Result<Vec<u8>, String> {
    let mut buf = vec![0u8; request.encoded_len().map_err(|e| format!("{e:?}"))?];
    request.encode(&mut buf).map_err(|e| format!("{e:?}"))?;
    stream
        .write_all(&buf)
        .await
        .map_err(|e| format!("write: {e}"))?;

    let mut header = [0u8; HEADER_SIZE];
    stream
        .read_exact(&mut header)
        .await
        .map_err(|e| format!("header: {e}"))?;

    let payload_len = u32::from_le_bytes(header[6..10].try_into().unwrap()) as usize;
    if payload_len > MAX_FRAME_SIZE - HEADER_SIZE {
        return Err("response too large".into());
    }

    let mut resp = vec![0u8; HEADER_SIZE + payload_len];
    resp[..HEADER_SIZE].copy_from_slice(&header);
    stream
        .read_exact(&mut resp[HEADER_SIZE..])
        .await
        .map_err(|e| format!("payload: {e}"))?;

    Ok(resp)
}

fn decode_response(buf: &[u8]) -> Result<Response<'_>, String> {
    let frame = Frame::parse(buf).map_err(|e| format!("parse: {e:?}"))?;
    Response::decode(frame.message_type, frame.payload).map_err(|e| format!("decode: {e:?}"))
}

struct Stats {
    label: String,
    times_us: Vec<u64>,
}

impl Stats {
    fn new(label: &str) -> Self {
        Self {
            label: label.to_string(),
            times_us: Vec::new(),
        }
    }

    fn report(&self) {
        if self.times_us.is_empty() {
            return;
        }
        let mut sorted = self.times_us.clone();
        sorted.sort_unstable();
        let n = sorted.len();
        let sum: u64 = sorted.iter().sum();
        let avg = sum / n as u64;
        let p50 = sorted[n / 2];
        let p99 = sorted[(n as f64 * 0.99) as usize];
        let min = sorted[0];
        let max = sorted[n - 1];
        let ops_per_sec = if avg > 0 {
            1_000_000.0 / avg as f64
        } else {
            0.0
        };
        println!(
            "  {:<24} n={:<5} avg={:>7} us  p50={:>7} us  p99={:>7} us  min={:>7} us  max={:>7} us  {:.0} ops/s",
            self.label, n, avg, p50, p99, min, max, ops_per_sec
        );
    }
}

async fn bench_get_pid(stream: &mut NamedPipeClient, name: &str, iters: usize) -> (u64, Stats) {
    let mut stats = Stats::new("GetProcessId");
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(
            stream,
            &Request::GetProcessId {
                name: name.as_bytes(),
            },
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);

        match decode_response(&resp_buf).unwrap() {
            Response::ProcessId(pid) => {
                if pid == 0 {
                    eprintln!("  process '{}' not found", name);
                }
                return (pid, stats);
            }
            Response::Error(code) => {
                eprintln!("  service error: {code}");
                return (0, stats);
            }
            other => {
                eprintln!("  unexpected: {other:?}");
                return (0, stats);
            }
        }
    }
    (0, stats)
}

async fn bench_get_base(stream: &mut NamedPipeClient, pid: u64, iters: usize) -> (u64, Stats) {
    let mut stats = Stats::new("GetProcessBase");
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(stream, &Request::GetProcessBase { pid })
            .await
            .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);

        match decode_response(&resp_buf).unwrap() {
            Response::ProcessBase(base) => {
                return (base, stats);
            }
            Response::Error(code) => {
                eprintln!("  service error: {code}");
                return (0, stats);
            }
            other => {
                eprintln!("  unexpected: {other:?}");
                return (0, stats);
            }
        }
    }
    (0, stats)
}

async fn bench_read_i32(stream: &mut NamedPipeClient, pid: u64, addr: u64, iters: usize) -> Stats {
    let mut stats = Stats::new("ReadMemory(4)");
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(
            stream,
            &Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: addr,
                size: 4,
            }),
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);
        let _ = decode_response(&resp_buf);
    }
    stats
}

async fn bench_read_bytes(
    stream: &mut NamedPipeClient,
    pid: u64,
    addr: u64,
    size: u64,
    iters: usize,
) -> Stats {
    let mut stats = Stats::new(&format!("ReadMemory({size})"));
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(
            stream,
            &Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: addr,
                size,
            }),
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);
        let _ = decode_response(&resp_buf);
    }
    stats
}

async fn bench_batch_read(
    stream: &mut NamedPipeClient,
    pid: u64,
    base: u64,
    entry_size: u32,
    count: usize,
    iters: usize,
) -> Stats {
    let mut addrs_raw = Vec::with_capacity(count * 8);
    for i in 0..count as u64 {
        addrs_raw.extend_from_slice(&(base + i * entry_size as u64).to_le_bytes());
    }

    let mut stats = Stats::new(&format!("BatchRead({count}x{entry_size})"));
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(
            stream,
            &Request::BatchReadMemory {
                pid,
                size: entry_size,
                addresses: &addrs_raw,
            },
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);
        let _ = decode_response(&resp_buf);
    }
    stats
}

async fn bench_ptr_walk(
    stream: &mut NamedPipeClient,
    pid: u64,
    base: u64,
    offsets: &[u64],
    iters: usize,
) -> Stats {
    let offsets_raw: Vec<u8> = offsets.iter().flat_map(|o| o.to_le_bytes()).collect();
    let mut stats = Stats::new(&format!("PtrWalk({} offs)", offsets.len()));
    for _ in 0..iters {
        let t0 = Instant::now();
        let resp_buf = send_request(
            stream,
            &Request::TraversePointerChain {
                pid,
                base,
                offsets: &offsets_raw,
            },
        )
        .await
        .unwrap();
        let elapsed = t0.elapsed().as_micros() as u64;
        stats.times_us.push(elapsed);
        let _ = decode_response(&resp_buf);
    }
    stats
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args: Vec<String> = std::env::args().collect();
    let process_name = args.get(1).map(|s| s.as_str()).unwrap_or("notepad.exe");
    let iters: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(200);

    println!("ks-test: IPC + driver performance benchmark");
    println!("target process: {process_name}  iters: {iters}");
    println!();

    let mut stream = connect().await.expect("connect failed");
    println!("connected to {PIPE_NAME}");
    println!();

    let (pid, get_pid_stats) = bench_get_pid(&mut stream, process_name, 5).await;
    get_pid_stats.report();
    if pid == 0 {
        eprintln!("cannot continue without a valid PID");
        return;
    }
    println!("  PID: {pid}");

    let (base, get_base_stats) = bench_get_base(&mut stream, pid, 5).await;
    get_base_stats.report();
    if base == 0 {
        eprintln!("cannot continue without a valid base");
        return;
    }
    println!("  Base: 0x{:X}", base);
    println!();

    println!("--- Single Read Benchmarks ---");
    let s1 = bench_read_i32(&mut stream, pid, base, iters).await;
    s1.report();

    let s2 = bench_read_bytes(&mut stream, pid, base, 64, iters).await;
    s2.report();

    let s3 = bench_read_bytes(&mut stream, pid, base, 4096, iters).await;
    s3.report();
    println!();

    println!("--- Batch Read Benchmarks ---");
    let s4 = bench_batch_read(&mut stream, pid, base, 4, 50, iters).await;
    s4.report();

    let s5 = bench_batch_read(&mut stream, pid, base, 8, 100, iters).await;
    s5.report();

    let s6 = bench_batch_read(&mut stream, pid, base, 12, 200, iters).await;
    s6.report();
    println!();

    println!("--- Pointer Walk Benchmarks ---");
    let s7 = bench_ptr_walk(&mut stream, pid, base, &[0x100, 0x30], iters).await;
    s7.report();

    let s8 = bench_ptr_walk(&mut stream, pid, base, &[0x100, 0x30, 0x80], iters).await;
    s8.report();
    println!();

    println!("--- Sequential Chain (3 x ReadI32) ---");
    let mut s9 = Stats::new("Seq3_ReadI32");
    for _ in 0..iters {
        let t0 = Instant::now();
        let r1 = send_request(
            &mut stream,
            &Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: base + 0x100,
                size: 4,
            }),
        )
        .await;
        let _ = decode_response(&r1.unwrap());
        let r2 = send_request(
            &mut stream,
            &Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: base + 0x300,
                size: 4,
            }),
        )
        .await;
        let _ = decode_response(&r2.unwrap());
        let r3 = send_request(
            &mut stream,
            &Request::ReadProcessMemory(ReadProcessMemory {
                pid,
                target_address: base + 0x800,
                size: 4,
            }),
        )
        .await;
        let _ = decode_response(&r3.unwrap());
        let elapsed = t0.elapsed().as_micros() as u64;
        s9.times_us.push(elapsed);
    }
    s9.report();
    println!();

    println!("Done.");
}
