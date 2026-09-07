//! Explorer's virtual-file clipboard: descriptors are cheap; FileContents
//! starts a transfer only when the destination asks to paste the file.
#![allow(non_snake_case)]
use super::{source::Source, Fetch};
use anyhow::Result;
use crossbeam_channel::{bounded, Receiver, Sender};
use std::{
    mem::{size_of, ManuallyDrop},
    path::Path,
    sync::Arc,
    time::Duration,
};
use windows::{
    core::{implement, HRESULT, PCWSTR},
    Win32::{
        Foundation::*,
        Storage::FileSystem::FILE_ATTRIBUTE_NORMAL,
        System::{Com::*, DataExchange::RegisterClipboardFormatW, Memory::*, Ole::*},
        UI::{Shell::*, WindowsAndMessaging::*},
    },
};
pub struct Files {
    pub requests: Receiver<Fetch>,
    source: Arc<Source>,
    stop: Option<Sender<()>>,
}
impl Files {
    pub fn new(parent: &Path, files: &[lynxrdp_proto::FileEntry]) -> Result<Self> {
        let (source, requests) = Source::new(parent, files)?;
        Ok(Self {
            requests,
            source,
            stop: None,
        })
    }
    pub fn publish(&mut self) -> Result<()> {
        let source = self.source.clone();
        let (ready, rx) = bounded(1);
        let (stop, done) = bounded(1);
        std::thread::Builder::new()
            .name("file-clipboard-ole".into())
            .spawn(move || unsafe {
                let result = (|| -> windows::core::Result<IDataObject> {
                    OleInitialize(None)?;
                    let object: IDataObject = DataObject {
                        source,
                        descriptor: format("FileGroupDescriptorW"),
                        contents: format("FileContents"),
                    }
                    .into();
                    if let Err(error) = OleSetClipboard(&object) {
                        OleUninitialize();
                        return Err(error);
                    }
                    Ok(object)
                })();
                match result {
                    Ok(object) => {
                        let _ = ready.send(Ok(()));
                        loop {
                            if !matches!(
                                done.try_recv(),
                                Err(crossbeam_channel::TryRecvError::Empty)
                            ) {
                                break;
                            }
                            let mut msg = MSG::default();
                            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                                let _ = TranslateMessage(&msg);
                                DispatchMessageW(&msg);
                            }
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        if OleIsCurrentClipboard(&object).is_ok() {
                            let _ = OleSetClipboard(None);
                        }
                        drop(object);
                        OleUninitialize();
                    }
                    Err(e) => {
                        let _ = ready.send(Err(e.to_string()));
                    }
                }
            })?;
        rx.recv_timeout(Duration::from_secs(5))?
            .map_err(anyhow::Error::msg)?;
        self.stop = Some(stop);
        Ok(())
    }
}
impl Drop for Files {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.try_send(());
        }
    }
}
fn format(name: &str) -> u16 {
    let text: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    unsafe { RegisterClipboardFormatW(PCWSTR(text.as_ptr())) as u16 }
}
fn unsupported() -> windows::core::Error {
    HRESULT(0x80040064u32 as i32).into()
}
#[implement(IDataObject)]
struct DataObject {
    source: Arc<Source>,
    descriptor: u16,
    contents: u16,
}
impl DataObject {
    fn formats(&self) -> [FORMATETC; 2] {
        [
            FORMATETC {
                cfFormat: self.descriptor,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: -1,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            },
            FORMATETC {
                cfFormat: self.contents,
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
                ..Default::default()
            },
        ]
    }
}
impl IDataObject_Impl for DataObject_Impl {
    fn GetData(&self, ptr: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        self.QueryGetData(ptr).ok()?;
        let f = unsafe { &*ptr };
        if f.cfFormat == self.descriptor {
            let bytes = 4 + self.source.files.len() * size_of::<FILEDESCRIPTORW>();
            unsafe {
                let memory = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes)?;
                let data = GlobalLock(memory) as *mut u8;
                if data.is_null() {
                    let _ = GlobalFree(memory);
                    return Err(windows::core::Error::from_win32());
                }
                (data as *mut u32).write_unaligned(self.source.files.len() as u32);
                for (index, file) in self.source.files.iter().enumerate() {
                    let mut descriptor = FILEDESCRIPTORW {
                        dwFlags: (FD_ATTRIBUTES.0 | FD_FILESIZE.0) as u32,
                        dwFileAttributes: FILE_ATTRIBUTE_NORMAL.0,
                        nFileSizeHigh: (file.size >> 32) as u32,
                        nFileSizeLow: file.size as u32,
                        ..Default::default()
                    };
                    let mut name = [0u16; 260];
                    for (slot, c) in name
                        .iter_mut()
                        .zip(self.source.names[index].encode_utf16().take(259))
                    {
                        *slot = c;
                    }
                    descriptor.cFileName = name;
                    (data.add(4 + index * size_of::<FILEDESCRIPTORW>()) as *mut FILEDESCRIPTORW)
                        .write_unaligned(descriptor);
                }
                let _ = GlobalUnlock(memory);
                Ok(STGMEDIUM {
                    tymed: TYMED_HGLOBAL.0 as u32,
                    u: STGMEDIUM_0 { hGlobal: memory },
                    ..Default::default()
                })
            }
        } else {
            let path = self
                .source
                .contents(f.lindex as usize)
                .map_err(|e| windows::core::Error::new(E_FAIL, e.to_string()))?;
            use std::os::windows::ffi::OsStrExt;
            let wide: Vec<_> = path.as_os_str().encode_wide().chain(Some(0)).collect();
            let stream = unsafe {
                SHCreateStreamOnFileEx(
                    PCWSTR(wide.as_ptr()),
                    (STGM_READ | STGM_SHARE_DENY_WRITE).0,
                    0,
                    false,
                    None,
                )
            }?;
            Ok(STGMEDIUM {
                tymed: TYMED_ISTREAM.0 as u32,
                u: STGMEDIUM_0 {
                    pstm: ManuallyDrop::new(Some(stream)),
                },
                ..Default::default()
            })
        }
    }
    fn GetDataHere(&self, _: *const FORMATETC, _: *mut STGMEDIUM) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }
    fn QueryGetData(&self, ptr: *const FORMATETC) -> HRESULT {
        let Some(f) = (unsafe { ptr.as_ref() }) else {
            return E_POINTER;
        };
        if f.dwAspect != DVASPECT_CONTENT.0 {
            return DV_E_DVASPECT;
        }
        if f.cfFormat == self.descriptor && f.lindex == -1 && f.tymed & TYMED_HGLOBAL.0 as u32 != 0
        {
            return S_OK;
        }
        if f.cfFormat == self.contents
            && f.tymed & TYMED_ISTREAM.0 as u32 != 0
            && f.lindex >= 0
            && (f.lindex as usize) < self.source.files.len()
        {
            return S_OK;
        }
        DV_E_FORMATETC
    }
    fn GetCanonicalFormatEtc(&self, _: *const FORMATETC, out: *mut FORMATETC) -> HRESULT {
        if !out.is_null() {
            unsafe {
                (*out).ptd = std::ptr::null_mut();
            }
        }
        DATA_S_SAMEFORMATETC
    }
    fn SetData(
        &self,
        _: *const FORMATETC,
        medium: *const STGMEDIUM,
        release: BOOL,
    ) -> windows::core::Result<()> {
        if release.as_bool() && !medium.is_null() {
            unsafe {
                ReleaseStgMedium(medium as *mut STGMEDIUM);
            }
        }
        Ok(())
    }
    fn EnumFormatEtc(&self, direction: u32) -> windows::core::Result<IEnumFORMATETC> {
        if direction != DATADIR_GET.0 as u32 {
            return Err(unsupported());
        }
        unsafe { SHCreateStdEnumFmtEtc(&self.formats()) }
    }
    fn DAdvise(
        &self,
        _: *const FORMATETC,
        _: u32,
        _: Option<&IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn DUnadvise(&self, _: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn object(source: Arc<Source>) -> IDataObject {
        DataObject {
            source,
            descriptor: format("FileGroupDescriptorW"),
            contents: format("FileContents"),
        }
        .into()
    }
    #[test]
    fn explorer_metadata_does_not_fetch_but_paste_returns_a_stream() {
        let directory = tempfile::tempdir().unwrap();
        let (source, requests) = Source::new(
            directory.path(),
            &[lynxrdp_proto::FileEntry {
                path: "/remote/report.txt".into(),
                size: 5,
            }],
        )
        .unwrap();
        let data = object(source.clone());
        let descriptor = FORMATETC {
            cfFormat: format("FileGroupDescriptorW"),
            dwAspect: DVASPECT_CONTENT.0,
            lindex: -1,
            tymed: TYMED_HGLOBAL.0 as u32,
            ..Default::default()
        };
        unsafe {
            data.QueryGetData(&descriptor).ok().unwrap();
            let _formats = data.EnumFormatEtc(DATADIR_GET.0 as u32).unwrap();
            let mut medium = data.GetData(&descriptor).unwrap();
            let memory = medium.u.hGlobal;
            let bytes = GlobalLock(memory) as *const u8;
            assert_eq!((bytes as *const u32).read_unaligned(), 1);
            let file = (bytes.add(4) as *const FILEDESCRIPTORW).read_unaligned();
            let size = file.nFileSizeLow;
            assert_eq!(size, 5);
            let name = file.cFileName;
            assert_eq!(String::from_utf16_lossy(&name[..10]), "report.txt");
            let _ = GlobalUnlock(memory);
            ReleaseStgMedium(&mut medium);
        }
        assert!(requests.is_empty());
        let worker = std::thread::spawn(move || unsafe {
            let data = object(source);
            let content = FORMATETC {
                cfFormat: format("FileContents"),
                dwAspect: DVASPECT_CONTENT.0,
                lindex: 0,
                tymed: TYMED_ISTREAM.0 as u32,
                ..Default::default()
            };
            let mut medium = data.GetData(&content).unwrap();
            let stream = medium.u.pstm.as_ref().unwrap();
            let mut bytes = [0u8; 5];
            let mut read = 0;
            stream
                .Read(bytes.as_mut_ptr().cast(), 5, Some(&mut read))
                .ok()
                .unwrap();
            assert_eq!(read, 5);
            assert_eq!(&bytes, b"hello");
            ReleaseStgMedium(&mut medium);
        });
        let fetch = requests.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(fetch.remote, "/remote/report.txt");
        std::fs::write(&fetch.destination, b"hello").unwrap();
        fetch.result.send(Some(fetch.destination)).unwrap();
        worker.join().unwrap();
        assert!(requests.is_empty());
    }
}
