use std::io;
use std::path::Path;

pub fn tree(root: &Path, shards: usize, files_per_shard: usize) -> io::Result<()> {
    for shard in 0..shards {
        let dir = root.join(format!("{shard:03}"));
        std::fs::create_dir_all(&dir)?;
        for i in 0..files_per_shard {
            std::fs::write(dir.join(format!("file-{i:04}.txt")), b"small fixture\n")?;
        }
    }
    Ok(())
}
