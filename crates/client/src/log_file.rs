use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
};

const MAX_BYTES: u64 = 5 * 1024 * 1024;

pub struct LogFile(PathBuf);

impl LogFile {
    pub fn new(path: PathBuf) -> io::Result<Self> {
        open_log(&path)?;
        Ok(Self(path))
    }
}

impl Write for LogFile {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut file = open_log(&self.0)?;
        let size = file.metadata()?.len();
        if size > 0 && size.saturating_add(bytes.len() as u64) > MAX_BYTES {
            drop(file);
            fs::rename(&self.0, self.0.with_extension("log.1"))?;
            file = open_log(&self.0)?;
        }
        file.write_all(bytes)?;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn open_log(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_at_five_mib_and_replaces_the_backup() -> io::Result<()> {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("rusttalk-log-{}-{nonce}", std::process::id()));
        fs::create_dir(&dir)?;
        let path = dir.join("rusttalk.log");
        let backup = path.with_extension("log.1");
        let mut log = LogFile::new(path.clone())?;
        fs::write(&backup, "old backup")?;
        OpenOptions::new()
            .write(true)
            .open(&path)?
            .set_len(MAX_BYTES)?;

        log.write_all(b"new entry\n")?;

        assert_eq!(fs::read(&path)?, b"new entry\n");
        assert_eq!(fs::metadata(&backup)?.len(), MAX_BYTES);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for file in [path, backup] {
                assert_eq!(fs::metadata(file)?.permissions().mode() & 0o777, 0o600);
            }
        }
        fs::remove_dir_all(dir)
    }
}
