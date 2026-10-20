//! headroom-hog — a real memory load for end-to-end checks.
//!
//! `headroom-hog GB SECS` touches GB of memory page by page, so it counts in
//! the process's footprint, then holds it for SECS. Used by the integration
//! tests and for trying the guard by hand; not installed by the formula.

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (Some(gb), Some(secs)) = (
        args.first().and_then(|a| a.parse::<f64>().ok()),
        args.get(1).and_then(|a| a.parse::<u64>().ok()),
    ) else {
        eprintln!("usage: headroom-hog GB SECS");
        std::process::exit(2);
    };
    let bytes = (gb * (1u64 << 30) as f64) as usize;
    let mut mem = vec![0u8; bytes];
    for i in (0..bytes).step_by(4096) {
        mem[i] = 1;
    }
    std::thread::sleep(std::time::Duration::from_secs(secs));
    // Read it back so the optimiser can't drop the allocation.
    let touched: usize = mem.iter().step_by(4096).map(|&b| b as usize).sum();
    println!("held {touched} pages");
}
