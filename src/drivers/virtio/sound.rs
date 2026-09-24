//! virtio-sound, through the `virtio-drivers` crate.
//!
//! The device is QEMU's `virtio-sound-pci` with an audio backend behind
//! it. The crate speaks the control queue - the jack and stream listings,
//! the parameter negotiation, the PCM commands - and hands over one
//! blocking call that moves bytes to the output stream. What is left here
//! is the choice of stream and format, made once at boot, and the stream
//! face userland gets: `/dev/dsp`, the OSS name, fed with raw PCM frames.
//!
//! The format is fixed rather than negotiated per open, and that is a
//! decision, not an omission: this kernel's userland has no ALSA and no
//! mixer to ask, so a fixed S16LE stereo stream at 44100 Hz - or the
//! nearest the device actually supports - is the one shape every writer
//! can target. The numbers the device accepted are what the boot log
//! prints; a program that wants to know what it is writing to reads the
//! log line, the same way it reads which disk it found.
//!
//! Reads answer end of input: the crate drives playback only, and a device
//! that cannot record should not pretend to have a silence it did not
//! capture.

use alloc::sync::Arc;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::sound::{PcmFormat, PcmRate, VirtIOSound};
use virtio_drivers::transport::pci::PciTransport;

use super::hal::{HalImpl, transport};

/// The modern device ID; virtio-sound never had a legacy form.
const DEVICE_SOUND: &[u16] = &[0x1055];

/// One period of audio, and the buffer the device holds behind the stream.
/// Four KiB is a quarter second of S16 stereo at 44100 Hz - long enough that
/// the host is not pinged per frame, short enough that a `write` returns
/// before a listener could notice the gap.
const PERIOD_BYTES: u32 = 4096;
const BUFFER_BYTES: u32 = 4 * PERIOD_BYTES;

pub struct VirtioSnd {
    /// The device, behind a lock: every operation the crate offers takes
    /// `&mut`, and one stream serves however many writers `/dev/dsp` has.
    device: Mutex<RefCell<VirtIOSound<HalImpl, PciTransport>>>,
    /// The output stream the parameters were set on.
    stream: u32,
    /// The format the device accepted, for the boot log and this file's
    /// documentation. The pair is (rate, channels); the sample format is
    /// always little-endian S16 in this driver, per the header's contract.
    rate: PcmRate,
    channels: u8,
}

