use std::env;
use std::fs;
use std::path::PathBuf;

const RT_ICON: u16 = 3;
const RT_GROUP_ICON: u16 = 14;
const RT_VERSION: u16 = 16;
const RT_MANIFEST: u16 = 24;

const LANG_NEUTRAL: u16 = 0x0000;
const LANG_EN_US: u16 = 0x0409;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=assets/airmonitor.ico");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    let ico_path = PathBuf::from("assets/airmonitor.ico");
    let ico_bytes = fs::read(&ico_path).expect("failed to read assets/airmonitor.ico");

    let pkg_version = env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.1.0".to_string());
    let major: u16 = env::var("CARGO_PKG_VERSION_MAJOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let minor: u16 = env::var("CARGO_PKG_VERSION_MINOR")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1);
    let patch: u16 = env::var("CARGO_PKG_VERSION_PATCH")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let description = env::var("CARGO_PKG_DESCRIPTION").unwrap_or_else(|_| {
        "Ultra-lightweight Windows taskbar hardware & wireless device battery monitor".to_string()
    });

    let res_bytes = build_windows_res(&ico_bytes, major, minor, patch, &pkg_version, &description)
        .expect("failed to build Windows resource file");

    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR not set"));
    let res_path = out_dir.join("airmonitor.res");
    fs::write(&res_path, res_bytes).expect("failed to write airmonitor.res");

    println!("cargo:rustc-link-arg-bin=airmonitor={}", res_path.display());

    if env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
        println!("cargo:rustc-link-arg=/DEPENDENTLOADFLAG:0x800");
    }
}

fn read_u16(b: &[u8], off: usize) -> Option<u16> {
    let slice = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]))
}

fn read_u32(b: &[u8], off: usize) -> Option<u32> {
    let slice = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn pad4(buf: &mut Vec<u8>) {
    while !buf.len().is_multiple_of(4) {
        buf.push(0);
    }
}

fn push_utf16_nul(buf: &mut Vec<u8>, s: &str) {
    for unit in s.encode_utf16() {
        buf.extend_from_slice(&unit.to_le_bytes());
    }
    buf.extend_from_slice(&0u16.to_le_bytes());
}

fn append_res_record(out: &mut Vec<u8>, res_type: u16, res_id: u16, lang_id: u16, data: &[u8]) {
    let data_size = data.len() as u32;
    let header_size: u32 = 32;

    out.extend_from_slice(&data_size.to_le_bytes());
    out.extend_from_slice(&header_size.to_le_bytes());
    // Type: ordinal 0xFFFF + res_type
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&res_type.to_le_bytes());
    // Name: ordinal 0xFFFF + res_id
    out.extend_from_slice(&0xFFFFu16.to_le_bytes());
    out.extend_from_slice(&res_id.to_le_bytes());
    // DataVersion, MemoryFlags (MOVEABLE | PURE | DISCARDABLE), LanguageId, Version, Characteristics
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0x1030u16.to_le_bytes());
    out.extend_from_slice(&lang_id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());

    out.extend_from_slice(data);
    pad4(out);
}

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

fn build_manifest(major: u16, minor: u16, patch: u16, description: &str) -> Vec<u8> {
    let desc_xml = xml_escape(description);
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<assembly xmlns="urn:schemas-microsoft-com:asm.v1" manifestVersion="1.0" xmlns:asmv3="urn:schemas-microsoft-com:asm.v3">
  <assemblyIdentity version="{major}.{minor}.{patch}.0" processorArchitecture="*" name="AirMonitor" type="win32"/>
  <description>{desc_xml}</description>
  <trustInfo xmlns="urn:schemas-microsoft-com:asm.v3">
    <security>
      <requestedPrivileges>
        <requestedExecutionLevel level="asInvoker" uiAccess="false"/>
      </requestedPrivileges>
    </security>
  </trustInfo>
  <compatibility xmlns="urn:schemas-microsoft-com:compatibility.v1">
    <application>
      <supportedOS Id="{{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}}"/>
    </application>
  </compatibility>
  <asmv3:application>
    <asmv3:windowsSettings>
      <dpiAware xmlns="http://schemas.microsoft.com/SMI/2005/WindowsSettings">true/pm</dpiAware>
      <dpiAwareness xmlns="http://schemas.microsoft.com/SMI/2016/WindowsSettings">PerMonitorV2</dpiAwareness>
    </asmv3:windowsSettings>
  </asmv3:application>
</assembly>
"#
    )
    .into_bytes()
}

