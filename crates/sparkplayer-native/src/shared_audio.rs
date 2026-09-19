//! Optional shared-memory audio bridge for external visualizers.
//!
//! The bridge is deliberately best-effort: callers should disable it if
//! `open` fails rather than making player startup depend on an external
//! consumer. Samples are published as interleaved stereo frames into a fixed
//! ring buffer. Readers should use `generation` as a seqlock-style guard:
//! read it before and after copying header/data and retry if it changed.

use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use anyhow::{Context, Result};

const MAGIC: u32 = u32::from_le_bytes(*b"SPRK");
const VERSION: u32 = 1;
const OUTPUT_CHANNELS: u32 = 2;
const DEFAULT_CAPACITY_FRAMES: u32 = 48_000 * 10;

pub struct SharedAudioControl {
    pub playback: Option<bool>,
    pub visualizer_delta: i32,
}

#[repr(C)]
struct SparkAudioHeader {
    magic: u32,
    version: u32,
    header_size: u32,
    capacity_frames: u32,
    channels: u32,
    sample_rate: u32,
    write_frame: u64,
    total_frames: u64,
    generation: u64,
    active: u32,
    reserved: [u32; 7],
}

/// Per-source channel assembler. This stays owned by the playback source so the
/// audio callback does not need to lock shared state for every sample.
pub(crate) struct SharedAudioFramePacker {
    source_channels: usize,
    pending_frame: Vec<f32>,
}

impl SharedAudioFramePacker {
    pub(crate) fn new(channels: u16) -> Self {
        let source_channels = channels.max(1) as usize;
        Self {
            source_channels,
            pending_frame: Vec::with_capacity(source_channels.min(8)),
        }
    }

    pub(crate) fn push_sample(&mut self, sample: f32) -> Option<(f32, f32)> {
        self.pending_frame.push(sample);
        if self.pending_frame.len() < self.source_channels {
            return None;
        }

        let frame = if self.source_channels == 1 {
            (self.pending_frame[0], self.pending_frame[0])
        } else {
            (self.pending_frame[0], self.pending_frame[1])
        };
        self.pending_frame.clear();
        Some(frame)
    }
}

#[derive(Clone)]
pub struct SharedAudioWriter {
    inner: Arc<SharedAudioInner>,
}

struct SharedAudioInner {
    _mapping: PlatformMapping,
    view: NonNull<u8>,
    capacity_frames: usize,
    sample_rate: AtomicU32,
    write_frame: AtomicU64,
    generation: AtomicU64,
    last_transport_control: AtomicU32,
    last_visualizer_control: AtomicU32,
}

unsafe impl Send for SharedAudioInner {}
unsafe impl Sync for SharedAudioInner {}

impl SharedAudioWriter {
    fn new(mapping: PlatformMapping) -> Self {
        let view = mapping.view();
        let capacity_frames = DEFAULT_CAPACITY_FRAMES as usize;
        let inner = SharedAudioInner {
            _mapping: mapping,
            view,
            capacity_frames,
            sample_rate: AtomicU32::new(44_100),
            write_frame: AtomicU64::new(0),
            generation: AtomicU64::new(1),
            last_transport_control: AtomicU32::new(0),
            last_visualizer_control: AtomicU32::new(0),
        };
        inner.initialize_header();
        Self {
            inner: Arc::new(inner),
        }
    }

    pub fn set_format(&self, _channels: u16, sample_rate: u32) {
        let sample_rate = sample_rate.max(1);
        if self.inner.sample_rate.swap(sample_rate, Ordering::Release) != sample_rate {
            self.reset();
        }
    }

    pub fn push_frame(&self, left: f32, right: f32) {
        self.inner.push_frame(left, right);
    }

    pub fn reset(&self) {
        self.inner.reset();
    }

    pub fn poll_control(&self) -> SharedAudioControl {
        self.inner.poll_control()
    }
}

impl SharedAudioInner {
    fn header(&self) -> *mut SparkAudioHeader {
        self.view.as_ptr().cast::<SparkAudioHeader>()
    }

    fn sample_ptr(&self) -> *mut f32 {
        unsafe {
            self.view
                .as_ptr()
                .add(size_of::<SparkAudioHeader>())
                .cast::<f32>()
        }
    }

