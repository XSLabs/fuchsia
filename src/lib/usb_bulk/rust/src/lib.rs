// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrapper to talk to USB devices from a host.
//!
//! # Supports
//!  - OS backends: Linux and MacOS.
//!  - Bulk only interfaces with a single IN and OUT pipe.
//!
//! # Examples
//!
//! See tests for examples using the Zedmon power monitor.

use fuchsia_async::unblock;
use futures::io::{AsyncRead, AsyncWrite};
use futures::task::{Context, Poll};
use lazy_static::lazy_static;
use std::collections::HashMap;
use std::fmt::{Debug, Formatter, Result as FmtResult};
use std::future::Future;
use std::io::{Read, Write};
use std::os::raw::c_void;
use std::pin::Pin;
use std::sync::{Mutex, RwLock};
use thiserror::Error;

mod usb;

#[derive(Error, Debug)]
pub enum Error {
    #[error("No device matched.")]
    NoDeviceMatched,
    #[error("Serial number missing.")]
    SerialNumberMissing,
}

/// Standard result type that defaults to our [`Error`] for the error type.
pub type Result<V, E = Error> = std::result::Result<V, E>;

/// A collection of information about an interface.
///
/// Used for matching interfaces to open.
pub type InterfaceInfo = usb::usb_ifc_info;

/// Opens a USB interface.
///
/// `matcher` will be called on every discovered interface.  When `matcher` returns true, that
/// interface will be opened.
pub trait Open<T> {
    fn open<F>(matcher: &mut F) -> Result<T>
    where
        F: FnMut(&InterfaceInfo) -> bool;
}

/// A USB Interface.
///
/// See top-level crate docs for an example.
#[derive(Debug)]
pub struct Interface {
    interface: *mut usb::UsbInterface,
}

/// Send implementation for USB interface.
///
///  This struct wraps a raw pointer which according to the Rust documentation found at
///  https://doc.rust-lang.org/nomicon/send-and-sync.html: "However raw pointers
///  are, strictly speaking, marked as thread-unsafe as more of a lint. Doing anything useful with
///  a raw pointer requires dereferencing it, which is already unsafe. In that sense, one could
///  argue that it would be "fine" for them to be marked as thread safe."
unsafe impl Send for Interface {}

lazy_static! {
    static ref IFACE_REGISTRY: RwLock<HashMap<String, Mutex<Interface>>> =
        RwLock::new(HashMap::new());
}

/// An Asynchronous USB Interface.  This wraps the synchronous calls to allow for yields between
/// writing.
pub struct AsyncInterface {
    serial: String,
    write_task: Option<Pin<Box<dyn Future<Output = std::io::Result<usize>> + Send>>>,
    read_task: Option<Pin<Box<dyn Future<Output = std::io::Result<Vec<u8>>> + Send>>>,
}

impl Interface {
    // This shouldn't be called more than once, since it deletes the underlying C++ object.
    unsafe fn close(&mut self) {
        // Foreign function call requires unsafe block.
        unsafe {
            usb::interface_close(self.interface);
        }
    }

    fn open_interface<F>(matcher: &mut F, timeout: u32) -> Result<Self>
    where
        F: FnMut(&InterfaceInfo) -> bool,
    {
        // Generate a trampoline for calling our matcher from a C callback.
        extern "C" fn trampoline<F>(ifc_ptr: *mut usb::usb_ifc_info, data: *mut c_void) -> bool
        where
            F: FnMut(&InterfaceInfo) -> bool,
        {
            // Undoes the cast of `matcher` to `*mut c_void` performed in the call to
            // `usb::interface_open`, below.
            let callback: &mut F = unsafe { &mut *(data as *mut F) };

            // Casts the raw interface pointer to a safe reference. Requires that `ifc_ptr`, as
            // as provided by the C++ `interface_open`, be a valid pointer to a `usb::usb_ifc_info`.
            let interface = unsafe { &*ifc_ptr };

            (*callback)(interface)
        }

        // Call into the low level driver to open the interface.  The matcher itself is passesd
        // as a void pointer which is re-intepreted by the above trampoline.  The foreign function
        // call requires an unsafe block.
        let device_ptr = unsafe {
            usb::interface_open(Some(trampoline::<F>), matcher as *mut F as *mut c_void, timeout)
        };
        if !device_ptr.is_null() {
            return Ok(Interface { interface: device_ptr as *mut usb::UsbInterface });
        } else {
            return Err(Error::NoDeviceMatched);
        }
    }
}

impl Open<Interface> for Interface {
    fn open<F>(matcher: &mut F) -> Result<Interface>
    where
        F: FnMut(&InterfaceInfo) -> bool,
    {
        Self::open_interface(matcher, 200)
    }
}

impl Drop for Interface {
    fn drop(&mut self) {
        // Unsafe: This is the only call to this function
        unsafe {
            self.close();
        }
    }
}

impl Read for Interface {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let buf_ptr = buf.as_mut_ptr() as *mut c_void;

        // Foreign function call requires unsafe block.
        let ret =
            unsafe { usb::interface_read(self.interface, buf_ptr, buf.len() as usb::ssize_t) };

