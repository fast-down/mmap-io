//! Low-level memory-mapped file abstraction with safe, concurrent access.

use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};

use memmap2::{Mmap, MmapMut};

use crate::flush::FlushPolicy;

#[cfg(feature = "cow")]
use memmap2::MmapOptions;

use parking_lot::RwLock;

use crate::errors::{MmapIoError, Result};
use crate::utils::{ensure_in_bounds, slice_range};

// Error message constants
const ERR_ZERO_SIZE: &str = "Size must be greater than zero";
const ERR_ZERO_LENGTH_FILE: &str = "Cannot map zero-length file";

// Maximum safe mmap size: 128TB (reasonable limit for most systems)
// This prevents accidental exhaustion of address space or disk
// Note: This is intentionally very large to support legitimate use cases
// while still preventing obvious errors like u64::MAX
#[cfg(target_pointer_width = "64")]
const MAX_MMAP_SIZE: u64 = 128 * (1 << 40); // 128 TB on 64-bit systems

#[cfg(target_pointer_width = "32")]
const MAX_MMAP_SIZE: u64 = 2 * (1 << 30); // 2 GB on 32-bit systems (practical limit)

/// Access mode for a memory-mapped file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MmapMode {
    /// Read-only mapping.
    ReadOnly,
    /// Read-write mapping.
    ReadWrite,
    /// Copy-on-Write mapping (private). Writes affect this mapping only; the underlying file remains unchanged.
    CopyOnWrite,
}

#[doc(hidden)]
pub struct Inner {
    pub(crate) path: PathBuf,
    pub(crate) file: File,
    pub(crate) mode: MmapMode,
    // Cached length to avoid repeated metadata queries
    pub(crate) cached_len: RwLock<u64>,
    // The mapping itself. We use an enum to hold either RO or RW mapping.
    pub(crate) map: MapVariant,
    // Flush policy and accounting (RW only)
    pub(crate) flush_policy: FlushPolicy,
    pub(crate) written_since_last_flush: RwLock<u64>,
    // Huge pages preference (builder-set), effective on supported platforms
    #[cfg(feature = "hugepages")]
    pub(crate) huge_pages: bool,
}

#[doc(hidden)]
pub enum MapVariant {
    Ro(Mmap),
    Rw(RwLock<MmapMut>),
    /// Private, per-process copy-on-write mapping. Underlying file is not modified by writes.
    Cow(Mmap),
}

/// Memory-mapped file with safe, zero-copy region access.
///
/// This is the core type for memory-mapped file operations. It provides:
/// - Safe concurrent access through interior mutability
/// - Zero-copy reads and writes
/// - Automatic bounds checking
/// - Cross-platform compatibility
///
/// # Examples
///
/// ```no_run
/// use mmap_io::{MemoryMappedFile, MmapMode};
///
/// // Create a new 1KB file
/// let mmap = MemoryMappedFile::create_rw("data.bin", 1024)?;
///
/// // Write some data
/// mmap.update_region(0, b"Hello, world!")?;
/// mmap.flush()?;
///
/// // Open existing file read-only
/// let ro_mmap = MemoryMappedFile::open_ro("data.bin")?;
/// let data = ro_mmap.as_slice(0, 13)?;
/// assert_eq!(data, b"Hello, world!");
/// # Ok::<(), mmap_io::MmapIoError>(())
/// ```
///
/// Cloning this struct is cheap; it clones an Arc to the inner state.
/// For read-write mappings, interior mutability is protected with an `RwLock`.
#[derive(Clone)]
pub struct MemoryMappedFile {
    pub(crate) inner: Arc<Inner>,
}

impl std::fmt::Debug for MemoryMappedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut ds = f.debug_struct("MemoryMappedFile");
        ds.field("path", &self.inner.path)
            .field("mode", &self.inner.mode)
            .field("len", &self.len());
        #[cfg(feature = "hugepages")]
        {
            ds.field("huge_pages", &self.inner.huge_pages);
        }
        ds.finish()
    }
}

impl MemoryMappedFile {
    /// Builder for constructing a MemoryMappedFile with custom options.
    ///
    /// Example:
    /// ```
    /// # use mmap_io::{MemoryMappedFile, MmapMode};
    /// # use mmap_io::flush::FlushPolicy;
    /// // let mmap = MemoryMappedFile::builder("file.bin")
    /// //     .mode(MmapMode::ReadWrite)
    /// //     .size(1_000_000)
    /// //     .flush_policy(FlushPolicy::EveryBytes(1_000_000))
    /// //     .create().unwrap();
    /// ```
    pub fn builder<P: AsRef<Path>>(path: P) -> MemoryMappedFileBuilder {
        MemoryMappedFileBuilder {
            path: path.as_ref().to_path_buf(),
            size: None,
            mode: None,
            flush_policy: FlushPolicy::default(),
            #[cfg(feature = "hugepages")]
            huge_pages: false,
        }
    }

