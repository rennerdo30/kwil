//! Recoverable Windows pair installation. Also built on Unix for filesystem tests.
//! This is transactional, not a crash-atomic swap of two directory entries.

use crate::{Error, expected_sha256, hex};
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

const JOURNAL: &str = ".kwil-update-journal";
const LOCK: &str = ".kwil-update.lock";
const FILES: [&str; 2] = ["kwil.exe", "kwil.com"];
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique() -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_nanos()
    )
}

fn lock(dir: &Path) -> Result<File, Error> {
    // Keep the lock file: unlinking it would permit locks on different inodes.
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(dir.join(LOCK))?;
    file.try_lock().map_err(|e| {
        Error(format!("update/startup lock unavailable: {e}; another Kwil may be updating; retry after it finishes"))
    })?;
    Ok(file)
}

fn digest(path: &Path) -> Result<String, Error> {
    let mut file = File::open(path)?;
    let mut hash = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hash.update(&buf[..n]);
    }
    Ok(hex(&hash.finalize()))
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

fn copy_synced(from: &Path, to: &Path) -> Result<(), Error> {
    let mut src = File::open(from)?;
    let mut dst = OpenOptions::new().write(true).create_new(true).open(to)?;
    std::io::copy(&mut src, &mut dst)?;
    dst.sync_all()?;
    Ok(())
}

// Flat, uniquely created staging folders only. A running Windows image can
// keep a retired file locked; leave that file/folder rather than lose evidence.
fn cleanup(stage: &Path) {
    if let Ok(files) = fs::read_dir(stage) {
        for file in files.flatten() {
            if file.file_type().is_ok_and(|t| t.is_file()) {
                let _ = fs::remove_file(file.path());
            }
        }
    }
    let _ = fs::remove_dir(stage);
}

struct Journal {
    stage: PathBuf,
    old: [Option<String>; 2],
    new: [String; 2],
}

impl Journal {
    fn publish(&self, dir: &Path) -> Result<(), Error> {
        let name =
            self.stage.file_name().and_then(|v| v.to_str()).ok_or_else(|| Error("invalid staging folder".into()))?;
        let text = format!(
            "kwil-pair-v1\n{name}\n{}\n{}\n{}\n{}\n",
            self.old[0].as_deref().unwrap_or("-"),
            self.old[1].as_deref().unwrap_or("-"),
            self.new[0],
            self.new[1]
        );
        let temp = self.stage.join("journal");
        write_synced(&temp, text.as_bytes())?;
        fs::rename(temp, dir.join(JOURNAL))?;
        Ok(())
    }

    fn read(dir: &Path) -> Result<Self, Error> {
        let text = fs::read_to_string(dir.join(JOURNAL))?;
        let lines: Vec<_> = text.lines().collect();
        let valid_hash = |v: &str| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit());
        if lines.len() != 6
            || lines[0] != "kwil-pair-v1"
            || !lines[1].starts_with(".kwil-stage-")
            || !lines[1].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            || !valid_hash(lines[2])
            || !(lines[3] == "-" || valid_hash(lines[3]))
            || !valid_hash(lines[4])
            || !valid_hash(lines[5])
        {
            return Err(Error("invalid paired-update journal; recovery cannot proceed".into()));
        }
        Ok(Self {
            stage: dir.join(lines[1]),
            old: [Some(lines[2].into()), (lines[3] != "-").then(|| lines[3].into())],
            new: [lines[4].into(), lines[5].into()],
        })
    }
}

fn retire(target: &Path, stage: &Path) -> Result<(), Error> {
    if target.try_exists()? {
        // Windows permits moving the running image but not overwriting it.
        fs::rename(target, stage.join(format!("retired-{}", unique())))?;
    }
    Ok(())
}

fn replace(from: &Path, target: &Path, stage: &Path) -> Result<(), Error> {
    let incoming = stage.join(format!("incoming-{}", unique()));
    copy_synced(from, &incoming)?;
    retire(target, stage)?;
    fs::rename(incoming, target)?;
    Ok(())
}

fn matches(path: &Path, expected: &str) -> Result<(), Error> {
    if digest(path)? != expected {
        return Err(Error(format!("checksum mismatch for {}", path.display())));
    }
    Ok(())
}

fn rollback(dir: &Path, journal: &Journal) -> Result<(), Error> {
    // Verify *every* backup before changing either installed path. Backups
    // remain immutable, so recovery can itself be interrupted and retried.
    for (i, hash) in journal.old.iter().enumerate() {
        if let Some(hash) = hash {
            matches(&journal.stage.join(format!("old-{i}")), hash)?;
        }
    }
    for (i, name) in FILES.iter().enumerate() {
        let target = dir.join(name);
        if let Some(hash) = &journal.old[i] {
            if digest(&target).is_ok_and(|actual| actual == *hash) {
                continue;
            }
            replace(&journal.stage.join(format!("old-{i}")), &target, &journal.stage)?;
            matches(&target, hash)?;
        } else {
            retire(&target, &journal.stage)?;
        }
    }
    fs::remove_file(dir.join(JOURNAL))?;
    cleanup(&journal.stage);
    Ok(())
}

