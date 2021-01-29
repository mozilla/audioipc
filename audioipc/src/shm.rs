// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

use crate::{errors::*, PlatformHandle};
use memmap::{MmapMut, MmapOptions};
use std::convert::TryInto;
use std::env::temp_dir;
use std::fs::{remove_file, File, OpenOptions};
use std::{ffi::c_void, slice};

fn open_shm_file(id: &str) -> Result<File> {
    #[cfg(target_os = "linux")]
    {
        let id_cstring = std::ffi::CString::new(id).unwrap();
        unsafe {
            let r = libc::syscall(libc::SYS_memfd_create, id_cstring.as_ptr(), 0);
            if r >= 0 {
                use std::os::unix::io::FromRawFd as _;
                return Ok(File::from_raw_fd(r.try_into().unwrap()));
            }
        }

        let mut path = std::path::PathBuf::from("/dev/shm");
        path.push(id);

        if let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
        {
            let _ = remove_file(&path);
            return Ok(file);
        }
    }

    let mut path = temp_dir();
    path.push(id);

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&path)?;

    let _ = remove_file(&path);
    Ok(file)
}

#[cfg(unix)]
fn handle_enospc(s: &str) -> Result<()> {
    let err = std::io::Error::last_os_error();
    let errno = err.raw_os_error().unwrap_or(0);
    assert_ne!(errno, 0);
    debug!("allocate_file: {} failed errno={}", s, errno);
    if errno == libc::ENOSPC {
        return Err(err.into());
    }
    Ok(())
}

#[cfg(unix)]
fn allocate_file(file: &File, size: usize) -> Result<()> {
    use std::os::unix::io::AsRawFd;

    // First, set the file size.  This may create a sparse file on
    // many systems, which can fail with SIGBUS when accessed via a
    // mapping and the lazy backing allocation fails due to low disk
    // space.  To avoid this, try to force the entire file to be
    // preallocated before mapping using OS-specific approaches below.

    file.set_len(size.try_into().unwrap())?;

    let fd = file.as_raw_fd();
    let size: libc::off_t = size.try_into().unwrap();

    // Try Linux-specific fallocate.
    #[cfg(target_os = "linux")]
    {
        if unsafe { libc::fallocate(fd, 0, 0, size) } == 0 {
            return Ok(());
        }
        handle_enospc("fallocate()")?;
    }

    // Try macOS-specific fcntl.
    #[cfg(target_os = "macos")]
    {
        let params = libc::fstore_t {
            fst_flags: libc::F_ALLOCATEALL,
            fst_posmode: libc::F_PEOFPOSMODE,
            fst_offset: 0,
            fst_length: size,
            fst_bytesalloc: 0,
        };
        if unsafe { libc::fcntl(fd, libc::F_PREALLOCATE, &params) } == 0 {
            return Ok(());
        }
        handle_enospc("fcntl(F_PREALLOCATE)")?;
    }

    // Fall back to portable version, where available.
    #[cfg(any(target_os = "linux", target_os = "freebsd", target_os = "dragonfly"))]
    {
        if unsafe { libc::posix_fallocate(fd, 0, size) } == 0 {
            return Ok(());
        }
        handle_enospc("posix_fallocate()")?;
    }

    Ok(())
}

#[cfg(windows)]
fn allocate_file(file: &File, size: usize) -> Result<()> {
    // CreateFileMapping will ensure the entire file is allocated
    // before it's mapped in, so we simply set the size here.
    file.set_len(size.try_into().unwrap())?;
    Ok(())
}

#[derive(Copy, Clone)]
pub struct SharedMemView {
    view: *mut c_void,
    size: usize,
}

unsafe impl Send for SharedMemView {}

impl SharedMemView {
    pub unsafe fn get_slice(&self, size: usize) -> Result<&[u8]> {
        let map = slice::from_raw_parts(self.view as _, self.size);
        if size <= self.size {
            let buf = &map[..size];
            Ok(buf)
        } else {
            bail!("mmap size");
        }
    }

    pub unsafe fn get_mut_slice(&mut self, size: usize) -> Result<&mut [u8]> {
        let map = slice::from_raw_parts_mut(self.view as _, self.size);
        if size <= self.size {
            Ok(&mut map[..size])
        } else {
            bail!("mmap size")
        }
    }
}

pub struct SharedMem {
    _mmap: MmapMut,
    view: SharedMemView,
}

unsafe impl Send for SharedMem {}

impl SharedMem {
    pub fn new(id: &str, size: usize) -> Result<(SharedMem, PlatformHandle)> {
        let file = open_shm_file(id)?;
        allocate_file(&file, size)?;
        let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
        assert_eq!(mmap.len(), size);
        let view = SharedMemView {
            view: mmap.as_mut_ptr() as _,
            size,
        };
        let handle = PlatformHandle::from(file);
        Ok((SharedMem { _mmap: mmap, view }, handle))
    }

    pub unsafe fn from(handle: &PlatformHandle, size: usize) -> Result<SharedMem> {
        let mut mmap = MmapOptions::new().map_mut(&handle.into_file())?;
        assert_eq!(mmap.len(), size);
        let view = SharedMemView {
            view: mmap.as_mut_ptr() as _,
            size,
        };
        Ok(SharedMem { _mmap: mmap, view })
    }

    pub unsafe fn unsafe_view(&self) -> SharedMemView {
        self.view
    }

    pub unsafe fn get_slice(&self, size: usize) -> Result<&[u8]> {
        self.view.get_slice(size)
    }

    pub unsafe fn get_mut_slice(&mut self, size: usize) -> Result<&mut [u8]> {
        self.view.get_mut_slice(size)
    }
}
