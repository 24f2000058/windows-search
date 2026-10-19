//! Validation probe for the reference PC (run elevated for journal access):
//!
//! ```text
//! ntfs_probe.exe            list volumes
//! ntfs_probe.exe C:         query journal, enumerate MFT, read journal
//! ```
//!
//! Prints counts and timings, plus the behaviours this adapter has not yet
//! verified on real hardware (hard-link records, enumeration metadata).

#[cfg(windows)]
fn main() {
    use flint_index::source::JournalSource;
    use flint_ntfs::win::{list_volumes, NtfsVolume};
    use std::collections::HashMap;
    use std::time::Instant;
    let arg = std::env::args().nth(1);
    let vols = list_volumes();
    for v in &vols {
        println!("{:<52} {:<8} {:<10} type={} mounts={:?}", v.guid_path, v.filesystem, v.label, v.drive_type, v.mount_points);
    }
    let Some(want) = arg else { return };
    let want = want.trim_end_matches('\\').to_uppercase();
    let Some(info) = vols.into_iter().find(|v| v.mount_points.iter().any(|m| m.trim_end_matches('\\').eq_ignore_ascii_case(&want))) else {
        eprintln!("no volume mounted at {want}");
        std::process::exit(2);
    };
    let mut vol = match NtfsVolume::open(info) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("open failed: {e:?} (journal access needs elevation; the app falls back to traversal)");
            std::process::exit(3);
        }
    };
    println!("root frn: {:#x}", vol.root_frn);
    match vol.query() {
        Ok(j) => println!("journal: {j:?}"),
        Err(e) => println!("journal query failed: {e:?}"),
    }
    // Raw $MFT read on its own, then the combined enumeration used by the index.
    let s = Instant::now();
    let (mut mn, mut sized) = (0u64, 0u64);
    let r = vol.enumerate_mft(&mut |rec| {
        mn += 1;
        if rec.size.is_some_and(|x| x > 0) {
            sized += 1;
        }
        true
    });
    println!("raw $MFT: {r:?}, {mn} link records ({sized} with size) in {:.2}s", s.elapsed().as_secs_f64());
    let s = Instant::now();
    let (mut n, mut dirs, mut per_frn): (u64, u64, HashMap<u64, u32>) = (0, 0, HashMap::new());
    let r = vol.enumerate(&mut |rec| {
        n += 1;
        if rec.attrs & 0x10 != 0 {
            dirs += 1;
        }
        *per_frn.entry(rec.frn).or_default() += 1;
        true
    });
    let multi = per_frn.values().filter(|&&c| c > 1).count();
    println!("enumerate: {r:?}, {n} records ({dirs} dirs) in {:.2}s", s.elapsed().as_secs_f64());
    println!("  FRNs with >1 enumeration record (hard links): {multi}; MFT stats {:?}, MFT error {:?}", vol.last_mft, vol.last_mft_error);
    let s = Instant::now();
    let mut un = 0u64;
    let r = vol.enumerate_usn_data(&mut |_| {
        un += 1;
        true
    });
    println!("FSCTL_ENUM_USN_DATA alone: {r:?}, {un} records in {:.2}s (compare link counts with raw $MFT)", s.elapsed().as_secs_f64());
    if let Ok(j) = vol.query() {
        let s = Instant::now();
        let from = (j.next_usn - 1_000_000).max(j.lowest_valid_usn);
        match vol.read(j.journal_id, from, usize::MAX) {
            Ok((recs, next)) => println!("read journal: {} records, next={next}, {:.1} ms", recs.len(), s.elapsed().as_secs_f64() * 1e3),
            Err(e) => println!("read journal failed: {e:?}"),
        }
    }
    if let Some((&frn, _)) = per_frn.iter().next() {
        println!("meta({frn:#x}) = {:?}", vol.meta(frn));
        println!("links({frn:#x}) = {:?}", vol.links(frn));
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("ntfs_probe only runs on Windows");
}