fn recover_locked(dir: &Path) -> Result<bool, Error> {
    if !dir.join(JOURNAL).try_exists()? {
        return Ok(false);
    }
    let journal = Journal::read(dir)?;
    if journal.stage.join("committed").try_exists()?
        && FILES.iter().enumerate().all(|(i, name)| digest(&dir.join(name)).is_ok_and(|h| h == journal.new[i]))
    {
        fs::remove_file(dir.join(JOURNAL))?;
        cleanup(&journal.stage);
    } else {
        rollback(dir, &journal)?;
    }
    Ok(true)
}

pub(super) fn startup(dir: &Path) -> Result<(), Error> {
    startup_with(dir, || {})
}

fn startup_with(dir: &Path, after_shared_check: impl FnOnce()) -> Result<(), Error> {
    // No update has begun while the permanent lock file is absent. This
    // permits read-only/package-managed installs to start without writing.
    // An updater creating the lock after this check only changes an image
    // that this process has already loaded, like any ordinary live update.
    let shared = match File::open(dir.join(LOCK)) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && !dir.join(JOURNAL).try_exists()? => return Ok(()),
        Err(e) => return Err(Error(format!("cannot check update/startup lock: {e}"))),
    };
    shared.try_lock_shared().map_err(|e| {
        Error(format!("update/startup lock unavailable: {e}; an update is in progress; retry after it finishes"))
    })?;
    if !dir.join(JOURNAL).try_exists()? {
        return Ok(());
    }
    drop(shared);
    after_shared_check();
    let _lock = lock(dir)?;
    match recover_locked(dir) {
        // The Windows loader may have mapped a file from the interrupted
        // generation before our lock check. Never run that image after repair.
        // Even if another startup removed the journal during lock upgrade,
        // this process observed an interrupted generation and must restart.
        Ok(_) => Err(Error("interrupted update recovered; restart Kwil to load the consistent executable pair".into())),
        Err(e) => Err(Error(format!(
            "update recovery failed: {e}; Kwil will not start; keep the journal/backups and reinstall a verified executable pair"
        ))),
    }
}

pub(super) fn install(dir: &Path, assets: [(&str, &[u8]); 2], sums: &str) -> Result<(), Error> {
    install_with(dir, assets, sums, |_, from, target, stage| replace(from, target, stage))
}