        if ret < 0 {
            return Err(std::io::Error::other(format!("Read error: {}", ret)));
        }
        return Ok(ret as usize);
    }
}

impl Write for Interface {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let buf_ptr = buf.as_ptr() as *const c_void;

        // Foreign function call requires unsafe block.
        let ret =
            unsafe { usb::interface_write(self.interface, buf_ptr, buf.len() as usb::ssize_t) };

        if ret < 0 {
            return Err(std::io::Error::other(format!("Write error: {}", ret)));
        }
        return Ok(ret as usize);
    }

    fn flush(&mut self) -> std::io::Result<()> {
        // Do nothing as we're not buffered.
        Ok(())
    }
}

impl Debug for AsyncInterface {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.debug_struct("AsyncInterface").field("serial", &self.serial).finish()
    }
}

impl Drop for AsyncInterface {
    fn drop(&mut self) {
        let mut write_guard =
            IFACE_REGISTRY.write().expect("could not acquire write lock on interface registry");
        if let Some(iface) = (*write_guard).remove(&self.serial) {
            // To be clear, when the Interface is dropped, it will close and clean up the
            // C++ object.
            drop(iface);
        }
    }
}

impl Open<AsyncInterface> for AsyncInterface {
    fn open<F>(matcher: &mut F) -> Result<AsyncInterface>
    where
        F: FnMut(&InterfaceInfo) -> bool,
    {
        Self::open_interface(matcher, true)
    }
}

impl AsyncInterface {
    /// Opens the interface with the matcher (not draining the
    /// device's buffer), and returns if the connection could be successfully opened
    pub fn check<F>(matcher: &mut F) -> bool
    where
        F: FnMut(&InterfaceInfo) -> bool,
    {
        log::debug!("AsyncInterface checking interface");
        Self::open_interface(matcher, false).is_ok()
    }

    /// Opens the interface with the matcher.
    ///
    /// If `drain_input_queue` is true, the device's input queue will be
    /// drained before returning.
    fn open_interface<F>(matcher: &mut F, drain_input_queue: bool) -> Result<Self>
    where
        F: FnMut(&InterfaceInfo) -> bool,
    {
        // Mutex to hold the serial number of the opened device.
        let serial: Mutex<Option<String>> = Mutex::new(None);
        // Mutex to hold the matcher.
        let m_doom = Mutex::new(&mut *matcher);
        // Callback function to be passed to the C library.
        let mut cb = |info: &InterfaceInfo| -> bool {
            match m_doom.lock() {
                Ok(mut matcher) => {
                    if matcher(info) {
                        let null_pos = match info.serial_number.iter().position(|&c| c == 0) {
                            Some(p) => p,
                            None => {
                                return false;
                            }
                        };
                        // Since the interface has a serial number, go ahead and
                        // update the serial number we are tracking. The
                        // `Interface` struct does not expose the serial serial
                        // number or the InterfaceInfo after it has been opened
                        // so this is our chance to record it.
                        match serial.lock() {
                            Ok(mut ser) => {
                                ser.replace(
                                    (*String::from_utf8_lossy(&info.serial_number[..null_pos]))
                                        .to_string(),
                                );
                                return true;
                            }
                            Err(_) => return false,
                        }
                    } else {
                        false
                    }
                }
                Err(_) => false,
            }
        };

        // Callback function to be passed to the C library when draining the
        // input queue.
        let mut drain_cb = |info: &InterfaceInfo| -> bool {
            match m_doom.lock() {
                // First make sure we match.
                Ok(mut matcher) => {
                    if matcher(info) {
                        // Since we match we need to check that the interface
                        // actually has a serial number.
                        match info.serial_number.iter().position(|&c| c == 0) {
                            Some(_) => true,
                            None => false,
                        }
                    } else {
                        false
                    }
                }
                Err(_) => false,
            }
        };
        // If `drain_input_queue` is true, drain the input queue before opening the interface.
        if drain_input_queue {
            // Open the interface with the drain callback.
            // Set the timeout to 200 ms so that we dont block forever on the
            // `read_to_end` call below. If the timeout is 0 we will block
            // waiting on the device to return forever and if there is nothing
            // to drain from the queue we'll just be waiting forever.
            let mut iface = Interface::open_interface(&mut drain_cb, 200)?;
            // Clears out anything that was in the usb buffer waiting.
            log::debug!("AsyncInterface about to drain input queue");
            let mut buffer = Vec::new();
            let _read_res = iface.read_to_end(&mut buffer);
            drop(iface);
            // Set timeout to 0 to help reduce spamming the target with
            // read requests.
            Self::add_interface_to_registry(&serial, &mut cb, 0)
        } else {
            // Open interface with standard callback
            // Set timeout to 0 to help reduce spamming the target with
            // read requests.
            Self::add_interface_to_registry(&serial, &mut cb, 0)
        }
    }

