// SPDX-FileCopyrightText: Copyright natyamatsya and sccache contributors
// SPDX-License-Identifier: Apache-2.0

//! ReFS block cloning via `FSCTL_DUPLICATE_EXTENTS_TO_FILE`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use std::sync::{LazyLock, RwLock};

use windows_sys::Win32::Foundation::HANDLE;
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ID_INFO, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileEndOfFileInfo,
    FileIdInfo, GetFileInformationByHandleEx, GetVolumeInformationByHandleW,
    SetFileInformationByHandle,
};
use windows_sys::Win32::System::IO::DeviceIoControl;
use windows_sys::Win32::System::Ioctl::{
    DUPLICATE_EXTENTS_DATA, FSCTL_DUPLICATE_EXTENTS_TO_FILE, FSCTL_GET_INTEGRITY_INFORMATION,
    FSCTL_GET_INTEGRITY_INFORMATION_BUFFER, FSCTL_SET_INTEGRITY_INFORMATION,
    FSCTL_SET_INTEGRITY_INFORMATION_BUFFER,
};

use super::CloneError;

/// `FILE_SUPPORTS_BLOCK_REFCOUNTING`, the authoritative answer to "can this
/// volume clone".
///
/// Declared here because windows-sys 0.52 does not export it; it first appears in
/// 0.61 under `Win32_System_SystemServices`, which is a large feature to pull in
/// for one constant. Preferred over comparing the filesystem name to "ReFS": the
/// flag is what the filesystem actually promises, and it stays correct for any
/// future filesystem that gains the capability.
const FILE_SUPPORTS_BLOCK_REFCOUNTING: u32 = 0x0800_0000;

/// Clone in bounded chunks. The ioctl takes a byte count, and very large single
/// requests are more likely to be refused; 1 GiB is comfortably cluster-aligned.
const MAX_CHUNK: i64 = 1 << 30;

/// Below this, a clone's handful of syscalls costs more than simply writing the
/// bytes. Expressed in clusters so it scales with the volume's geometry.
const MIN_CLONE_CLUSTERS: u64 = 4;

/// What a volume can do, remembered for the life of the process.
///
/// Keyed by the 64-bit volume serial from `FILE_ID_INFO` rather than by drive
/// letter, so mount points and substituted drives cannot alias. Process-global
/// rather than hung off the server, because in client-side mode the client does
/// the materializing and needs the same answers.
struct VolumeCaps {
    can_clone: bool,
    cluster_size: u64,
    integrity_algorithm: u16,
}

static VOLUME_CAPS: LazyLock<RwLock<HashMap<u64, VolumeCaps>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

fn io_err<T>(what: &str) -> Result<T, CloneError> {
    let e = std::io::Error::last_os_error();
    trace!("blockclone: {what} failed: {e}");
    Err(CloneError::Io(e))
}

/// The 64-bit volume serial for an open file.
fn volume_serial(file: &File) -> Result<u64, CloneError> {
    let mut info: FILE_ID_INFO = unsafe { std::mem::zeroed() };
    // SAFETY: `info` is a correctly sized, zeroed FILE_ID_INFO and the handle is
    // owned by `file` for the duration of the call.
    let ok = unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle() as HANDLE,
            FileIdInfo,
            (&mut info as *mut FILE_ID_INFO).cast(),
            std::mem::size_of::<FILE_ID_INFO>() as u32,
        )
    };
    if ok == 0 {
        return io_err("GetFileInformationByHandleEx(FileIdInfo)");
    }
    Ok(info.VolumeSerialNumber)
}

