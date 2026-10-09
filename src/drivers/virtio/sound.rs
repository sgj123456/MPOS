//! virtio-sound, through the `virtio-drivers` crate.
//!
//! The device is QEMU's `virtio-sound` with an audio backend behind it - a PCI
//! function on the first machine, an MMIO window on the second, and the same
//! crate driver either way. The crate speaks the control queue - the jack and stream listings,
//! the parameter negotiation, the PCM commands - and hands over one
//! blocking call that moves bytes to the output stream. What is left here
//! is the stream face userland gets: `/dev/dsp`, the OSS name, fed with
//! raw PCM frames - and the OSS parameter ioctls that *belong* on it, so
//! the writer of a sound chooses its rate, channels and format, exactly
//! as it does on Linux, rather than inheriting a boot-time decision it
//! cannot ask about or change.
//!
//! The boot default is the shape the old fixed decision left behind -
//! S16LE, two channels, 44100 Hz or the nearest the device actually
//! supports - but it is a *default* now: `SNDCTL_DSP_SPEED`, `STEREO`,
//! `CHANNELS`, `SETFMT` and their cousins renegotiate the running stream
//! (stop, `SET_PARAMS`, start) and read back what the device settled on.
//! A parameter the stream cannot be rebuilt with answers `EINVAL`, and
//! `SNDCTL_DSP_SETFRAGMENT`, `SUBDIVIDE` and `NONBLOCK` answer it today,
//! named in `NOTES.md` - the honest refusal the caller's own fallback
//! reads, rather than a success that changed nothing.
//!
//! Reads answer end of input: the crate drives playback only, and a device
//! that cannot record should not pretend to have a silence it did not
//! capture.
//!
//! What the card is built *over* is the machine's and nothing above it names
//! one: [`VirtioSnd<T>`] takes the crate's `VirtIOSound` over whatever transport
//! the machine opened it on, and everything above - the OSS ioctls, the staging
//! buffer, the devfs node - is one copy reached through the [`SoundCard`]
//! contract (`NOTES.md` §13.27).

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::RefCell;

use critical_section::Mutex;
use virtio_drivers::device::sound::{PcmFormat, PcmRate, VirtIOSound};
use virtio_drivers::transport::Transport;

use super::SoundCard;
use super::hal::HalImpl;
#[cfg(target_arch = "x86_64")]
use super::hal::transport;
use crate::syscall::linux::abi::{
    AFMT_S16_LE, SNDCTL_DSP_CHANNELS, SNDCTL_DSP_GETBLKSIZE, SNDCTL_DSP_GETFMTS,
    SNDCTL_DSP_GETISPACE, SNDCTL_DSP_GETOSPACE, SNDCTL_DSP_NONBLOCK, SNDCTL_DSP_POST,
    SNDCTL_DSP_RESET, SNDCTL_DSP_SETFMT, SNDCTL_DSP_SETFRAGMENT, SNDCTL_DSP_SPEED,
    SNDCTL_DSP_STEREO, SNDCTL_DSP_SUBDIVIDE, SNDCTL_DSP_SYNC, oss_round_rate,
};

/// The modern device ID; virtio-sound never had a legacy form.
///
/// `0x1040 + VIRTIO_ID_SOUND`, and `VIRTIO_ID_SOUND` is **25** (`/usr/include/
/// linux/virtio_ids.h`), so the ID is `0x1059` - confirmed against the machine
/// this kernel boots on, where QEMU's `info pci` lists the card QEMU creates for
/// `-device virtio-sound-pci` as `1af4:1059`.
///
/// It used to read `0x1055`, which is device 21 and names no sound card. The
/// lookup therefore never matched, `init` took its `Probe::Absent` path - which
/// stays silent, because a machine with no card is a quieter boot, not a failed
/// one - and `/dev/dsp` was never registered. Every OSS call then answered
/// `ENOENT`/`EBADF` and the whole of `t-dsp` went red while the driver itself was
/// fine. A wrong ID on the *absent* branch is invisible by construction: the
/// bug's only trace is the device that never appears.
///
/// It names the PCI vendor's ID range, so it is the first machine's: the second
/// finds the same card by the `virtio,mmio` window its device tree names, whose
/// `DeviceID` is `25` (`VIRTIO_ID_SOUND`).
#[cfg(target_arch = "x86_64")]
const DEVICE_SOUND: &[u16] = &[0x1059];

