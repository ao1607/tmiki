//! pixel_limit.py: the largest canvas (CLIP / MDP2) and raster image tmiki
//! agrees to decode. Over the limit Python raises CanvasTooLargeError with
//! a message naming where the limit comes from; that is left to Python
//! (the import declines).

use tmi_store::json::{Int, Value};

/// Pillow's hard-error threshold (2 x its default MAX_IMAGE_PIXELS).
pub const FLOOR: u64 = 178_956_970;
pub const LAYERED_BYTES_PER_PIXEL: u64 = 100;
pub const RASTER_BYTES_PER_PIXEL: u64 = 20;

/// The effective limits (0 = no limit).
#[derive(Clone, Copy, Debug)]
pub struct Limits {
    pub layered: u64,
    pub raster: u64,
}

impl Limits {
    pub const NONE: Limits = Limits { layered: 0, raster: 0 };

    /// `check_pixels(width, height)` for a layered canvas: Err when over.
    pub fn check_layered(&self, width: i64, height: i64) -> Result<(), String> {
        if width == 0 || height == 0 {
            return Ok(());
        }
        let pixels = (width as i128) * (height as i128);
        if self.layered != 0 && pixels > self.layered as i128 {
            return Err(format!("canvas of {pixels} pixels is over core.maxImagePixels: Python reports it"));
        }
        Ok(())
    }

    /// `check_raster_pixels(size)`.
    pub fn check_raster(&self, width: usize, height: usize) -> Result<(), String> {
        let pixels = (width.max(1) as u128) * (height.max(1) as u128);
        if self.raster != 0 && pixels > self.raster as u128 {
            return Err(format!("image of {pixels} pixels is over core.maxImagePixels: Python reports it"));
        }
        Ok(())
    }
}

/// `parse_value(raw)` for the plain forms; Err (Python decides, maybe with
/// an error) for anything else.
pub fn parse_value(raw: &Value) -> Result<u64, String> {
    let text = match raw {
        Value::Str(s) => s.clone(),
        Value::Int(Int::Small(v)) => v.to_string(),
        _ => return Err("core.maxImagePixels is not a plain number".into()),
    };
    let t: String = text.trim().replace('_', "");
    let digits = t.strip_prefix('+').unwrap_or(&t);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) || digits.len() > 18 {
        return Err("core.maxImagePixels is not a plain number".into());
    }
    Ok(digits.parse().unwrap())
}

/// `resolve(kind)` for both kinds: `TMI_MAX_IMAGE_PIXELS`, then the repo
/// config's value, then the user config's (both given by the caller as
/// `core.maxImagePixels`, None when unset), then the RAM default.
pub fn resolve(repo_value: Option<&Value>, user_value: Option<&Value>) -> Result<Limits, String> {
    let env = std::env::var("TMI_MAX_IMAGE_PIXELS").unwrap_or_default();
    let env = env.trim();
    if !env.is_empty() {
        let v = parse_value(&Value::Str(env.to_string()))?;
        return Ok(Limits { layered: v, raster: v });
    }
    for raw in [repo_value, user_value].into_iter().flatten() {
        if !raw.is_null() {
            let v = parse_value(raw)?;
            return Ok(Limits { layered: v, raster: v });
        }
    }
    let ram = total_ram_bytes();
    let default = |bpp: u64| match ram {
        None | Some(0) => FLOOR,
        Some(r) => FLOOR.max((r / 2) / bpp),
    };
    Ok(Limits { layered: default(LAYERED_BYTES_PER_PIXEL), raster: default(RASTER_BYTES_PER_PIXEL) })
}

/// Installed physical memory.
pub fn total_ram_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        // SAFETY: a zeroed MEMORYSTATUSEX with its length set, as the API asks.
        unsafe {
            let mut status: MEMORYSTATUSEX = std::mem::zeroed();
            status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
            if GlobalMemoryStatusEx(&mut status) == 0 {
                return None;
            }
            Some(status.ullTotalPhys).filter(|&v| v != 0)
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // SAFETY: sysconf takes no pointers.
        let (pages, size) = unsafe { (libc::sysconf(libc::_SC_PHYS_PAGES), libc::sysconf(libc::_SC_PAGE_SIZE)) };
        if pages <= 0 || size <= 0 {
            return None;
        }
        Some(pages as u64 * size as u64)
    }
    #[cfg(target_os = "macos")]
    {
        let mut value: u64 = 0;
        let mut size = std::mem::size_of::<u64>();
        // SAFETY: a u64 out-buffer of the size passed, as sysctlbyname asks.
        let r = unsafe {
            libc::sysctlbyname(
                b"hw.memsize\0".as_ptr() as *const libc::c_char,
                &mut value as *mut u64 as *mut libc::c_void,
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        (r == 0 && value != 0).then_some(value)
    }
}