    fn initialize_header(&self) {
        unsafe {
            std::ptr::write(
                self.header(),
                SparkAudioHeader {
                    magic: MAGIC,
                    version: VERSION,
                    header_size: size_of::<SparkAudioHeader>() as u32,
                    capacity_frames: self.capacity_frames as u32,
                    channels: OUTPUT_CHANNELS,
                    sample_rate: self.sample_rate.load(Ordering::Acquire),
                    write_frame: 0,
                    total_frames: 0,
                    generation: self.generation.load(Ordering::Acquire),
                    active: 1,
                    reserved: [0; 7],
                },
            );
        }
        self.clear_audio();
    }

    fn reset(&self) {
        let next_generation = self
            .generation
            .fetch_add(1, Ordering::AcqRel)
            .wrapping_add(1);
        self.write_frame.store(0, Ordering::Release);
        self.clear_audio();
        let header = self.header();
        unsafe {
            std::ptr::addr_of_mut!((*header).sample_rate)
                .write_volatile(self.sample_rate.load(Ordering::Acquire));
            std::ptr::addr_of_mut!((*header).write_frame).write_volatile(0);
            std::ptr::addr_of_mut!((*header).total_frames).write_volatile(0);
            std::ptr::addr_of_mut!((*header).generation).write_volatile(next_generation);
            std::ptr::addr_of_mut!((*header).active).write_volatile(1);
        }
    }

    fn clear_audio(&self) {
        let sample_count = self.capacity_frames * OUTPUT_CHANNELS as usize;
        unsafe {
            std::ptr::write_bytes(self.sample_ptr(), 0, sample_count);
        }
    }

    fn push_frame(&self, left: f32, right: f32) {
        let frame = self.write_frame.fetch_add(1, Ordering::AcqRel);
        let frame_index = (frame as usize) % self.capacity_frames;
        let sample_offset = frame_index * OUTPUT_CHANNELS as usize;
        unsafe {
            let sample_ptr = self.sample_ptr();
            sample_ptr.add(sample_offset).write(left);
            sample_ptr.add(sample_offset + 1).write(right);

            let total = frame.wrapping_add(1);
            let header = self.header();
            std::ptr::addr_of_mut!((*header).write_frame).write_volatile(total);
            std::ptr::addr_of_mut!((*header).total_frames).write_volatile(total);
        }
    }

    fn poll_control(&self) -> SharedAudioControl {
        let header = self.header();
        let transport_generation =
            unsafe { std::ptr::addr_of!((*header).reserved[0]).read_volatile() };
        let transport_state = unsafe { std::ptr::addr_of!((*header).reserved[1]).read_volatile() };
        let visualizer_generation =
            unsafe { std::ptr::addr_of!((*header).reserved[2]).read_volatile() };
        let visualizer_delta =
            unsafe { std::ptr::addr_of!((*header).reserved[3]).read_volatile() as i32 };

        let playback = if transport_generation
            != self
                .last_transport_control
                .swap(transport_generation, Ordering::AcqRel)
        {
            match transport_state {
                1 => Some(true),
                2 => Some(false),
                _ => None,
            }
        } else {
            None
        };

        let delta = if visualizer_generation
            != self
                .last_visualizer_control
                .swap(visualizer_generation, Ordering::AcqRel)
        {
            visualizer_delta.clamp(-16, 16)
        } else {
            0
        };

        SharedAudioControl {
            playback,
            visualizer_delta: delta,
        }
    }
}

impl Drop for SharedAudioInner {
    fn drop(&mut self) {
        let header = self.header();
        unsafe {
            std::ptr::addr_of_mut!((*header).active).write_volatile(0);
        }
    }
}

pub fn open(name: &str) -> Result<SharedAudioWriter> {
    let mapping = PlatformMapping::open(name)
        .with_context(|| format!("opening shared audio stream {name}"))?;
    Ok(SharedAudioWriter::new(mapping))
}

struct PlatformMapping {
    imp: PlatformMappingImp,
}

impl PlatformMapping {
    fn open(name: &str) -> Result<Self> {
        Ok(Self {
            imp: PlatformMappingImp::open(name)?,
        })
    }