/// One period of audio, and the buffer the device holds behind the stream.
/// Four KiB is a quarter second of S16 stereo at 44100 Hz - long enough that
/// the host is not pinged per frame, short enough that a `write` returns
/// before a listener could notice the gap.
const PERIOD_BYTES: u32 = 4096;
const BUFFER_BYTES: u32 = 4 * PERIOD_BYTES;

/// Every rate this driver can name, oldest first. `PcmRate`'s values are the
/// protocol's wire encoding, so "is this Hz one of the device's" and "which
/// enum do I hand the device" both run through this table rather than a cast.
const ALL_RATES: [PcmRate; 14] = [
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
];

pub struct VirtioSnd<T: Transport> {
    /// The device, behind a lock: every operation the crate offers takes
    /// `&mut`, and one stream serves however many writers `/dev/dsp` has. Which
    /// transport it arrived on is the type parameter and nothing else - see
    /// [`SoundCard`] for why the card is erased above this line.
    device: Mutex<RefCell<VirtIOSound<HalImpl, T>>>,
    /// The output stream the parameters are set on.
    stream: u32,
    /// The rates, in Hz, the device offered at probe - what a requested
    /// rate rounds onto, and the honest bound on `SNDCTL_DSP_SPEED`.
    rates: Vec<u32>,
    /// The channel range the stream accepts, the same kind of bound for
    /// `SNDCTL_DSP_CHANNELS`.
    ch_min: u8,
    ch_max: u8,
    /// What the stream is running: `(rate Hz, channels)`. The sample format
    /// is always little-endian S16 in this driver - the one format the
    /// device was required to have at probe - so it needs no half here.
    params: Mutex<RefCell<(u32, u8)>>,
    /// The DMA-able staging buffer, one period of bytes, allocated once at
    /// probe from the kernel's own heap.
    ///
    /// It exists because `write(2)` hands the driver a slice of the *caller's*
    /// memory (`handle_write` builds it straight over the user buffer), and the
    /// virtio crate turns a buffer's address into an address the device can
    /// read with `mm::virt_to_phys` (`drivers::virtio::hal`) - which is only
    /// defined on the direct map. Feeding it a user address is a kernel panic
    /// that ends the machine, and that is exactly what the first real `/dev/dsp`
    /// write did once the card was found. So the bytes are copied here, and the
    /// address handed to the device is always the kernel's.
    ///
    /// This is also Linux's shape: ALSA's dsp write path `copy_from_user`s into
    /// the DMA ring rather than mapping the caller's pages. The copy is one
    /// period at a time, so its size is a constant of this driver and not a
    /// number the caller steers.
    stage: Mutex<RefCell<Vec<u8>>>,
}

impl<T: Transport> VirtioSnd<T> {
    /// Hand `buf` to the output stream, in period-sized pieces, and return
    /// once the device has consumed all of it. A refusal is a short write:
    /// the `CharDevice` contract counts bytes, and a caller that sent ten
    /// and hears "six" knows to try again for the rest.
    ///
    /// `buf` is the caller's memory, so no byte of it goes to the device
    /// directly: each piece is copied into `stage` first, and `stage`'s address
    /// is the one the crate hands to the DMA path. See [`VirtioSnd::stage`].
    fn play(&self, buf: &[u8]) -> usize {
        let period = PERIOD_BYTES as usize;
        let mut sent = 0;
        while sent < buf.len() {
            let want = (buf.len() - sent).min(period);
            let accepted = critical_section::with(|cs| {
                let mut stage = self.stage.borrow(cs).borrow_mut();
                stage[..want].copy_from_slice(&buf[sent..sent + want]);
                let mut device = self.device.borrow(cs).borrow_mut();
                device.pcm_xfer(self.stream, &stage[..want]).is_ok()
            });
            if !accepted {
                break;
            }
            sent += want;
        }
        sent
    }