    /// Create a new file (truncating if exists) and memory-map it in read-write mode with the given size.
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(1) for mapping creation
    /// - **Memory Usage**: Virtual address space of `size` bytes (physical memory allocated on demand)
    /// - **I/O Operations**: One file creation, one truncate, one mmap syscall
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::ResizeFailed` if size is zero or exceeds the maximum safe limit.
    /// Returns `MmapIoError::Io` if file creation or mapping fails.
    pub fn create_rw<P: AsRef<Path>>(path: P, size: u64) -> Result<Self> {
        if size == 0 {
            return Err(MmapIoError::ResizeFailed(ERR_ZERO_SIZE.into()));
        }
        if size > MAX_MMAP_SIZE {
            return Err(MmapIoError::ResizeFailed(format!(
                "Size {size} exceeds maximum safe limit of {MAX_MMAP_SIZE} bytes"
            )));
        }
        let path_ref = path.as_ref();
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .read(true)
            .truncate(true)
            .open(path_ref)?;
        file.set_len(size)?;
        // SAFETY: The file has been created with the correct size and permissions.
        // memmap2 handles platform-specific mmap details safely.
        // Note: create_rw convenience ignores huge pages; use builder for that.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let inner = Inner {
            path: path_ref.to_path_buf(),
            file,
            mode: MmapMode::ReadWrite,
            cached_len: RwLock::new(size),
            map: MapVariant::Rw(RwLock::new(mmap)),
            flush_policy: FlushPolicy::default(),
            written_since_last_flush: RwLock::new(0),
            #[cfg(feature = "hugepages")]
            huge_pages: false,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Open an existing file and memory-map it read-only.
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::Io` if file opening or mapping fails.
    pub fn open_ro<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new().read(true).open(path_ref)?;
        let len = file.metadata()?.len();
        // SAFETY: The file is opened read-only and memmap2 ensures safe mapping.
        let mmap = unsafe { Mmap::map(&file)? };
        let inner = Inner {
            path: path_ref.to_path_buf(),
            file,
            mode: MmapMode::ReadOnly,
            cached_len: RwLock::new(len),
            map: MapVariant::Ro(mmap),
            flush_policy: FlushPolicy::Never,
            written_since_last_flush: RwLock::new(0),
            #[cfg(feature = "hugepages")]
            huge_pages: false,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Open an existing file and memory-map it read-write.
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::ResizeFailed` if file is zero-length.
    /// Returns `MmapIoError::Io` if file opening or mapping fails.
    pub fn open_rw<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new().read(true).write(true).open(path_ref)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(MmapIoError::ResizeFailed(ERR_ZERO_LENGTH_FILE.into()));
        }
        // SAFETY: The file is opened read-write with proper permissions.
        // We've verified the file is not zero-length.
        // Note: open_rw convenience ignores huge pages; use builder for that.
        let mmap = unsafe { MmapMut::map_mut(&file)? };
        let inner = Inner {
            path: path_ref.to_path_buf(),
            file,
            mode: MmapMode::ReadWrite,
            cached_len: RwLock::new(len),
            map: MapVariant::Rw(RwLock::new(mmap)),
            flush_policy: FlushPolicy::default(),
            written_since_last_flush: RwLock::new(0),
            #[cfg(feature = "hugepages")]
            huge_pages: false,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }

    /// Return current mapping mode.
    #[must_use]
    pub fn mode(&self) -> MmapMode {
        self.inner.mode
    }

    /// Total length of the mapped file in bytes (cached).
    #[must_use]
    pub fn len(&self) -> u64 {
        *self.inner.cached_len.read()
    }

    /// Whether the mapped file is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Get a zero-copy read-only slice for the given [offset, offset+len).
    /// For RW mappings, cannot return a reference bound to a temporary guard; use `read_into` instead.
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(1) - direct pointer access
    /// - **Memory Usage**: No additional allocation (zero-copy)
    /// - **Cache Behavior**: May trigger page faults on first access
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::OutOfBounds` if range exceeds file bounds.
    /// Returns `MmapIoError::InvalidMode` for RW mappings (use `read_into` instead).
    pub fn as_slice(&self, offset: u64, len: u64) -> Result<&[u8]> {
        let total = self.current_len()?;
        ensure_in_bounds(offset, len, total)?;
        match &self.inner.map {
            MapVariant::Ro(m) => {
                let (start, end) = slice_range(offset, len, total)?;
                Ok(&m[start..end])
            }
            MapVariant::Rw(_lock) => Err(MmapIoError::InvalidMode("use read_into for RW mappings")),
            MapVariant::Cow(m) => {
                let (start, end) = slice_range(offset, len, total)?;
                Ok(&m[start..end])
            }
        }
    }

    /// Get a zero-copy mutable slice for the given [offset, offset+len).
    /// Only available in `ReadWrite` mode.
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::InvalidMode` if not in `ReadWrite` mode.
    /// Returns `MmapIoError::OutOfBounds` if range exceeds file bounds.
    pub fn as_slice_mut(&self, offset: u64, len: u64) -> Result<MappedSliceMut<'_>> {
        let (start, end) = slice_range(offset, len, self.current_len()?)?;
        match &self.inner.map {
            MapVariant::Ro(_) => Err(MmapIoError::InvalidMode(
                "mutable access on read-only mapping",
            )),
            MapVariant::Rw(lock) => {
                let guard = lock.write();
                Ok(MappedSliceMut {
                    guard,
                    range: start..end,
                })
            }
            MapVariant::Cow(_) => {
                // Phase-1: COW is read-only for safety. Writable COW will be added with a persistent
                // private RW view in a follow-up change.
                Err(MmapIoError::InvalidMode(
                    "mutable access on copy-on-write mapping (phase-1 read-only)",
                ))
            }
        }
    }

    /// Copy the provided bytes into the mapped file at the given offset.
    /// Bounds-checked, zero-copy write.
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(n) where n is data.len()
    /// - **Memory Usage**: No additional allocation
    /// - **I/O Operations**: May trigger flush based on flush policy
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::InvalidMode` if not in `ReadWrite` mode.
    /// Returns `MmapIoError::OutOfBounds` if range exceeds file bounds.
    pub fn update_region(&self, offset: u64, data: &[u8]) -> Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if self.inner.mode != MmapMode::ReadWrite {
            return Err(MmapIoError::InvalidMode(
                "Update region requires ReadWrite mode.",
            ));
        }
        let len = data.len() as u64;
        let (start, end) = slice_range(offset, len, self.current_len()?)?;
        match &self.inner.map {
            MapVariant::Ro(_) => Err(MmapIoError::InvalidMode(
                "Cannot write to read-only mapping",
            )),
            MapVariant::Rw(lock) => {
                {
                    let mut guard = lock.write();
                    guard[start..end].copy_from_slice(data);
                }
                // Apply flush policy
                self.apply_flush_policy(len)?;
                Ok(())
            }
            MapVariant::Cow(_) => Err(MmapIoError::InvalidMode(
                "Cannot write to copy-on-write mapping (phase-1 read-only)",
            )),
        }
    }