fn write_version_node(
    out: &mut Vec<u8>,
    key: &str,
    w_type: u16,
    w_value_length: u16,
    value_bytes: &[u8],
    write_children: impl FnOnce(&mut Vec<u8>),
) {
    pad4(out);
    let start = out.len();
    // Placeholder for wLength (u16)
    out.extend_from_slice(&0u16.to_le_bytes());
    out.extend_from_slice(&w_value_length.to_le_bytes());
    out.extend_from_slice(&w_type.to_le_bytes());
    push_utf16_nul(out, key);

    if !value_bytes.is_empty() {
        pad4(out);
        out.extend_from_slice(value_bytes);
    }

    let before_children = out.len();
    write_children(out);
    if out.len() == before_children && value_bytes.is_empty() {
        // Empty container edge case (unused here, kept for completeness)
        pad4(out);
    }

    let total_len = u16::try_from(out.len() - start).expect("VERSIONINFO node exceeded u16");
    out[start..start + 2].copy_from_slice(&total_len.to_le_bytes());
}

fn append_version_string(out: &mut Vec<u8>, key: &str, value: &str) {
    let utf16_units: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
    let w_value_length = u16::try_from(utf16_units.len()).expect("VERSIONINFO string exceeded u16");
    let mut value_bytes = Vec::with_capacity(utf16_units.len() * 2);
    for unit in utf16_units {
        value_bytes.extend_from_slice(&unit.to_le_bytes());
    }
    write_version_node(out, key, 1, w_value_length, &value_bytes, |_| {});
}

fn build_version_info(
    major: u16,
    minor: u16,
    patch: u16,
    pkg_version: &str,
    description: &str,
) -> Vec<u8> {
    let ver_ms = ((major as u32) << 16) | (minor as u32);
    let ver_ls = (patch as u32) << 16;

    // VS_FIXEDFILEINFO (52 bytes)
    let mut fixed = Vec::with_capacity(52);
    fixed.extend_from_slice(&0xFEEF_04BDu32.to_le_bytes()); // dwSignature
    fixed.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // dwStrucVersion
    fixed.extend_from_slice(&ver_ms.to_le_bytes()); // dwFileVersionMS
    fixed.extend_from_slice(&ver_ls.to_le_bytes()); // dwFileVersionLS
    fixed.extend_from_slice(&ver_ms.to_le_bytes()); // dwProductVersionMS
    fixed.extend_from_slice(&ver_ls.to_le_bytes()); // dwProductVersionLS
    fixed.extend_from_slice(&0x0000_003Fu32.to_le_bytes()); // dwFileFlagsMask (VS_FFI_FILEFLAGSMASK)
    fixed.extend_from_slice(&0u32.to_le_bytes()); // dwFileFlags
    fixed.extend_from_slice(&0x0004_0004u32.to_le_bytes()); // dwFileOS (VOS_NT_WINDOWS32)
    fixed.extend_from_slice(&0x0000_0001u32.to_le_bytes()); // dwFileType (VFT_APP)
    fixed.extend_from_slice(&0u32.to_le_bytes()); // dwFileSubtype
    fixed.extend_from_slice(&0u32.to_le_bytes()); // dwFileDateMS
    fixed.extend_from_slice(&0u32.to_le_bytes()); // dwFileDateLS

    let four_part_ver = format!("{major}.{minor}.{patch}.0");
    let mut out = Vec::with_capacity(1024);

    write_version_node(
        &mut out,
        "VS_VERSION_INFO",
        0,
        fixed.len() as u16,
        &fixed,
        |root| {
            write_version_node(root, "StringFileInfo", 1, 0, &[], |sfi| {
                write_version_node(sfi, "040904B0", 1, 0, &[], |table| {
                    append_version_string(table, "CompanyName", "AirMonitor");
                    append_version_string(table, "FileDescription", description);
                    append_version_string(table, "FileVersion", &four_part_ver);
                    append_version_string(table, "InternalName", "airmonitor");
                    append_version_string(
                        table,
                        "LegalCopyright",
                        "Copyright (c) 2026 AirMonitor Contributors",
                    );
                    append_version_string(table, "OriginalFilename", "airmonitor.exe");
                    append_version_string(table, "ProductName", "AirMonitor");
                    append_version_string(table, "ProductVersion", pkg_version);
                });
            });

            write_version_node(root, "VarFileInfo", 1, 0, &[], |vfi| {
                let translation = [
                    0x0409u16.to_le_bytes()[0],
                    0x0409u16.to_le_bytes()[1],
                    0x04B0u16.to_le_bytes()[0],
                    0x04B0u16.to_le_bytes()[1],
                ];
                write_version_node(
                    vfi,
                    "Translation",
                    0,
                    translation.len() as u16,
                    &translation,
                    |_| {},
                );
            });
        },
    );

    out
}

