//! COM1: legacy 16550 on I/O ports 0x3f8..0x3ff.

use core::cell::RefCell;

use critical_section::Mutex;
use uart_16550::{Config, Uart16550};

const COM1_BASE_PORT: u16 = 0x3f8;

type PortUart = Uart16550<uart_16550::backend::PioBackend>;

static SERIAL: Mutex<RefCell<Option<PortUart>>> = Mutex::new(RefCell::new(None));

pub fn init() {
    // SAFETY: COM1 is ours alone, and it stays valid for the life of the kernel.
    let mut uart = unsafe { PortUart::new_port(COM1_BASE_PORT) }.expect("COM1 base port invalid");
    uart.init(Config::default())
        .expect("COM1 rejected the default line config");

    critical_section::with(|cs| {
        *SERIAL.borrow(cs).borrow_mut() = Some(uart);
    });
}

pub fn write_bytes(bytes: &[u8]) {
    critical_section::with(|cs| {
        if let Some(uart) = SERIAL.borrow(cs).borrow_mut().as_mut() {
            uart.send_bytes_exact(bytes);
        }
    });
}