/// Cluster size and integrity settings, which also tells us whether this is ReFS
/// at all: anything else fails this ioctl with `ERROR_INVALID_FUNCTION`.
fn integrity_info(file: &File) -> Result<(u64, u16, u32), CloneError> {
    let mut out: FSCTL_GET_INTEGRITY_INFORMATION_BUFFER = unsafe { std::mem::zeroed() };
    let mut returned: u32 = 0;
    // SAFETY: output buffer is correctly sized for the documented output type;
    // no input buffer is required by this control code.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_GET_INTEGRITY_INFORMATION,
            std::ptr::null(),
            0,
            (&mut out as *mut FSCTL_GET_INTEGRITY_INFORMATION_BUFFER).cast(),
            std::mem::size_of::<FSCTL_GET_INTEGRITY_INFORMATION_BUFFER>() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(CloneError::Unsupported);
    }
    Ok((
        out.ClusterSizeInBytes as u64,
        out.ChecksumAlgorithm,
        out.Flags,
    ))
}

/// Whether the volume holding `file` advertises block cloning.
fn advertises_cloning(file: &File) -> bool {
    let mut flags: u32 = 0;
    // SAFETY: all optional out-parameters are null; only the flags word is
    // requested, and it is a valid pointer to a u32 we own.
    let ok = unsafe {
        GetVolumeInformationByHandleW(
            file.as_raw_handle() as HANDLE,
            std::ptr::null_mut(),
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut flags,
            std::ptr::null_mut(),
            0,
        )
    };
    ok != 0 && (flags & FILE_SUPPORTS_BLOCK_REFCOUNTING) != 0
}

fn caps_for(file: &File, serial: u64) -> Result<(bool, u64, u16), CloneError> {
    if let Ok(map) = VOLUME_CAPS.read()
        && let Some(c) = map.get(&serial)
    {
        return Ok((c.can_clone, c.cluster_size, c.integrity_algorithm));
    }

    let advertised = advertises_cloning(file);
    let (cluster_size, algorithm) = match integrity_info(file) {
        Ok((cluster, algo, _flags)) => (cluster, algo),
        Err(_) => (0, 0),
    };
    let can_clone = advertised && cluster_size > 0;

    if let Ok(mut map) = VOLUME_CAPS.write() {
        map.insert(
            serial,
            VolumeCaps {
                can_clone,
                cluster_size,
                integrity_algorithm: algorithm,
            },
        );
    }
    debug!(
        "blockclone: volume {serial:#x} clone={can_clone} cluster={cluster_size} integrity={algorithm}"
    );
    Ok((can_clone, cluster_size, algorithm))
}

/// Give up on a volume for the rest of the process.
///
/// A filesystem that refuses one clone for a structural reason will refuse them
/// all, and retrying an ioctl per file to relearn that is pure overhead.
fn poison(serial: u64) {
    if let Ok(mut map) = VOLUME_CAPS.write()
        && let Some(c) = map.get_mut(&serial)
    {
        c.can_clone = false;
    }
}

/// Whether files under `path` can be cloned. Best effort; a `false` here is
/// definitive but a `true` still leaves per-file reasons to fall back.
pub fn volume_supports_cloning(path: &Path) -> bool {
    // FILE_FLAG_BACKUP_SEMANTICS, without which a directory cannot be opened at
    // all on Windows -- and the thing being asked about is usually a directory.
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    let Ok(file) = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
        .open(path)
    else {
        return false;
    };
    let Ok(serial) = volume_serial(&file) else {
        return false;
    };
    caps_for(&file, serial)
        .map(|(ok, _, _)| ok)
        .unwrap_or(false)
}

/// Clone `src` to a newly created `dst`, sharing storage rather than copying it.
///
/// `dst` must not already exist.
pub fn clone_whole_file(src: &Path, dst: &Path) -> Result<(), CloneError> {
    let source = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(src)?;

    let len = source.metadata()?.len();
    let src_serial = volume_serial(&source)?;
    let (can_clone, cluster_size, algorithm) = caps_for(&source, src_serial)?;
    if !can_clone {
        return Err(CloneError::Unsupported);
    }
    if len < MIN_CLONE_CLUSTERS * cluster_size {
        return Err(CloneError::TooSmall);
    }

    // Exclusive while we build it: a half-cloned file must never be observable.
    let target = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .share_mode(0)
        .open(dst)?;

    let result = duplicate_extents(&source, &target, len, cluster_size, algorithm, src_serial);
    if result.is_err() {
        drop(target);
        let _ = std::fs::remove_file(dst);
    }
    result
}

