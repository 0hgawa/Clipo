//! Rasterises the brand SVG natively at each icon size (no downscaling: every
//! frame is rendered from vectors) and writes two assets the app embeds:
//!   - assets/icon.ico    — full ladder, the Windows exe / taskbar / Explorer icon
//!   - assets/tray-32.rgba — straight RGBA the tray loads via Icon::from_rgba
use resvg::tiny_skia::{Pixmap, Transform};
use resvg::usvg::{Options, Tree};

// 256 down to 16 — the sizes Windows asks for across DPIs and surfaces.
const SIZES: [u32; 10] = [256, 128, 96, 64, 48, 40, 32, 24, 20, 16];
const SVG: &str = r"D:\Imagens\Clipo\camera-svgrepo-com-indigo.svg";

fn main() {
    let svg = std::fs::read(SVG).expect("read svg");
    let tree = Tree::from_data(&svg, &Options::default()).expect("parse svg");
    let unit = tree.size().width(); // square; viewBox is 32×32 scaled to 256

    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for &s in &SIZES {
        let rgba = render(&tree, unit, s);
        if s == 32 {
            std::fs::write("../../assets/tray-32.rgba", &rgba).expect("write tray rgba");
        }
        let img = ico::IconImage::from_rgba_data(s, s, rgba);
        dir.add_entry(ico::IconDirEntry::encode(&img).expect("encode entry"));
    }
    let file = std::fs::File::create("../../assets/icon.ico").expect("create ico");
    dir.write(file).expect("write ico");
    println!("wrote assets/icon.ico ({} sizes) + assets/tray-32.rgba", SIZES.len());
}

/// Render the SVG into a `size`×`size` straight-alpha RGBA buffer.
fn render(tree: &Tree, unit: f32, size: u32) -> Vec<u8> {
    let mut pm = Pixmap::new(size, size).expect("pixmap");
    let scale = size as f32 / unit;
    resvg::render(tree, Transform::from_scale(scale, scale), &mut pm.as_mut());
    // tiny-skia stores premultiplied alpha; the .ico/HICON formats want straight.
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for px in pm.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    out
}
