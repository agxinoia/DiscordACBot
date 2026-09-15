//! Static analysis of fetched payloads.
//!
//! None of this is required for change detection — that is driven purely by
//! the SHA-256 in [`crate::eac`]. Everything here exists to answer the *next*
//! question, "what changed and how does this build relate to the last one",
//! which is what a reverse engineer actually needs. Every entry point is
//! therefore best-effort and returns `None` rather than failing a poll.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Every digest of a payload, full length. The embed shows a short form, but
/// the full values are what cross-reference against external sample databases.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Hashes {
    pub md5: String,
    pub sha1: String,
    pub sha256: String,
}

pub fn hashes(bytes: &[u8]) -> Hashes {
    use md5::Md5;
    use sha1::Sha1;
    use sha2::{Digest, Sha256};

    Hashes {
        md5: hex::encode(Md5::digest(bytes)),
        sha1: hex::encode(Sha1::digest(bytes)),
        sha256: hex::encode(Sha256::digest(bytes)),
    }
}

/// Shannon entropy in bits per byte, 0.0–8.0.
///
/// Around 7.9+ over a whole section suggests compression or encryption; a
/// packer appearing or disappearing between builds shows up here first.
pub fn shannon_entropy(bytes: &[u8]) -> f64 {
    if bytes.is_empty() {
        return 0.0;
    }
    let mut counts = [0u64; 256];
    for &b in bytes {
        counts[b as usize] += 1;
    }
    let len = bytes.len() as f64;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / len;
            p * p.log2()
        })
        .sum::<f64>()
}

/// Container format guessed from magic bytes.
pub fn detect_format(bytes: &[u8]) -> &'static str {
    const MACHO: [&[u8]; 4] = [
        &[0xFE, 0xED, 0xFA, 0xCE],
        &[0xFE, 0xED, 0xFA, 0xCF],
        &[0xCE, 0xFA, 0xED, 0xFE],
        &[0xCF, 0xFA, 0xED, 0xFE],
    ];

    match bytes {
        b if b.starts_with(b"MZ") => {
            // Distinguish a real PE from a bare DOS stub via e_lfanew.
            let offset = bytes
                .get(0x3C..0x40)
                .map(|o| u32::from_le_bytes([o[0], o[1], o[2], o[3]]) as usize);
            match offset.and_then(|o| bytes.get(o..o + 4)) {
                Some(b"PE\0\0") => "PE",
                _ => "MZ (DOS)",
            }
        }
        b if b.starts_with(b"\x7fELF") => "ELF",
        b if MACHO.iter().any(|m| b.starts_with(m)) => "Mach-O",
        b if b.starts_with(b"PK\x03\x04") => "ZIP",
        b if b.starts_with(b"\x1f\x8b") => "gzip",
        b if b.starts_with(b"BZh") => "bzip2",
        b if b.starts_with(b"\xFD7zXZ") => "xz",
        b if b.starts_with(b"\x28\xB5\x2F\xFD") => "zstd",
        b if b.starts_with(b"7z\xBC\xAF\x27\x1C") => "7z",
        b => {
            let head = b.iter().position(|c| !c.is_ascii_whitespace()).unwrap_or(0);
            match b.get(head) {
                Some(b'{') | Some(b'[') => "JSON",
                Some(b'<') => "XML/HTML",
                _ => "unknown",
            }
        }
    }
}

/// TLSH fuzzy hash, or `None` when the payload is too small or too uniform
/// for TLSH to produce one (it needs roughly 50 bytes of varied input).
pub fn tlsh(bytes: &[u8]) -> Option<String> {
    let hash = tlsh2::TlshDefaultBuilder::build_from(bytes)?;
    Some(String::from_utf8_lossy(&hash.hash()).into_owned())
}