    /// The rate and channel count the stream runs at right now - what a
    /// parameter query answers, and the base a change is computed against.
    pub fn format(&self) -> (u32, u8) {
        critical_section::with(|cs| *self.params.borrow(cs).borrow())
    }

    /// Rebuild the stream at `rate` Hz and `channels`, rounding the rate onto
    /// the device's own list, and answer with what it actually settled on -
    /// the OSS contract, which reports rather than promises. A rate the
    /// device has no listing for, a channel count outside its range, or a
    /// device that refuses the new parameters all answer `EINVAL`, and the
    /// stream is left running its old ones.
    fn configure(&self, rate: u32, channels: u8) -> Result<(u32, u8), isize> {
        let Some(actual) = oss_round_rate(rate, &self.rates) else {
            return Err(crate::err::EINVAL);
        };
        if self.format() == (actual, channels) {
            return Ok((actual, channels));
        }
        if channels < self.ch_min || channels > self.ch_max {
            return Err(crate::err::EINVAL);
        }
        critical_section::with(|cs| {
            let mut device = self.device.borrow(cs).borrow_mut();
            let old = *self.params.borrow(cs).borrow();
            // The running stream must be stopped before its parameters are
            // re-issued - the protocol's own state machine - and the old
            // parameters go back on any refusal, so a failed negotiation
            // leaves the stream as playable as it found it.
            device.pcm_stop(self.stream).ok();
            let mut apply = |hz: u32, ch: u8| -> bool {
                let Some(rate) = hz_to_rate(hz) else {
                    return false;
                };
                device
                    .pcm_set_params(
                        self.stream,
                        BUFFER_BYTES,
                        PERIOD_BYTES,
                        virtio_drivers::device::sound::PcmFeatures::empty(),
                        ch,
                        PcmFormat::S16,
                        rate,
                    )
                    .is_ok()
                    && device.pcm_prepare(self.stream).is_ok()
                    && device.pcm_start(self.stream).is_ok()
            };
            if apply(actual, channels) {
                *self.params.borrow(cs).borrow_mut() = (actual, channels);
                Ok((actual, channels))
            } else {
                let _ = apply(old.0, old.1);
                Err(crate::err::EINVAL)
            }
        })
    }

