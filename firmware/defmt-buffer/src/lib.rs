//! [`defmt`](https://github.com/knurling-rs/defmt) global logger for custom transports.
//!
//! To use this crate, link to it by importing it somewhere in your project.
//!
//! ```
//! // src/main.rs or src/bin/my-app.rs
//! use defmt_buffer as _;
//! ```

#![no_std]

use core::{
    ptr::NonNull,
    sync::atomic::{AtomicBool, Ordering},
};

use cortex_m::register;

use bbqueue::{
    self, consts,
    framed::{FrameGrantW, FrameProducer},
};

#[defmt::global_logger]
struct Logger;

struct Writer {
    written: usize,
    grant: FrameGrantW<'static, consts::U16384>,
}

impl defmt::Write for Writer {
    fn write(&mut self, bytes: &[u8]) {
        let buf = &mut self.grant;
        let available = buf.len() - self.written;
        if bytes.len() <= available {
            buf[self.written..self.written + bytes.len()].copy_from_slice(bytes);
            self.written += bytes.len();
        } else {
            self.written = buf.len(); // Ensure no more data is written
        }
    }
}

// TODO: Are there any cortex-m that have more than 16?
const PRIO_LEVELS: usize = 16;

static mut WRITERS: [Option<Writer>; PRIO_LEVELS] = [
    None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None,
];
static mut PRODS: [Option<FrameProducer<'static, consts::U16384>>; PRIO_LEVELS] = [
    None, None, None, None, None, None, None, None, None, None, None, None, None, None, None, None,
];

unsafe impl defmt::Logger for Logger {
    fn acquire() -> Option<NonNull<dyn defmt::Write>> {
        let primask = register::primask::read();
        let basepri = register::basepri::read();
        let priority = if primask == register::primask::Primask::Active {
            0
        } else {
            basepri as usize >> 4 // levels use the most significant bits
        };
        debug_assert!(priority < PRIO_LEVELS);
        // SAFETY: Access to producers is separated by thread priority
        let prod = unsafe { PRODS[priority.min(PRIO_LEVELS)].as_mut() }?;
        let grant = prod.grant(1024).ok()?;
        let writer = Writer { grant, written: 0 };
        // Store in WRITERS just so we can give out a pointer
        unsafe {
            WRITERS[priority] = Some(writer);
            Some(NonNull::from(WRITERS[priority].as_mut().unwrap()))
        }
    }

    unsafe fn release(_: NonNull<dyn defmt::Write>) {
        let primask = register::primask::read();
        let basepri = register::basepri::read();
        let priority = if primask == register::primask::Primask::Active {
            0
        } else {
            basepri as usize >> 4 // levels use the most significant bits
        };
        debug_assert!(priority < PRIO_LEVELS);
        // SAFETY: Access to writers is separated by thread priority
        if let Some(writer) = WRITERS[priority].take() {
            if writer.written < writer.grant.len() {
                writer.grant.commit(writer.written);
            } else {
                // grant wasn't large enough, discard
                // TODO: Report
                writer.grant.commit(0);
            }
        } else {
            debug_assert!(false, "Logger for priority {} already released", priority);
        }
    }
}

// https://github.com/jamesmunns/bbqueue/pull/87 would let this handle different size buffers
pub fn init(producers: [FrameProducer<'static, consts::U16384>; PRIO_LEVELS]) {
    static INIT: AtomicBool = AtomicBool::new(false);
    if INIT.swap(true, Ordering::SeqCst) {
        panic!("init called twice");
    }
    for (i, prod) in core::array::IntoIter::new(producers).enumerate() {
        unsafe {
            PRODS[i] = Some(prod);
        }
    }
}