/// Distance between two payloads: 0 is identical, higher is more different.
/// Roughly, under ~30 is a small patch and over ~200 is effectively unrelated.
///
/// Takes raw bytes rather than hash strings because tlsh2 offers no way to
/// parse a hash back into a comparable value — which is why the archive has to
/// retain the previous payload for this to work.
pub fn tlsh_distance(a: &[u8], b: &[u8]) -> Option<i32> {
    let a = tlsh2::TlshDefaultBuilder::build_from(a)?;
    let b = tlsh2::TlshDefaultBuilder::build_from(b)?;
    Some(a.diff(&b, true))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SectionInfo {
    pub name: String,
    pub virtual_size: u32,
    pub raw_size: u32,
    pub entropy: f64,
    pub characteristics: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VersionInfo {
    pub file_version: Option<String>,
    pub product_version: Option<String>,
    /// StringFileInfo entries such as CompanyName and OriginalFilename.
    pub strings: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertInfo {
    pub subject: String,
    pub issuer: String,
    /// Unix seconds; a signing-certificate rotation is itself a notable event.
    pub not_before: i64,
    pub not_after: i64,
    pub serial: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub entry_count: usize,
    pub total_bytes: usize,
    pub certificates: Vec<CertInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeInfo {
    pub machine: String,
    pub machine_raw: u16,
    /// COFF TimeDateStamp, unix seconds. The single most useful field for
    /// correlating a build against other artifacts.
    pub timestamp: u32,
    pub is_dll: bool,
    pub is_64: bool,
    pub entry: u32,
    pub image_base: u64,
    pub subsystem: Option<u16>,
    /// Build-machine PDB path from the CodeView debug directory.
    pub pdb_path: Option<String>,
    pub sections: Vec<SectionInfo>,
    pub libraries: Vec<String>,
    pub export_count: usize,
    pub version: Option<VersionInfo>,
    pub signature: Option<SignatureInfo>,
}

pub fn machine_name(machine: u16) -> &'static str {
    match machine {
        0x014C => "x86",
        0x8664 => "x64",
        0xAA64 => "arm64",
        0x01C0 | 0x01C4 => "arm",
        0x0200 => "ia64",
        0x5032 => "riscv32",
        0x5064 => "riscv64",
        0 => "unknown",
        _ => "other",
    }
}

/// Parse a payload as a PE image. Returns `None` for anything else.
pub fn analyse_pe(bytes: &[u8]) -> Option<PeInfo> {
    let pe = goblin::pe::PE::parse(bytes).ok()?;
    let coff = &pe.header.coff_header;

    let sections = pe
        .sections
        .iter()
        .map(|s| {
            let start = s.pointer_to_raw_data as usize;
            let end = start.saturating_add(s.size_of_raw_data as usize);
            let data = bytes.get(start..end.min(bytes.len())).unwrap_or(&[]);
            SectionInfo {
                name: s.name().unwrap_or("<invalid>").to_string(),
                virtual_size: s.virtual_size,
                raw_size: s.size_of_raw_data,
                entropy: shannon_entropy(data),
                characteristics: s.characteristics,
            }
        })
        .collect();

    let pdb_path = pe
        .debug_data
        .as_ref()
        .and_then(|d| d.codeview_pdb70_debug_info.as_ref())
        .map(|cv| {
            String::from_utf8_lossy(cv.filename)
                .trim_end_matches('\0')
                .to_string()
        })
        .filter(|p| !p.is_empty());

    Some(PeInfo {
        machine: machine_name(coff.machine).to_string(),
        machine_raw: coff.machine,
        timestamp: coff.time_date_stamp,
        is_dll: pe.is_lib,
        is_64: pe.is_64,
        entry: pe.entry,
        image_base: pe.image_base,
        subsystem: pe
            .header
            .optional_header
            .map(|o| o.windows_fields.subsystem),
        pdb_path,
        sections,
        libraries: pe.libraries.iter().map(|l| l.to_string()).collect(),
        export_count: pe.exports.len(),
        version: parse_version_info(bytes),
        signature: parse_signature(&pe),
    })
}

/// Authenticode certificates from the PE's attribute certificate table.
fn parse_signature(pe: &goblin::pe::PE) -> Option<SignatureInfo> {
    let entries = &pe.certificates;
    if entries.is_empty() {
        return None;
    }
    let total_bytes = entries.iter().map(|c| c.certificate.len()).sum();
    let certificates = entries
        .iter()
        .flat_map(|entry| parse_pkcs7_certs(entry.certificate))
        .collect();

    Some(SignatureInfo {
        entry_count: entries.len(),
        total_bytes,
        certificates,
    })
}

/// Pull the X.509 certificates out of a PKCS#7 SignedData blob.
fn parse_pkcs7_certs(blob: &[u8]) -> Vec<CertInfo> {
    use cms::content_info::ContentInfo;
    use cms::signed_data::SignedData;
    use der::Decode;

    let Ok(content_info) = ContentInfo::from_der(blob) else {
        return Vec::new();
    };
    let Ok(signed_data) = content_info.content.decode_as::<SignedData>() else {
        return Vec::new();
    };
    let Some(certificates) = signed_data.certificates else {
        return Vec::new();
    };

    certificates
        .0
        .iter()
        .filter_map(|choice| match choice {
            cms::cert::CertificateChoices::Certificate(cert) => {
                let tbs = &cert.tbs_certificate;
                Some(CertInfo {
                    subject: tbs.subject.to_string(),
                    issuer: tbs.issuer.to_string(),
                    not_before: tbs.validity.not_before.to_unix_duration().as_secs() as i64,
                    not_after: tbs.validity.not_after.to_unix_duration().as_secs() as i64,
                    serial: hex::encode(tbs.serial_number.as_bytes()),
                })
            }
            _ => None,
        })
        .collect()
}

/// Keys worth lifting out of StringFileInfo.
const VERSION_KEYS: &[&str] = &[
    "CompanyName",
    "FileDescription",
    "FileVersion",
    "InternalName",
    "LegalCopyright",
    "OriginalFilename",
    "ProductName",
    "ProductVersion",
];

/// VS_FIXEDFILEINFO signature, little-endian.
const FIXED_FILE_INFO_MAGIC: [u8; 4] = [0xBD, 0x04, 0xEF, 0xFE];

/// Extract version metadata.
///
/// goblin navigates the resource tree but does not decode VS_VERSIONINFO, so
/// this locates the fixed block by its magic and reads StringFileInfo pairs
/// directly. Scanning rather than walking the tree keeps it working on
/// payloads that are a container of images rather than a single PE.
pub fn parse_version_info(bytes: &[u8]) -> Option<VersionInfo> {
    let mut info = VersionInfo::default();

    if let Some(pos) = find(bytes, &FIXED_FILE_INFO_MAGIC) {
        // dwSignature, dwStrucVersion, then the two version DWORD pairs.
        let read = |offset: usize| -> Option<u32> {
            let b = bytes.get(pos + offset..pos + offset + 4)?;
            Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        if let (Some(fms), Some(fls)) = (read(8), read(12)) {
            info.file_version = Some(format_version(fms, fls));
        }
        if let (Some(pms), Some(pls)) = (read(16), read(20)) {
            info.product_version = Some(format_version(pms, pls));
        }
    }

    for key in VERSION_KEYS {
        if let Some(value) = find_utf16_value(bytes, key) {
            info.strings.insert((*key).to_string(), value);
        }
    }

    let empty =
        info.file_version.is_none() && info.product_version.is_none() && info.strings.is_empty();
    (!empty).then_some(info)
}

fn format_version(most: u32, least: u32) -> String {
    format!(
        "{}.{}.{}.{}",
        most >> 16,
        most & 0xFFFF,
        least >> 16,
        least & 0xFFFF
    )
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn to_utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// Read the UTF-16LE value that follows a StringFileInfo key.
///
/// The format pads between key and value to a 4-byte boundary relative to the
/// resource, whose base is not known here, so trailing NUL units are skipped
/// instead of computing alignment.
fn find_utf16_value(bytes: &[u8], key: &str) -> Option<String> {
    let encoded = to_utf16le(key);
    let start = find(bytes, &encoded)? + encoded.len();

    let mut units = bytes[start..]
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]));

    // Skip the key's own terminator plus any alignment padding.
    let mut value = String::new();
    let mut seen_padding = false;
    for unit in units.by_ref().take(4096) {
        if unit == 0 {
            if value.is_empty() && !seen_padding {
                seen_padding = true;
                continue;
            }
            if value.is_empty() {
                continue;
            }
            break;
        }
        match char::from_u32(unit as u32) {
            Some(c) => value.push(c),
            None => break,
        }
    }

    let value = value.trim().to_string();
    (!value.is_empty()).then_some(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but structurally valid PE32+ image so the parser is
    /// exercised against real headers rather than a mock.
    fn minimal_pe(machine: u16, timestamp: u32, section: (&[u8; 8], &[u8])) -> Vec<u8> {
        minimal_pe_with_cert(machine, timestamp, section, None)
    }

    /// As `minimal_pe`, optionally appending an attribute certificate table
    /// carrying a PKCS#7 blob the way a signed image does.
    fn minimal_pe_with_cert(
        machine: u16,
        timestamp: u32,
        section: (&[u8; 8], &[u8]),
        certificate: Option<&[u8]>,
    ) -> Vec<u8> {
        const PE_OFFSET: usize = 0x80;
        const OPT_HEADER_SIZE: u16 = 240;
        let (section_name, section_data) = section;

        let headers_end = PE_OFFSET + 4 + 20 + OPT_HEADER_SIZE as usize + 40;
        let raw_offset = headers_end.next_multiple_of(512);

        let mut pe = vec![0u8; PE_OFFSET];
        pe[0..2].copy_from_slice(b"MZ");
        pe[0x3C..0x40].copy_from_slice(&(PE_OFFSET as u32).to_le_bytes());

        pe.extend_from_slice(b"PE\0\0");
        // COFF header.
        pe.extend_from_slice(&machine.to_le_bytes());
        pe.extend_from_slice(&1u16.to_le_bytes()); // one section
        pe.extend_from_slice(&timestamp.to_le_bytes());
        pe.extend_from_slice(&0u32.to_le_bytes()); // symbol table
        pe.extend_from_slice(&0u32.to_le_bytes()); // symbol count
        pe.extend_from_slice(&OPT_HEADER_SIZE.to_le_bytes());
        pe.extend_from_slice(&0x2022u16.to_le_bytes()); // EXECUTABLE | LARGE_ADDRESS_AWARE | DLL

        // Optional header (PE32+).
        let opt_start = pe.len();
        pe.extend_from_slice(&0x020Bu16.to_le_bytes()); // PE32+ magic
        pe.extend_from_slice(&[14, 0]); // linker version
        pe.extend_from_slice(&(section_data.len() as u32).to_le_bytes()); // size of code
        pe.extend_from_slice(&0u32.to_le_bytes());
        pe.extend_from_slice(&0u32.to_le_bytes());
        pe.extend_from_slice(&0x1000u32.to_le_bytes()); // entry point
        pe.extend_from_slice(&0x1000u32.to_le_bytes()); // base of code
        pe.extend_from_slice(&0x1_4000_0000u64.to_le_bytes()); // image base
        pe.extend_from_slice(&0x1000u32.to_le_bytes()); // section alignment
        pe.extend_from_slice(&512u32.to_le_bytes()); // file alignment
        for _ in 0..3 {
            pe.extend_from_slice(&[6, 0, 0, 0]); // os / image / subsystem versions
        }
        pe.extend_from_slice(&0u32.to_le_bytes()); // win32 version
        pe.extend_from_slice(&0x2000u32.to_le_bytes()); // size of image
        pe.extend_from_slice(&(raw_offset as u32).to_le_bytes()); // size of headers
        pe.extend_from_slice(&0u32.to_le_bytes()); // checksum
        pe.extend_from_slice(&3u16.to_le_bytes()); // subsystem: console
        pe.extend_from_slice(&0u16.to_le_bytes()); // dll characteristics
        for _ in 0..4 {
            pe.extend_from_slice(&0x10000u64.to_le_bytes()); // stack/heap
        }
        pe.extend_from_slice(&0u32.to_le_bytes()); // loader flags
        pe.extend_from_slice(&16u32.to_le_bytes()); // number of data directories
        let data_dirs_start = pe.len();
        pe.extend_from_slice(&[0u8; 16 * 8]); // data directories, patched below
        assert_eq!(pe.len() - opt_start, OPT_HEADER_SIZE as usize);

        // Section table.
        pe.extend_from_slice(section_name);
        pe.extend_from_slice(&(section_data.len() as u32).to_le_bytes()); // virtual size
        pe.extend_from_slice(&0x1000u32.to_le_bytes()); // virtual address
        pe.extend_from_slice(&(section_data.len() as u32).to_le_bytes()); // raw size
        pe.extend_from_slice(&(raw_offset as u32).to_le_bytes());
        pe.extend_from_slice(&0u32.to_le_bytes()); // relocations
        pe.extend_from_slice(&0u32.to_le_bytes()); // line numbers
        pe.extend_from_slice(&0u16.to_le_bytes());
        pe.extend_from_slice(&0u16.to_le_bytes());
        pe.extend_from_slice(&0x6000_0020u32.to_le_bytes()); // CODE | EXECUTE | READ

        pe.resize(raw_offset, 0);
        pe.extend_from_slice(section_data);

        if let Some(blob) = certificate {
            let cert_offset = pe.len();
            // WIN_CERTIFICATE: length, revision 2.0, PKCS#7 signed data.
            pe.extend_from_slice(&((8 + blob.len()) as u32).to_le_bytes());
            pe.extend_from_slice(&0x0200u16.to_le_bytes());
            pe.extend_from_slice(&0x0002u16.to_le_bytes());
            pe.extend_from_slice(blob);
            while !pe.len().is_multiple_of(8) {
                pe.push(0);
            }

            // Data directory 4 is the certificate table. Uniquely among the
            // directories, its address is a file offset, not an RVA.
            let dir = data_dirs_start + 4 * 8;
            let table_len = (pe.len() - cert_offset) as u32;
            pe[dir..dir + 4].copy_from_slice(&(cert_offset as u32).to_le_bytes());
            pe[dir + 4..dir + 8].copy_from_slice(&table_len.to_le_bytes());
        }
        pe
    }

    /// A genuine PKCS#7 SignedData blob, generated with openssl.
    const SIGNED_P7B: &[u8] = include_bytes!("../tests/fixtures/authenticode.p7b");

    #[test]
    fn hashes_match_known_vectors() {
        // The canonical empty-input digests.
        let h = hashes(b"");
        assert_eq!(h.md5, "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(h.sha1, "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(
            h.sha256,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let h = hashes(b"abc");
        assert_eq!(h.md5, "900150983cd24fb0d6963f7d28e17f72");
        assert_eq!(h.sha1, "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            h.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn entropy_spans_the_expected_range() {
        assert_eq!(shannon_entropy(b""), 0.0);
        assert_eq!(shannon_entropy(&[0x41; 1000]), 0.0, "uniform data has none");

        // Every byte value equally often is the maximum, 8 bits per byte.
        let all: Vec<u8> = (0..=255u8).collect();
        assert!((shannon_entropy(&all) - 8.0).abs() < 1e-9);

        // English-ish text sits well below a packed section.
        let text = b"the quick brown fox jumps over the lazy dog".repeat(10);
        let e = shannon_entropy(&text);
        assert!((3.0..5.0).contains(&e), "got {e}");
    }

    #[test]
    fn detects_container_formats() {
        assert_eq!(detect_format(b"\x7fELF\x02\x01"), "ELF");
        assert_eq!(detect_format(b"PK\x03\x04rest"), "ZIP");
        assert_eq!(detect_format(b"\x1f\x8b\x08\x00"), "gzip");
        assert_eq!(detect_format(b"\x28\xB5\x2F\xFDxx"), "zstd");
        assert_eq!(detect_format(b"  {\"a\":1}"), "JSON");
        assert_eq!(detect_format(b"[1,2]"), "JSON");
        assert_eq!(detect_format(b"\x00\x01\x02\x03"), "unknown");
    }

    #[test]
    fn distinguishes_a_pe_from_a_bare_dos_stub() {
        let pe = minimal_pe(0x8664, 0, (b".text\0\0\0", b"\x90\x90\x90\x90"));
        assert_eq!(detect_format(&pe), "PE");

        // "MZ" alone, with e_lfanew pointing at nothing meaningful.
        let mut stub = vec![0u8; 0x40];
        stub[0..2].copy_from_slice(b"MZ");
        assert_eq!(detect_format(&stub), "MZ (DOS)");
    }

    #[test]
    fn parses_a_real_pe_header() {
        const BUILD_TIME: u32 = 1_735_689_600; // 2025-01-01T00:00:00Z
        let body = b"\x90\x90\xCC\xCC\x48\x31\xC0\xC3";
        let pe = minimal_pe(0xAA64, BUILD_TIME, (b".text\0\0\0", body));

        let info = analyse_pe(&pe).expect("minimal PE must parse");
        assert_eq!(info.machine, "arm64");
        assert_eq!(info.machine_raw, 0xAA64);
        assert_eq!(info.timestamp, BUILD_TIME);
        assert!(info.is_64);
        assert!(info.is_dll, "characteristics set the DLL flag");
        assert_eq!(info.image_base, 0x1_4000_0000);
        assert_eq!(info.subsystem, Some(3));

        assert_eq!(info.sections.len(), 1);
        let section = &info.sections[0];
        assert_eq!(section.name, ".text");
        assert_eq!(section.raw_size, body.len() as u32);
        assert_eq!(section.entropy, shannon_entropy(body));

        // Nothing was signed or versioned, so those stay absent.
        assert!(info.signature.is_none());
    }

    #[test]
    fn maps_machine_types_used_by_eac_targets() {
        assert_eq!(machine_name(0x8664), "x64");
        assert_eq!(machine_name(0x014C), "x86");
        assert_eq!(machine_name(0xAA64), "arm64");
        assert_eq!(machine_name(0xFFFF), "other");
    }

    #[test]
    fn rejects_non_pe_payloads() {
        assert!(analyse_pe(b"not a pe at all").is_none());
        assert!(analyse_pe(b"").is_none());
        assert!(analyse_pe(br#"{"modules":[]}"#).is_none());
    }

    #[test]
    fn formats_version_dwords() {
        // 1.2.3.4 packed the way VS_FIXEDFILEINFO stores it.
        assert_eq!(format_version(0x0001_0002, 0x0003_0004), "1.2.3.4");
        assert_eq!(format_version(0, 0), "0.0.0.0");
    }

    #[test]
    fn extracts_version_info_from_a_resource_blob() {
        let mut blob = vec![0xAAu8; 32];
        blob.extend_from_slice(&FIXED_FILE_INFO_MAGIC);
        blob.extend_from_slice(&0x0001_0000u32.to_le_bytes()); // struct version
        blob.extend_from_slice(&0x0001_0002u32.to_le_bytes()); // file version MS
        blob.extend_from_slice(&0x0003_0004u32.to_le_bytes()); // file version LS
        blob.extend_from_slice(&0x0005_0006u32.to_le_bytes()); // product version MS
        blob.extend_from_slice(&0x0007_0008u32.to_le_bytes()); // product version LS
        blob.extend_from_slice(&[0u8; 28]); // rest of the fixed block

        // A StringFileInfo pair: key, terminator, padding, value.
        blob.extend_from_slice(&to_utf16le("CompanyName"));
        blob.extend_from_slice(&[0, 0, 0, 0]);
        blob.extend_from_slice(&to_utf16le("Epic Games, Inc."));
        blob.extend_from_slice(&[0, 0]);

        let info = parse_version_info(&blob).expect("version info must be found");
        assert_eq!(info.file_version.as_deref(), Some("1.2.3.4"));
        assert_eq!(info.product_version.as_deref(), Some("5.6.7.8"));
        assert_eq!(
            info.strings.get("CompanyName").map(String::as_str),
            Some("Epic Games, Inc.")
        );
    }

    #[test]
    fn version_info_is_absent_when_nothing_matches() {
        assert!(parse_version_info(b"no version data here at all").is_none());
    }

    #[test]
    fn tlsh_distance_orders_by_similarity() {
        // TLSH needs a reasonable amount of varied input to produce a hash.
        let base: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();

        let mut small_patch = base.clone();
        for b in small_patch.iter_mut().take(32) {
            *b ^= 0xFF;
        }
        let unrelated: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(40503).wrapping_add(7) >> 5) as u8)
            .collect();

        assert!(tlsh(&base).is_some(), "base payload must hash");
        assert_eq!(tlsh_distance(&base, &base), Some(0), "identical is zero");

        let near = tlsh_distance(&base, &small_patch).unwrap();
        let far = tlsh_distance(&base, &unrelated).unwrap();
        assert!(
            near < far,
            "a small patch ({near}) must be closer than unrelated data ({far})"
        );
    }

    #[test]
    fn tlsh_declines_on_input_that_is_too_small() {
        assert!(tlsh(b"tiny").is_none());
    }

    #[test]
    fn parses_certificates_out_of_a_real_pkcs7_blob() {
        let certs = parse_pkcs7_certs(SIGNED_P7B);
        assert_eq!(certs.len(), 1, "fixture carries one signer certificate");

        let cert = &certs[0];
        assert!(
            cert.subject.contains("EAC Tracker Test Signer"),
            "got subject {}",
            cert.subject
        );
        assert!(
            cert.issuer.contains("EAC Tracker Test Signer"),
            "self-signed"
        );
        assert!(cert.not_after > cert.not_before);
        // The fixture was generated with -days 3650.
        assert_eq!((cert.not_after - cert.not_before) / 86_400, 3650);
        assert!(!cert.serial.is_empty());
    }

    #[test]
    fn surfaces_the_signature_from_a_signed_image() {
        let pe = minimal_pe_with_cert(
            0x8664,
            0,
            (b".text\0\0\0", b"\x90\x90\x90\x90"),
            Some(SIGNED_P7B),
        );
        let info = analyse_pe(&pe).expect("signed PE must parse");

        let sig = info.signature.expect("signature must be surfaced");
        assert_eq!(sig.entry_count, 1);
        assert!(sig.total_bytes >= SIGNED_P7B.len());
        assert_eq!(sig.certificates.len(), 1);
        assert!(
            sig.certificates[0]
                .subject
                .contains("EAC Tracker Test Signer"),
            "got {}",
            sig.certificates[0].subject
        );
    }

    #[test]
    fn malformed_signature_blobs_are_ignored_rather_than_fatal() {
        assert!(parse_pkcs7_certs(b"not der at all").is_empty());
        assert!(parse_pkcs7_certs(&[]).is_empty());
    }
}