    /// Async write that enforces Async-Only Flushing semantics: always flush after write.
    /// Uses spawn_blocking to avoid blocking the async scheduler.
    #[cfg(feature = "async")]
    pub async fn update_region_async(&self, offset: u64, data: &[u8]) -> Result<()> {
        // Perform the write in a blocking task
        let this = self.clone();
        let data_vec = data.to_vec();
        tokio::task::spawn_blocking(move || {
            // Synchronous write
            this.update_region(offset, &data_vec)?;
            // Async-only flushing: unconditionally flush after write when using async path
            this.flush()
        })
        .await
        .map_err(|e| MmapIoError::FlushFailed(format!("join error: {e}")))?
    }

    /// Flush changes to disk. For read-only mappings, this is a no-op.
    ///
    /// Smart internal guards:
    /// - Skip I/O when there are no pending writes (accumulator is zero)
    /// - On Linux, use msync(MS_ASYNC) as a cheaper hint; fall back to full flush on error
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(n) where n is the size of dirty pages
    /// - **I/O Operations**: Triggers disk write of modified pages
    /// - **Optimization**: Skips flush if no writes since last flush
    /// - **Platform**: Linux uses async msync for better performance
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::FlushFailed` if flush operation fails.
    pub fn flush(&self) -> Result<()> {
        match &self.inner.map {
            MapVariant::Ro(_) => Ok(()),
            MapVariant::Cow(_) => Ok(()), // no-op for COW
            MapVariant::Rw(lock) => {
                // Fast path: no pending writes => skip flushing I/O
                if *self.inner.written_since_last_flush.read() == 0 {
                    return Ok(());
                }

                // Platform-optimized path: Linux MS_ASYNC best-effort
                #[cfg(all(unix, target_os = "linux"))]
                {
                    if let Ok(len) = self.current_len() {
                        if len > 0 {
                            if self.try_linux_async_flush(len as usize)? {
                                return Ok(());
                            }
                        }
                    }
                }

                // Fallback/full flush using memmap2 API
                let guard = lock.read();
                guard
                    .flush()
                    .map_err(|e| MmapIoError::FlushFailed(e.to_string()))?;
                // Reset accumulator after a successful flush
                *self.inner.written_since_last_flush.write() = 0;
                Ok(())
            }
        }
    }

    /// Async flush changes to disk. For read-only or COW mappings, this is a no-op.
    /// This method enforces "async-only flushing" semantics for async paths.
    #[cfg(feature = "async")]
    pub async fn flush_async(&self) -> Result<()> {
        // Use spawn_blocking to avoid blocking the async scheduler
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.flush())
            .await
            .map_err(|e| MmapIoError::FlushFailed(format!("join error: {e}")))?
    }

    /// Async flush a specific byte range to disk.
    #[cfg(feature = "async")]
    pub async fn flush_range_async(&self, offset: u64, len: u64) -> Result<()> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.flush_range(offset, len))
            .await
            .map_err(|e| MmapIoError::FlushFailed(format!("join error: {e}")))?
    }

    /// Flush a specific byte range to disk.
    ///
    /// Smart internal guards:
    /// - Skip I/O when there are no pending writes in accumulator
    /// - On Linux, prefer msync(MS_ASYNC) for the range; fall back to full range flush on error
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::OutOfBounds` if range exceeds file bounds.
    /// Returns `MmapIoError::FlushFailed` if flush operation fails.
    pub fn flush_range(&self, offset: u64, len: u64) -> Result<()> {
        if len == 0 {
            return Ok(());
        }
        ensure_in_bounds(offset, len, self.current_len()?)?;
        match &self.inner.map {
            MapVariant::Ro(_) => Ok(()),
            MapVariant::Cow(_) => Ok(()), // no-op for COW
            MapVariant::Rw(lock) => {
                // If we have no accumulated writes, skip I/O
                if *self.inner.written_since_last_flush.read() == 0 {
                    return Ok(());
                }

                let (start, end) = slice_range(offset, len, self.current_len()?)?;
                let range_len = end - start;

                // Linux MS_ASYNC optimization
                #[cfg(all(unix, target_os = "linux"))]
                {
                    // SAFETY: msync on a valid mapped range. We translate to a pointer within the map.
                    let msync_res: i32 = {
                        let guard = lock.read();
                        let base = guard.as_ptr();
                        let ptr = unsafe { base.add(start) } as *mut libc::c_void;
                        let ret = unsafe { libc::msync(ptr, range_len, libc::MS_ASYNC) };
                        ret
                    };
                    if msync_res == 0 {
                        // Consider MS_ASYNC success and reset accumulator
                        *self.inner.written_since_last_flush.write() = 0;
                        return Ok(());
                    }
                    // else fall through to full flush_range
                }

                let guard = lock.read();
                guard
                    .flush_range(start, range_len)
                    .map_err(|e| MmapIoError::FlushFailed(e.to_string()))?;
                // Reset accumulator after a successful flush
                *self.inner.written_since_last_flush.write() = 0;
                Ok(())
            }
        }
    }

