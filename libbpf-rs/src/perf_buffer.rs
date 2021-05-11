use core::ffi::c_void;
use std::boxed::Box;
use std::slice;
use std::time::Duration;

use crate::*;

fn is_power_of_two(i: usize) -> bool {
    i > 0 && (i & (i - 1)) == 0
}

// Workaround for `trait_alias`
// (https://doc.rust-lang.org/unstable-book/language-features/trait-alias.html)
// not being available yet. This is just a custom trait plus a blanket implementation.
pub trait SampleCb<T>: FnMut(Option<&T>, i32, &[u8]) + 'static {}
impl<T, U> SampleCb<U> for T where T: FnMut(Option<&U>, i32, &[u8]) + 'static {}

pub trait LostCb: FnMut(i32, u64) + 'static {}
impl<T> LostCb for T where T: FnMut(i32, u64) + 'static {}

pub struct CbStruct<T> {
    pub sample_cb: Option<Box<dyn SampleCb<T>>>,
    pub lost_cb: Option<Box<dyn LostCb>>,
    pub ctx: Option<Box<T>>,
}

/// Builds [`PerfBuffer`] instances.
pub struct PerfBufferBuilder<'a, T> {
    map: &'a Map,
    pages: usize,
    sample_cb: Option<Box<dyn SampleCb<T>>>,
    lost_cb: Option<Box<dyn LostCb>>,
    ctx: Option<Box<T>>, // used for passing to the perf buffer opts
}

impl<'a, T> PerfBufferBuilder<'a, T> {
    pub fn new(map: &'a Map) -> Self {
        Self {
            map,
            pages: 64,
            sample_cb: None,
            lost_cb: None,
            ctx: None,
        }
    }
}