fn duplicate_extents(
    source: &File,
    target: &File,
    len: u64,
    cluster_size: u64,
    algorithm: u16,
    src_serial: u64,
) -> Result<(), CloneError> {
    if volume_serial(target)? != src_serial {
        return Err(CloneError::DifferentVolume);
    }

    // Integrity streams must match, and can only be set while the file is empty.
    if algorithm != 0 {
        let mut set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: algorithm,
            Reserved: 0,
            Flags: 0,
        };
        let mut returned: u32 = 0;
        // SAFETY: input buffer is a correctly sized, initialized struct of the
        // type this control code documents; no output buffer is required.
        let ok = unsafe {
            DeviceIoControl(
                target.as_raw_handle() as HANDLE,
                FSCTL_SET_INTEGRITY_INFORMATION,
                (&mut set as *mut FSCTL_SET_INTEGRITY_INFORMATION_BUFFER).cast(),
                std::mem::size_of::<FSCTL_SET_INTEGRITY_INFORMATION_BUFFER>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return io_err("FSCTL_SET_INTEGRITY_INFORMATION");
        }
    }

    // Extents are duplicated in whole clusters, so the destination is first sized
    // up to a cluster boundary and trimmed back afterwards. The source's own
    // allocation is cluster-rounded too, so reading past `len` stays in bounds.
    //
    // Note this deliberately does not use SetFileValidData, which would require
    // SE_MANAGE_VOLUME_NAME -- a privilege an ordinary developer account does not
    // hold, and one that exposes uninitialized disk contents when granted.
    let aligned = len.div_ceil(cluster_size) * cluster_size;
    set_end_of_file(target, aligned as i64)?;

    let mut offset: i64 = 0;
    while (offset as u64) < aligned {
        let remaining = aligned - offset as u64;
        let count = remaining.min(MAX_CHUNK as u64) as i64;
        let mut request = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle() as HANDLE,
            SourceFileOffset: offset,
            TargetFileOffset: offset,
            ByteCount: count,
        };
        let mut returned: u32 = 0;
        // SAFETY: the request struct is fully initialized and holds a handle that
        // outlives the call; the control code takes no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                target.as_raw_handle() as HANDLE,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&mut request as *mut DUPLICATE_EXTENTS_DATA).cast(),
                std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            // Sharing limits and transient contention are per-file facts; anything
            // else says this volume cannot do it after all.
            const ERROR_BLOCK_TOO_MANY_REFERENCES: i32 = 347;
            const ERROR_SHARING_VIOLATION: i32 = 32;
            match e.raw_os_error() {
                Some(ERROR_BLOCK_TOO_MANY_REFERENCES) | Some(ERROR_SHARING_VIOLATION) => {}
                _ => poison(src_serial),
            }
            debug!("blockclone: FSCTL_DUPLICATE_EXTENTS_TO_FILE failed: {e}");
            return Err(CloneError::Refused(e.to_string()));
        }
        offset += count;
    }

    // Trim back to the real length; the tail cluster stays shared.
    set_end_of_file(target, len as i64)?;
    Ok(())
}

fn set_end_of_file(file: &File, end: i64) -> Result<(), CloneError> {
    #[repr(C)]
    struct EndOfFile {
        end_of_file: i64,
    }
    let info = EndOfFile { end_of_file: end };
    // SAFETY: FILE_END_OF_FILE_INFO is a single i64; the pointer and length
    // describe exactly that.
    let ok = unsafe {
        SetFileInformationByHandle(
            file.as_raw_handle() as HANDLE,
            FileEndOfFileInfo,
            (&info as *const EndOfFile).cast(),
            std::mem::size_of::<EndOfFile>() as u32,
        )
    };
    if ok == 0 {
        return io_err("SetFileInformationByHandle(FileEndOfFileInfo)");
    }
    Ok(())
}

