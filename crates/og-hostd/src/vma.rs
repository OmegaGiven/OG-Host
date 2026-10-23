//! Import Proxmox VE backups (vzdump `.vma`, `.vma.zst`, `.vma.gz`) into storage pools: each
//! disk becomes a volume, written sparsely (VMA leaves out zero blocks; a new thin volume reads
//! as zeros). Streams, so a backup can come straight over SSH from where it's kept.
//!
//! Format (Proxmox's vma_spec.txt): a header (magic `VMA\0`, version 1, uuid, the blob buffer
//! with the config files and device names, 256 device entries), then extents: a 512-byte header
//! (`VMAE`, 59 block infos: 16-bit mask of the non-zero 4 KiB blocks in a 64 KiB cluster, device
//! id, cluster number) followed by those blocks. Integers are big-endian.

use std::io::{Read, Write};
use std::os::unix::fs::FileExt;
use std::path::Path;

use anyhow::{bail, Context, Result};

const CLUSTER: u64 = 65536;
const BLOCK: usize = 4096;

pub struct Imported {
    /// (device name in the backup, volume, size)
    pub disks: Vec<(String, String, u64)>,
    /// config files in the backup (qemu-server.conf, ...)
    pub configs: Vec<(String, Vec<u8>)>,
}

fn be16(b: &[u8]) -> u16 {
    u16::from_be_bytes([b[0], b[1]])
}
fn be32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}
fn be64(b: &[u8]) -> u64 {
    u64::from_be_bytes(b[..8].try_into().unwrap())
}

/// A blob from the header's blob buffer: a 2-byte size, then the data.
fn blob(buf: &[u8], off: u32) -> Option<&[u8]> {
    let o = off as usize;
    if o == 0 || o + 2 > buf.len() {
        return None;
    }
    // the size is little-endian in Proxmox's writer, despite the spec's "big-endian" heading
    let le = u16::from_le_bytes([buf[o], buf[o + 1]]) as usize;
    let be = be16(&buf[o..]) as usize;
    let n = if o + 2 + le <= buf.len() { le } else { be };
    buf.get(o + 2..o + 2 + n)
}

fn cstr(b: &[u8]) -> String {
    String::from_utf8_lossy(b.split(|c| *c == 0).next().unwrap_or(b)).into_owned()
}

/// Open a backup file (or `-` for stdin), decompressing zstd/gzip by their magic bytes.
pub fn open(path: &Path) -> Result<Box<dyn Read>> {
    let mut r: Box<dyn Read> = if path.as_os_str() == "-" { Box::new(std::io::stdin().lock()) } else { Box::new(std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?) };
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic)?;
    let r: Box<dyn Read> = Box::new(std::io::Cursor::new(magic.to_vec()).chain(r));
    Ok(match magic {
        [0x28, 0xb5, 0x2f, 0xfd] => Box::new(zstd::stream::read::Decoder::new(r)?),
        [0x1f, 0x8b, ..] => Box::new(flate2::read::MultiGzDecoder::new(r)),
        _ => r,
    })
}

