fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let out_dir = std::env::var("OUT_DIR").unwrap();
    let ico_path = format!("{}/icon.ico", out_dir);
    create_bmp_ico("assets/icon.png", &ico_path).expect("failed to create icon.ico");
    let mut res = winresource::WindowsResource::new();
    res.set_icon(&ico_path);
    res.compile().expect("failed to compile Windows resources");
}

// Generates a BMP-format ICO compatible with rc.exe.
// PNG-in-ICO is rejected by some rc.exe versions; raw BMP DIB always works.
fn create_bmp_ico(png_path: &str, ico_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let src = image::open(png_path)?;
    let sizes: &[u32] = &[16, 32, 48, 256];

    let mut frames: Vec<Vec<u8>> = Vec::new();
    for &sz in sizes {
        let resized = src.resize_exact(sz, sz, image::imageops::FilterType::Lanczos3);
        let rgba = resized.to_rgba8();
        let pixels = rgba.as_raw();

        let mut data: Vec<u8> = Vec::new();

        // BITMAPINFOHEADER (40 bytes)
        data.extend_from_slice(&40u32.to_le_bytes());   // biSize
        data.extend_from_slice(&sz.to_le_bytes());       // biWidth
        data.extend_from_slice(&(sz * 2).to_le_bytes()); // biHeight (XOR+AND stacked)
        data.extend_from_slice(&1u16.to_le_bytes());     // biPlanes
        data.extend_from_slice(&32u16.to_le_bytes());    // biBitCount
        data.extend_from_slice(&0u32.to_le_bytes());     // biCompression (BI_RGB)
        data.extend_from_slice(&0u32.to_le_bytes());     // biSizeImage
        data.extend_from_slice(&0u32.to_le_bytes());     // biXPelsPerMeter
        data.extend_from_slice(&0u32.to_le_bytes());     // biYPelsPerMeter
        data.extend_from_slice(&0u32.to_le_bytes());     // biClrUsed
        data.extend_from_slice(&0u32.to_le_bytes());     // biClrImportant

        // XOR mask: BGRA pixels, bottom-up row order
        for y in (0..sz as usize).rev() {
            for x in 0..sz as usize {
                let i = (y * sz as usize + x) * 4;
                data.push(pixels[i + 2]); // B
                data.push(pixels[i + 1]); // G
                data.push(pixels[i]);     // R
                data.push(pixels[i + 3]); // A
            }
        }

        // AND mask: 1-bit per pixel, rows padded to 32-bit boundary; all zeros (alpha handles transparency)
        let and_stride = ((sz + 31) / 32) * 4;
        data.extend(std::iter::repeat(0u8).take((and_stride * sz) as usize));

        frames.push(data);
    }

    let mut ico: Vec<u8> = Vec::new();
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = ICO
    ico.extend_from_slice(&(sizes.len() as u16).to_le_bytes());

    let mut offset = (6 + 16 * sizes.len()) as u32;
    for (i, &sz) in sizes.iter().enumerate() {
        let w = if sz == 256 { 0u8 } else { sz as u8 };
        ico.push(w); ico.push(w); // width, height (0 = 256)
        ico.push(0); ico.push(0); // color count, reserved
        ico.extend_from_slice(&1u16.to_le_bytes());  // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bpp
        ico.extend_from_slice(&(frames[i].len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += frames[i].len() as u32;
    }
    for frame in &frames { ico.extend_from_slice(frame); }

    std::fs::write(ico_path, ico)?;
    Ok(())
}