fn install_with(
    dir: &Path,
    assets: [(&str, &[u8]); 2],
    sums: &str,
    mut replace_asset: impl FnMut(usize, &Path, &Path, &Path) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut hashes = [String::new(), String::new()];
    // Neither installed files nor staging/lock metadata is touched on mismatch.
    for (i, (name, bytes)) in assets.iter().enumerate() {
        let expected = expected_sha256(sums, name).ok_or_else(|| Error(format!("SHA256SUMS lists no {name}")))?;
        hashes[i] = hex(&Sha256::digest(bytes));
        if hashes[i] != expected {
            return Err(Error(format!("checksum mismatch for {name}")));
        }
    }
    let stage = dir.join(format!(".kwil-stage-{}", unique()));
    fs::create_dir(&stage)?;
    let mut published = false;
    let result = (|| {
        for (i, (_, bytes)) in assets.iter().enumerate() {
            write_synced(&stage.join(format!("new-{i}")), bytes)?;
        }
        let _lock = lock(dir)?;
        if recover_locked(dir)? {
            return Err(Error("previous interrupted update recovered; restart before updating".into()));
        }
        let mut old = [None, None];
        for (i, name) in FILES.iter().enumerate() {
            let target = dir.join(name);
            if target.try_exists()? {
                let hash = digest(&target)?;
                copy_synced(&target, &stage.join(format!("old-{i}")))?;
                matches(&stage.join(format!("old-{i}")), &hash)?;
                old[i] = Some(hash);
            } else if i == 0 {
                return Err(Error("kwil.exe is missing; cannot back up the installed executable".into()));
            }
        }
        let journal = Journal { stage: stage.clone(), old, new: hashes };
        journal.publish(dir)?;
        published = true;
        let change = (|| {
            // For an older exe-only install, add the recovery-capable console
            // first. At least one entry point then survives every rename gap.
            let order = if journal.old[1].is_none() { [1, 0] } else { [0, 1] };
            for i in order {
                let name = FILES[i];
                matches(&stage.join(format!("new-{i}")), &journal.new[i])?;
                replace_asset(i, &stage.join(format!("new-{i}")), &dir.join(name), &stage)?;
                matches(&dir.join(name), &journal.new[i])?;
            }
            write_synced(&stage.join("committed"), b"kwil-pair-v1\n")?;
            Ok::<_, Error>(())
        })();
        if let Err(e) = change {
            return match rollback(dir, &journal) {
                Ok(()) => Err(Error(format!("update failed, both executables restored: {e}"))),
                Err(recovery) => Err(Error(format!(
                    "update failed: {e}; rollback failed: {recovery}; keep journal/backups; startup will retry recovery"
                ))),
            };
        }
        fs::remove_file(dir.join(JOURNAL))?;
        Ok(())
    })();
    if !published || !dir.join(JOURNAL).exists() {
        cleanup(&stage);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    const EXE: &[u8] = b"new GUI executable";
    const COM: &[u8] = b"new console executable";
    const ASSETS: [(&str, &[u8]); 2] = [("kwil-target.exe", EXE), ("kwil-target.com", COM)];

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            // unique() includes a per-process monotonic counter, PID and time.
            let dir = std::env::temp_dir().join(format!("kwil-pair-test-{}", unique()));
            fs::create_dir(&dir).unwrap();
            fs::write(dir.join(FILES[0]), b"old GUI").unwrap();
            fs::write(dir.join(FILES[1]), b"old console").unwrap();
            Self(dir)
        }
        fn old(&self) {
            assert_eq!(fs::read(self.0.join(FILES[0])).unwrap(), b"old GUI");
            assert_eq!(fs::read(self.0.join(FILES[1])).unwrap(), b"old console");
        }
        fn prepared(&self) -> Journal {
            let _lock = lock(&self.0).unwrap();
            let stage = self.0.join(format!(".kwil-stage-{}", unique()));
            fs::create_dir(&stage).unwrap();
            let mut old = [None, None];
            for (i, name) in FILES.iter().enumerate() {
                copy_synced(&self.0.join(name), &stage.join(format!("old-{i}"))).unwrap();
                old[i] = Some(digest(&self.0.join(name)).unwrap());
                write_synced(&stage.join(format!("new-{i}")), ASSETS[i].1).unwrap();
            }
            let j = Journal { stage, old, new: [hex(&Sha256::digest(EXE)), hex(&Sha256::digest(COM))] };
            j.publish(&self.0).unwrap();
            j
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            if let Ok(files) = fs::read_dir(&self.0) {
                for file in files.flatten() {
                    if file.file_type().is_ok_and(|t| t.is_dir()) {
                        cleanup(&file.path());
                    }
                }
            }
            cleanup(&self.0);
        }
    }
    fn sums() -> String {
        format!("{}  {}\n{}  {}\n", hex(&Sha256::digest(EXE)), ASSETS[0].0, hex(&Sha256::digest(COM)), ASSETS[1].0)
    }

    #[test]
    fn both_replaced() {
        let f = Fixture::new();
        install(&f.0, ASSETS, &sums()).unwrap();
        assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), EXE);
        assert_eq!(fs::read(f.0.join(FILES[1])).unwrap(), COM);
        assert!(!f.0.join(JOURNAL).exists());
        startup(&f.0).unwrap();
    }

    #[test]
    fn second_replace_fails_both_restored() {
        let f = Fixture::new();
        let mut saw_first_replaced = false;
        let err = install_with(&f.0, ASSETS, &sums(), |i, from, target, stage| {
            if i == 1 {
                assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), EXE);
                saw_first_replaced = true;
                return Err(Error("injected second replace failure".into()));
            }
            replace(from, target, stage)
        })
        .unwrap_err();
        assert!(saw_first_replaced);
        assert!(err.0.contains("both executables restored"));
        f.old();
        assert!(!f.0.join(JOURNAL).exists());
    }

    #[test]
    fn interrupted_journal_recovered_on_start() {
        let f = Fixture::new();
        let journal = f.prepared();
        replace(&journal.stage.join("new-0"), &f.0.join(FILES[0]), &journal.stage).unwrap();
        // Simulates termination after the first replacement and before commit.
        let err = startup(&f.0).unwrap_err();
        assert!(err.0.contains("recovered; restart"));
        f.old();
        assert!(!f.0.join(JOURNAL).exists());
        startup(&f.0).unwrap();
    }

    #[test]
    fn concurrent_recovery_still_requires_loaded_image_restart() {
        let f = Fixture::new();
        let j = f.prepared();
        replace(&j.stage.join("new-0"), &f.0.join(FILES[0]), &j.stage).unwrap();
        let result = startup_with(&f.0, || {
            // Another startup repairs the pair after our shared check and
            // before we acquire the exclusive recovery lock.
            assert!(startup(&f.0).unwrap_err().0.contains("recovered; restart"));
        });
        assert!(result.unwrap_err().0.contains("recovered; restart"));
        f.old();
    }

    #[test]
    fn interrupted_rename_gap_and_rollback_can_be_retried() {
        let f = Fixture::new();
        let j = f.prepared();
        // Simulate the gap after retiring the exe but before placing its new
        // copy. The surviving console entry can recover this state.
        retire(&f.0.join(FILES[0]), &j.stage).unwrap();
        assert!(!f.0.join(FILES[0]).exists());
        assert!(startup(&f.0).unwrap_err().0.contains("recovered; restart"));
        f.old();
        let j = f.prepared();
        // A recovery can itself stop after restoring the exe. Its immutable
        // backups must still restore the console on the next attempt.
        replace(&j.stage.join("new-1"), &f.0.join(FILES[1]), &j.stage).unwrap();
        assert!(startup(&f.0).unwrap_err().0.contains("recovered; restart"));
        f.old();
        startup(&f.0).unwrap();
    }

    #[test]
    fn lock_contention_blocks_startup_and_update() {
        let f = Fixture::new();
        let held = lock(&f.0).unwrap();
        assert!(startup(&f.0).unwrap_err().0.contains("lock unavailable"));
        assert!(install(&f.0, ASSETS, &sums()).unwrap_err().0.contains("lock unavailable"));
        f.old();
        drop(held);
        startup(&f.0).unwrap();
        install(&f.0, ASSETS, &sums()).unwrap();
    }

    #[test]
    fn checksum_mismatch_touches_nothing() {
        let f = Fixture::new();
        let bad = [(ASSETS[0].0, EXE), (ASSETS[1].0, b"corrupt".as_slice())];
        assert!(install(&f.0, bad, &sums()).unwrap_err().0.contains("checksum mismatch"));
        f.old();
        assert_eq!(fs::read_dir(&f.0).unwrap().count(), 2);
    }

    #[test]
    fn missing_backup_fails_closed_without_changing_either_file() {
        let f = Fixture::new();
        let j = f.prepared();
        replace(&j.stage.join("new-0"), &f.0.join(FILES[0]), &j.stage).unwrap();
        fs::remove_file(j.stage.join("old-1")).unwrap();
        assert!(startup(&f.0).unwrap_err().0.contains("recovery failed"));
        assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), EXE);
        assert_eq!(fs::read(f.0.join(FILES[1])).unwrap(), b"old console");
        assert!(f.0.join(JOURNAL).exists());
    }

    #[test]
    fn interrupted_after_commit_keeps_both_new() {
        let f = Fixture::new();
        let j = f.prepared();
        for (i, name) in FILES.iter().enumerate() {
            replace(&j.stage.join(format!("new-{i}")), &f.0.join(name), &j.stage).unwrap();
        }
        write_synced(&j.stage.join("committed"), b"kwil-pair-v1\n").unwrap();
        assert!(startup(&f.0).unwrap_err().0.contains("recovered; restart"));
        assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), EXE);
        assert_eq!(fs::read(f.0.join(FILES[1])).unwrap(), COM);
        assert!(!f.0.join(JOURNAL).exists());
    }

    #[test]
    fn incomplete_committed_pair_uses_verified_backups() {
        let f = Fixture::new();
        let j = f.prepared();
        replace(&j.stage.join("new-0"), &f.0.join(FILES[0]), &j.stage).unwrap();
        // A commit marker can survive a failed rollback. It must never make
        // recovery give up on intact backups or accept an incomplete pair.
        write_synced(&j.stage.join("committed"), b"kwil-pair-v1\n").unwrap();
        assert!(startup(&f.0).unwrap_err().0.contains("recovered; restart"));
        f.old();
        assert!(!f.0.join(JOURNAL).exists());
    }

    #[test]
    fn exe_only_install_gains_console_and_rolls_back_absence() {
        let f = Fixture::new();
        fs::remove_file(f.0.join(FILES[1])).unwrap();
        let err = install_with(&f.0, ASSETS, &sums(), |i, from, to, stage| {
            if i == 1 {
                assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), b"old GUI");
            }
            replace(from, to, stage)?;
            if i == 1 { Err(Error("failed after adding console".into())) } else { Ok(()) }
        })
        .unwrap_err();
        assert!(err.0.contains("both executables restored"));
        assert_eq!(fs::read(f.0.join(FILES[0])).unwrap(), b"old GUI");
        assert!(!f.0.join(FILES[1]).exists());
        install(&f.0, ASSETS, &sums()).unwrap();
        assert_eq!(fs::read(f.0.join(FILES[1])).unwrap(), COM);
    }
}