impl VirtioSnd {
    /// Hand `buf` to the output stream, in period-sized pieces, and return
    /// once the device has consumed all of it. A refusal is a short write:
    /// the `CharDevice` contract counts bytes, and a caller that sent ten
    /// and hears "six" knows to try again for the rest.
    fn play(&self, buf: &[u8]) -> usize {
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            if device.pcm_xfer(self.stream, buf).is_ok() {
                buf.len()
            } else {
                0
            }
        })
    }

    /// The rate and channel count the device accepted - the two numbers a
    /// writer has to know to shape its frames. The rate is in the units a
    /// reader of "44100 Hz" expects, not the wire encoding the enum
    /// carries.
    pub fn format(&self) -> (u32, u8) {
        (rate_hz(self.rate), self.channels)
    }

    /// Probe the PCI bus and bring up the first virtio-sound device, with
    /// its first output stream armed. `None` when there is no card, or the
    /// card has no stream to play through - a machine with no sound is a
    /// quieter boot, not a failed one.
    pub fn init() -> Option<Arc<Self>> {
        let mut device = VirtIOSound::<HalImpl, _>::new(transport(DEVICE_SOUND)?).ok()?;

        let stream = device.output_streams().ok()?.first().copied()?;
        // The rate the device supports that is closest to the one a writer
        // expects: 44100 exactly when the host offers it, otherwise the
        // highest of the rates this driver has a name for - a device that
        // only plays 48 kHz should not be handed 44.1 and silently resampled
        // by nobody.
        let rates = device.rates_supported(stream).ok()?;
        let rate = [
            PcmRate::Rate44100,
            PcmRate::Rate48000,
            PcmRate::Rate96000,
            PcmRate::Rate32000,
            PcmRate::Rate16000,
        ]
        .into_iter()
        .find(|rate| rates.contains((*rate).into()))
        .or_else(|| {
            // Whatever the device does offer, take the best of it. A rate
            // this driver has no constant for is still a rate, and refusing
            // to play at all is the one answer worse than an unexpected one.
            [
                PcmRate::Rate5512,
                PcmRate::Rate8000,
                PcmRate::Rate11025,
                PcmRate::Rate16000,
                PcmRate::Rate22050,
                PcmRate::Rate32000,
                PcmRate::Rate44100,
                PcmRate::Rate48000,
                PcmRate::Rate64000,
                PcmRate::Rate88200,
                PcmRate::Rate96000,
                PcmRate::Rate176400,
                PcmRate::Rate192000,
                PcmRate::Rate384000,
            ]
            .into_iter()
            .filter(|rate| rates.contains((*rate).into()))
            .max_by_key(|rate| *rate as u8)
        })?;

        let formats = device.formats_supported(stream).ok()?;
        if !formats.contains(PcmFormat::S16.into()) {
            // S16 is the one format this driver's writers are promised; a
            // device without it would need a converter, and inventing one
            // here is how audio drivers grow three thousand lines.
            return None;
        }
        let (min_channels, max_channels) = device
            .channel_range_supported(stream)
            .map(|range| (*range.start(), *range.end()))
            .ok()?;
        let channels = if min_channels > 2 { min_channels } else { 2 }.min(max_channels);

        device
            .pcm_set_params(
                stream,
                BUFFER_BYTES,
                PERIOD_BYTES,
                virtio_drivers::device::sound::PcmFeatures::empty(),
                channels,
                PcmFormat::S16,
                rate,
            )
            .ok()?;
        device.pcm_prepare(stream).ok()?;
        device.pcm_start(stream).ok()?;

        Some(Arc::new(VirtioSnd {
            device: Mutex::new(RefCell::new(device)),
            stream,
            rate,
            channels,
        }))
    }
}

/// The rate a `PcmRate` names, in Hz. The enum's own values are the wire
/// encoding - a `u8` the protocol spells, not a frequency - so the boot
/// log and anyone reading it want this table, not a cast.
fn rate_hz(rate: PcmRate) -> u32 {
    match rate {
        PcmRate::Rate5512 => 5512,
        PcmRate::Rate8000 => 8000,
        PcmRate::Rate11025 => 11025,
        PcmRate::Rate16000 => 16000,
        PcmRate::Rate22050 => 22050,
        PcmRate::Rate32000 => 32000,
        PcmRate::Rate44100 => 44100,
        PcmRate::Rate48000 => 48000,
        PcmRate::Rate64000 => 64000,
        PcmRate::Rate88200 => 88200,
        PcmRate::Rate96000 => 96000,
        PcmRate::Rate176400 => 176400,
        PcmRate::Rate192000 => 192000,
        PcmRate::Rate384000 => 384000,
    }
}

/// The device the boot found, if it found one.
static GLOBAL: Mutex<RefCell<Option<Arc<VirtioSnd>>>> = Mutex::new(RefCell::new(None));

/// Probe for the card and record it under the name userland will open.
pub fn init_global() -> Option<Arc<VirtioSnd>> {
    let dev = VirtioSnd::init()?;
    critical_section::with(|cs| {
        let mut slot = GLOBAL.borrow(cs).borrow_mut();
        *slot = Some(dev.clone());
    });
    Some(dev)
}

/// The card, if the machine has one.
pub fn global() -> Option<Arc<VirtioSnd>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// The `CharDevice` face of the stream, for the devfs builder: writes play,
/// and a read is end of input - the crate drives playback only.
pub struct DspDev(Arc<VirtioSnd>);

impl DspDev {
    pub fn new(dev: Arc<VirtioSnd>) -> Self {
        DspDev(dev)
    }
}

impl crate::vfs::devfs::CharDevice for DspDev {
    fn read(&self, _buf: &mut [u8]) -> usize {
        0
    }

    fn write(&self, buf: &[u8]) -> usize {
        self.0.play(buf)
    }
}