    /// Bring up a card the machine has already opened: its first output stream,
    /// the rates and channel range it answers with, and the boot-default
    /// parameters.
    ///
    /// The machine's half is the `VirtIOSound` and how it was opened - a PCI
    /// function on the first machine, an MMIO window on the second
    /// (`super::mmio::sound_device`); everything below is the driver's and is the
    /// same on both. `Err` is a card that answered and then refused a question,
    /// which is worth a line: "no /dev/dsp" with a device present is a bug's
    /// footprint, not a machine's absence.
    pub fn new(mut device: VirtIOSound<HalImpl, T>) -> Result<Arc<Self>, Probe> {
        let stream = device
            .output_streams()
            .ok()
            .and_then(|v| v.first().copied())
            .ok_or(Probe::NoStream)?;
        // The rates the device lists, in Hz: the bounds `SNDCTL_DSP_SPEED`
        // rounds onto. A rate this driver has no constant for is still a
        // rate on the wire, but not one this driver could ever name in
        // `SET_PARAMS`, so it does not enter the list.
        let supported = device.rates_supported(stream).map_err(|_| Probe::Rates)?;
        let rates: Vec<u32> = ALL_RATES
            .iter()
            .filter(|rate| supported.contains((**rate).into()))
            .map(|rate| rate_hz(*rate))
            .collect();
        if rates.is_empty() {
            return Err(Probe::Rates);
        }

        let formats = device
            .formats_supported(stream)
            .map_err(|_| Probe::Formats)?;
        if !formats.contains(PcmFormat::S16.into()) {
            // S16 is the one format this driver's writers are promised; a
            // device without it would need a converter, and inventing one
            // here is how audio drivers grow three thousand lines.
            return Err(Probe::Formats);
        }
        let (ch_min, ch_max) = device
            .channel_range_supported(stream)
            .map(|range| (*range.start(), *range.end()))
            .map_err(|_| Probe::Channels)?;

        // The boot default is the old fixed choice - 44100 Hz or the
        // nearest, and two channels where the device allows it - but it is
        // a starting point now, not the last word: the `SNDCTL_DSP_*`
        // ioctls move it at any writer's asking.
        let rate = oss_round_rate(44100, &rates).ok_or(Probe::Rates)?;
        let channels = if ch_min > 2 { ch_min } else { 2 }.min(ch_max);
        let wire_rate = hz_to_rate(rate).ok_or(Probe::Rates)?;

        device
            .pcm_set_params(
                stream,
                BUFFER_BYTES,
                PERIOD_BYTES,
                virtio_drivers::device::sound::PcmFeatures::empty(),
                channels,
                PcmFormat::S16,
                wire_rate,
            )
            .map_err(|_| Probe::Params)?;
        device.pcm_prepare(stream).map_err(|_| Probe::Params)?;
        device.pcm_start(stream).map_err(|_| Probe::Params)?;

        Ok(Arc::new(VirtioSnd {
            device: Mutex::new(RefCell::new(device)),
            stream,
            rates,
            ch_min,
            ch_max,
            params: Mutex::new(RefCell::new((rate, channels))),
            stage: Mutex::new(RefCell::new(alloc::vec![0u8; PERIOD_BYTES as usize])),
        }))
    }
}

/// The erase point: each method is the inherent one above, so the devfs node,
/// the OSS ioctls and the boot's probe line name the *contract* and never a
/// transport. `T: Send` is what the card's `Arc` needs to be shareable, and it
/// is the whole of what the type parameter adds here.
impl<T: Transport + Send> SoundCard for VirtioSnd<T> {
    fn format(&self) -> (u32, u8) {
        self.format()
    }

    fn configure(&self, rate: u32, channels: u8) -> Result<(u32, u8), isize> {
        self.configure(rate, channels)
    }

    fn play(&self, buf: &[u8]) -> usize {
        self.play(buf)
    }
}

/// The `PcmRate` a frequency names - the inverse of [`rate_hz`], because the
/// ioctl hands back a rounding of a number the caller wrote, and re-issuing
/// parameters needs the wire form of it again.
fn hz_to_rate(hz: u32) -> Option<PcmRate> {
    ALL_RATES.into_iter().find(|rate| rate_hz(*rate) == hz)
}

/// How far the probe got before the card stopped answering. `Absent` is a
/// machine with no sound at all - silent, as a quieter boot should be; the
/// rest name a device that was found and then refused, which is a bug worth
/// the one log line it costs.
pub enum Probe {
    Absent,
    Init,
    NoStream,
    Rates,
    Formats,
    Channels,
    Params,
}