    fn view(&self) -> NonNull<u8> {
        self.imp.view()
    }
}

#[cfg(windows)]
struct PlatformMappingImp {
    mapping: windows_sys::Win32::Foundation::HANDLE,
    view: NonNull<u8>,
}

#[cfg(windows)]
impl PlatformMappingImp {
    fn open(name: &str) -> Result<Self> {
        use std::ptr;
        use windows_sys::Win32::Foundation::{
            CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, INVALID_HANDLE_VALUE,
        };
        use windows_sys::Win32::System::Memory::{
            CreateFileMappingW, FILE_MAP_ALL_ACCESS, MapViewOfFile, PAGE_READWRITE,
        };

        let byte_len = shared_byte_len();
        let mapping_name = normalize_windows_name(name);
        let mut wide = mapping_name.encode_utf16().collect::<Vec<_>>();
        wide.push(0);

        let mapping = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                ptr::null(),
                PAGE_READWRITE,
                (byte_len as u64 >> 32) as u32,
                byte_len as u32,
                wide.as_ptr(),
            )
        };
        if mapping.is_null() {
            anyhow::bail!("CreateFileMappingW failed for {name}");
        }
        if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
            unsafe {
                CloseHandle(mapping);
            }
            anyhow::bail!("shared audio stream {mapping_name} is already in use");
        }

        let view_address = unsafe { MapViewOfFile(mapping, FILE_MAP_ALL_ACCESS, 0, 0, byte_len) };
        let view = NonNull::new(view_address.Value.cast::<u8>()).ok_or_else(|| {
            unsafe {
                CloseHandle(mapping);
            }
            anyhow::anyhow!("MapViewOfFile failed for {name}")
        })?;

        Ok(Self { mapping, view })
    }

    fn view(&self) -> NonNull<u8> {
        self.view
    }
}

#[cfg(windows)]
impl Drop for PlatformMappingImp {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Memory::{MEMORY_MAPPED_VIEW_ADDRESS, UnmapViewOfFile};

        unsafe {
            UnmapViewOfFile(MEMORY_MAPPED_VIEW_ADDRESS {
                Value: self.view.as_ptr().cast(),
            });
            CloseHandle(self.mapping);
        }
    }
}

#[cfg(windows)]
fn normalize_windows_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.eq_ignore_ascii_case("Local\\SparkPlayerAudio")
        || trimmed.eq_ignore_ascii_case("Local\\sparkplayer_audio")
    {
        return "Local\\SparkPlayerAudio".to_string();
    }
    if trimmed.starts_with("Local\\") || trimmed.starts_with("Global\\") {
        return trimmed.to_string();
    }

    let raw = if trimmed.is_empty() {
        "sparkplayer_audio"
    } else if let Some(rest) = trimmed.strip_prefix('/') {
        rest
    } else {
        trimmed
            .rsplit(['\\', '/', ':'])
            .next()
            .unwrap_or("sparkplayer_audio")
    };

    if raw.eq_ignore_ascii_case("SparkPlayerAudio") || raw.eq_ignore_ascii_case("sparkplayer_audio")
    {
        return "Local\\SparkPlayerAudio".to_string();
    }

    let mut normalized = String::from("Local\\");
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            normalized.push(ch);
        } else {
            normalized.push('_');
        }
    }
    if normalized == "Local\\" {
        normalized.push_str("SparkPlayerAudio");
    }
    normalized
}

#[cfg(unix)]
struct PlatformMappingImp {
    fd: i32,
    name: std::ffi::CString,
    view: NonNull<u8>,
    byte_len: usize,
}

#[cfg(unix)]
impl PlatformMappingImp {
    fn open(name: &str) -> Result<Self> {
        use std::ptr;

        use libc::{
            EEXIST, MAP_FAILED, MAP_SHARED, O_CREAT, O_EXCL, O_RDWR, PROT_READ, PROT_WRITE, c_void,
            close, ftruncate, mmap, munmap, shm_open, shm_unlink,
        };

        let name = normalize_posix_name(name)?;
        let byte_len = shared_byte_len();
        let fd = unsafe { shm_open(name.as_ptr(), O_CREAT | O_EXCL | O_RDWR, 0o600) };
        if fd < 0 {
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(EEXIST) {
                anyhow::bail!(
                    "shared audio stream {} is already in use",
                    name.to_string_lossy()
                );
            }
            anyhow::bail!("shm_open failed for {}: {error}", name.to_string_lossy());
        }

        if unsafe { ftruncate(fd, byte_len as libc::off_t) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                close(fd);
                shm_unlink(name.as_ptr());
            }
            anyhow::bail!("ftruncate failed for {}: {error}", name.to_string_lossy());
        }