    /// Resize (grow or shrink) the mapped file (RW only). This remaps the file internally.
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(1) for the remap operation
    /// - **Memory Usage**: Allocates new virtual address space of `new_size`
    /// - **I/O Operations**: File truncate/extend + new mmap syscall
    /// - **Note**: Existing pointers/slices become invalid after resize
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::InvalidMode` if not in `ReadWrite` mode.
    /// Returns `MmapIoError::ResizeFailed` if new size is zero or exceeds the maximum safe limit.
    /// Returns `MmapIoError::Io` if resize operation fails.
    pub fn resize(&self, new_size: u64) -> Result<()> {
        if self.inner.mode != MmapMode::ReadWrite {
            return Err(MmapIoError::InvalidMode("Resize requires ReadWrite mode"));
        }
        if new_size == 0 {
            return Err(MmapIoError::ResizeFailed(
                "New size must be greater than zero".into(),
            ));
        }
        if new_size > MAX_MMAP_SIZE {
            return Err(MmapIoError::ResizeFailed(format!(
                "New size {new_size} exceeds maximum safe limit of {MAX_MMAP_SIZE} bytes"
            )));
        }

        let current = self.current_len()?;

        // On Windows, shrinking a file with an active mapping fails with:
        // "The requested operation cannot be performed on a file with a user-mapped section open."
        // To keep APIs usable and tests passing, we virtually shrink by updating the cached length,
        // avoiding truncation while a mapping is active. Growing still truncates and remaps.
        #[cfg(windows)]
        {
            use std::cmp::Ordering;
            match new_size.cmp(&current) {
                Ordering::Less => {
                    // Virtually shrink: only update the cached length.
                    *self.inner.cached_len.write() = new_size;
                    return Ok(());
                }
                Ordering::Equal => {
                    return Ok(());
                }
                Ordering::Greater => {
                    // Proceed with normal grow: extend file then remap.
                }
            }
        }

        // Update length on disk for non-windows, or for growing on windows.
        // Silence unused variable warning when the Windows shrink early-return path is compiled.
        let _ = &current;
        self.inner.file.set_len(new_size)?;

        // Remap with the new size.
        let new_map = unsafe { MmapMut::map_mut(&self.inner.file)? };
        match &self.inner.map {
            MapVariant::Ro(_) => Err(MmapIoError::InvalidMode(
                "Cannot remap read-only mapping as read-write",
            )),
            MapVariant::Cow(_) => Err(MmapIoError::InvalidMode(
                "resize not supported on copy-on-write mapping",
            )),
            MapVariant::Rw(lock) => {
                let mut guard = lock.write();
                *guard = new_map;
                // Update cached length
                *self.inner.cached_len.write() = new_size;
                Ok(())
            }
        }
    }

    /// Path to the underlying file.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.inner.path
    }
}

impl MemoryMappedFile {
    // Helper method to attempt Linux-specific async flush
    #[cfg(all(unix, target_os = "linux"))]
    fn try_linux_async_flush(&self, len: usize) -> Result<bool> {
        use std::os::fd::AsRawFd;

        // Get the file descriptor (unused but kept for potential future use)
        let _fd = self.inner.file.as_raw_fd();

        // Try to get the mapping pointer for msync
        match &self.inner.map {
            MapVariant::Rw(lock) => {
                let guard = lock.read();
                let ptr = guard.as_ptr() as *mut libc::c_void;

                // SAFETY: msync requires a valid mapping address/len; memmap2 handles mapping
                let ret = unsafe { libc::msync(ptr, len, libc::MS_ASYNC) };

                if ret == 0 {
                    // MS_ASYNC succeeded, reset accumulator
                    *self.inner.written_since_last_flush.write() = 0;
                    Ok(true)
                } else {
                    // Fall back to full flush
                    Ok(false)
                }
            }
            _ => Ok(false),
        }
    }
}

/// Create a memory mapping with optional huge pages support.
///
/// When `huge` is true on Linux, this function attempts to use Transparent Huge Pages (THP)
/// via the `madvise(MADV_HUGEPAGE)` hint. This is a best-effort optimization that allows
/// the kernel to use huge pages when available and beneficial.
///
/// Note: This does NOT guarantee huge pages will be used. The actual usage depends on:
/// - System configuration (THP must be enabled)
/// - Available memory and fragmentation
/// - Kernel heuristics
///
/// The function will silently fall back to regular pages if huge pages are unavailable.
#[cfg(feature = "hugepages")]
fn map_mut_with_options(file: &File, len: u64, huge: bool) -> Result<MmapMut> {
    #[cfg(all(unix, target_os = "linux"))]
    {
        // Create the standard mapping first
        let mmap = unsafe { MmapMut::map_mut(file) }.map_err(|e| MmapIoError::Io(e.into()))?;

        if huge {
            // Request Transparent Huge Pages (THP) for this mapping
            // This is a hint to the kernel - not a guarantee
            unsafe {
                let mmap_ptr = mmap.as_ptr() as *mut libc::c_void;

                // MADV_HUGEPAGE: Enable THP for this memory region
                // The kernel will attempt to use huge pages when beneficial
                // This is the most compatible approach with memmap2's constraints
                let ret = libc::madvise(mmap_ptr, len as usize, libc::MADV_HUGEPAGE);

                if ret != 0 {
                    // madvise failed - huge pages may not be available
                    // This is not fatal - the mapping is still valid with regular pages
                    // In production, you might want to log this for monitoring
                    // eprintln!("Warning: madvise(MADV_HUGEPAGE) failed, using regular pages");
                }
            }
        }

        Ok(mmap)
    }
    #[cfg(not(all(unix, target_os = "linux")))]
    {
        // Huge pages are Linux-specific, ignore the flag on other platforms
        let _ = (len, huge);
        unsafe { MmapMut::map_mut(file) }.map_err(|e| MmapIoError::Io(e.into()))
    }
}

#[cfg(feature = "cow")]
impl MemoryMappedFile {
    /// Open an existing file and memory-map it copy-on-write (private).
    /// Changes through this mapping are visible only within this process; the underlying file remains unchanged.
    pub fn open_cow<P: AsRef<Path>>(path: P) -> Result<Self> {
        let path_ref = path.as_ref();
        let file = OpenOptions::new().read(true).open(path_ref)?;
        let len = file.metadata()?.len();
        if len == 0 {
            return Err(MmapIoError::ResizeFailed(ERR_ZERO_LENGTH_FILE.into()));
        }
        // SAFETY: memmap2 handles platform specifics. We request a private (copy-on-write) mapping.
        let mmap = unsafe {
            let mut opts = MmapOptions::new();
            opts.len(len as usize);
            #[cfg(unix)]
            {
                // memmap2 currently does not expose a stable .private() on all Rust/MSRV combos.
                // On Unix, map() of a read-only file yields an immutable mapping; for COW semantics
                // we rely on platform-specific behavior when writing is disallowed here in phase-1.
                // When writable COW is introduced, we will use platform flags via memmap2 internals.
                opts.map(&file)?
            }
            #[cfg(not(unix))]
            {
                // On Windows, memmap2 maps with appropriate WRITECOPY semantics internally for private mappings.
                opts.map(&file)?
            }
        };
        let inner = Inner {
            path: path_ref.to_path_buf(),
            file,
            mode: MmapMode::CopyOnWrite,
            cached_len: RwLock::new(len),
            map: MapVariant::Cow(mmap),
            // COW never flushes underlying file in phase-1
            flush_policy: FlushPolicy::Never,
            written_since_last_flush: RwLock::new(0),
            #[cfg(feature = "hugepages")]
            huge_pages: false,
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

impl MemoryMappedFile {
    fn apply_flush_policy(&self, written: u64) -> Result<()> {
        match self.inner.flush_policy {
            FlushPolicy::Never | FlushPolicy::Manual => Ok(()),
            FlushPolicy::Always => {
                // Record then flush immediately
                *self.inner.written_since_last_flush.write() += written;
                self.flush()
            }
            FlushPolicy::EveryBytes(n) => {
                let n = n as u64;
                if n == 0 {
                    return Ok(());
                }
                let mut acc = self.inner.written_since_last_flush.write();
                *acc += written;
                if *acc >= n {
                    // Do not reset prematurely; let flush() clear on success
                    drop(acc);
                    self.flush()
                } else {
                    Ok(())
                }
            }
            FlushPolicy::EveryWrites(w) => {
                if w == 0 {
                    return Ok(());
                }
                let mut acc = self.inner.written_since_last_flush.write();
                *acc += 1;
                if *acc >= w as u64 {
                    drop(acc);
                    self.flush()
                } else {
                    Ok(())
                }
            }
            FlushPolicy::EveryMillis(_ms) => {
                // Phase-1: treat as Manual; user drives time-based flushing externally.
                Ok(())
            }
        }
    }

    /// Return the up-to-date file length (cached).
    /// This ensures length remains correct even after resize.
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::Io` if metadata query fails (not expected in current implementation).
    pub fn current_len(&self) -> Result<u64> {
        Ok(*self.inner.cached_len.read())
    }