fn build_windows_res(
    ico: &[u8],
    major: u16,
    minor: u16,
    patch: u16,
    pkg_version: &str,
    description: &str,
) -> Result<Vec<u8>, &'static str> {
    if ico.len() < 6 {
        return Err("ico too short");
    }
    let reserved = read_u16(ico, 0).ok_or("bad header")?;
    let ico_type = read_u16(ico, 2).ok_or("bad type")?;
    let count = read_u16(ico, 4).ok_or("bad count")?;
    if reserved != 0 || ico_type != 1 || count == 0 {
        return Err("invalid ICONDIR");
    }
    let dir_end = 6 + (count as usize) * 16;
    if ico.len() < dir_end {
        return Err("truncated ICONDIRENTRY table");
    }

    let mut res = Vec::with_capacity(ico.len() + 2048);
    // Standard 32-byte empty resource header sentinel at start of .res file
    append_res_record(&mut res, 0, 0, LANG_NEUTRAL, &[]);

    // Build RT_GROUP_ICON payload (6-byte header + N * 14-byte GRPICONDIRENTRY)
    let mut group = Vec::with_capacity(6 + (count as usize) * 14);
    group.extend_from_slice(&0u16.to_le_bytes());
    group.extend_from_slice(&1u16.to_le_bytes());
    group.extend_from_slice(&count.to_le_bytes());

    for i in 0..count {
        let entry_off = 6 + (i as usize) * 16;
        let bytes_in_res = read_u32(ico, entry_off + 8).ok_or("bad size")? as usize;
        let image_offset = read_u32(ico, entry_off + 12).ok_or("bad offset")? as usize;
        let img_end = image_offset.checked_add(bytes_in_res).ok_or("overflow")?;
        let img = ico
            .get(image_offset..img_end)
            .ok_or("truncated icon frame")?;

        let icon_id = i + 1;
        append_res_record(&mut res, RT_ICON, icon_id, LANG_NEUTRAL, img);

        // Copy first 12 bytes of ICONDIRENTRY, then write 2-byte nID
        group.extend_from_slice(&ico[entry_off..entry_off + 12]);
        group.extend_from_slice(&icon_id.to_le_bytes());
    }

    append_res_record(&mut res, RT_GROUP_ICON, 1, LANG_NEUTRAL, &group);

    let version_info = build_version_info(major, minor, patch, pkg_version, description);
    append_res_record(&mut res, RT_VERSION, 1, LANG_EN_US, &version_info);

    let manifest = build_manifest(major, minor, patch, description);
    append_res_record(&mut res, RT_MANIFEST, 1, LANG_NEUTRAL, &manifest);

    Ok(res)
}
