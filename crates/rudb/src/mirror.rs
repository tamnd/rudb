//! Native mirrors of Parquet files: where they live, what they are called, and how one is made.
//!
//! Document 33 in `spec/storage-v3` is the design. The binder reads a file through a mirror the
//! catalog holds and asks for one it does not, and this is what answers the ask: it finds the
//! mirror on disk by its key or builds it, opens it, and hands the catalog its table.
//!
//! A mirror is named by its key and nothing else records it, so a file on disk with the right name
//! is a mirror of the file as it is now, and a file that has changed has a key nobody has built.

use std::fs::File;
use std::path::{Path, PathBuf};

use rudb_catalog::FileStamp;
use rudb_common::{Error, Result};

use crate::config::Config;
use crate::database::Database;

/// Where mirrors go: `RUDB_MIRROR_DIR`, else the cache directory the platform convention names.
fn directory() -> Option<PathBuf> {
    let set = |name: &str| std::env::var_os(name).filter(|value| !value.is_empty());
    if let Some(directory) = set("RUDB_MIRROR_DIR") {
        return Some(PathBuf::from(directory));
    }
    if let Some(cache) = set("XDG_CACHE_HOME") {
        return Some(PathBuf::from(cache).join("rudb").join("mirror"));
    }
    set("HOME").map(|home| PathBuf::from(home).join(".cache").join("rudb").join("mirror"))
}

/// The key of the Parquet file at `path` under these options, as it is now.
///
/// Everything document 33 lists goes in: the path, the stamp, the options, and the footer, which is
/// the part a tool that restores a modification time cannot make agree with different contents.
/// The native format is folded in by [`rudb_native::content_name`].
fn key(path: &str, binary_as_string: bool, stamp: FileStamp) -> Result<u128> {
    let file = File::open(path)
        .map_err(|error| Error::io(format!("could not open {path} to mirror it: {error}")))?;
    let mut tail = [0; 8];
    if stamp.size < 12 {
        return Err(Error::io(format!("{path} is too short to be a Parquet file")));
    }
    read_at(&file, stamp.size - 8, &mut tail, path)?;
    if &tail[4..] != b"PAR1" {
        return Err(Error::io(format!("{path} does not end the way a Parquet file does")));
    }
    let length = u64::from(u32::from_le_bytes(tail[..4].try_into().expect("four bytes")));
    if length + 12 > stamp.size {
        return Err(Error::io(format!("{path} states a footer longer than itself")));
    }
    let mut material = Vec::with_capacity(path.len() + 64 + length as usize);
    material.extend_from_slice(path.as_bytes());
    material.push(0);
    material.push(u8::from(binary_as_string));
    material.extend_from_slice(&stamp.device.to_le_bytes());
    material.extend_from_slice(&stamp.inode.to_le_bytes());
    material.extend_from_slice(&stamp.size.to_le_bytes());
    material.extend_from_slice(&stamp.modified.to_le_bytes());
    let footer = material.len();
    material.resize(footer + length as usize, 0);
    read_at(&file, stamp.size - 8 - length, &mut material[footer..], path)?;
    Ok(rudb_native::content_name(&material))
}

#[cfg(unix)]
fn read_at(file: &File, offset: u64, into: &mut [u8], path: &str) -> Result<()> {
    use std::os::unix::fs::FileExt;
    file.read_exact_at(into, offset)
        .map_err(|error| Error::io(format!("could not read {path} to mirror it: {error}")))
}

#[cfg(not(unix))]
fn read_at(file: &File, offset: u64, into: &mut [u8], path: &str) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = file;
    file.seek(SeekFrom::Start(offset))
        .and_then(|_| file.read_exact(into))
        .map_err(|error| Error::io(format!("could not read {path} to mirror it: {error}")))
}

/// The mirror of the Parquet file at `path`, built first if there is none, with the stamp it was
/// made against, or `None` where there is nowhere to put one or the file changed while it was read.
///
/// `config` is the asking database's, so the load runs under the same memory limit and threads as
/// the query that asked, with mirroring off so that the load reads the file itself.
///
/// # Errors
///
/// When the file cannot be read or the load fails. The caller reads the file directly instead.
pub(crate) fn ensure(
    path: &str,
    binary_as_string: bool,
    config: Config,
) -> Result<Option<(FileStamp, rudb_native::Reader)>> {
    let Some(directory) = directory() else { return Ok(None) };
    let Some(stamp) = FileStamp::of(Path::new(path)) else { return Ok(None) };
    let name = key(path, binary_as_string, stamp)?;
    let mirror = directory.join(format!("{name:032x}.rudb"));
    if !mirror.exists() {
        build(path, binary_as_string, config, &directory, &mirror)?;
        // A file written while it was being loaded may have been read half before and half after,
        // and the mirror is named for the before. Nobody would ask for it by that name again, since
        // the stamp moved, so it is removed rather than left to be found.
        if FileStamp::of(Path::new(path)) != Some(stamp) {
            let _ = std::fs::remove_file(&mirror);
            return Ok(None);
        }
    }
    let native = rudb_native::Catalog::open(&mirror)?;
    Ok(Some((stamp, native.table(TABLE)?)))
}

/// The one table a mirror holds.
const TABLE: &str = "mirror";

/// Loads the file into a new native file and publishes it at `mirror` by rename.
///
/// The load is the contract's own statement, `CREATE TABLE ... AS SELECT * FROM read_parquet`, so
/// the mirror holds what the reader returns and there is no second decoder to disagree with it.
fn build(
    path: &str,
    binary_as_string: bool,
    config: Config,
    directory: &Path,
    mirror: &Path,
) -> Result<()> {
    std::fs::create_dir_all(directory).map_err(|error| {
        Error::io(format!("could not make the mirror directory {}: {error}", directory.display()))
    })?;
    let temporary = mirror.with_extension(format!("{}.building", std::process::id()));
    let _ = std::fs::remove_file(&temporary);
    let loaded = (|| {
        let spelled = temporary
            .to_str()
            .ok_or_else(|| Error::io("the mirror directory's name is not UTF-8".to_string()))?;
        let database =
            Database::open_with(spelled, config.with_parquet_mirror(false).with_read_only(false))?;
        let quoted = path.replace('\'', "''");
        database.execute(&format!(
            "CREATE TABLE {TABLE} AS SELECT * FROM read_parquet('{quoted}', \
             binary_as_string={binary_as_string})"
        ))?;
        database.execute("CHECKPOINT")?;
        drop(database);
        crate::database::publish(&rudb_io::RealFilesystem::new(), &temporary, mirror).map_err(
            |error| {
                Error::io(format!("could not publish the mirror {}: {error}", mirror.display()))
            },
        )
    })();
    if loaded.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    loaded
}
