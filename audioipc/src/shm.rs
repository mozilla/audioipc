// Copyright © 2017 Mozilla Foundation
//
// This program is made available under an ISC-style license.  See the
// accompanying file LICENSE for details.

use crate::errors::*;
use crate::PlatformHandle;
use std::{convert::TryInto, ffi::c_void, slice};

#[cfg(unix)]
pub use unix::SharedMem;
#[cfg(windows)]
pub use windows::SharedMem;

#[derive(Copy, Clone)]
pub struct SharedMemView {
    ptr: *mut c_void,
    size: usize,
}

unsafe impl Send for SharedMemView {}

impl SharedMemView {
    pub unsafe fn get_slice(&self, size: usize) -> Result<&[u8]> {
        let map = slice::from_raw_parts(self.ptr as _, self.size);
        if size <= self.size {
            Ok(&map[..size])
        } else {
            bail!("mmap size");
        }
    }

    pub unsafe fn get_mut_slice(&mut self, size: usize) -> Result<&mut [u8]> {
        let map = slice::from_raw_parts_mut(self.ptr as _, self.size);
        if size <= self.size {
            Ok(&mut map[..size])
        } else {
            bail!("mmap size")
        }
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use memmap::{MmapMut, MmapOptions};
    use std::env::temp_dir;
    use std::fs::{remove_file, File, OpenOptions};
    use std::os::unix::io::FromRawFd;

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

    pub struct SharedMem {
        _mmap: MmapMut,
        view: SharedMemView,
    }

    impl SharedMem {
        pub fn new(id: &str, size: usize) -> Result<(SharedMem, PlatformHandle)> {
            let file = open_shm_file(id)?;
            allocate_file(&file, size)?;
            let mut mmap = unsafe { MmapOptions::new().map_mut(&file)? };
            assert_eq!(mmap.len(), size);
            let view = SharedMemView {
                ptr: mmap.as_mut_ptr() as _,
                size,
            };
            let handle = PlatformHandle::from(file);
            Ok((SharedMem { _mmap: mmap, view }, handle))
        }

        pub unsafe fn from(handle: &PlatformHandle, size: usize) -> Result<SharedMem> {
            let mut mmap = {
                let file = File::from_raw_fd(handle.into_raw());
                MmapOptions::new().map_mut(&file)?
            };
            assert_eq!(mmap.len(), size);
            let view = SharedMemView {
                ptr: mmap.as_mut_ptr() as _,
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
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::ptr;
    use winapi::{
        shared::{minwindef::DWORD, ntdef::HANDLE},
        um::{
            handleapi::CloseHandle,
            memoryapi::{MapViewOfFile, UnmapViewOfFile, FILE_MAP_ALL_ACCESS},
            winbase::CreateFileMappingA,
            winnt::PAGE_READWRITE,
        },
    };

    use crate::INVALID_HANDLE_VALUE;

    pub struct SharedMem {
        handle: HANDLE,
        view: SharedMemView,
    }

    unsafe impl Send for SharedMem {}

    impl Drop for SharedMem {
        fn drop(&mut self) {
            unsafe {
                let ok = UnmapViewOfFile(self.view.ptr);
                assert_ne!(ok, 0);
                if self.handle != INVALID_HANDLE_VALUE {
                    let ok = CloseHandle(self.handle);
                    assert_ne!(ok, 0);
                }
            }
        }
    }

    impl SharedMem {
        pub fn new(_id: &str, size: usize) -> Result<(SharedMem, PlatformHandle)> {
            unsafe {
                let handle = CreateFileMappingA(
                    INVALID_HANDLE_VALUE,
                    ptr::null_mut(),
                    PAGE_READWRITE,
                    (size >> 32).try_into().unwrap(),
                    (size & (DWORD::MAX as usize)).try_into().unwrap(),
                    ptr::null(),
                );
                if handle.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }

                let ptr = MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, size);
                if ptr.is_null() {
                    return Err(std::io::Error::last_os_error().into());
                }

                let handle2 = PlatformHandle::duplicate(handle)?;
                Ok((
                    SharedMem {
                        handle,
                        view: SharedMemView { ptr, size },
                    },
                    handle2,
                ))
            }
        }

        pub unsafe fn from(handle: &PlatformHandle, size: usize) -> Result<SharedMem> {
            let ptr = MapViewOfFile(handle.as_raw(), FILE_MAP_ALL_ACCESS, 0, 0, size);
            if ptr.is_null() {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(SharedMem {
                handle: INVALID_HANDLE_VALUE,
                view: SharedMemView { ptr, size },
            })
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
}