/// Clone `len` bytes at `src_offset` inside `src` into the start of `dst`.
///
/// This is what materializing a cache hit needs: the object lives at an offset
/// inside the entry's zip archive, not at the start of a file of its own.
/// `FSCTL_DUPLICATE_EXTENTS_TO_FILE` addresses regions, so the whole-file
/// restriction of [`clone_whole_file`] was never the ioctl's -- only the API's.
///
/// The offset must be cluster-aligned; the writer aligns object payloads for
/// exactly this purpose (see `cache_io.rs`). The *length* need not be: whole
/// clusters are cloned and the sub-cluster tail is copied through the handle,
/// which costs one small write against the syscalls the clone already spent.
///
/// Takes an open handle for the destination rather than a path, unlike
/// [`clone_whole_file`]: the caller owns a just-created temp file it will
/// atomically persist afterwards, and making this function create the file
/// would force the caller to name temp files itself. The mod-level comment
/// about path-based APIs is about macOS `clonefile`, which has no region form
/// at all -- a region API is per-platform by nature.
pub fn clone_region_into(
    src: &Path,
    src_offset: u64,
    len: u64,
    dst: &File,
) -> Result<(), CloneError> {
    let source = OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .open(src)?;

    let src_serial = volume_serial(&source)?;
    let (can_clone, cluster_size, algorithm) = caps_for(&source, src_serial)?;
    if !can_clone {
        return Err(CloneError::Unsupported);
    }
    if !src_offset.is_multiple_of(cluster_size) {
        // The writer aligns to 4096; a volume with larger clusters (ReFS also
        // ships 64K) sees a misaligned offset and must copy instead.
        return Err(CloneError::Refused(format!(
            "offset {src_offset} not aligned to the {cluster_size}-byte cluster"
        )));
    }
    if len < MIN_CLONE_CLUSTERS * cluster_size {
        return Err(CloneError::TooSmall);
    }
    if volume_serial(dst)? != src_serial {
        return Err(CloneError::DifferentVolume);
    }

    // Only whole clusters can be duplicated mid-file; the tail is written.
    let aligned = (len / cluster_size) * cluster_size;

    // Integrity streams must match the source's and can only be set while the
    // destination is empty.
    if algorithm != 0 {
        let mut set = FSCTL_SET_INTEGRITY_INFORMATION_BUFFER {
            ChecksumAlgorithm: algorithm,
            Reserved: 0,
            Flags: 0,
        };
        let mut returned: u32 = 0;
        // SAFETY: input buffer is a correctly sized, initialized struct of the
        // type this control code documents; no output buffer is required.
        let ok = unsafe {
            DeviceIoControl(
                dst.as_raw_handle() as HANDLE,
                FSCTL_SET_INTEGRITY_INFORMATION,
                (&mut set as *mut FSCTL_SET_INTEGRITY_INFORMATION_BUFFER).cast(),
                std::mem::size_of::<FSCTL_SET_INTEGRITY_INFORMATION_BUFFER>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return io_err("FSCTL_SET_INTEGRITY_INFORMATION");
        }
    }

    set_end_of_file(dst, aligned as i64)?;

    let mut done: u64 = 0;
    while done < aligned {
        let count = (aligned - done).min(MAX_CHUNK as u64) as i64;
        let mut request = DUPLICATE_EXTENTS_DATA {
            FileHandle: source.as_raw_handle() as HANDLE,
            SourceFileOffset: (src_offset + done) as i64,
            TargetFileOffset: done as i64,
            ByteCount: count,
        };
        let mut returned: u32 = 0;
        // SAFETY: the request struct is fully initialized and holds a handle
        // that outlives the call; the control code takes no output buffer.
        let ok = unsafe {
            DeviceIoControl(
                dst.as_raw_handle() as HANDLE,
                FSCTL_DUPLICATE_EXTENTS_TO_FILE,
                (&mut request as *mut DUPLICATE_EXTENTS_DATA).cast(),
                std::mem::size_of::<DUPLICATE_EXTENTS_DATA>() as u32,
                std::ptr::null_mut(),
                0,
                &mut returned,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            let e = std::io::Error::last_os_error();
            const ERROR_BLOCK_TOO_MANY_REFERENCES: i32 = 347;
            const ERROR_SHARING_VIOLATION: i32 = 32;
            match e.raw_os_error() {
                Some(ERROR_BLOCK_TOO_MANY_REFERENCES) | Some(ERROR_SHARING_VIOLATION) => {}
                _ => poison(src_serial),
            }
            debug!("blockclone: FSCTL_DUPLICATE_EXTENTS_TO_FILE (region) failed: {e}");
            return Err(CloneError::Refused(e.to_string()));
        }
        done += count as u64;
    }

    // The sub-cluster tail, by hand. Positional I/O rather than seeking, so
    // the caller's cursor is left where it found it (at 0, on a fresh temp).
    if len > aligned {
        use std::os::windows::fs::FileExt;
        let mut tail = vec![0u8; (len - aligned) as usize];
        let read = source.seek_read(&mut tail, src_offset + aligned)?;
        if read as u64 != len - aligned {
            return Err(CloneError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "cache entry ended inside an object payload",
            )));
        }
        dst.seek_write(&tail, aligned)?;
    }

    Ok(())
}