    fn add_interface_to_registry<F>(
        serial: &Mutex<Option<String>>,
        cb: &mut F,
        timeout: u32,
    ) -> Result<Self>
    where
        F: Fn(&InterfaceInfo) -> bool,
    {
        let iface = Interface::open_interface(cb, timeout)?;
        match serial.lock() {
            Ok(guard) => match *guard {
                Some(ref s) => {
                    log::debug!("AsyncInterface open_interface() for serial {}.", s);
                    let mut write_guard = IFACE_REGISTRY
                        .write()
                        .expect("could not acquire write lock on interface registry");
                    (*write_guard).insert(s.clone(), Mutex::new(iface));
                    Ok(AsyncInterface { serial: s.to_owned(), write_task: None, read_task: None })
                }
                None => Err(Error::SerialNumberMissing),
            },
            Err(_e) => Err(Error::SerialNumberMissing),
        }
    }
}

impl AsyncWrite for AsyncInterface {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.write_task.is_none() {
            let buffer = buf[..].to_vec();
            let serial_clone = self.serial.clone();
            self.write_task.replace(Box::pin(unblock(move || {
                let read_guard = IFACE_REGISTRY
                    .read()
                    .expect("could not acquire read lock on interface registry");
                if let Some(iface_lock) = (*read_guard).get(&serial_clone) {
                    iface_lock.lock().unwrap().write(&buffer)
                } else {
                    Err(std::io::Error::other("Interface missing from registry"))
                }
            })));
        }

        if let Some(ref mut task) = self.write_task {
            match task.as_mut().poll(cx) {
                Poll::Ready(s) => {
                    self.write_task = None;
                    Poll::Ready(s)
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(std::io::Error::other("Could not add async task to write")))
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_close(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // Interface is closed in the drop method.
        Poll::Ready(Ok(()))
    }
}

impl AsyncRead for AsyncInterface {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        mut buf: &mut [u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.read_task.is_none() {
            let mut buffer = buf[..].to_vec();
            let serial_clone = self.serial.clone();
            self.read_task.replace(Box::pin(unblock(move || {
                let read_guard = IFACE_REGISTRY
                    .read()
                    .expect("could not acquire read lock on interface registry");
                if let Some(iface_lock) = (*read_guard).get(&serial_clone) {
                    let read = iface_lock.lock().unwrap().read(&mut buffer)?;
                    buffer.truncate(read);
                    Ok(buffer)
                } else {
                    Err(std::io::Error::other("Interface missing from registry"))
                }
            })));
        }

        if let Some(ref mut task) = self.read_task {
            match task.as_mut().poll(cx) {
                Poll::Ready(s) => {
                    self.read_task = None;
                    match s {
                        Ok(v) => Poll::Ready(buf.write(&v)),
                        Err(e) => Poll::Ready(Err(e)),
                    }
                }
                Poll::Pending => Poll::Pending,
            }
        } else {
            Poll::Ready(Err(std::io::Error::other("Could not add async task to read")))
        }
    }
}

/// Tests are based on the [Zedmon power monitor](https://fuchsia.googlesource.com/zedmon).
///
/// In order to run them, the host should be connected to exactly one Zedmon device, satisfying:
///  - Hardware version 2.1;
///  - Firmware built from the Zedmon repository's revision 9765b27b5f, or equivalent.
#[cfg(test)]
mod tests {
    use super::*;

    fn zedmon_match(ifc: &InterfaceInfo) -> bool {
        (ifc.dev_vendor == 0x18d1)
            && (ifc.dev_product == 0xaf00)
            && (ifc.ifc_class == 0xff)
            && (ifc.ifc_subclass == 0xff)
            && (ifc.ifc_protocol == 0x00)
    }

    #[test]
    fn test_zedmon_enumeration() {
        let mut serials = Vec::new();
        let mut cb = |info: &InterfaceInfo| -> bool {
            if zedmon_match(info) {
                let null_pos = match info.serial_number.iter().position(|&c| c == 0) {
                    Some(p) => p,
                    None => {
                        return false;
                    }
                };
                serials
                    .push((*String::from_utf8_lossy(&info.serial_number[..null_pos])).to_string());
            }
            false
        };
        let result = Interface::open(&mut cb);
        assert!(result.is_err(), "Enumeration matcher should not open any device.");
        assert_eq!(serials.len(), 1, "Host should have exactly one zedmon device connected");
    }

    #[test]
    fn test_zedmon_read_parameter() {
        // Open USB interface.
        let mut matcher = |info: &InterfaceInfo| -> bool { zedmon_match(info) };
        let mut interface = Interface::open(&mut matcher).unwrap();

        // Send a Query Parameter request.
        interface.write_all(&[0x02, 0x00]).unwrap();

        // Read response.
        let mut packet = [0x00; 64];
        let len = interface.read(&mut packet).unwrap();

        // Verify the parameter is as we expect.  Format of this packet can be
        // found at https://fuchsia.googlesource.com/zedmon/+/HEAD/docs/usb_proto.md
        assert_eq!(
            packet[..len - 1],
            [
                0x83, 0x73, 0x68, 0x75, 0x6e, 0x74, 0x5f, 0x72, 0x65, 0x73, 0x69, 0x73, 0x74, 0x61,
                0x6e, 0x63, 0x65, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
                0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x40,
                0x0a, 0xd7, 0x23, 0x3c, 0x00, 0x00, 0x00
            ][..]
        );
    }
}