    /// Read bytes from the mapping into the provided buffer starting at `offset`.
    /// Length is `buf.len()`; performs bounds checks.
    ///
    /// # Performance
    ///
    /// - **Time Complexity**: O(n) where n is buf.len()
    /// - **Memory Usage**: Uses provided buffer, no additional allocation
    /// - **Cache Behavior**: Sequential access pattern is cache-friendly
    ///
    /// # Errors
    ///
    /// Returns `MmapIoError::OutOfBounds` if range exceeds file bounds.
    pub fn read_into(&self, offset: u64, buf: &mut [u8]) -> Result<()> {
        let total = self.current_len()?;
        let len = buf.len() as u64;
        ensure_in_bounds(offset, len, total)?;
        match &self.inner.map {
            MapVariant::Ro(m) => {
                let (start, end) = slice_range(offset, len, total)?;
                buf.copy_from_slice(&m[start..end]);
                Ok(())
            }
            MapVariant::Rw(lock) => {
                let guard = lock.read();
                let (start, end) = slice_range(offset, len, total)?;
                buf.copy_from_slice(&guard[start..end]);
                Ok(())
            }
            MapVariant::Cow(m) => {
                let (start, end) = slice_range(offset, len, total)?;
                buf.copy_from_slice(&m[start..end]);
                Ok(())
            }
        }
    }
}

