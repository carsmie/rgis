//! Compare two PNGs (e.g. QGIS Server vs rGIS output).
//!
//! cargo run --release --example compare_png -- a.png b.png [diff.png]
//!
//! Both images are composited over white, then compared per pixel. Prints the mean
//! absolute channel error and the share of pixels differing by more than 64 in any
//! channel; optionally writes a diff image (mismatches in red over a faded copy of `a`).

use tiny_skia::{Color, Pixmap, PixmapPaint, Transform};

fn over_white(path: &str) -> Pixmap {
    let src = Pixmap::load_png(path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"));
    let mut out = Pixmap::new(src.width(), src.height()).unwrap();
    out.fill(Color::WHITE);
    out.draw_pixmap(
        0,
        0,
        src.as_ref(),
        &PixmapPaint::default(),
        Transform::identity(),
        None,
    );
    out
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: compare_png <a.png> <b.png> [diff.png]");
        std::process::exit(2);
    }
    let (a, b) = (over_white(&args[0]), over_white(&args[1]));
    assert_eq!(
        (a.width(), a.height()),
        (b.width(), b.height()),
        "image sizes differ"
    );
    let mut diff = a.clone();
    let (mut sum, mut bad) = (0u64, 0usize);
    let n = a.pixels().len();
    for (i, (pa, pb)) in a.pixels().iter().zip(b.pixels()).enumerate() {
        let ch = [
            (pa.red(), pb.red()),
            (pa.green(), pb.green()),
            (pa.blue(), pb.blue()),
        ];
        let d: Vec<u8> = ch.iter().map(|(x, y)| x.abs_diff(*y)).collect();
        sum += d.iter().map(|v| *v as u64).sum::<u64>();
        let px = &mut diff.pixels_mut()[i];
        if d.iter().any(|v| *v > 64) {
            bad += 1;
            *px = tiny_skia::ColorU8::from_rgba(230, 0, 0, 255).premultiply();
        } else {
            let fade = |v: u8| 255 - (255 - v) / 4;
            *px = tiny_skia::ColorU8::from_rgba(
                fade(pa.red()),
                fade(pa.green()),
                fade(pa.blue()),
                255,
            )
            .premultiply();
        }
    }
    println!(
        "{} x {}: mean abs error {:.2}/255, pixels differing > 64: {} ({:.2}%)",
        a.width(),
        a.height(),
        sum as f64 / (n * 3) as f64,
        bad,
        100.0 * bad as f64 / n as f64
    );
    if let Some(out) = args.get(2) {
        diff.save_png(out).expect("write diff");
        println!("diff image written to {out}");
    }
}
