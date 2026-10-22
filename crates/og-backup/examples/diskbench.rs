//! diskbench IMAGE STORE [stream]: back up a disk image (blockwise, or as a stream like
//! og-hostd did before), restore it, back it up again; prints timings.
use std::io::{Read, Seek, Write};
use std::time::Instant;
fn main() -> anyhow::Result<()> {
    let a: Vec<String> = std::env::args().collect();
    let (img, store) = (std::path::Path::new(&a[1]), std::path::Path::new(&a[2]));
    let stream = a.get(3).map(String::as_str) == Some("stream");
    let repo = og_backup::Repo::open(store, None)?;
    let ok = || Ok(());
    let mut first = None;
    for (i, label) in ["backup", "backup_again"].iter().enumerate() {
        let t = Instant::now();
        let mut snap = repo.begin("vm/bench", og_backup::now() + i as u64)?;
        let f = std::fs::File::open(img)?;
        let info = if stream {
            let mut w = snap.archive("raw-disk0")?;
            std::io::copy(&mut &f, &mut w)?;
            w.finish()?
        } else {
            snap.disk_archive("raw-disk0", &f, &ok)?
        };
        eprintln!("  {} bytes, {} new", info.size, info.new_bytes);
        let m = snap.commit("h", serde_json::json!({}), "", 0)?;
        println!("{label}_s {:.2}", t.elapsed().as_secs_f64());
        if i == 0 {
            first = Some(m);
        }
    }
    let m = first.unwrap();
    let out = store.join("restored.img");
    let _ = std::fs::remove_file(&out);
    let t = Instant::now();
    std::fs::File::create(&out)?.set_len(std::fs::metadata(img)?.len())?;
    let o = std::fs::OpenOptions::new().write(true).open(&out)?;
    if stream {
        let mut o = o;
        let mut rd = repo.reader(&m, "raw-disk0")?;
        let mut buf = vec![0u8; 1 << 20];
        loop {
            let mut n = 0;
            while n < buf.len() {
                let k = rd.read(&mut buf[n..])?;
                if k == 0 {
                    break;
                }
                n += k;
            }
            if n == 0 {
                break;
            }
            for blk in buf[..n].chunks(65536) {
                if blk.iter().all(|b| *b == 0) {
                    o.seek(std::io::SeekFrom::Current(blk.len() as i64))?;
                } else {
                    o.write_all(blk)?;
                }
            }
        }
        o.sync_all()?;
    } else {
        repo.restore_disk(&m, "raw-disk0", &o, true, &ok)?;
    }
    println!("restore_s {:.2}", t.elapsed().as_secs_f64());
    let t = Instant::now();
    let v = repo.verify(&m.id())?;
    println!(
        "verify_s {:.2} ok={}",
        t.elapsed().as_secs_f64(),
        v.verified.unwrap().ok
    );
    Ok(())
}
