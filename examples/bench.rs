//! HTTP load generator for comparing map servers.
//!
//! cargo run --release --example bench -- -c 10 -n 2000 -f urls.txt http://127.0.0.1:8098/ [more base URLs…]
//!
//! `urls.txt` holds one query string per line. Request i uses line i (cycled) and base URL
//! i % bases (round-robin, e.g. across several single-threaded QGIS processes).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() {
    let mut args = std::env::args().skip(1);
    let (mut conc, mut total, mut file, mut bases) = (1usize, 100usize, String::new(), vec![]);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-c" => conc = args.next().unwrap().parse().unwrap(),
            "-n" => total = args.next().unwrap().parse().unwrap(),
            "-f" => file = args.next().unwrap(),
            b => bases.push(b.to_string()),
        }
    }
    let queries: Vec<String> = std::fs::read_to_string(&file)
        .expect("url file")
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(String::from)
        .collect();
    let (queries, bases) = (Arc::new(queries), Arc::new(bases));
    let next = Arc::new(AtomicUsize::new(0));
    let client = reqwest::Client::builder()
        .pool_max_idle_per_host(conc)
        .build()
        .unwrap();
    let started = Instant::now();
    let mut tasks = vec![];
    for _ in 0..conc {
        let (q, b, n, c) = (queries.clone(), bases.clone(), next.clone(), client.clone());
        tasks.push(tokio::spawn(async move {
            let (mut lat, mut errors, mut bytes) = (vec![], 0usize, 0usize);
            loop {
                let i = n.fetch_add(1, Ordering::Relaxed);
                if i >= total {
                    break;
                }
                let url = format!("{}?{}", b[i % b.len()], q[i % q.len()]);
                let t = Instant::now();
                match c.get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        bytes += r.bytes().await.map(|b| b.len()).unwrap_or(0)
                    }
                    _ => errors += 1,
                }
                lat.push(t.elapsed());
            }
            (lat, errors, bytes)
        }));
    }
    let (mut lat, mut errors, mut bytes) = (Vec::<Duration>::new(), 0, 0);
    for t in tasks {
        let (l, e, b) = t.await.unwrap();
        lat.extend(l);
        errors += e;
        bytes += b;
    }
    let wall = started.elapsed();
    lat.sort();
    let pct =
        |p: f64| lat[((lat.len() as f64 * p) as usize).min(lat.len() - 1)].as_secs_f64() * 1000.0;
    println!(
        "{:>7.1} req/s  p50 {:>7.1} ms  p95 {:>7.1} ms  p99 {:>7.1} ms  errors {}  avg {:.0} KB  ({} req, c={}, {:.1}s)",
        lat.len() as f64 / wall.as_secs_f64(),
        pct(0.50),
        pct(0.95),
        pct(0.99),
        errors,
        bytes as f64 / lat.len().max(1) as f64 / 1024.0,
        lat.len(),
        conc,
        wall.as_secs_f64()
    );
}