impl<'a, T> PerfBufferBuilder<'a, T> {
    /// Callback to run when a sample is received.
    ///
    /// This callback provides a raw byte slice. You may find libraries such as
    /// [`plain`](https://crates.io/crates/plain) helpful.
    ///
    /// Callback arguments are: `(cpu, data)`.
    pub fn sample_cb<NewCb: SampleCb<T>>(self, cb: NewCb) -> PerfBufferBuilder<'a, T> {
        PerfBufferBuilder {
            map: self.map,
            pages: self.pages,
            sample_cb: Some(Box::new(cb)),
            lost_cb: self.lost_cb,
            ctx: self.ctx,
        }
    }

    /// Callback to run when a sample is received.
    ///
    /// Callback arguments are: `(cpu, lost_count)`.
    pub fn lost_cb<NewCb: LostCb>(self, cb: NewCb) -> PerfBufferBuilder<'a, T> {
        PerfBufferBuilder {
            map: self.map,
            pages: self.pages,
            sample_cb: self.sample_cb,
            lost_cb: Some(Box::new(cb)),
            ctx: self.ctx
        }
    }

    /// Context used within the perf buffer when exercising of the callbacks
    pub fn ctx(self, ctx: T) -> PerfBufferBuilder<'a, T> {
        PerfBufferBuilder {
            map: self.map,
            pages: self.pages,
            sample_cb: self.sample_cb,
            lost_cb: self.lost_cb,
            ctx: Some(Box::new(ctx)),
        }
    }

    /// The number of pages to size the ring buffer.
    pub fn pages(&mut self, pages: usize) -> &mut Self {
        self.pages = pages;
        self
    }

    pub fn build(self) -> Result<PerfBuffer<T>> {
        if self.map.map_type() != MapType::PerfEventArray {
            return Err(Error::InvalidInput(
                "Must use a PerfEventArray map".to_string(),
            ));
        }

        if !is_power_of_two(self.pages) {
            return Err(Error::InvalidInput(
                "Page count must be power of two".to_string(),
            ));
        }

        let c_sample_cb: libbpf_sys::perf_buffer_sample_fn = if self.sample_cb.is_some() {
            Some(Self::call_sample_cb)
        } else {
            None
        };

        let c_lost_cb: libbpf_sys::perf_buffer_lost_fn = if self.lost_cb.is_some() {
            Some(Self::call_lost_cb)
        } else {
            None
        };

         let callback_struct_ptr = Box::into_raw(Box::new(CbStruct {
            sample_cb: self.sample_cb,
            lost_cb: self.lost_cb,
            ctx: self.ctx,
        }));

         /*
        let callback_struct_ptr = if self.ctxj

        let callback_struct_ptr = Box::into_raw(Box::new(CbStruct {
            sample_cb: self.sample_cb,
            lost_cb: self.lost_cb,
        }));

        let c_ctx: *mut std::ffi::c_void; // = std::ptr::null_mut();
        if let Some(context) = self.ctx {
            c_ctx = context;
        } else {
            c_ctx = callback_struct_ptr as *mut _;
        }
        */

        let opts = libbpf_sys::perf_buffer_opts {
            sample_cb: c_sample_cb,
            lost_cb: c_lost_cb,
            ctx: callback_struct_ptr as *mut _,
        };

        let ptr = unsafe {
            libbpf_sys::perf_buffer__new(self.map.fd(), self.pages as libbpf_sys::size_t, &opts)
        };
        let err = unsafe { libbpf_sys::libbpf_get_error(ptr as *const _) };
        if err != 0 {
            Err(Error::System(err as i32))
        } else {
            Ok(PerfBuffer {
                ptr,
                _cb_struct: unsafe { Box::from_raw(callback_struct_ptr) },
            })
        }
    }

    unsafe extern "C" fn call_sample_cb(ctx: *mut c_void, cpu: i32, data: *mut c_void, size: u32) {
        let callback_struct = ctx as *mut CbStruct<T>;

        if let Some(cb) = &mut (*callback_struct).sample_cb {

            // If context of type T is supplied by the user, pass it to the callback
            // as Some(T), otherwise pass it as None.
            // This order is necessary to unwrap the Option<Box<T>> down to an Option<T>
            if let Some(context) = &(*callback_struct).ctx {
                let c = context.as_ref();
                cb(Some(c), cpu, slice::from_raw_parts(data as *const u8, size as usize));
            } else {
                cb(None, cpu, slice::from_raw_parts(data as *const u8, size as usize));
            }

        }
    }

    unsafe extern "C" fn call_lost_cb(ctx: *mut c_void, cpu: i32, count: u64) {
        let callback_struct = ctx as *mut CbStruct<T>;

        if let Some(cb) = &mut (*callback_struct).lost_cb {
            cb(cpu, count);
        }
    }
}

/// Represents a special kind of [`Map`]. Typically used to transfer data between
/// [`Program`]s and userspace.
pub struct PerfBuffer<T> {
    ptr: *mut libbpf_sys::perf_buffer,
    // Hold onto the box so it'll get dropped when PerfBuffer is dropped
    _cb_struct: Box<CbStruct<T>>,
}

impl<T> PerfBuffer<T> {
    pub fn poll(&self, timeout: Duration) -> Result<()> {
        let ret = unsafe { libbpf_sys::perf_buffer__poll(self.ptr, timeout.as_millis() as i32) };
        if ret < 0 {
            Err(Error::System(-ret))
        } else {
            Ok(())
        }
    }
}

impl<T> Drop for PerfBuffer<T> {
    fn drop(&mut self) {
        unsafe {
            libbpf_sys::perf_buffer__free(self.ptr);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_power_of_two_slow(i: usize) -> bool {
        if i == 0 {
            return false;
        }

        let mut n = i;
        while n > 1 {
            if n & 0x01 as usize == 1 {
                return false;
            }
            n >>= 1;
        }
        true
    }

    #[test]
    fn test_is_power_of_two() {
        for i in 0..=256 {
            assert_eq!(is_power_of_two(i), is_power_of_two_slow(i));
        }
    }
}