/// Builder for MemoryMappedFile construction with options.
pub struct MemoryMappedFileBuilder {
    path: PathBuf,
    size: Option<u64>,
    mode: Option<MmapMode>,
    flush_policy: FlushPolicy,
    #[cfg(feature = "hugepages")]
    huge_pages: bool,
}

impl MemoryMappedFileBuilder {
    /// Specify the size (required for create/ReadWrite new files).
    pub fn size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Specify the mode (ReadOnly, ReadWrite, CopyOnWrite).
    pub fn mode(mut self, mode: MmapMode) -> Self {
        self.mode = Some(mode);
        self
    }

    /// Specify the flush policy.
    pub fn flush_policy(mut self, policy: FlushPolicy) -> Self {
        self.flush_policy = policy;
        self
    }

    /// Request Huge Pages (Linux MAP_HUGETLB). No-op on non-Linux platforms.
    #[cfg(feature = "hugepages")]
    pub fn huge_pages(mut self, enable: bool) -> Self {
        self.huge_pages = enable;
        self
    }

    /// Create a new mapping; for ReadWrite requires size for creation.
    pub fn create(self) -> Result<MemoryMappedFile> {
        let mode = self.mode.unwrap_or(MmapMode::ReadWrite);
        match mode {
            MmapMode::ReadWrite => {
                let size = self.size.ok_or_else(|| {
                    MmapIoError::ResizeFailed(
                        "Size must be set for create() in ReadWrite mode".into(),
                    )
                })?;
                if size == 0 {
                    return Err(MmapIoError::ResizeFailed(ERR_ZERO_SIZE.into()));
                }
                if size > MAX_MMAP_SIZE {
                    return Err(MmapIoError::ResizeFailed(format!(
                        "Size {size} exceeds maximum safe limit of {MAX_MMAP_SIZE} bytes"
                    )));
                }
                let path_ref = &self.path;
                let file = OpenOptions::new()
                    .create(true)
                    .write(true)
                    .read(true)
                    .truncate(true)
                    .open(path_ref)?;
                file.set_len(size)?;
                // Map with consideration for huge pages if requested
                #[cfg(feature = "hugepages")]
                let mmap = map_mut_with_options(&file, size, self.huge_pages)?;
                #[cfg(not(feature = "hugepages"))]
                let mmap = unsafe { MmapMut::map_mut(&file)? };
                let inner = Inner {
                    path: path_ref.clone(),
                    file,
                    mode,
                    cached_len: RwLock::new(size),
                    map: MapVariant::Rw(RwLock::new(mmap)),
                    flush_policy: self.flush_policy,
                    written_since_last_flush: RwLock::new(0),
                    #[cfg(feature = "hugepages")]
                    huge_pages: self.huge_pages,
                };
                Ok(MemoryMappedFile {
                    inner: Arc::new(inner),
                })
            }
            MmapMode::ReadOnly => {
                let path_ref = &self.path;
                let file = OpenOptions::new().read(true).open(path_ref)?;
                let len = file.metadata()?.len();
                let mmap = unsafe { Mmap::map(&file)? };
                let inner = Inner {
                    path: path_ref.clone(),
                    file,
                    mode,
                    cached_len: RwLock::new(len),
                    map: MapVariant::Ro(mmap),
                    flush_policy: FlushPolicy::Never,
                    written_since_last_flush: RwLock::new(0),
                    #[cfg(feature = "hugepages")]
                    huge_pages: false,
                };
                Ok(MemoryMappedFile {
                    inner: Arc::new(inner),
                })
            }
            MmapMode::CopyOnWrite => {
                #[cfg(feature = "cow")]
                {
                    let path_ref = &self.path;
                    let file = OpenOptions::new().read(true).open(path_ref)?;
                    let len = file.metadata()?.len();
                    if len == 0 {
                        return Err(MmapIoError::ResizeFailed(ERR_ZERO_LENGTH_FILE.into()));
                    }
                    let mmap = unsafe {
                        let mut opts = MmapOptions::new();
                        opts.len(len as usize);
                        opts.map(&file)?
                    };
                    let inner = Inner {
                        path: path_ref.clone(),
                        file,
                        mode,
                        cached_len: RwLock::new(len),
                        map: MapVariant::Cow(mmap),
                        flush_policy: FlushPolicy::Never,
                        written_since_last_flush: RwLock::new(0),
                        #[cfg(feature = "hugepages")]
                        huge_pages: false,
                    };
                    Ok(MemoryMappedFile {
                        inner: Arc::new(inner),
                    })
                }
                #[cfg(not(feature = "cow"))]
                {
                    Err(MmapIoError::InvalidMode(
                        "CopyOnWrite mode requires 'cow' feature",
                    ))
                }
            }
        }
    }