        let view = unsafe {
            mmap(
                ptr::null_mut(),
                byte_len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
        };
        if view == MAP_FAILED {
            let error = std::io::Error::last_os_error();
            unsafe {
                close(fd);
                shm_unlink(name.as_ptr());
            }
            anyhow::bail!("mmap failed for {}: {error}", name.to_string_lossy());
        }

        let view = NonNull::new(view.cast::<u8>()).expect("mmap returned MAP_FAILED or non-null");
        Ok(Self {
            fd,
            name,
            view,
            byte_len,
        })
    }

    fn view(&self) -> NonNull<u8> {
        self.view
    }
}

#[cfg(unix)]
impl Drop for PlatformMappingImp {
    fn drop(&mut self) {
        use libc::{c_void, close, munmap, shm_unlink};

        unsafe {
            munmap(self.view.as_ptr().cast::<c_void>(), self.byte_len);
            close(self.fd);
            shm_unlink(self.name.as_ptr());
        }
    }
}

#[cfg(unix)]
fn normalize_posix_name(name: &str) -> Result<std::ffi::CString> {
    let trimmed = name.trim();
    let raw = if trimmed.is_empty() {
        "sparkplayer_audio"
    } else if let Some(rest) = trimmed.strip_prefix('/') {
        rest
    } else {
        trimmed
            .rsplit(['\\', '/', ':'])
            .next()
            .unwrap_or("sparkplayer_audio")
    };
    let mut normalized = String::with_capacity(raw.len() + 1);
    normalized.push('/');
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            normalized.push(ch);
        } else {
            normalized.push('_');
        }
    }
    if normalized.len() == 1 {
        normalized.push_str("sparkplayer_audio");
    }
    std::ffi::CString::new(normalized).context("shared audio stream name contains a NUL byte")
}

#[cfg(not(any(windows, unix)))]
struct PlatformMappingImp {
    storage: Box<std::cell::UnsafeCell<Vec<u8>>>,
}

#[cfg(not(any(windows, unix)))]
impl PlatformMappingImp {
    fn open(_name: &str) -> Result<Self> {
        Ok(Self {
            storage: Box::new(std::cell::UnsafeCell::new(vec![0; shared_byte_len()])),
        })
    }

    fn view(&self) -> NonNull<u8> {
        let ptr = unsafe { (*self.storage.get()).as_mut_ptr() };
        NonNull::new(ptr).expect("Vec allocation returned null")
    }
}

fn shared_byte_len() -> usize {
    size_of::<SparkAudioHeader>() + DEFAULT_CAPACITY_FRAMES as usize * OUTPUT_CHANNELS as usize * 4
}

#[cfg(test)]
mod tests {
    use super::SharedAudioFramePacker;

    #[test]
    fn mono_samples_are_duplicated_to_stereo_frames() {
        let mut packer = SharedAudioFramePacker::new(1);
        assert_eq!(packer.push_sample(0.25), Some((0.25, 0.25)));
        assert_eq!(packer.push_sample(-0.5), Some((-0.5, -0.5)));
    }

    #[test]
    fn stereo_samples_are_packed_in_pairs() {
        let mut packer = SharedAudioFramePacker::new(2);
        assert_eq!(packer.push_sample(0.25), None);
        assert_eq!(packer.push_sample(-0.5), Some((0.25, -0.5)));
    }

    #[test]
    fn multichannel_sources_keep_the_first_stereo_pair() {
        let mut packer = SharedAudioFramePacker::new(6);
        for sample in [0.1, 0.2, 0.3, 0.4, 0.5] {
            assert_eq!(packer.push_sample(sample), None);
        }
        assert_eq!(packer.push_sample(0.6), Some((0.1, 0.2)));
    }
}