/// Physical cluster runs backing a file, used by tests to prove that a clone
/// shares storage rather than merely producing identical bytes.
#[cfg(test)]
pub(crate) fn extent_lcns(path: &Path) -> std::io::Result<Vec<(i64, i64)>> {
    use windows_sys::Win32::System::Ioctl::{
        FSCTL_GET_RETRIEVAL_POINTERS, RETRIEVAL_POINTERS_BUFFER, STARTING_VCN_INPUT_BUFFER,
    };

    let file = File::open(path)?;
    let input = STARTING_VCN_INPUT_BUFFER { StartingVcn: 0 };
    // Generous single-shot buffer; these files have few runs.
    let mut buf = vec![0u8; 64 * 1024];
    let mut returned: u32 = 0;
    // SAFETY: input is a correctly sized STARTING_VCN_INPUT_BUFFER; the output
    // buffer is a byte vector whose length is passed accurately, and the result
    // is only read via the documented header/extent layout below.
    let ok = unsafe {
        DeviceIoControl(
            file.as_raw_handle() as HANDLE,
            FSCTL_GET_RETRIEVAL_POINTERS,
            (&input as *const STARTING_VCN_INPUT_BUFFER).cast(),
            std::mem::size_of::<STARTING_VCN_INPUT_BUFFER>() as u32,
            buf.as_mut_ptr().cast(),
            buf.len() as u32,
            &mut returned,
            std::ptr::null_mut(),
        )
    };
    if ok == 0 {
        return Err(std::io::Error::last_os_error());
    }

    // SAFETY: the ioctl succeeded, so the buffer holds a RETRIEVAL_POINTERS_BUFFER
    // followed by `ExtentCount` extent records.
    let header = unsafe { &*buf.as_ptr().cast::<RETRIEVAL_POINTERS_BUFFER>() };
    let count = header.ExtentCount as usize;
    let mut out = Vec::with_capacity(count);
    let extents = header.Extents.as_ptr();
    for i in 0..count {
        let e = unsafe { &*extents.add(i) };
        out.push((e.NextVcn, e.Lcn));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir() -> Option<std::path::PathBuf> {
        // Opt in explicitly: cloning needs a filesystem that supports it, and a
        // test that silently passes on NTFS would assert nothing.
        let root = crate::envvar::var_os("SCCION_TEST_CLONE_DIR")?;
        let dir = std::path::PathBuf::from(root).join(format!(
            "sccion-clone-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).ok()?;
        Some(dir)
    }

    #[test]
    fn clone_shares_extents_and_preserves_contents() {
        let Some(dir) = scratch_dir() else {
            eprintln!("skipping: set SCCION_TEST_CLONE_DIR to a clone-capable volume");
            return;
        };
        let src = dir.join("src.bin");
        let dst = dir.join("dst.bin");

        // Distinctive, incompressible-ish contents, several clusters wide.
        let body: Vec<u8> = (0..(4 * 1024 * 1024u32))
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        std::fs::write(&src, &body).unwrap();

        match clone_whole_file(&src, &dst) {
            Ok(()) => {}
            Err(CloneError::Unsupported) => {
                eprintln!("skipping: {} cannot clone", dir.display());
                let _ = std::fs::remove_dir_all(&dir);
                return;
            }
            Err(e) => panic!("clone failed: {e}"),
        }

        assert_eq!(
            std::fs::read(&dst).unwrap(),
            body,
            "clone must copy contents"
        );
        assert_eq!(
            std::fs::metadata(&dst).unwrap().len(),
            body.len() as u64,
            "clone must not leave the file rounded up to a cluster boundary"
        );

        // The point of the exercise: identical physical runs means the storage is
        // genuinely shared, not merely that the bytes match.
        let a = extent_lcns(&src).unwrap();
        let b = extent_lcns(&dst).unwrap();
        assert_eq!(a, b, "cloned file must occupy the same physical clusters");
        assert!(!a.is_empty(), "expected at least one extent");

        // Writing to the clone must not disturb the original.
        std::fs::write(&dst, b"overwritten").unwrap();
        assert_eq!(
            std::fs::read(&src).unwrap(),
            body,
            "source must be unaffected"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Not an assertion, a measurement: how long does sharing extents actually
    /// take against writing the same bytes? Printed rather than asserted, since
    /// absolute timings are a property of the machine.
    #[test]
    fn report_clone_versus_copy_cost() {
        let Some(dir) = scratch_dir() else {
            eprintln!("skipping: set SCCION_TEST_CLONE_DIR to a clone-capable volume");
            return;
        };
        // Sized like a real MSVC /Z7 object.
        let body: Vec<u8> = (0..1_458_123u32)
            .map(|i| (i.wrapping_mul(2654435761) >> 13) as u8)
            .collect();
        let src = dir.join("bench-src.obj");
        std::fs::write(&src, &body).unwrap();

        let runs = 50;
        let mut cloned = 0u32;

        let t0 = std::time::Instant::now();
        for i in 0..runs {
            let dst = dir.join(format!("clone-{i}.obj"));
            match clone_whole_file(&src, &dst) {
                Ok(()) => cloned += 1,
                Err(CloneError::Unsupported) => {
                    eprintln!("skipping: volume cannot clone");
                    let _ = std::fs::remove_dir_all(&dir);
                    return;
                }
                Err(e) => panic!("clone failed: {e}"),
            }
        }
        let clone_ms = t0.elapsed().as_secs_f64() * 1000.0 / runs as f64;

        let t1 = std::time::Instant::now();
        for i in 0..runs {
            std::fs::copy(&src, dir.join(format!("copy-{i}.obj"))).unwrap();
        }
        let copy_ms = t1.elapsed().as_secs_f64() * 1000.0 / runs as f64;

        let t2 = std::time::Instant::now();
        for i in 0..runs {
            std::fs::write(dir.join(format!("write-{i}.obj")), &body).unwrap();
        }
        let write_ms = t2.elapsed().as_secs_f64() * 1000.0 / runs as f64;

        eprintln!(
            "BLOCKCLONE BENCH ({} KiB object, {runs} runs, {cloned} cloned)
               clone      {clone_ms:6.3} ms
               fs::copy   {copy_ms:6.3} ms
               write      {write_ms:6.3} ms",
            body.len() / 1024
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
    #[test]
    fn clone_refuses_when_destination_exists() {
        let Some(dir) = scratch_dir() else { return };
        let src = dir.join("a.bin");
        let dst = dir.join("b.bin");
        std::fs::write(&src, vec![7u8; 1024 * 1024]).unwrap();
        std::fs::write(&dst, b"already here").unwrap();
        assert!(
            clone_whole_file(&src, &dst).is_err(),
            "must not clobber an existing destination"
        );
        assert_eq!(std::fs::read(&dst).unwrap(), b"already here");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
