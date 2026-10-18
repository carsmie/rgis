//! List a font's named instances (fvar) with their coordinates.
use skrifa::MetadataProvider;

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let data = std::fs::read(&a[1]).unwrap();
    let font = skrifa::FontRef::new(&data).unwrap();
    for ni in font.named_instances().iter() {
        let name = font
            .localized_strings(ni.subfamily_name_id())
            .english_or_first()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let coords: Vec<String> = ni.user_coords().zip(font.axes().iter()).map(|(c, ax)| format!("{}={c}", ax.tag())).collect();
        println!("{name}: {}", coords.join(" "));
    }
}