impl core::fmt::Display for Probe {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let words = match self {
            Probe::Absent => "absent",
            Probe::Init => "device refused initialisation",
            Probe::NoStream => "no output stream",
            Probe::Rates => "no rate list",
            Probe::Formats => "no S16_LE format",
            Probe::Channels => "no channel range",
            Probe::Params => "SET_PARAMS refused",
        };
        f.write_str(words)
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
static GLOBAL: Mutex<RefCell<Option<Arc<dyn SoundCard>>>> = Mutex::new(RefCell::new(None));

/// Make `card` the machine's sound card, and hand it back.
///
/// The one door in: the first machine arrives through [`init_global`], the
/// second with the card its MMIO window opened, and everything above - the devfs
/// node, the OSS ioctls, the boot's line - is the same either way.
pub fn install(card: Arc<dyn SoundCard>) -> Arc<dyn SoundCard> {
    critical_section::with(|cs| {
        *GLOBAL.borrow(cs).borrow_mut() = Some(card.clone());
    });
    crate::drivers::devices::publish("virtio-sound", publish_nodes);
    card
}

/// Publish `/dev/dsp` for a device set built after this driver came up -
/// registered by [`install`], not named by `devices::standard_devfs`.
fn publish_nodes(dev: &crate::vfs::devfs::DevFs) {
    if let Some(snd) = global() {
        dev.register("dsp", Arc::new(DspDev::new(snd)));
    }
}

/// Bring up the card a machine opened, record it under the name userland will
/// open, and answer it - or say why the machine has none.
///
/// `opened` is the crate's device as the machine's bus handed it back, or the
/// reason there is none: `Err(Probe::Absent)` is no card at all, which is a
/// quieter boot rather than a failed one, while any other refusal is a card that
/// answered and then would not play, which is worth a line.
pub fn install_probed<T: Transport + Send + 'static>(
    opened: Result<VirtIOSound<HalImpl, T>, Probe>,
) -> Option<Arc<dyn SoundCard>> {
    match opened.and_then(VirtioSnd::new) {
        Ok(card) => Some(install(card)),
        Err(Probe::Absent) => None,
        Err(other) => {
            crate::println!("virtio-sound: {other}");
            None
        }
    }
}

/// The first machine's door: probe the PCI bus for the card and bring it up.
#[cfg(target_arch = "x86_64")]
pub fn init_global() -> Option<Arc<dyn SoundCard>> {
    let opened = match transport(DEVICE_SOUND) {
        Some(transport) => VirtIOSound::<HalImpl, _>::new(transport).map_err(|_| Probe::Init),
        None => Err(Probe::Absent),
    };
    install_probed(opened)
}

/// The card, if the machine has one.
pub fn global() -> Option<Arc<dyn SoundCard>> {
    critical_section::with(|cs| GLOBAL.borrow(cs).borrow().clone())
}

/// The `CharDevice` face of the stream, for the devfs builder: writes play,
/// and a read is end of input - the crate drives playback only.
pub struct DspDev(Arc<dyn SoundCard>);

impl DspDev {
    pub fn new(dev: Arc<dyn SoundCard>) -> Self {
        DspDev(dev)
    }
}

/// Read one host-order `int` from the caller's buffer. The ioctl dispatcher
/// has already checked the word against the request's size and direction;
/// every OSS parameter request here names a four-byte `int`.
fn read_int(p: usize) -> i32 {
    unsafe { (p as *const i32).read_unaligned() }
}

/// The same, writing.
fn write_int(p: usize, value: i32) {
    unsafe { (p as *mut i32).write_unaligned(value) }
}

