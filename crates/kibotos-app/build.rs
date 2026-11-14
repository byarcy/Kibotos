// Embeds the application manifest and icon with the MSVC linker. The icon
// resource (.res) is written here directly from assets/kibotos.ico, so no
// resource compiler (rc.exe) is needed.

use std::path::{Path, PathBuf};

fn main() {
    let dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let manifest = dir.join("kibotos.manifest");
    let icon = dir.join("assets").join("kibotos.ico");
    println!("cargo:rerun-if-changed=kibotos.manifest");
    println!("cargo:rerun-if-changed=assets/kibotos.ico");
    if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() != Ok("msvc") {
        return;
    }
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}", manifest.display());

    let res = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("kibotos.res");
    write_icon_res(&icon, &res).expect("cannot build the icon resource");
    println!("cargo:rustc-link-arg-bins={}", res.display());

    // These DLLs are only needed after the window is visible (icons,
    // dialogs, COM). Delay-loading them shortens process start.
    for dll in ["shell32.dll", "comdlg32.dll", "ole32.dll", "oleaut32.dll", "imm32.dll"] {
        println!("cargo:rustc-link-arg-bins=/DELAYLOAD:{dll}");
    }
    println!("cargo:rustc-link-arg-bins=delayimp.lib");
}

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
/// Group icon id; `kibotos.exe,0` and `LoadIconW(.., 1)` refer to it.
const ICON_GROUP_ID: u16 = 1;

/// Writes a .res file with one RT_GROUP_ICON and an RT_ICON per image of
/// the .ico (the images are copied as they are, PNG or BMP).
fn write_icon_res(ico: &Path, out: &Path) -> std::io::Result<()> {
    let data = std::fs::read(ico)?;
    let u16_at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
    let u32_at = |o: usize| u32::from_le_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]);
    let count = u16_at(4) as usize;

    let mut res = Vec::new();
    // A .res file starts with an empty entry.
    entry(&mut res, 0, 0, &[]);
    let mut group = Vec::new();
    group.extend_from_slice(&0u16.to_le_bytes()); // reserved
    group.extend_from_slice(&1u16.to_le_bytes()); // type: icon
    group.extend_from_slice(&(count as u16).to_le_bytes());
    for i in 0..count {
        let e = 6 + 16 * i;
        let (size, offset) = (u32_at(e + 8) as usize, u32_at(e + 12) as usize);
        let id = (i + 1) as u16;
        entry(&mut res, RT_ICON, id, &data[offset..offset + size]);
        // GRPICONDIRENTRY: the ICONDIRENTRY's first 12 bytes, then the id.
        group.extend_from_slice(&data[e..e + 12]);
        group.extend_from_slice(&id.to_le_bytes());
    }
    entry(&mut res, RT_GROUP_ICON, ICON_GROUP_ID, &group);
    std::fs::write(out, res)
}

fn entry(res: &mut Vec<u8>, kind: u16, name: u16, data: &[u8]) {
    res.extend_from_slice(&(data.len() as u32).to_le_bytes()); // DataSize
    res.extend_from_slice(&32u32.to_le_bytes()); // HeaderSize
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&kind.to_le_bytes()); // TYPE (ordinal)
    res.extend_from_slice(&[0xFF, 0xFF]);
    res.extend_from_slice(&name.to_le_bytes()); // NAME (ordinal)
    res.extend_from_slice(&0u32.to_le_bytes()); // DataVersion
    let flags: u16 = if kind == 0 { 0 } else { 0x1010 }; // moveable, discardable
    res.extend_from_slice(&flags.to_le_bytes());
    res.extend_from_slice(&0u16.to_le_bytes()); // language neutral
    res.extend_from_slice(&0u32.to_le_bytes()); // Version
    res.extend_from_slice(&0u32.to_le_bytes()); // Characteristics
    res.extend_from_slice(data);
    while res.len() % 4 != 0 {
        res.push(0);
    }
}
