//! Print font metrics and shaped glyph positions, for comparing with Qt.
//! Usage: text_probe FONTFILE PX wght opsz TEXT...
use skrifa::{MetadataProvider, instance::Size};

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let px: f32 = a[2].parse().unwrap();
    let wght: f32 = a[3].parse().unwrap();
    let opsz: f32 = a[4].parse().unwrap();
    let font = skrifa::FontRef::new(&data).unwrap();
    for ax in font.axes().iter() {
        println!(
            "axis {} {}..{} default {}",
            ax.tag(),
            ax.min_value(),
            ax.max_value(),
            ax.default_value()
        );
    }
    let loc = font.axes().location([("wght", wght), ("opsz", opsz)]);
    let m = font.metrics(Size::new(px), &loc);
    println!(
        "ascent {} descent {} leading {} cap {:?} upem {}",
        m.ascent, m.descent, m.leading, m.cap_height, m.units_per_em
    );
    let mu = font.metrics(Size::unscaled(), &loc);
    println!(
        "unscaled ascent {} descent {} leading {} cap {:?}",
        mu.ascent, mu.descent, mu.leading, mu.cap_height
    );
    let data_ = harfrust::ShaperData::new(&font);
    let inst = harfrust::ShaperInstance::from_variations(
        &font,
        [
            harfrust::Variation {
                tag: harfrust::Tag::new(b"wght"),
                value: wght,
            },
            harfrust::Variation {
                tag: harfrust::Tag::new(b"opsz"),
                value: opsz,
            },
        ],
    );
    let shaper = data_.shaper(&font).instance(Some(&inst)).build();
    let upem = m.units_per_em as f32;
    for t in &a[5..] {
        let mut buf = harfrust::UnicodeBuffer::new();
        buf.push_str(t);
        buf.guess_segment_properties();
        let ptem: f32 = std::env::var("PTEM")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(px);
        let out = shaper.shape(buf, harfrust::ShapeOptions::new().point_size(Some(ptem)));
        let gm = font.glyph_metrics(Size::new(px), &loc);
        let raw: Vec<String> = out
            .glyph_infos()
            .iter()
            .map(|i| {
                format!(
                    "{:.4}",
                    gm.advance_width(skrifa::GlyphId::new(i.glyph_id))
                        .unwrap_or(0.0)
                )
            })
            .collect();
        let shaped: Vec<String> = out
            .glyph_positions()
            .iter()
            .map(|p| format!("{:.4}", p.x_advance as f32 * px / upem))
            .collect();
        println!("raw {}\nshaped {}", raw.join(" "), shaped.join(" "));
        let mut x = 0.0f32;
        let mut s = String::new();
        for (i, p) in out.glyph_infos().iter().zip(out.glyph_positions()) {
            s += &format!("({}, {:.4}) ", i.glyph_id, x * px / upem);
            x += p.x_advance as f32;
        }
        println!("{t} adv {:.4} {s}", x * px / upem);
        struct Pen(Vec<String>);
        impl skrifa::outline::OutlinePen for Pen {
            fn move_to(&mut self, x: f32, y: f32) {
                self.0.push(format!("M{x:.4},{:.4}", -y))
            }
            fn line_to(&mut self, x: f32, y: f32) {
                self.0.push(format!("L{x:.4},{:.4}", -y))
            }
            fn quad_to(&mut self, _: f32, _: f32, x: f32, y: f32) {
                self.0.push(format!("Q{x:.4},{:.4}", -y))
            }
            fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, x: f32, y: f32) {
                self.0.push(format!("C{x:.4},{:.4}", -y))
            }
            fn close(&mut self) {
                self.0.push("Z".into())
            }
        }
        let g = font
            .outline_glyphs()
            .get(skrifa::GlyphId::new(out.glyph_infos()[0].glyph_id))
            .unwrap();
        let mut pen = Pen(vec![]);
        g.draw(
            skrifa::outline::DrawSettings::unhinted(Size::new(px), &loc),
            &mut pen,
        )
        .unwrap();
        println!("outline {}", pen.0[..pen.0.len().min(10)].join(" "));
    }
}