impl crate::vfs::devfs::CharDevice for DspDev {
    fn read(&self, _buf: &mut [u8]) -> crate::vfs::VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, buf: &[u8]) -> usize {
        self.0.play(buf)
    }

    /// The OSS parameter face of the stream: `n_tty`-of-audio, in that every
    /// number that shapes the bytes is one the *writer* asks about and
    /// changes, and the answer is what the device actually runs. A parameter
    /// set to zero is the family's own "tell me", and never a value; a set
    /// rounds onto what the device lists and reports the rounding back.
    fn ioctl(&self, request: u32, arg: usize) -> Option<isize> {
        match request {
            // Reset and sync: writes hand their bytes to the device
            // synchronously, so what this driver still holds when either is
            // asked for is nothing a `write` has not already delivered.
            SNDCTL_DSP_RESET | SNDCTL_DSP_SYNC | SNDCTL_DSP_POST => Some(0),
            // The three this stream cannot honour today: fragment geometry
            // is fixed at `SET_PARAMS` time by this driver's buffer, and
            // "non-blocking" would promise a write shape the device's
            // blocking hand-off does not have. EINVAL is the refusal the
            // family itself reserves for a parameter it will not take, and
            // each is named in `NOTES.md` with what it would need.
            SNDCTL_DSP_SUBDIVIDE | SNDCTL_DSP_SETFRAGMENT | SNDCTL_DSP_NONBLOCK => {
                Some(crate::err::EINVAL)
            }
            SNDCTL_DSP_SPEED => {
                let want = read_int(arg);
                let (rate, channels) = self.0.format();
                if want == 0 {
                    write_int(arg, rate as i32);
                    return Some(0);
                }
                match self.0.configure(want.max(0) as u32, channels) {
                    Ok((actual, _)) => {
                        write_int(arg, actual as i32);
                        Some(0)
                    }
                    Err(e) => Some(e),
                }
            }
            SNDCTL_DSP_STEREO => {
                let want = read_int(arg);
                let (rate, _) = self.0.format();
                let channels: u8 = match want {
                    0 => 1,
                    1 => 2,
                    _ => return Some(crate::err::EINVAL),
                };
                match self.0.configure(rate, channels) {
                    Ok(_) => {
                        write_int(arg, want);
                        Some(0)
                    }
                    Err(e) => Some(e),
                }
            }
            SNDCTL_DSP_CHANNELS => {
                let want = read_int(arg);
                let (rate, channels) = self.0.format();
                if want == 0 {
                    write_int(arg, channels as i32);
                    return Some(0);
                }
                if want > u8::MAX as i32 {
                    return Some(crate::err::EINVAL);
                }
                match self.0.configure(rate, want as u8) {
                    Ok((_, actual)) => {
                        write_int(arg, actual as i32);
                        Some(0)
                    }
                    Err(e) => Some(e),
                }
            }
            SNDCTL_DSP_GETBLKSIZE => {
                write_int(arg, PERIOD_BYTES as i32);
                Some(0)
            }
            SNDCTL_DSP_SETFMT => {
                let want = read_int(arg);
                if want == 0 || want & AFMT_S16_LE as i32 != 0 {
                    // Zero is the query; S16LE is the one format this
                    // device was required to speak, and the stream always
                    // runs it.
                    write_int(arg, AFMT_S16_LE as i32);
                    Some(0)
                } else {
                    // A format the stream cannot run is refused, and the
                    // caller's number is left alone - what OSS answers zero
                    // for, this kernel will not paper over with a success.
                    write_int(arg, 0);
                    Some(crate::err::EINVAL)
                }
            }
            SNDCTL_DSP_GETFMTS => {
                write_int(arg, AFMT_S16_LE as i32);
                Some(0)
            }
            SNDCTL_DSP_GETOSPACE => {
                // `struct audio_buf_info`: the geometry the stream is built
                // with, and the space a write can be handed into. The
                // hand-off itself is synchronous, so `bytes` means "what one
                // write may present", not "what is buffered on the card" -
                // a writer pacing on it never blocks longer than one period.
                let ints = [
                    (BUFFER_BYTES / PERIOD_BYTES) as i32, // fragments
                    (BUFFER_BYTES / PERIOD_BYTES) as i32, // fragstotal
                    PERIOD_BYTES as i32,                  // fragsize
                    BUFFER_BYTES as i32,                  // bytes
                ];
                write_info(arg, &ints);
                Some(0)
            }
            SNDCTL_DSP_GETISPACE => {
                // There is no captured silence to report - reads answer end
                // of input - so the honest geometry is an empty queue.
                write_info(arg, &[0, 0, 0, 0]);
                Some(0)
            }
            _ => None,
        }
    }
}

/// `struct audio_buf_info` is four host-order `int`s; the dispatcher has
/// checked the buffer is the sixteen bytes this writes.
fn write_info(p: usize, ints: &[i32; 4]) {
    unsafe {
        for (i, value) in ints.iter().enumerate() {
            (p as *mut i32).add(i).write_unaligned(*value);
        }
    }
}