/// Read a VMA stream; `make_volume(device name, size)` gives the volume to write each disk into
/// (its path); returns what was imported. `progress(bytes)` is called now and then.
pub fn import(mut r: impl Read, mut make_volume: impl FnMut(&str, u64) -> Result<(String, std::path::PathBuf)>, mut progress: impl FnMut(u64)) -> Result<Imported> {
    let mut head = vec![0u8; 12288];
    r.read_exact(&mut head).context("reading the VMA header")?;
    if &head[0..4] != b"VMA\0" {
        bail!("not a VMA backup (vzdump .vma, .vma.zst or .vma.gz)");
    }
    if be32(&head[4..]) != 1 {
        bail!("VMA version {} isn't supported", be32(&head[4..]));
    }
    let uuid = head[8..24].to_vec();
    let (blob_off, blob_size, header_size) = (be32(&head[48..]) as usize, be32(&head[52..]) as usize, be32(&head[56..]) as usize);
    if header_size < 12288 || blob_off + blob_size > header_size || header_size > 64 << 20 {
        bail!("damaged VMA header");
    }
    head.resize(header_size, 0);
    r.read_exact(&mut head[12288..]).context("reading the VMA header")?;
    let blobs = &head[blob_off..blob_off + blob_size];
    let mut configs = vec![];
    for i in 0..256 {
        let (n, d) = (be32(&head[2044 + i * 4..]), be32(&head[3068 + i * 4..]));
        if let (Some(name), Some(data)) = (blob(blobs, n), blob(blobs, d)) {
            configs.push((cstr(name), data.to_vec()));
        }
    }
    // devices: index = dev_id (0 is unused)
    let mut devs: Vec<Option<(String, u64, std::fs::File, String)>> = (0..256).map(|_| None).collect();
    let mut disks = vec![];
    for (i, dev) in devs.iter_mut().enumerate().skip(1) {
        let e = &head[4096 + i * 32..4096 + (i + 1) * 32];
        let size = be64(&e[8..]);
        let Some(name) = blob(blobs, be32(e)).map(cstr) else { continue };
        if size == 0 || name == "vmstate" {
            continue;
        }
        let (vol, path) = make_volume(&name, size)?;
        let f = std::fs::OpenOptions::new().write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
        disks.push((name.clone(), vol.clone(), size));
        *dev = Some((name, size, f, vol));
    }
    if disks.is_empty() {
        bail!("the backup holds no disks");
    }
    let mut eh = [0u8; 512];
    let mut block = vec![0u8; BLOCK];
    let mut done: u64 = 0;
    loop {
        match r.read_exact(&mut eh) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        if &eh[0..4] != b"VMAE" || eh[8..24] != uuid[..] {
            bail!("damaged backup: bad extent header after {done} bytes");
        }
        for k in 0..59 {
            let bi = &eh[40 + k * 8..48 + k * 8];
            let mask = be16(bi);
            let dev_id = bi[3] as usize;
            let cluster = be32(&bi[4..]) as u64;
            if dev_id == 0 {
                continue;
            }
            let dev = devs[dev_id].as_ref().with_context(|| format!("damaged backup: unknown device {dev_id}"))?;
            for j in 0..16 {
                if mask & (1 << j) == 0 {
                    continue; // a zero block: nothing stored, nothing to write
                }
                r.read_exact(&mut block).context("reading a block")?;
                let off = cluster * CLUSTER + j as u64 * BLOCK as u64;
                if off >= dev.1 {
                    bail!("damaged backup: block past the end of {}", dev.0);
                }
                if block.iter().any(|b| *b != 0) {
                    dev.2.write_all_at(&block, off)?;
                }
                done += BLOCK as u64;
            }
        }
        progress(done);
    }
    for d in devs.into_iter().flatten() {
        d.2.sync_all()?;
    }
    Ok(Imported { disks, configs })
}

/// `og-hostd import-vma FILE|- --pool POOL --name NAME`: disks as volumes `vm-NAME-<device>`,
/// configs saved next to them in `<root>/imported/NAME/`.
pub fn cli(root: &Path, file: &Path, pool: &str, name: &str) -> Result<()> {
    if !crate::vms::valid_name(name) {
        bail!("names are lowercase letters, digits and -");
    }
    let pools = crate::pools::Pools::new(root);
    let r = open(file)?;
    let t0 = std::time::Instant::now();
    let mut last = std::time::Instant::now();
    let imp = import(
        r,
        |dev, size| {
            let short: String = dev.trim_start_matches("drive-").chars().filter(|c| c.is_ascii_alphanumeric()).collect();
            let vol = format!("{pool}:vm-{name}-{short}");
            let gib = size.div_ceil(1 << 30);
            pools.create(&vol, gib).with_context(|| format!("making {vol}"))?;
            eprintln!("{dev}: {:.1} GiB -> {vol}", size as f64 / (1u64 << 30) as f64);
            Ok((vol.clone(), pools.path(&vol)?))
        },
        |done| {
            if last.elapsed().as_secs() >= 10 {
                last = std::time::Instant::now();
                eprintln!("  {:.1} GiB of data written ({:.0} MB/s)", done as f64 / (1u64 << 30) as f64, done as f64 / 1e6 / t0.elapsed().as_secs_f64().max(1.0));
            }
        },
    )?;
    let dir = root.join("imported").join(name);
    std::fs::create_dir_all(&dir)?;
    for (n, data) in &imp.configs {
        let safe: String = n.chars().filter(|c| c.is_ascii_alphanumeric() || "._-".contains(*c)).collect();
        std::fs::File::create(dir.join(&safe))?.write_all(data)?;
    }
    for (dev, vol, size) in &imp.disks {
        println!("{dev}\t{vol}\t{size}");
    }
    eprintln!("imported {} disk(s) in {:.0} s; configs in {}", imp.disks.len(), t0.elapsed().as_secs_f64(), dir.display());
    Ok(())
}