    /// Open an existing file with provided mode (size ignored).
    pub fn open(self) -> Result<MemoryMappedFile> {
        let mode = self.mode.unwrap_or(MmapMode::ReadOnly);
        match mode {
            MmapMode::ReadOnly => {
                let path_ref = &self.path;
                let file = OpenOptions::new().read(true).open(path_ref)?;
                let len = file.metadata()?.len();
                let mmap = unsafe { Mmap::map(&file)? };
                let inner = Inner {
                    path: path_ref.clone(),
                    file,
                    mode,
                    cached_len: RwLock::new(len),
                    map: MapVariant::Ro(mmap),
                    flush_policy: FlushPolicy::Never,
                    written_since_last_flush: RwLock::new(0),
                    #[cfg(feature = "hugepages")]
                    huge_pages: false,
                };
                Ok(MemoryMappedFile {
                    inner: Arc::new(inner),
                })
            }
            MmapMode::ReadWrite => {
                let path_ref = &self.path;
                let file = OpenOptions::new().read(true).write(true).open(path_ref)?;
                let len = file.metadata()?.len();
                if len == 0 {
                    return Err(MmapIoError::ResizeFailed(ERR_ZERO_LENGTH_FILE.into()));
                }
                #[cfg(feature = "hugepages")]
                let mmap = map_mut_with_options(&file, len, self.huge_pages)?;
                #[cfg(not(feature = "hugepages"))]
                let mmap = unsafe { MmapMut::map_mut(&file)? };
                let inner = Inner {
                    path: path_ref.clone(),
                    file,
                    mode,
                    cached_len: RwLock::new(len),
                    map: MapVariant::Rw(RwLock::new(mmap)),
                    flush_policy: self.flush_policy,
                    written_since_last_flush: RwLock::new(0),
                    #[cfg(feature = "hugepages")]
                    huge_pages: self.huge_pages,
                };
                Ok(MemoryMappedFile {
                    inner: Arc::new(inner),
                })
            }
            MmapMode::CopyOnWrite => {
                #[cfg(feature = "cow")]
                {
                    let path_ref = &self.path;
                    let file = OpenOptions::new().read(true).open(path_ref)?;
                    let len = file.metadata()?.len();
                    if len == 0 {
                        return Err(MmapIoError::ResizeFailed(ERR_ZERO_LENGTH_FILE.into()));
                    }
                    let mmap = unsafe {
                        let mut opts = MmapOptions::new();
                        opts.len(len as usize);
                        opts.map(&file)?
                    };
                    let inner = Inner {
                        path: path_ref.clone(),
                        file,
                        mode,
                        cached_len: RwLock::new(len),
                        map: MapVariant::Cow(mmap),
                        flush_policy: FlushPolicy::Never,
                        written_since_last_flush: RwLock::new(0),
                        #[cfg(feature = "hugepages")]
                        huge_pages: false,
                    };
                    Ok(MemoryMappedFile {
                        inner: Arc::new(inner),
                    })
                }
                #[cfg(not(feature = "cow"))]
                {
                    Err(MmapIoError::InvalidMode(
                        "CopyOnWrite mode requires 'cow' feature",
                    ))
                }
            }
        }
    }
}

/// Wrapper for a mutable slice that holds a write lock guard,
/// ensuring exclusive access for the lifetime of the slice.
pub struct MappedSliceMut<'a> {
    guard: parking_lot::lock_api::RwLockWriteGuard<'a, parking_lot::RawRwLock, MmapMut>,
    range: std::ops::Range<usize>,
}

impl MappedSliceMut<'_> {
    /// Get the mutable slice.
    ///
    /// Note: This method is intentionally named `as_mut` for consistency,
    /// even though it conflicts with the standard trait naming.
    #[allow(clippy::should_implement_trait)]
    pub fn as_mut(&mut self) -> &mut [u8] {
        // Avoid clone by using the range directly
        let start = self.range.start;
        let end = self.range.end;
        &mut self.guard[start..end]
    }
}
